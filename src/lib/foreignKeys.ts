/**
 * Following a foreign key from a result cell (CYBERNEURA-DEV-1319).
 *
 * Pure helpers (tests/foreign-keys.test.mts): which table's foreign keys to look up for a
 * result, and the SELECT that opens the referenced row. The SELECT only reads, so it runs
 * with the Writable switch off and gets the auto LIMIT like any query from the editor.
 */

import type { ForeignKeyInfo } from "./api";
import {
  normalizeTableName,
  quoteIdent,
  singleTableSelectTable,
  type NormalizedEngine,
} from "./editableResult.ts";

/// The table whose foreign keys apply to the result of `sql`, or null. Same detection as
/// cell editing: only a single-table SELECT of bare columns (or `*`) guarantees that a result
/// column is the table's column of the same name. Duplicate column names are ambiguous.
export function foreignKeyLookupTable(
  engine: NormalizedEngine,
  sql: string,
  columns: readonly string[],
): string | null {
  const table = singleTableSelectTable(sql);
  if (!table || new Set(columns).size !== columns.length) {
    return null;
  }
  return normalizeTableName(engine, table);
}

/// `SELECT * FROM <referenced table> WHERE <referenced column> = <literal>`. The catalog
/// names are quoted as stored (each part separately, so a name with a dot stays one part).
/// `literal` is the SQL literal of the cell value (literalFromValue).
export function referencedRowSql(
  engine: NormalizedEngine,
  fk: ForeignKeyInfo,
  literal: string,
): string {
  const table = [fk.ref_schema, fk.ref_table]
    .filter((part): part is string => part !== null && part !== "")
    .map((part) => quoteIdent(engine, part))
    .join(".");
  return `SELECT * FROM ${table} WHERE ${quoteIdent(engine, fk.ref_column)} = ${literal}`;
}
