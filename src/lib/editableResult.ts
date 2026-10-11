/// Helpers for converting cell edits in the result grid into UPDATE statements.
///
/// A design that errs on the safe side (corresponds to AGENTS.md / the task's design decisions):
/// - Editing is allowed only for a "single-table SELECT" whose result contains the primary key columns.
///   JOIN / aggregates / subqueries / aliased columns etc. are not editable (singleTableSelectTable is null).
/// - WHERE is built from the primary key. The primary key columns themselves are not editable (row identification would break).
/// - Value quoting is per engine. Ambiguous cases can be fixed by hand with the Edit button (pastes the SQL into the editor),
///   so only a straightforward guess is made here.

export type NormalizedEngine = "mysql" | "postgres" | "sqlite";

/// Normalizes variant spellings of ConnectionInfo.engine (mariadb / postgresql / sqlite3 etc.).
export function normalizeEngine(engine: string): NormalizedEngine {
  const e = engine.toLowerCase();
  if (e === "mysql" || e === "mariadb") return "mysql";
  if (e === "postgres" || e === "postgresql") return "postgres";
  return "sqlite";
}

/// Whether this is a simple identifier (including schema.table). Aligned with the backend's
/// validate_relation_name rule (the first character is a letter or _, the rest are
/// alphanumerics / _ / $, and at most two dot-separated parts). Quoted identifiers are out of scope.
function isPlainIdentifier(token: string): boolean {
  const parts = token.split(".");
  if (parts.length === 0 || parts.length > 2) return false;
  return parts.every((p) => /^[A-Za-z_][A-Za-z0-9_$]*$/.test(p));
}

/// Strips comments (-- lines, /* */ blocks) from SQL. String literals are preserved.
function stripComments(sql: string): string {
  let out = "";
  let i = 0;
  const n = sql.length;
  while (i < n) {
    const c = sql[i];
    const next = sql[i + 1];
    // Pass string literals (' or ") through as is
    if (c === "'" || c === '"' || c === "`") {
      const quote = c;
      out += c;
      i++;
      while (i < n) {
        out += sql[i];
        if (sql[i] === quote) {
          // Escapes via '' / "" / `` advance by one character and continue
          if (sql[i + 1] === quote) {
            out += sql[i + 1];
            i += 2;
            continue;
          }
          i++;
          break;
        }
        i++;
      }
      continue;
    }
    if (c === "-" && next === "-") {
      while (i < n && sql[i] !== "\n") i++;
      continue;
    }
    if (c === "/" && next === "*") {
      i += 2;
      while (i < n && !(sql[i] === "*" && sql[i + 1] === "/")) i++;
      i += 2;
      out += " ";
      continue;
    }
    out += c;
    i++;
  }
  return out;
}

/// If the SELECT is a "fetch from a single table", returns that table name (a simple identifier).
/// Any shape whose detection is even slightly doubtful (JOIN / comma join / subquery / UNION / quoted name /
/// aliased) is treated as not editable and returns null (safe side).
export function singleTableSelectTable(sql: string): string | null {
  const cleaned = stripComments(sql).trim();
  if (!cleaned) return null;

  // Blank out string literals / quoted identifiers while tracking the parenthesis depth. Only positions at depth 0
  // are subject to keyword search (the insides of subqueries and function arguments are ignored).
  // Also collect the words appearing at depth 0 as a token sequence.
  const lower = cleaned.toLowerCase();
  let depth = 0;
  let inString: string | null = null;
  // The depth-0 text only, concatenated (for keyword / comma detection).
  let topText = "";
  for (let i = 0; i < cleaned.length; i++) {
    const c = cleaned[i];
    if (inString) {
      if (c === inString) {
        if (cleaned[i + 1] === inString) {
          i++;
          continue;
        }
        inString = null;
      }
      continue;
    }
    if (c === "'" || c === '"' || c === "`") {
      inString = c;
      // Do not put the quoted contents into topText (put a placeholder)
      if (depth === 0) topText += "\0";
      continue;
    }
    if (c === "(") {
      // Leave a placeholder for an opening parenthesis at depth 0. Otherwise, a subquery
      // "(SELECT ...) alias" right after FROM would have its contents erased, leaving only the alias,
      // and be mistaken for a single table.
      if (depth === 0) topText += "\0";
      depth++;
      continue;
    }
    if (c === ")") {
      if (depth > 0) depth--;
      continue;
    }
    if (depth === 0) topText += lower[i];
  }

  // Must start with SELECT (WITH / EXPLAIN / VALUES etc. are out of scope)
  if (!/^\s*select\b/.test(topText)) return null;
  // Set operations, JOIN, or a depth-0 comma join mean it is not a single table.
  // GROUP BY / HAVING aggregate rows, so the displayed PK column can become an arbitrary value of the "group's representative row"
  // (WHERE pk = <representative value> would update something unintended), so reject them.
  if (/\b(join|union|intersect|except|group|having)\b/.test(topText)) {
    return null;
  }

  // Find the depth-0 FROM
  const fromMatch = /\bfrom\b/.exec(topText);
  if (!fromMatch) return null;

  // Verify the SELECT list is in a form usable for editing. To guarantee the displayed columns correspond to the real table columns,
  // allow only "*" or a list of bare column names with no aliases or expressions.
  // Example: `SELECT id+1 AS id, b AS a FROM t` could make the displayed values and real columns diverge and update a different row / column,
  // so reject it (topText is lowercased, and parentheses / strings are collapsed to \0).
  const selectList = topText
    .slice(0, fromMatch.index)
    .replace(/^\s*select\s+/, "")
    .replace(/^(?:distinct|all)\s+/, "")
    .trim();
  if (selectList !== "*") {
    const items = selectList.split(",").map((s) => s.trim());
    if (!items.every((it) => /^[a-z_][a-z0-9_$]*$/.test(it))) return null;
  }

  const afterFrom = topText.slice(fromMatch.index + 4);
  // Cut what follows FROM at the next clause keyword / semicolon
  const clause = afterFrom.split(
    /\b(where|group|having|order|limit|offset|window|for|fetch)\b|;/,
  )[0];
  const ref = clause.trim();
  // Not allowed if it contains a quote placeholder (\0), has a comma, or is empty
  if (!ref || ref.includes("\0") || ref.includes(",")) return null;
  // Aliased ("users u" or "users as u") becomes multiple tokens separated by whitespace -> not allowed
  const tokens = ref.split(/\s+/).filter(Boolean);
  if (tokens.length !== 1) return null;
  const table = tokens[0];
  if (!isPlainIdentifier(table)) return null;
  // topText is lowercased, so extract the actual spelling from the original SQL
  return extractOriginalTable(cleaned, table);
}

/// Returns the actual spelling in the original SQL for a lowercased table name (preserving case).
function extractOriginalTable(cleaned: string, lowerTable: string): string | null {
  const re = new RegExp(
    `\\bfrom\\s+(${lowerTable.replace(/[.$]/g, "\\$&")})\\b`,
    "i",
  );
  const m = re.exec(cleaned);
  return m ? m[1] : lowerTable;
}

/// Normalizes the detected (unquoted, simple) table name according to the engine's folding rule for
/// unquoted identifiers. This is then quoted and embedded in the UPDATE.
/// - PostgreSQL: unquoted identifiers fold to lowercase, so lowercase it
///   (`FROM Users` is the real table `users`. Without folding, `UPDATE "Users"` would fail with
///   relation does not exist).
/// - MySQL: unquoted identifiers are not folded (case depends on storage), so keep as is.
/// - SQLite: identifier matching is case-insensitive, so keeping as is is fine.
export function normalizeTableName(
  engine: NormalizedEngine,
  table: string,
): string {
  return engine === "postgres" ? table.toLowerCase() : table;
}

/// Quotes an identifier per engine.
export function quoteIdent(engine: NormalizedEngine, ident: string): string {
  if (engine === "mysql") return "`" + ident.replace(/`/g, "``") + "`";
  return '"' + ident.replace(/"/g, '""') + '"';
}

/// For the schema.table form, quotes each part individually.
export function quoteQualified(engine: NormalizedEngine, table: string): string {
  return table
    .split(".")
    .map((p) => quoteIdent(engine, p))
    .join(".");
}

/// Turns a string into an SQL literal per engine.
function quoteString(engine: NormalizedEngine, s: string): string {
  if (engine === "mysql") {
    // MySQL treats backslash as the escape character by default, so double it
    return "'" + s.replace(/\\/g, "\\\\").replace(/'/g, "''") + "'";
  }
  return "'" + s.replace(/'/g, "''") + "'";
}

const NUMERIC_RE = /^-?\d+(\.\d+)?$/;

/// Turns the original cell value (JSON) into an SQL literal for WHERE.
export function literalFromValue(
  engine: NormalizedEngine,
  value: unknown,
): string {
  if (value === null || value === undefined) return "NULL";
  if (typeof value === "number") return String(value);
  if (typeof value === "boolean") {
    if (engine === "postgres") return value ? "TRUE" : "FALSE";
    return value ? "1" : "0";
  }
  // String (including stringified datetimes and large integers)
  return quoteString(engine, String(value));
}

/// Converts the new value entered by the user into an SQL literal, using the original cell's type as a hint.
/// Numeric columns get numeric literals, boolean columns get boolean literals, anything else becomes a string.
export function literalFromInput(
  engine: NormalizedEngine,
  original: unknown,
  input: string,
): string {
  if (typeof original === "number") {
    return NUMERIC_RE.test(input.trim())
      ? input.trim()
      : quoteString(engine, input);
  }
  if (typeof original === "boolean") {
    const t = input.trim().toLowerCase();
    const truthy = t === "true" || t === "t" || t === "1";
    const falsy = t === "false" || t === "f" || t === "0";
    if (truthy || falsy) {
      if (engine === "postgres") return truthy ? "TRUE" : "FALSE";
      return truthy ? "1" : "0";
    }
    return quoteString(engine, input);
  }
  // Original is null / string: the column type is unknown, so always use a string literal.
  // (This prevents transforming "00123" etc. into a number when entering it into a null cell. For numeric
  //  columns, assignment-time casts work even with a string literal, so the harm is small.)
  return quoteString(engine, input);
}

/// An edit of one cell.
export interface CellEdit {
  rowIndex: number;
  column: string;
  original: unknown;
  input: string;
}

/// Builds an array of UPDATE statements from the primary key and the edits (1 row = 1 statement; multiple edits on the same row are
/// combined into one statement). The order is stabilized by row number.
export function buildUpdateStatements(
  engine: NormalizedEngine,
  table: string,
  pkColumns: string[],
  columns: string[],
  rows: unknown[][],
  edits: CellEdit[],
): string[] {
  const pkIndexes = pkColumns.map((pk) => columns.indexOf(pk));
  // Group the edits by row number
  const byRow = new Map<number, CellEdit[]>();
  for (const e of edits) {
    const list = byRow.get(e.rowIndex) ?? [];
    list.push(e);
    byRow.set(e.rowIndex, list);
  }
  const qualifiedTable = quoteQualified(engine, table);
  const statements: string[] = [];
  for (const rowIndex of [...byRow.keys()].sort((a, b) => a - b)) {
    const rowEdits = byRow.get(rowIndex)!;
    const row = rows[rowIndex];
    const setClause = rowEdits
      .map(
        (e) =>
          `${quoteIdent(engine, e.column)} = ${literalFromInput(engine, e.original, e.input)}`,
      )
      .join(", ");
    const whereClause = pkColumns
      .map((pk, i) => {
        const value = row[pkIndexes[i]];
        const lit = literalFromValue(engine, value);
        return lit === "NULL"
          ? `${quoteIdent(engine, pk)} IS NULL`
          : `${quoteIdent(engine, pk)} = ${lit}`;
      })
      .join(" AND ");
    statements.push(
      `UPDATE ${qualifiedTable} SET ${setClause} WHERE ${whereClause}`,
    );
  }
  return statements;
}
