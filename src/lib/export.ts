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
const uniqueColumnKeys = (columns: string[]): string[] => {
  const counts = new Map<string, number>();
  return columns.map((column) => {
    const seen = counts.get(column) ?? 0;
    counts.set(column, seen + 1);
    return seen === 0 ? column : `${column}_${seen + 1}`;
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
