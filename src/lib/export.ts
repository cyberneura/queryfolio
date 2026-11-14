import type { QueryResult } from "$lib/api";

const cellToString = (value: unknown): string => {
  if (value === null || value === undefined) {
    return "";
  }
  if (typeof value === "object") {
    return JSON.stringify(value);
  }
  return String(value);
};

// Characters that a spreadsheet interprets as a formula when pasted.
// Only string values from the DB are escaped, so numeric values (e.g. -1) are not broken.
const FORMULA_TRIGGER = /^[=+\-@\t\r]/;

const escapeFormulaInjection = (value: unknown, text: string): string => {
  if (typeof value === "string" && FORMULA_TRIGGER.test(text)) {
    return `'${text}`;
  }
  return text;
};

// Headers (column names / aliases) are always strings, so check them unconditionally
const escapeHeaderFormula = (header: string): string =>
  FORMULA_TRIGGER.test(header) ? `'${header}` : header;

const escapeCsvField = (field: string): string => {
  if (/[",\n\r]/.test(field)) {
    return `"${field.replace(/"/g, '""')}"`;
  }
  return field;
};

export const toCsv = (result: QueryResult): string => {
  const lines = [
    result.columns.map((c) => escapeCsvField(escapeHeaderFormula(c))).join(","),
  ];
  for (const row of result.rows) {
    lines.push(
      row
        .map((v) => escapeCsvField(escapeFormulaInjection(v, cellToString(v))))
        .join(","),
    );
  }
  return lines.join("\n");
};

// Rectangular selection range of the result table. Rows and columns are 0-based closed intervals (both ends included).
export interface CellRange {
  rowStart: number;
  rowEnd: number;
  colStart: number;
  colEnd: number;
}

// Convert only the selected range to CSV (for Cmd+C copy). withHeaders includes the header row.
// The range is assumed to be already clamped to the result size by the caller.
export const toCsvRange = (
  result: QueryResult,
  range: CellRange,
  withHeaders: boolean,
): string => {
  const { rowStart, rowEnd, colStart, colEnd } = range;
  const lines: string[] = [];
  if (withHeaders) {
    const header: string[] = [];
    for (let c = colStart; c <= colEnd; c++) {
      header.push(escapeCsvField(escapeHeaderFormula(result.columns[c])));
    }
    lines.push(header.join(","));
  }
  for (let r = rowStart; r <= rowEnd; r++) {
    const row = result.rows[r];
    if (!row) {
      continue;
    }
    const fields: string[] = [];
    for (let c = colStart; c <= colEnd; c++) {
      const v = row[c];
      fields.push(escapeCsvField(escapeFormulaInjection(v, cellToString(v))));
    }
    lines.push(fields.join(","));
  }
  return lines.join("\n");
};

const sanitizeTsv = (field: string) =>
  field.replace(/\t/g, " ").replace(/\r?\n/g, " ");

// Convert only the selected range to TSV (for Cmd+C copy). withHeaders includes the header row.
// The range is assumed to be already clamped to the result size by the caller.
export const toTsvRange = (
  result: QueryResult,
  range: CellRange,
  withHeaders: boolean,
): string => {
  const { rowStart, rowEnd, colStart, colEnd } = range;
  const lines: string[] = [];
  if (withHeaders) {
    const header: string[] = [];
    for (let c = colStart; c <= colEnd; c++) {
      header.push(sanitizeTsv(escapeHeaderFormula(result.columns[c])));
    }
    lines.push(header.join("\t"));
  }
  for (let r = rowStart; r <= rowEnd; r++) {
    const row = result.rows[r];
    if (!row) {
      continue;
    }
    const fields: string[] = [];
    for (let c = colStart; c <= colEnd; c++) {
      const v = row[c];
      fields.push(sanitizeTsv(escapeFormulaInjection(v, cellToString(v))));
    }
    lines.push(fields.join("\t"));
  }
  return lines.join("\n");
};

/// TSV conversion with limits on the total character count and on characters per cell.
/// Also returns whether it was truncated.
///
/// `db.rs` does not truncate cell text on the sqlx path, so a 100KB TEXT or a long JSON
/// reaches the frontend as is. Processing that scans every cell costs in proportion to the
/// **total character count**, not the row count, so a row limit alone is not enough
/// (even 499 rows x 100KB cells can reach hundreds of MB).
///
/// The limits work in two key ways:
/// - **Truncate before sanitizing.** Truncating afterwards would scan the discarded part with
///   the regex too, defeating the purpose of the limit.
/// - **Check the budget per cell.** Checking only at the start of a row lets a wide result
///   (1,600 columns x 2,000 chars) overshoot the budget by a whole row.
///
/// Remaining limitation: `JSON.stringify` for object values is the one thing that runs before
/// truncation (there is no standard serializer that can stop midway). Once the budget is used
/// up, later cells are not touched, so the total cost stays within "budget / cell limit" cells.
export const toTsvCapped = (
  result: QueryResult,
  maxChars: number,
  maxCellChars: number,
): { text: string; truncated: boolean } => {
  let truncated = false;
  let total = 0;

  // Formula-injection protection checks only the first character, so do it before truncation
  // (it just prepends `'`, so the order relative to cutting the tail does not matter)
  const field = (escaped: string): string => {
    if (escaped.length <= maxCellChars) {
      return sanitizeTsv(escaped);
    }
    truncated = true;
    return `${sanitizeTsv(escaped.slice(0, maxCellChars))}…`;
  };

  const header: string[] = [];
  for (const column of result.columns) {
    if (total >= maxChars) {
      truncated = true;
      break;
    }
    const text = field(escapeHeaderFormula(column));
    header.push(text);
    total += text.length + 1;
  }
  const lines = [header.join("\t")];

  for (const row of result.rows) {
    if (total >= maxChars) {
      truncated = true;
      break;
    }
    const fields: string[] = [];
    for (const value of row) {
      if (total >= maxChars) {
        truncated = true;
        break;
      }
      const text = field(escapeFormulaInjection(value, cellToString(value)));
      fields.push(text);
      total += text.length + 1;
    }
    lines.push(fields.join("\t"));
    total += 1;
  }
  return { text: lines.join("\n"), truncated };
};

export const toTsv = (result: QueryResult): string => {
  const sanitize = sanitizeTsv;
  const lines = [
    result.columns.map((c) => sanitize(escapeHeaderFormula(c))).join("\t"),
  ];
  for (const row of result.rows) {
    lines.push(
      row
        .map((v) => sanitize(escapeFormulaInjection(v, cellToString(v))))
        .join("\t"),
    );
  }
  return lines.join("\n");
};

// When columns with the same name line up (e.g. in a JOIN), make them unique by appending
// _2, _3 ... from the second one on, so that a later one does not overwrite and lose a value.
// A generated key skips names already taken, so `id, id, id_2` gives `id, id_3, id_2` rather
// than two `id_2` (same rule as the backend's Run and Log output, run_log_output.rs)
const uniqueColumnKeys = (columns: string[]): string[] => {
  const taken = new Set(columns);
  const seen = new Set<string>();
  return columns.map((column) => {
    if (!seen.has(column)) {
      seen.add(column);
      return column;
    }
    let n = 2;
    while (taken.has(`${column}_${n}`)) {
      n++;
    }
    const key = `${column}_${n}`;
    taken.add(key);
    return key;
  });
};

export const toJson = (result: QueryResult): string => {
  const keys = uniqueColumnKeys(result.columns);
  const records = result.rows.map((row) =>
    Object.fromEntries(keys.map((key, i) => [key, row[i]])),
  );
  return JSON.stringify(records, null, 2);
};

// Convert only the selected range to JSON (for Cmd+C copy). An array of objects keyed by the selected columns only.
// withHeaders is ignored for JSON because keys are always present (an argument kept to simplify the caller's branching).
// The range is assumed to be already clamped to the result size by the caller.
export const toJsonRange = (
  result: QueryResult,
  range: CellRange,
  _withHeaders: boolean,
): string => {
  const { rowStart, rowEnd, colStart, colEnd } = range;
  const cols = result.columns.slice(colStart, colEnd + 1);
  const keys = uniqueColumnKeys(cols);
  const records: Record<string, unknown>[] = [];
  for (let r = rowStart; r <= rowEnd; r++) {
    const row = result.rows[r];
    if (!row) {
      continue;
    }
    records.push(
      Object.fromEntries(keys.map((key, i) => [key, row[colStart + i]])),
    );
  }
  return JSON.stringify(records, null, 2);
};

// ---- Markdown table (CYBERNEURA-DEV-1267) ----
//
// A GitHub Flavored Markdown table (`| a | b |` + `| --- | --- |`) for pasting a result into
// GitHub, Taskshoot, Slack or an AI chat. Differences from CSV / TSV:
// - NULL is written as `NULL` (an empty cell would be indistinguishable from an empty string
//   for a human reader, and nothing parses this output back).
// - The formula-injection guard is not applied: it protects spreadsheets, and a leading `'`
//   would only corrupt the value shown in a rendered table.
// - A table always has a header row, so there is no "without headers" variant.

// Integer / decimal / exponent, optionally signed. The grammar is unambiguous (see
// NUMERIC_STRING in resultSort.ts for why `\d+\.?\d*` must not be used)
const MARKDOWN_NUMERIC_STRING = /^[+-]?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?$/;
// Longer strings are never numbers (bounds the regex work per cell)
const MARKDOWN_MAX_NUMERIC_CHARS = 64;

const markdownCellText = (value: unknown): string => {
  if (value === null || value === undefined) {
    return "NULL";
  }
  if (typeof value === "object") {
    // Compact JSON is always a single line
    return JSON.stringify(value);
  }
  return String(value);
};

const MARKDOWN_ENTITIES: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
};

const isAsciiAlphanumeric = (ch: string | undefined): boolean =>
  ch !== undefined && /^[0-9A-Za-z]$/.test(ch);

/// Escape one cell (or header) so that it is shown as the text it is where it is pasted:
/// - `|` and a line break cannot break the table (`\|`, `<br>`),
/// - `&` `<` `>` become entities, so nothing is read as HTML and the only tag this output
///   ever contains is the `<br>` made here,
/// - the inline Markdown delimiters (backslash, backtick, `*`, `~`) are backslash-escaped,
///   so a value like `**x**` is not rendered as formatting and a backtick cannot turn the
///   entities and `<br>` into literal code-span text,
/// - `]` is escaped where it would complete a link or an image (`](` and `][`), so
///   `[label](url)` / `![img](url)` stay text and no remote image is loaded. Other brackets
///   are left alone: a JSON array cell would otherwise be full of backslashes,
/// - `_` is escaped unless it sits between two ASCII letters / digits: an intraword
///   underscore never starts emphasis in GFM, and leaving it alone keeps snake_case
///   column names and identifiers readable when the table is read as plain text.
/// A single pass, so the backslashes and the `<br>` added here are not escaped again.
export const escapeMarkdownCell = (text: string): string =>
  text.replace(/\r\n|[\r\n\\|&<>`*_\]~]/g, (m, offset: number) => {
    if (m === "\r\n" || m === "\r" || m === "\n") {
      return "<br>";
    }
    if (m in MARKDOWN_ENTITIES) {
      return MARKDOWN_ENTITIES[m];
    }
    if (
      m === "_" &&
      isAsciiAlphanumeric(text[offset - 1]) &&
      isAsciiAlphanumeric(text[offset + 1])
    ) {
      return m;
    }
    if (m === "]" && text[offset + 1] !== "(" && text[offset + 1] !== "[") {
      return m;
    }
    return `\\${m}`;
  });

const isMarkdownNumeric = (value: unknown): boolean => {
  if (typeof value === "number" || typeof value === "bigint") {
    return true;
  }
  // BIGINT beyond 2^53 and DECIMAL arrive as strings (db.rs json_i64)
  return (
    typeof value === "string" &&
    value.length <= MARKDOWN_MAX_NUMERIC_CHARS &&
    MARKDOWN_NUMERIC_STRING.test(value)
  );
};

/// Markdown table of the rows [rowStart, rowEnd] and columns [colStart, colEnd] (closed
/// intervals). A column is right-aligned (`---:`) when it has at least one non-NULL value in
/// the output rows and all of them are numbers.
const markdownTable = (result: QueryResult, range: CellRange): string => {
  const { rowStart, rowEnd, colStart, colEnd } = range;
  const width = Math.max(0, colEnd - colStart + 1);
  const numeric: boolean[] = new Array(width).fill(true);
  const hasValue: boolean[] = new Array(width).fill(false);
  const body: string[] = [];
  for (let r = rowStart; r <= rowEnd; r++) {
    const row = result.rows[r];
    if (!row) {
      continue;
    }
    const fields: string[] = [];
    for (let i = 0; i < width; i++) {
      const v = row[colStart + i];
      if (v !== null && v !== undefined) {
        hasValue[i] = true;
        if (numeric[i] && !isMarkdownNumeric(v)) {
          numeric[i] = false;
        }
      }
      fields.push(escapeMarkdownCell(markdownCellText(v)));
    }
    body.push(`| ${fields.join(" | ")} |`);
  }
  const header: string[] = [];
  const separator: string[] = [];
  for (let i = 0; i < width; i++) {
    header.push(escapeMarkdownCell(result.columns[colStart + i] ?? ""));
    separator.push(numeric[i] && hasValue[i] ? "---:" : "---");
  }
  return [`| ${header.join(" | ")} |`, `| ${separator.join(" | ")} |`, ...body].join(
    "\n",
  );
};

export const toMarkdown = (result: QueryResult): string =>
  markdownTable(result, {
    rowStart: 0,
    rowEnd: result.rows.length - 1,
    colStart: 0,
    colEnd: result.columns.length - 1,
  });

// Convert only the selected range to a Markdown table (for Cmd+C copy).
// withHeaders is ignored because a Markdown table cannot exist without its header row
// (an argument kept to simplify the caller's branching, like toJsonRange).
// The range is assumed to be already clamped to the result size by the caller.
export const toMarkdownRange = (
  result: QueryResult,
  range: CellRange,
  _withHeaders: boolean,
): string => markdownTable(result, range);
