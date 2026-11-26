/// Schema-based SQL completion (tables / columns / aliases), CYBERNEURA-DEV-1355.
///
/// Replaces the schema completion of @codemirror/lang-sql, which only offered columns after
/// `table.` / `alias.`, resolved names case-sensitively, did not see aliases outside
/// `SELECT ... FROM`, and could not read SQL Server's `[dbo].[users]` keys. This module:
///
/// - offers the columns of the tables the statement references, without a qualifier
///   (`SELECT na| FROM users`, `WHERE st|`, `INSERT INTO t (c|`, `UPDATE t SET c|`)
/// - resolves `alias.`, `table.`, `schema.` and `schema.table.` case-insensitively
/// - ranks tables first right after FROM / JOIN / UPDATE / INTO / TABLE, columns first elsewhere
///
/// It is pure (no CodeMirror imports) so that it can be tested with node. SqlEditor wraps
/// `completeSql` into a CodeMirror completion source.

import { stripLeadingComments } from "../batchSession.ts";

export type SqlCompletionDialect = "mysql" | "postgres" | "sqlite" | "mssql" | "other";

export interface SqlCompletionOption {
  label: string;
  /// CodeMirror completion type (decides the icon)
  type: "type" | "property" | "constant" | "namespace";
  detail?: string;
  /// Text to insert when it differs from the label (quoted / qualified names)
  apply?: string;
  boost?: number;
}

export interface SqlCompletionResult {
  from: number;
  options: SqlCompletionOption[];
}

interface ModelTable {
  /// Key in the schema map (what the TABLES pane inserts: `users`, `sales.orders`, `[dbo].[users]`)
  key: string;
  schema: string | null;
  name: string;
  columns: string[];
}

export interface SqlCompletionModel {
  dialect: SqlCompletionDialect;
  tables: ModelTable[];
  /// lowercased table name -> tables (several schemas can hold the same name, and a
  /// case-sensitive catalog can hold names that differ only in case)
  byName: Map<string, ModelTable[]>;
  /// lowercased schema name -> schema name as written in the catalog
  schemas: Map<string, string>;
  /// Options for every table / schema at the top level, built once (the catalog can hold
  /// thousands of tables and the source runs while typing). `tableFirst` is the same list
  /// ranked for a table position (after FROM / JOIN / ...)
  topLevel: SqlCompletionOption[];
  topLevelTableFirst: SqlCompletionOption[];
}

/// The document is scanned around the cursor only; beyond this many characters on each side the
/// rest is ignored (a query file can carry megabytes of Run and Log comments). The caller slices
/// the document with `completionWindow` so that the whole text is never materialized.
export const SCAN_WINDOW_CHARS = 100_000;

/// The part of a document of `length` characters to hand to `completeSql` for the cursor `pos`
export const completionWindow = (
  length: number,
  pos: number,
): { from: number; to: number } => ({
  from: Math.max(0, pos - SCAN_WINDOW_CHARS),
  to: Math.min(length, pos + SCAN_WINDOW_CHARS),
});

/// Upper limit on column options of one completion (a statement joining very wide tables)
const MAX_COLUMN_OPTIONS = 5_000;

const WORD_CHAR = /[\p{L}\p{N}_$]/u;

/// What a completion session stays valid for while typing (CodeMirror re-filters the same
/// options instead of calling the source again)
export const SQL_COMPLETION_VALID_FOR = /^[\p{L}\p{N}_$]*$/u;

export const dialectForEngine = (engine: string | null): SqlCompletionDialect => {
  switch ((engine ?? "").toLowerCase()) {
    case "mysql":
    case "mariadb":
      return "mysql";
    case "postgres":
    case "postgresql":
    case "duckdb":
      return "postgres";
    case "sqlite":
    case "sqlite3":
      return "sqlite";
    case "mssql":
    case "sqlserver":
      return "mssql";
    default:
      return "other";
  }
};

/// Split a schema map key into its parts. The backend writes keys as `table` or `schema.table`
/// with the names as they are in the catalog (`sales.orders` -> [sales, orders]); only SQL
/// Server brackets every part (`[dbo].[my.table]` -> [dbo, my.table], `]]` is an escaped `]`).
export const splitQualifiedKey = (key: string, dialect: SqlCompletionDialect): string[] => {
  if (dialect !== "mssql") {
    return key.split(".");
  }
  const parts: string[] = [];
  let current = "";
  let i = 0;
  while (i < key.length) {
    const ch = key[i];
    if (ch === "[") {
      i += 1;
      while (i < key.length) {
        if (key[i] === "]") {
          if (key[i + 1] === "]") {
            current += "]";
            i += 2;
            continue;
          }
          i += 1;
          break;
        }
        current += key[i];
        i += 1;
      }
    } else if (ch === ".") {
      parts.push(current);
      current = "";
      i += 1;
    } else {
      current += ch;
      i += 1;
    }
  }
  parts.push(current);
  return parts;
};

export const buildCompletionModel = (
  map: Record<string, string[]> | null,
  dialect: SqlCompletionDialect,
): SqlCompletionModel | null => {
  if (!map) {
    return null;
  }
  const tables: ModelTable[] = [];
  const byName = new Map<string, ModelTable[]>();
  const schemas = new Map<string, string>();
  for (const key of Object.keys(map)) {
    const parts = splitQualifiedKey(key, dialect);
    const name = parts[parts.length - 1];
    if (!name) {
      continue;
    }
    const schema = parts.length > 1 ? parts[parts.length - 2] : null;
    const table: ModelTable = { key, schema, name, columns: map[key] ?? [] };
    tables.push(table);
    const lower = name.toLowerCase();
    const sameName = byName.get(lower);
    if (sameName) {
      sameName.push(table);
    } else {
      byName.set(lower, [table]);
    }
    if (schema && !schemas.has(schema.toLowerCase())) {
      schemas.set(schema.toLowerCase(), schema);
    }
  }
  const topLevelFor = (tableBoost: number): SqlCompletionOption[] => {
    const options: SqlCompletionOption[] = [];
    for (const table of tables) {
      options.push(
        withApply(
          {
            label: table.name,
            type: "type",
            boost: tableBoost,
            ...(table.schema ? { detail: table.schema } : {}),
          },
          tableInsertText(table, dialect),
        ),
      );
    }
    for (const schema of schemas.values()) {
      options.push(
        withApply({ label: schema, type: "namespace", boost: -1 }, quoteIfNeeded(schema, dialect)),
      );
    }
    return options;
  };
  return {
    dialect,
    tables,
    byName,
    schemas,
    topLevel: topLevelFor(0),
    topLevelTableFirst: topLevelFor(20),
  };
};

type TokenKind = "word" | "quoted" | "string" | "comment" | "punct";

interface Token {
  kind: TokenKind;
  /// word: the text. quoted: the identifier without quotes. punct: the character
  text: string;
  from: number;
  to: number;
  /// string / comment / quoted: whether it was closed
  closed: boolean;
}

/// A small SQL lexer: just enough to tell identifiers from strings and comments
const tokenize = (text: string, dialect: SqlCompletionDialect): Token[] => {
  const tokens: Token[] = [];
  const n = text.length;
  let i = 0;
  while (i < n) {
    const ch = text[i];
    if (ch === " " || ch === "\t" || ch === "\n" || ch === "\r") {
      i += 1;
      continue;
    }
    const start = i;
    if (
      (ch === "-" && text[i + 1] === "-") ||
      (ch === "#" && dialect === "mysql")
    ) {
      const newline = text.indexOf("\n", i);
      i = newline < 0 ? n : newline;
      // Never "closed": the cursor at the end of a line comment is still inside it
      tokens.push({ kind: "comment", text: "", from: start, to: i, closed: false });
      continue;
    }
    if (ch === "/" && text[i + 1] === "*") {
      // PostgreSQL block comments nest; the others end at the first `*/`
      const nests = dialect === "postgres";
      let depth = 1;
      i += 2;
      while (i < n && depth > 0) {
        if (text[i] === "*" && text[i + 1] === "/") {
          depth -= 1;
          i += 2;
        } else if (nests && text[i] === "/" && text[i + 1] === "*") {
          depth += 1;
          i += 2;
        } else {
          i += 1;
        }
      }
      tokens.push({ kind: "comment", text: "", from: start, to: i, closed: depth === 0 });
      continue;
    }
    const isBracketQuote = ch === "[" && dialect === "mssql";
    if (ch === "'" || ch === '"' || ch === "`" || isBracketQuote) {
      const close = isBracketQuote ? "]" : ch;
      // MySQL reads "..." as a string unless ANSI_QUOTES is on; treating it as an identifier
      // would only matter inside the literal, where nothing is offered either way
      const kind: TokenKind = ch === "'" ? "string" : "quoted";
      let value = "";
      let closed = false;
      i += 1;
      while (i < n) {
        if (text[i] === close) {
          if (text[i + 1] === close) {
            value += close;
            i += 2;
            continue;
          }
          i += 1;
          closed = true;
          break;
        }
        if (text[i] === "\\" && ch === "'" && dialect === "mysql" && i + 1 < n) {
          value += text[i + 1];
          i += 2;
          continue;
        }
        value += text[i];
        i += 1;
      }
      tokens.push({ kind, text: value, from: start, to: i, closed });
      continue;
    }
    if (ch === "$" && dialect === "postgres") {
      // Dollar-quoted string: $$ ... $$ or $tag$ ... $tag$. Its body is a literal (often a
      // function body with semicolons), not SQL of this statement
      const tag = /^\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$/.exec(text.slice(i, i + 64));
      if (tag) {
        const end = text.indexOf(tag[0], i + tag[0].length);
        i = end < 0 ? n : end + tag[0].length;
        tokens.push({ kind: "string", text: "", from: start, to: i, closed: end >= 0 });
        continue;
      }
    }
    if (WORD_CHAR.test(ch)) {
      i += 1;
      while (i < n && WORD_CHAR.test(text[i])) {
        i += 1;
      }
      tokens.push({ kind: "word", text: text.slice(start, i), from: start, to: i, closed: true });
      continue;
    }
    // Surrogate pairs that are not word characters (emoji) stay one token
    const size = text.codePointAt(i)! > 0xffff ? 2 : 1;
    i += size;
    tokens.push({ kind: "punct", text: text.slice(start, i), from: start, to: i, closed: true });
  }
  return tokens;
};

/// Keywords after which a table name is expected
const TABLE_KEYWORDS = new Set(["from", "join", "update", "into", "table", "describe", "desc"]);

/// Words that cannot be a table alias (what may follow a table reference)
const NOT_AN_ALIAS = new Set([
  "where", "on", "join", "inner", "left", "right", "full", "cross", "natural", "outer",
  "using", "set", "group", "order", "having", "limit", "union", "intersect", "except",
  "values", "select", "window", "offset", "fetch", "for", "returning", "straight_join",
  "lateral", "partition", "tablesample", "with", "as", "default", "when", "then", "do",
  "force", "use", "ignore", "add", "drop", "alter", "rename", "to", "like", "if", "and", "or",
  "not", "is", "in", "between", "exists", "case", "end", "only", "output", "option", "qualify",
]);

export interface TableReference {
  /// Name parts as written (`sales.orders` -> [sales, orders])
  path: string[];
  alias: string | null;
}

const isIdentifier = (token: Token | undefined): token is Token =>
  !!token && (token.kind === "quoted" || token.kind === "word");

const isKeyword = (token: Token | undefined, word: string): boolean =>
  !!token && token.kind === "word" && token.text.toLowerCase() === word;

/// Collect the tables a statement references (FROM / JOIN / UPDATE / INTO, at any nesting
/// level — a column of a subquery's table is still a better guess than none)
const collectReferences = (tokens: Token[]): TableReference[] => {
  const references: TableReference[] = [];
  let i = 0;
  while (i < tokens.length) {
    const token = tokens[i];
    i += 1;
    if (token.kind !== "word") {
      continue;
    }
    const keyword = token.text.toLowerCase();
    if (keyword !== "from" && keyword !== "join" && keyword !== "update" && keyword !== "into") {
      continue;
    }
    // `FROM a x, b y` lists several tables; the other keywords take one
    for (;;) {
      if (isKeyword(tokens[i], "only")) {
        i += 1;
      }
      const first = tokens[i];
      if (!isIdentifier(first) || (first.kind === "word" && NOT_AN_ALIAS.has(first.text.toLowerCase()))) {
        break;
      }
      const path = [first.text];
      i += 1;
      while (tokens[i]?.kind === "punct" && tokens[i].text === "." && isIdentifier(tokens[i + 1])) {
        path.push(tokens[i + 1].text);
        i += 2;
      }
      let alias: string | null = null;
      if (isKeyword(tokens[i], "as") && isIdentifier(tokens[i + 1])) {
        alias = tokens[i + 1].text;
        i += 2;
      } else if (
        isIdentifier(tokens[i]) &&
        (tokens[i].kind === "quoted" || !NOT_AN_ALIAS.has(tokens[i].text.toLowerCase()))
      ) {
        alias = tokens[i].text;
        i += 1;
      }
      references.push({ path, alias });
      if (keyword === "from" && tokens[i]?.kind === "punct" && tokens[i].text === ",") {
        i += 1;
        continue;
      }
      break;
    }
  }
  return references;
};

/// Exposed for tests: the tables referenced by the statement around `pos`
export const referencedTables = (
  text: string,
  pos: number,
  dialect: SqlCompletionDialect = "other",
): TableReference[] => {
  const tokens = tokenize(text, dialect).filter((token) => token.kind !== "comment");
  return collectReferences(statementTokens(tokens, pos));
};

/// The tokens of the statement that contains `pos` (statements are separated by `;`)
const statementTokens = (tokens: Token[], pos: number): Token[] => {
  let start = 0;
  let end = tokens.length;
  for (let i = 0; i < tokens.length; i++) {
    const token = tokens[i];
    if (token.kind === "punct" && token.text === ";") {
      if (token.to <= pos) {
        start = i + 1;
      } else {
        end = i;
        break;
      }
    }
  }
  return tokens.slice(start, end);
};

/// Among candidates that match case-insensitively, prefer the ones written exactly like that
/// (PostgreSQL can hold both "Users" and users)
const preferExact = (candidates: ModelTable[], name: string): ModelTable[] => {
  const exact = candidates.filter((table) => table.name === name);
  return exact.length > 0 ? exact : candidates;
};

const resolveReference = (
  model: SqlCompletionModel,
  path: string[],
): ModelTable[] => {
  const name = path[path.length - 1];
  const sameName = model.byName.get(name.toLowerCase()) ?? [];
  // A bare name means the table of the default schema (the key without a schema part) when
  // there is one; otherwise every table of that name (SQL Server keys always carry the schema)
  const unqualified = sameName.filter((table) => table.schema === null);
  if (path.length >= 2) {
    const schema = path[path.length - 2];
    const inSchema = sameName.filter(
      (table) => table.schema !== null && table.schema.toLowerCase() === schema.toLowerCase(),
    );
    if (inSchema.length > 0) {
      const exactSchema = inSchema.filter((table) => table.schema === schema);
      return preferExact(exactSchema.length > 0 ? exactSchema : inSchema, name);
    }
    // The catalog leaves the default schema out of its keys (PostgreSQL `public`, DuckDB
    // `main`), and a MySQL query can qualify a table with the current database. A qualifier
    // that is not a schema of the map is taken for that
    return model.schemas.has(schema.toLowerCase()) ? [] : preferExact(unqualified, name);
  }
  return preferExact(unqualified.length > 0 ? unqualified : sameName, name);
};

const SIMPLE_IDENTIFIER = /^[A-Za-z_][A-Za-z0-9_$]*$/;
const SIMPLE_LOWER_IDENTIFIER = /^[a-z_][a-z0-9_$]*$/;

/// Names that are keywords in every supported dialect and cannot be written bare as a column or
/// table name. Not exhaustive: it covers the words that actually get used as column names
const RESERVED_WORDS = new Set(
  (
    "all alter and any as asc between by case check column constraint create default delete " +
    "desc distinct drop else end exists foreign from grant group having in index insert into " +
    "is join key like limit not null offset on or order primary references select set table " +
    "then to union unique update user values when where"
  ).split(" "),
);

/// Escape `name` for use between the identifier quotes that close with `close`
const escapeQuoted = (name: string, close: string): string => name.replaceAll(close, close + close);

/// Quote an identifier when the dialect would not read the bare name as that identifier
export const quoteIfNeeded = (name: string, dialect: SqlCompletionDialect): string => {
  // PostgreSQL folds unquoted names to lower case, so a name with capitals needs quotes
  const simple = dialect === "postgres" ? SIMPLE_LOWER_IDENTIFIER : SIMPLE_IDENTIFIER;
  if (simple.test(name) && !RESERVED_WORDS.has(name.toLowerCase())) {
    return name;
  }
  switch (dialect) {
    case "mysql":
      return `\`${escapeQuoted(name, "`")}\``;
    case "mssql":
      return `[${escapeQuoted(name, "]")}]`;
    default:
      return `"${escapeQuoted(name, '"')}"`;
  }
};

const withApply = (
  option: SqlCompletionOption,
  insert: string,
): SqlCompletionOption => (insert === option.label ? option : { ...option, apply: insert });

/// Text inserted for a table at the top level: the schema map key (already qualified and, for
/// SQL Server, bracketed) unless it is a plain name that needs quoting
const tableInsertText = (table: ModelTable, dialect: SqlCompletionDialect): string => {
  if (table.key !== table.name) {
    if (dialect === "mssql") {
      return table.key;
    }
    return table.schema
      ? `${quoteIfNeeded(table.schema, dialect)}.${quoteIfNeeded(table.name, dialect)}`
      : table.key;
  }
  return quoteIfNeeded(table.name, dialect);
};

/// Text inserted for `name`: inside an identifier quote (`closeQuote` is its closing character)
/// only that character is escaped; outside, the name is quoted when the dialect needs it
const insertText = (
  name: string,
  dialect: SqlCompletionDialect,
  closeQuote: string | null,
): string => (closeQuote ? escapeQuoted(name, closeQuote) : quoteIfNeeded(name, dialect));

const columnOptions = (
  tables: ModelTable[],
  dialect: SqlCompletionDialect,
  closeQuote: string | null,
  boost: number,
): SqlCompletionOption[] => {
  // The same column name in several tables is offered once, listing where it comes from
  const sources = new Map<string, string[]>();
  for (const table of tables) {
    for (const column of table.columns) {
      const from = sources.get(column);
      if (from) {
        if (!from.includes(table.name)) {
          from.push(table.name);
        }
      } else {
        if (sources.size >= MAX_COLUMN_OPTIONS) {
          break;
        }
        sources.set(column, [table.name]);
      }
    }
  }
  const options: SqlCompletionOption[] = [];
  for (const [column, from] of sources) {
    const option: SqlCompletionOption = {
      label: column,
      type: "property",
      detail: from.length > 3 ? `${from.slice(0, 3).join(", ")}, …` : from.join(", "),
      boost,
    };
    options.push(withApply(option, insertText(column, dialect, closeQuote)));
  }
  return options;
};

/// Complete at `pos` of `text`. Returns null when nothing schema-related applies there (inside
/// a string or a comment, after an unknown qualifier, or on an empty word without an explicit
/// request).
///
/// `text` is the document, or the part of it given by `completionWindow` (then `pos` and the
/// returned `from` are relative to that part). Statement boundaries inside the window are still
/// found; a window that starts mid-comment can misread its first statement, which only costs
/// completion quality in a document of several hundred thousand characters.
export const completeSql = (
  model: SqlCompletionModel | null,
  text: string,
  pos: number,
  explicit: boolean,
): SqlCompletionResult | null => {
  if (!model || model.tables.length === 0) {
    return null;
  }
  const { dialect } = model;
  const at = pos;
  let allTokens = tokenize(text, dialect);
  // An identifier quote that is still open (`SELECT "na| FROM users`) swallows the rest of the
  // document, or pairs up with the opening quote of a later identifier. When what lies between
  // the cursor and the closing quote is not a plain word, cut the token at the cursor and read
  // what follows as SQL again
  const open = allTokens.findIndex(
    (token) =>
      token.kind === "quoted" &&
      token.from < at &&
      at <= token.to &&
      (!token.closed || !SQL_COMPLETION_VALID_FOR.test(text.slice(at, token.to - 1))),
  );
  if (open >= 0 && allTokens[open].to > at) {
    const cut = { ...allTokens[open], to: at, closed: false };
    const rest = tokenize(text.slice(at), dialect).map((token) => ({
      ...token,
      from: token.from + at,
      to: token.to + at,
    }));
    allTokens = [...allTokens.slice(0, open), cut, ...rest];
  }
  const statement = statementTokens(allTokens, at);

  // Inside a string or a comment (or at the end of an unterminated one): nothing to offer
  for (const token of statement) {
    if (token.from >= at) {
      break;
    }
    if (
      (token.kind === "string" || token.kind === "comment") &&
      at <= token.to &&
      (at < token.to || !token.closed)
    ) {
      return null;
    }
  }
  // Comments are whitespace from here on (`FROM /* c */ users`, `users /* c */ AS u`)
  const tokens = statement.filter((token) => token.kind !== "comment");

  // The token the cursor is in or directly after
  let index = -1;
  for (let i = 0; i < tokens.length; i++) {
    if (tokens[i].from < at && at <= tokens[i].to) {
      index = i;
      break;
    }
    if (tokens[i].from >= at) {
      break;
    }
  }
  const current = index >= 0 ? tokens[index] : undefined;

  let from = at;
  /// The closing character of the identifier quote the cursor is in (null outside quotes)
  let closeQuote: string | null = null;
  let before = index; // index of the last token before the word being typed
  if (current && current.kind === "word") {
    from = current.from;
    before = index - 1;
  } else if (current && current.kind === "quoted" && (at < current.to || !current.closed)) {
    // Inside a quoted identifier: replace its content, keep the user's quotes
    from = current.from + 1;
    const openQuote = text[current.from];
    closeQuote = openQuote === "[" ? "]" : openQuote;
    before = index - 1;
  } else if (current && current.kind === "quoted") {
    // Directly after a closed quoted identifier: nothing to complete
    return null;
  } else {
    // Between tokens: the last token that ends at or before the cursor
    before = -1;
    for (let i = 0; i < tokens.length; i++) {
      if (tokens[i].to <= at) {
        before = i;
      } else {
        break;
      }
    }
  }
  // A number is not an identifier being typed (`LIMIT 10|`)
  if (!closeQuote && from < at && /^[0-9]/.test(text.slice(from, at))) {
    return null;
  }

  // The qualifier before the word: `a.b.|` -> [a, b]
  const qualifier: string[] = [];
  let scan = before;
  while (
    scan >= 1 &&
    tokens[scan].kind === "punct" &&
    tokens[scan].text === "." &&
    isIdentifier(tokens[scan - 1])
  ) {
    qualifier.unshift(tokens[scan - 1].text);
    scan -= 2;
  }
  if (scan >= 0 && tokens[scan].kind === "punct" && tokens[scan].text === "." && qualifier.length === 0) {
    // `.|` after something that is not an identifier (`1.`, `).`)
    return null;
  }

  const references = collectReferences(tokens);
  const result = (options: SqlCompletionOption[]): SqlCompletionResult | null =>
    options.length > 0 ? { from, options } : null;

  if (qualifier.length > 0) {
    const options: SqlCompletionOption[] = [];
    const seenTables = new Set<ModelTable>();
    const addColumns = (tables: ModelTable[]) => {
      const fresh = tables.filter((table) => !seenTables.has(table));
      fresh.forEach((table) => seenTables.add(table));
      options.push(...columnOptions(fresh, dialect, closeQuote, 0));
    };
    if (qualifier.length === 1) {
      const lower = qualifier[0].toLowerCase();
      // An alias wins over a table of the same name (that is how SQL resolves it)
      const aliased = references.filter((r) => r.alias?.toLowerCase() === lower);
      if (aliased.length > 0) {
        aliased.forEach((r) => addColumns(resolveReference(model, r.path)));
      } else {
        // Prefer the tables the statement references (`sales.users` over `public.users`)
        const referenced = references
          .filter((r) => r.path[r.path.length - 1].toLowerCase() === lower)
          .flatMap((r) => resolveReference(model, r.path));
        addColumns(referenced.length > 0 ? referenced : resolveReference(model, [qualifier[0]]));
      }
      if (model.schemas.has(lower)) {
        for (const table of model.tables) {
          if (table.schema?.toLowerCase() === lower) {
            options.push(
              withApply(
                { label: table.name, type: "type" },
                insertText(table.name, dialect, closeQuote),
              ),
            );
          }
        }
      }
    } else {
      addColumns(resolveReference(model, qualifier.slice(-2)));
    }
    return result(options);
  }

  if (from === at && !explicit) {
    return null;
  }

  const previous = before >= 0 ? tokens[before] : undefined;
  const expectsTable =
    !!previous && previous.kind === "word" && TABLE_KEYWORDS.has(previous.text.toLowerCase());

  const options: SqlCompletionOption[] = [];
  if (!expectsTable) {
    const seen = new Set<ModelTable>();
    const tables: ModelTable[] = [];
    for (const reference of references) {
      for (const table of resolveReference(model, reference.path)) {
        if (!seen.has(table)) {
          seen.add(table);
          tables.push(table);
        }
      }
    }
    options.push(...columnOptions(tables, dialect, closeQuote, 20));
    const aliases = new Set<string>();
    for (const reference of references) {
      if (reference.alias && !aliases.has(reference.alias)) {
        aliases.add(reference.alias);
        options.push(
          withApply(
            {
              label: reference.alias,
              type: "constant",
              detail: reference.path.join("."),
              boost: 15,
            },
            insertText(reference.alias, dialect, closeQuote),
          ),
        );
      }
    }
  }
  if (closeQuote) {
    // Inside quotes the prebuilt options (qualified / quoted insert texts) do not apply
    const tableBoost = expectsTable ? 20 : 0;
    for (const table of model.tables) {
      options.push(
        withApply(
          {
            label: table.name,
            type: "type",
            boost: tableBoost,
            ...(table.schema ? { detail: table.schema } : {}),
          },
          escapeQuoted(table.name, closeQuote),
        ),
      );
    }
    for (const schema of model.schemas.values()) {
      options.push(
        withApply({ label: schema, type: "namespace", boost: -1 }, escapeQuoted(schema, closeQuote)),
      );
    }
    return result(options);
  }
  const shared = expectsTable ? model.topLevelTableFirst : model.topLevel;
  return result(options.length > 0 ? options.concat(shared) : shared);
};

/// Whether a statement that ran successfully may have changed the tables / columns
/// (CREATE / ALTER / DROP / RENAME, after leading comments). The schema map for completion is
/// re-fetched after such a statement, so that a table created a moment ago completes
export const changesSchema = (sql: string): boolean =>
  /^(?:create|alter|drop|rename)\b/i.test(stripLeadingComments(sql));

/// Whether two schema maps hold the same tables and columns in the same order
export const sameSchemaMap = (
  a: Record<string, string[]> | null,
  b: Record<string, string[]> | null,
): boolean => {
  if (a === b) {
    return true;
  }
  if (!a || !b) {
    return false;
  }
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) {
    return false;
  }
  for (const key of keys) {
    const left = a[key];
    const right = b[key];
    if (!right || left.length !== right.length) {
      return false;
    }
    for (let i = 0; i < left.length; i++) {
      if (left[i] !== right[i]) {
        return false;
      }
    }
  }
  return true;
};
