/**
 * Client-side filter of fetched result rows (CYBERNEURA-DEV-1266).
 *
 * Nothing is sent to the server: the filter only narrows the rows that are already in the
 * result tab (a result cut by default_limit / truncated is filtered as is). Like the sort
 * (resultSort.ts), `result.rows` itself is never changed. The filter drops entries from
 * `rowOrder`, the array of original row indexes in display order, so cell edits, the cell
 * inspector and pending edits (all keyed by the original index) keep pointing at the same row.
 *
 * The matching rule is the one of the CONNECTIONS / FILES / TABLES filters (paneFilter.ts):
 * the query is split on whitespace, matching is a case-insensitive substring match, and every
 * term has to appear in at least one cell of the row.
 */

import type { QueryResult } from "./api";
import { filterTerms } from "./paneFilter.ts";
import { sortedRowOrder, stringifyBounded, type SortState } from "./resultSort.ts";

// A cell contributes at most this many characters to the search text. Building the search
// text walks every cell of the result on the main thread, so its cost must grow with the
// number of cells, not with the total characters of the result (AGENTS.md: db.rs does not
// cut long TEXT / JSON cells). A term that only occurs beyond this limit is not found; the
// filter input says so in its tooltip. There is deliberately no per-row limit: it would make
// the short cells of the last columns of a wide result unsearchable.
export const MAX_CELL_SEARCH_CHARS = 2_000;

// Cells of one row are joined with a line feed. Terms are split on whitespace and therefore
// never contain one, so a term cannot match across two neighboring cells.
const CELL_SEPARATOR = "\n";

/// The text one cell is matched against (before lowercasing). NULL is the string `NULL` as
/// shown in the grid; objects / arrays (JSON cells) are serialized within the cell budget.
export function cellSearchText(value: unknown): string {
  if (value === null || value === undefined) return "NULL";
  if (typeof value === "string") {
    return value.length > MAX_CELL_SEARCH_CHARS
      ? value.slice(0, MAX_CELL_SEARCH_CHARS)
      : value;
  }
  if (typeof value === "object") return stringifyBounded(value, MAX_CELL_SEARCH_CHARS);
  return String(value);
}

/// The lowercase search text of one row: its cells joined with CELL_SEPARATOR.
export function rowSearchText(row: readonly unknown[]): string {
  return row.map(cellSearchText).join(CELL_SEPARATOR).toLowerCase();
}

/// The search text of every row, indexed by the original row index. Built once per result
/// and reused for every keystroke (the caller caches it), so typing costs one `includes`
/// per row and term instead of re-serializing the cells.
export function buildSearchIndex(rows: readonly (readonly unknown[])[]): string[] {
  return rows.map(rowSearchText);
}

/// Whether a row's search text contains every term. No terms = everything matches.
export const rowMatches = (searchText: string, terms: readonly string[]): boolean =>
  terms.every((term) => searchText.includes(term));

/// Drop the rows that do not match from a display order (original row indexes). The relative
/// order is kept, so a sorted order stays sorted. Returns the same array when there are no terms.
export function filterRowOrder(
  order: readonly number[],
  searchIndex: readonly string[],
  terms: readonly string[],
): readonly number[] {
  if (terms.length === 0) return order;
  return order.filter((rowIndex) => rowMatches(searchIndex[rowIndex] ?? "", terms));
}

/// The display order of `rows` for a sort and a filter query: the rows that pass the filter,
/// in sorted order. `searchIndex` is the cached result of `buildSearchIndex(rows)`; it is
/// built here when omitted.
export function visibleRowOrder(
  rows: readonly (readonly unknown[])[],
  sort: SortState | null,
  query: string,
  searchIndex?: readonly string[],
): number[] {
  const order = sortedRowOrder(rows, sort);
  const terms = filterTerms(query);
  if (terms.length === 0) return order;
  return [...filterRowOrder(order, searchIndex ?? buildSearchIndex(rows), terms)];
}

/// The result reduced to the rows on screen, in display order (for Copy / Export and Cmd+C,
/// which follow what is shown). Returns the same object when there is neither a sort nor a
/// filter, so the common path allocates nothing.
export function visibleRows<T extends Pick<QueryResult, "rows">>(
  result: T,
  sort: SortState | null,
  query: string,
  searchIndex?: readonly string[],
): T {
  if (!sort && filterTerms(query).length === 0) return result;
  const order = visibleRowOrder(result.rows, sort, query, searchIndex);
  return { ...result, rows: order.map((i) => result.rows[i]) };
}
