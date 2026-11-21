/**
 * Row detail panel of the result table (CYBERNEURA-DEV-1317): one row shown as a vertical
 * "column : value" list, for results with more columns than fit on screen.
 *
 * Pure functions so that they can be tested with `node --test` (tests/row-detail.test.mts).
 * The panel only displays: it never edits (cell editing stays in the grid).
 *
 * Rows are addressed like everywhere else in ResultsPane: `rowIndex` is the original index
 * into `result.rows`, and moving to the previous / next row walks `rowOrder` (the display
 * order after the sort and the filter), so ↑ / ↓ follow what is on screen.
 */

import { escapeFormulaInjection, escapeHeaderFormula } from "./export.ts";
import { filterTerms } from "./paneFilter.ts";
import { stringifyBounded } from "./resultSort.ts";

// A value shows at most this many characters in the panel. One row can have hundreds of
// columns and db.rs does not cut long TEXT / JSON cells, so the panel must not put whole
// 100KB values into the DOM (the full value is one click away in the cell inspector, and the
// copy buttons copy the full value).
export const ROW_DETAIL_MAX_CHARS = 2_000;

// JSON is pretty-printed only up to this size (parse + re-stringify of the whole value)
export const ROW_DETAIL_PRETTY_MAX_CHARS = 20_000;

export interface RowDetailField {
  /// Index of the column in the result
  colIndex: number;
  column: string;
  /// The value as in result.rows
  value: unknown;
}

export interface RowDetailDisplay {
  /// Text to show (cut at ROW_DETAIL_MAX_CHARS)
  text: string;
  /// Whether the value was cut for display
  cut: boolean;
  isNull: boolean;
  /// Whether the text is pretty-printed JSON
  isJson: boolean;
}

/// The full text of a cell, as the grid shows it (NULL, objects as one-line JSON).
export function rowDetailCellText(value: unknown): string {
  if (value === null || value === undefined) return "NULL";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

/// The columns of a row, narrowed by the column-name filter (space-separated terms, all of
/// which must appear in the column name, case-insensitive; the rule of the pane filters).
export function rowDetailFields(
  columns: readonly string[],
  row: readonly unknown[],
  query: string,
): RowDetailField[] {
  const terms = filterTerms(query);
  const fields: RowDetailField[] = [];
  for (let colIndex = 0; colIndex < columns.length; colIndex++) {
    const column = columns[colIndex];
    if (terms.length > 0) {
      const name = column.toLowerCase();
      if (!terms.every((term) => name.includes(term))) continue;
    }
    fields.push({ colIndex, column, value: row[colIndex] });
  }
  return fields;
}

const cut = (text: string): { text: string; cut: boolean } =>
  text.length > ROW_DETAIL_MAX_CHARS
    ? { text: `${text.slice(0, ROW_DETAIL_MAX_CHARS)}…`, cut: true }
    : { text, cut: false };

/// How a value is shown in the panel. JSON (objects, and strings that parse as a JSON object
/// or array) is pretty-printed like the cell inspector does, within a size budget.
export function rowDetailDisplay(value: unknown): RowDetailDisplay {
  if (value === null || value === undefined) {
    return { text: "NULL", cut: false, isNull: true, isJson: false };
  }
  if (typeof value === "object") {
    // Serialize within the budget first: JSON.stringify would walk a huge JSONB value whole
    const bounded = stringifyBounded(value, ROW_DETAIL_PRETTY_MAX_CHARS + 1);
    if (bounded.length <= ROW_DETAIL_PRETTY_MAX_CHARS) {
      return { ...cut(JSON.stringify(value, null, 2)), isNull: false, isJson: true };
    }
    return { ...cut(bounded), isNull: false, isJson: false };
  }
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (
      value.length <= ROW_DETAIL_PRETTY_MAX_CHARS &&
      (trimmed.startsWith("{") || trimmed.startsWith("["))
    ) {
      try {
        const parsed: unknown = JSON.parse(trimmed);
        return { ...cut(JSON.stringify(parsed, null, 2)), isNull: false, isJson: true };
      } catch {
        // Not JSON: shown as text
      }
    }
    return { ...cut(value), isNull: false, isJson: false };
  }
  return { text: String(value), cut: false, isNull: false, isJson: false };
}

/// "column<TAB>value" lines of the fields, for pasting into a spreadsheet or a chat. Tabs and
/// line breaks inside a value become spaces (the same as the TSV export) so that one field
/// stays on one line, and the same formula-injection guard as the TSV export applies (a
/// string starting with `=` `+` `-` `@` gets a leading `'`; numbers are left alone).
export function rowDetailTsv(fields: readonly RowDetailField[]): string {
  const clean = (text: string) => text.replace(/\t/g, " ").replace(/\r?\n|\r/g, " ");
  return fields
    .map(
      (f) =>
        `${clean(escapeHeaderFormula(f.column))}\t${clean(
          escapeFormulaInjection(f.value, rowDetailCellText(f.value)),
        )}`,
    )
    .join("\n");
}

/// The original row index `delta` rows away from `rowIndex` in the display order, clamped to
/// the first / last row on screen. null when the row is not on screen (filtered out).
export function stepRow(
  rowOrder: readonly number[],
  rowIndex: number,
  delta: number,
): number | null {
  const position = rowOrder.indexOf(rowIndex);
  if (position < 0) return null;
  const next = Math.min(rowOrder.length - 1, Math.max(0, position + delta));
  return rowOrder[next];
}
