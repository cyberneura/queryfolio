// Named query parameters (`:name`) filled in at run time (CYBERNEURA-DEV-1400).
//
// A statement may contain `:name` placeholders. Values come from
//   1. `-- @set name = <SQL expression>` lines in the same query file (inserted as written), and
//   2. otherwise a dialog that asks for each remaining name (a string literal by default, or the
//      text as written with "raw").
// The filled-in statement is what is run, guarded and saved in the history; the editor keeps the
// placeholders. Everything here is a pure function (tests/query-params.test.mts).
//
// Values are substituted as text rather than bound by the driver: sqlx binds a value of unknown
// type as TEXT, so PostgreSQL would reject `int_column = $1` and every value would need a cast.

/** Engines whose editor runs SQL with `:name` parameters */
export type ParamEngine = "mysql" | "postgres" | "sqlite" | "duckdb" | "mssql";

/** Maps ConnectionInfo.engine to a ParamEngine, or null for engines without parameters */
export function paramEngineOf(engine: string): ParamEngine | null {
  const e = engine.toLowerCase();
  if (e === "mysql" || e === "mariadb") return "mysql";
  if (e === "postgres" || e === "postgresql") return "postgres";
  if (e === "sqlite" || e === "sqlite3") return "sqlite";
  if (e === "duckdb") return "duckdb";
  if (e === "mssql" || e === "sqlserver") return "mssql";
  // redis / elasticsearch have their own syntax; DynamoDB's PartiQL uses `?` parameters
  return null;
}

/**
 * Blank out (with spaces, keeping offsets) everything a `:name` must not be found in: string
 * literals, quoted identifiers, comments, and per dialect PostgreSQL / DuckDB dollar quoting
 * (`$tag$ ... $tag$`) and `E'…'` escape strings, PostgreSQL nested block comments, MySQL `#`
 * comments (and `--` only when followed by whitespace, as MySQL requires), and SQL Server /
 * SQLite `[bracketed]` identifiers (`]]` escapes). An unclosed literal or comment blanks out the
 * rest of the text.
 *
 * With `keepLineComments`, line comments are skipped but left in place (used to find the real
 * `-- @set` lines: a `--` inside a string or block comment is blanked out, a real one is not).
 */
export function blankNonCode(sql: string, engine: ParamEngine, keepLineComments = false): string {
  const out = sql.split("");
  const blank = (from: number, to: number) => {
    for (let i = from; i < to && i < out.length; i++) {
      if (out[i] !== "\n") out[i] = " ";
    }
  };
  let i = 0;
  const n = sql.length;
  while (i < n) {
    const c = sql[i];
    const next = sql[i + 1];
    let end = -1;
    let keep = false;
    if (c === "'" || c === '"' || c === "`") {
      // Doubled quote is an escape inside the same literal. MySQL takes a backslash escape inside
      // '…' and "…"; PostgreSQL / DuckDB only inside E'…'
      const escapeString =
        c === "'" &&
        (engine === "postgres" || engine === "duckdb") &&
        i > 0 &&
        (sql[i - 1] === "E" || sql[i - 1] === "e") &&
        !(i > 1 && /[A-Za-z0-9_$]/.test(sql[i - 2]));
      const backslash = (engine === "mysql" && c !== "`") || escapeString;
      let j = i + 1;
      while (j < n) {
        if (backslash && sql[j] === "\\") {
          j += 2;
          continue;
        }
        if (sql[j] === c) {
          if (sql[j + 1] === c) {
            j += 2;
            continue;
          }
          break;
        }
        j++;
      }
      end = Math.min(j + 1, n);
    } else if (c === "[" && (engine === "mssql" || engine === "sqlite")) {
      let j = i + 1;
      while (j < n) {
        if (sql[j] === "]") {
          if (engine === "mssql" && sql[j + 1] === "]") {
            j += 2;
            continue;
          }
          break;
        }
        j++;
      }
      end = Math.min(j + 1, n);
    } else if (
      c === "-" &&
      next === "-" &&
      // MySQL: `--` starts a comment only when followed by whitespace (`1--1` is arithmetic)
      (engine !== "mysql" || i + 2 >= n || /\s/.test(sql[i + 2]))
    ) {
      const j = sql.indexOf("\n", i);
      end = j < 0 ? n : j;
      keep = keepLineComments;
    } else if (c === "#" && engine === "mysql") {
      const j = sql.indexOf("\n", i);
      end = j < 0 ? n : j;
      keep = keepLineComments;
    } else if (c === "/" && next === "*") {
      if (engine === "postgres" || engine === "mssql") {
        // PostgreSQL and SQL Server block comments nest
        let depth = 1;
        let j = i + 2;
        while (j < n && depth > 0) {
          if (sql[j] === "/" && sql[j + 1] === "*") {
            depth++;
            j += 2;
          } else if (sql[j] === "*" && sql[j + 1] === "/") {
            depth--;
            j += 2;
          } else {
            j++;
          }
        }
        end = j;
      } else {
        const j = sql.indexOf("*/", i + 2);
        end = j < 0 ? n : j + 2;
      }
    } else if (c === "$" && (engine === "postgres" || engine === "duckdb")) {
      const m = /^\$([A-Za-z_][A-Za-z0-9_]*)?\$/.exec(sql.slice(i));
      // `$1` (a positional parameter) and `a$b` (an identifier character) are not dollar quotes
      if (m && !(i > 0 && /[A-Za-z0-9_$]/.test(sql[i - 1]))) {
        const tag = m[0];
        const j = sql.indexOf(tag, i + tag.length);
        end = j < 0 ? n : j + tag.length;
      }
    }
    if (end > i) {
      if (!keep) blank(i, end);
      i = end;
    } else {
      i++;
    }
  }
  return out.join("");
}

/** A `:name` occurrence in a statement */
export interface ParamUse {
  name: string;
  /** Offset of the `:` in the statement */
  from: number;
  /** Offset just after the name */
  to: number;
}

const NAME_START = /[A-Za-z_]/;
const NAME_CHAR = /[A-Za-z0-9_]/;

/**
 * The `:name` placeholders of a statement, in order. Not a placeholder: `::type` casts
 * (PostgreSQL / DuckDB), `:=` (MySQL variable assignment), `a:b` (a colon right after an
 * identifier or number, e.g. slices `arr[1:2]`, time literals already being strings), and anything
 * inside literals / comments / quoted identifiers.
 */
/** Index of the last non-whitespace character before `i` (-1 if none) */
function prevNonSpace(text: string, i: number): number {
  let j = i - 1;
  while (j >= 0 && /\s/.test(text[j])) j--;
  return j;
}

/**
 * Whether the `[` at `bracket` is a subscript (`arr[...]`, `f(x)[...]`, `a[1][...]`) rather than an
 * array / list constructor (`ARRAY[...]`, DuckDB `[...]`).
 */
function isSubscriptBracket(code: string, bracket: number): boolean {
  // A subscript is written right after its operand; after whitespace (`SELECT [:a]`, `ARRAY [`)
  // the bracket starts a constructor
  const j = bracket - 1;
  if (j < 0 || /\s/.test(code[j])) return false;
  const c = code[j];
  if (c === ")" || c === "]") return true;
  if (!/[A-Za-z0-9_$"]/.test(c)) return false;
  let k = j;
  while (k >= 0 && /[A-Za-z0-9_$]/.test(code[k])) k--;
  return code.slice(k + 1, j + 1).toLowerCase() !== "array";
}

/** Depth of `{` … `}` at each offset (DuckDB struct / map literals) */
function braceDepths(code: string): number[] {
  const depths: number[] = [];
  let depth = 0;
  for (const ch of code) {
    if (ch === "{") depth++;
    depths.push(depth);
    if (ch === "}" && depth > 0) depth--;
  }
  return depths;
}

export function findParams(sql: string, engine: ParamEngine): ParamUse[] {
  const code = blankNonCode(sql, engine);
  const braces = engine === "duckdb" ? braceDepths(code) : null;
  const uses: ParamUse[] = [];
  for (let i = 0; i < code.length; i++) {
    if (code[i] !== ":") continue;
    const prev = i > 0 ? code[i - 1] : "";
    if (prev === ":" || /[A-Za-z0-9_$\]).]/.test(prev)) continue;
    // A colon right after a string / quoted identifier is a key separator (DuckDB `{'k':v}`),
    // never a placeholder (`'x' :y` is not valid SQL)
    const before = prevNonSpace(sql, i);
    if (before >= 0 && code[before] !== sql[before] && /['"`\]]/.test(sql[before])) continue;
    // DuckDB struct keys may also be bare identifiers: `{a: x}`. Only in a key position (the word
    // right after `{` or `,`), so `{'r': CASE WHEN :x THEN 1 END}` still finds `x`
    if (braces && braces[i] > 0 && before >= 0 && /[A-Za-z0-9_$]/.test(code[before])) {
      let k = before;
      while (k >= 0 && /[A-Za-z0-9_$]/.test(code[k])) k--;
      const lead = prevNonSpace(code, k + 1);
      if (lead >= 0 && (code[lead] === "{" || code[lead] === ",")) continue;
    }
    // PostgreSQL / DuckDB slices with an omitted lower bound: `arr[:upper]` (but `ARRAY[:id]` and
    // DuckDB `[:id]` construct arrays and keep their placeholders)
    if (
      prev === "[" &&
      (engine === "postgres" || engine === "duckdb") &&
      isSubscriptBracket(code, i - 1)
    ) {
      continue;
    }
    if (!NAME_START.test(code[i + 1] ?? "")) continue;
    let j = i + 2;
    while (j < code.length && NAME_CHAR.test(code[j])) j++;
    uses.push({ name: code.slice(i + 1, j), from: i, to: j });
    i = j - 1;
  }
  return uses;
}

/** Distinct parameter names in order of first appearance */
export function paramNames(uses: readonly ParamUse[]): string[] {
  return [...new Set(uses.map((u) => u.name))];
}

/**
 * Offset of the SQL body in a statement (after leading whitespace and comments). The editor's
 * statement range can start with comment lines (a bare `--` line makes lang-sql fold the comments
 * after it into the statement), and a `-- @set` among them still comes before the SQL.
 */
export function sqlBodyOffset(sql: string, engine: ParamEngine): number {
  const code = blankNonCode(sql, engine);
  const m = /\S/.exec(code);
  return m ? m.index : sql.length;
}

/** A `-- @set name = value` line of the query file */
export interface ParamDefinition {
  name: string;
  /** The SQL expression as written (inserted as is) */
  value: string;
  /** Offset of the start of the line in the document */
  at: number;
}

const DEFINITION_LINE = /^[ \t]*--[ \t]*@set[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*=[ \t]*(.*?)[ \t]*;?[ \t]*$/;

/**
 * The `-- @set name = value` definitions of a document (one per line, a line comment so the file
 * stays valid SQL for other tools). The value is everything after `=` up to the end of the line,
 * trimmed, with one trailing `;` dropped. An empty value is not a definition. Only real line
 * comments count: a `-- @set` inside a string, a dollar quote or a block comment is not one.
 *
 * `doc` must use the same line endings as the offsets it is compared with (the editor's LF text).
 */
export function parseDefinitions(doc: string, engine: ParamEngine): ParamDefinition[] {
  const code = blankNonCode(doc, engine, true);
  const defs: ParamDefinition[] = [];
  let at = 0;
  for (const line of doc.split("\n")) {
    const m = DEFINITION_LINE.exec(line.replace(/\r$/, ""));
    // The `--` survived blanking only if it really starts a comment
    const dash = line.indexOf("--");
    if (m && m[2] !== "" && code.slice(at + dash, at + dash + 2) === "--") {
      defs.push({ name: m[1], value: m[2], at });
    }
    at += line.length + 1;
  }
  return defs;
}

/**
 * The definition that applies to a statement starting at `statementFrom`: the last one above the
 * statement, otherwise the first one in the file (so a value can be redefined further down for
 * the statements below it, and a definition at the end of the file still works).
 */
export function definitionFor(
  defs: readonly ParamDefinition[],
  name: string,
  statementFrom: number,
): ParamDefinition | null {
  let above: ParamDefinition | null = null;
  let first: ParamDefinition | null = null;
  for (const d of defs) {
    if (d.name !== name) continue;
    if (first === null) first = d;
    if (d.at < statementFrom) above = d;
  }
  return above ?? first;
}

/** A value typed in the dialog */
export interface ParamInput {
  value: string;
  /** Insert the text as written (a number, NULL, an expression) instead of a string literal */
  raw: boolean;
}

function utf8Hex(s: string): string {
  return Array.from(new TextEncoder().encode(s), (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * A string literal in the engine's syntax that means the same value whatever the session's
 * string settings are. Without a backslash, `'…'` with doubled quotes is read the same way in
 * every mode. With one:
 * - MySQL: `CONVERT(X'…' USING utf8mb4)` — `'a\\b'` would change under NO_BACKSLASH_ESCAPES
 * - PostgreSQL: `E'…'` with doubled backslashes — a plain `'…'` is read differently (and a quote
 *   after a backslash would end the literal) under `standard_conforming_strings = off`
 */
export function quoteParamString(engine: ParamEngine, s: string): string {
  if (s.includes("\\")) {
    if (engine === "mysql") return `CONVERT(X'${utf8Hex(s)}' USING utf8mb4)`;
    if (engine === "postgres") return "E'" + s.replace(/\\/g, "\\\\").replace(/'/g, "''") + "'";
  }
  const quoted = "'" + s.replace(/'/g, "''") + "'";
  // N'…' so non-ASCII text is not converted to the database's code page
  return engine === "mssql" ? "N" + quoted : quoted;
}

/** The SQL text a dialog value becomes */
export function paramInputSql(engine: ParamEngine, input: ParamInput): string {
  return input.raw ? input.value : quoteParamString(engine, input.value);
}

export type FillResult = { ok: true; sql: string } | { ok: false; missing: string[] };

/**
 * Replaces every placeholder with its SQL text (`values[name]`). Names without a value are
 * reported and nothing is replaced.
 */
export function fillParams(
  sql: string,
  uses: readonly ParamUse[],
  values: Readonly<Record<string, string>>,
): FillResult {
  // Own properties only: `:constructor` must not find Object.prototype.constructor
  const missing = paramNames(uses).filter((n) => !Object.hasOwn(values, n));
  if (missing.length > 0) return { ok: false, missing };
  let out = "";
  let last = 0;
  for (const u of uses) {
    out += sql.slice(last, u.from) + values[u.name];
    last = u.to;
  }
  return { ok: true, sql: out + sql.slice(last) };
}

/** A NUL character cannot appear in an SQL string literal (same rule as the SQL INSERT export) */
export function invalidParamInput(input: ParamInput): string | null {
  if (input.value.includes("\u0000")) return "The value contains a NUL character";
  if (input.raw && input.value.trim() === "") return "A raw value cannot be empty";
  return null;
}
