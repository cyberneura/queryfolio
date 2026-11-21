/**
 * "SQL INSERT" output format of the result Copy / Export (CYBERNEURA-DEV-1318).
 *
 * Turns the rows of a single-table SELECT into `INSERT INTO t (c1, c2) VALUES (...);`
 * statements, one per row, for copying a few production rows into a development database or
 * pasting reproduction data into a test. The statements are only written to the clipboard or a
 * file, never executed, so the read-only / dangerous-statement guards are not involved.
 *
 * The table comes from `singleTableSelectTable` (the same detection as cell editing: JOINs,
 * aggregates, aliases and expressions are rejected, because the result columns must be real
 * columns of that table), and identifiers / values are quoted with the cell-editing helpers of
 * editableResult.ts, so the dialects (MySQL / PostgreSQL / SQLite) stay in one place.
 */

import type { CellRange } from "./export.ts";
import {
  literalFromValue,
  normalizeTableName,
  quoteIdent,
  quoteQualified,
  singleTableSelectTable,
  type NormalizedEngine,
} from "./editableResult.ts";

/// Where the INSERT format can be used: the table to insert into, or why it cannot be used.
export type InsertTarget =
  | { ok: true; engine: NormalizedEngine; table: string }
  | { ok: false; reason: string };

/// Decide whether the result of `sql` can be written as INSERT statements.
/// `engine` must already be one of the SQL dialects that cell editing supports (the caller
/// hides the format for the other engines via capabilities.supports_editable_cells).
export function insertTarget(
  engine: NormalizedEngine,
  sql: string,
  columns: readonly string[],
): InsertTarget {
  const table = singleTableSelectTable(sql);
  if (!table) {
    return {
      ok: false,
      reason:
        "SQL INSERT needs a single-table SELECT (no JOIN, aggregate, alias or expression)",
    };
  }
  if (columns.length === 0) {
    return { ok: false, reason: "The result has no columns" };
  }
  // The same column twice would put the same column twice in the INSERT (an error at best)
  if (new Set(columns).size !== columns.length) {
    return { ok: false, reason: "The result has duplicate column names" };
  }
  return { ok: true, engine, table: normalizeTableName(engine, table) };
}

/// The SQL literal of a cell value. JSON values (objects / arrays) are inserted as their JSON
/// text. The result has values but no column types, so a JSON column holding a bare scalar
/// (`"hello"`, `123`, JSON `null`) cannot be told from a plain value and is written as one
/// (README "Known limits"); everything else uses the WHERE-literal rules of cell editing (NULL, numbers,
/// booleans per dialect, strings with the dialect's escaping — dates and big integers arrive
/// from the backend as strings, and the database casts them on assignment).
export function insertLiteral(engine: NormalizedEngine, value: unknown): string {
  if (value !== null && typeof value === "object") {
    return literalFromValue(engine, JSON.stringify(value));
  }
  return literalFromValue(engine, value);
}

/// INSERT statements for the rows (one statement per row, `;`-terminated, one per line).
/// With a range, only the selected rows and columns are written (Cmd+C of a cell selection).
export function toInsertStatements(
  engine: NormalizedEngine,
  table: string,
  columns: readonly string[],
  rows: readonly (readonly unknown[])[],
  range?: CellRange,
): string {
  const colStart = range?.colStart ?? 0;
  const colEnd = range?.colEnd ?? columns.length - 1;
  const rowStart = range?.rowStart ?? 0;
  const rowEnd = range?.rowEnd ?? rows.length - 1;
  const colIndexes: number[] = [];
  for (let c = colStart; c <= colEnd; c++) colIndexes.push(c);
  const head = `INSERT INTO ${quoteQualified(engine, table)} (${colIndexes
    .map((c) => quoteIdent(engine, columns[c]))
    .join(", ")}) VALUES (`;
  const lines: string[] = [];
  for (let r = rowStart; r <= rowEnd; r++) {
    const row = rows[r];
    if (!row) continue;
    lines.push(`${head}${colIndexes.map((c) => insertLiteral(engine, row[c])).join(", ")});`);
  }
  return lines.join("\n");
}
