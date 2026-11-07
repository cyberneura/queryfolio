/**
 * Client-side sorting of fetched result rows (CYBERNEURA-DEV-1206).
 *
 * Nothing is sent to the server: the sort only reorders the rows that are already in the
 * result tab, so it is always "within the fetched rows" (a result cut by default_limit /
 * truncated is sorted as is). `result.rows` itself is never reordered. The display order is
 * expressed as `rowOrder`, an array of original row indexes, so that cell edits, the cell
 * inspector and pending edits (all keyed by the original index) keep pointing at the same row
 * no matter how the table is sorted. See ResultsPane.svelte for the wiring.
 */

import type { QueryResult } from "./api";

export type SortDirection = "asc" | "desc";

export interface SortState {
  colIndex: number;
  direction: SortDirection;
}

/// The next sort state when a column header toggle is clicked: ascending -> descending -> off.
/// Clicking another column starts from ascending on that column.
export function nextSort(
  current: SortState | null,
  colIndex: number,
): SortState | null {
  if (!current || current.colIndex !== colIndex) {
    return { colIndex, direction: "asc" };
  }
  return current.direction === "asc" ? { colIndex, direction: "desc" } : null;
}

const isNullish = (v: unknown): v is null | undefined =>
  v === null || v === undefined;

// Integer / decimal / exponent, optionally signed. Matched against trimmed strings only
// (" 12" is a text column value, not a number). The grammar is unambiguous (the fraction
// digits always follow a literal dot) so the regex cannot backtrack quadratically on a long
// digit run followed by a non-digit; `\d+\.?\d*` would (Codex found 5.6 s for one 40,000-digit cell).
const NUMERIC_STRING = /^[+-]?(\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?$/;
// Plain decimal notation (no exponent): compared digit by digit so that DECIMAL / BIGINT
// values beyond the precision of Number keep their order
const DECIMAL_STRING = /^[+-]?(\d+(\.\d*)?|\.\d+)$/;
// Cells longer than this are never treated as numbers (no real numeric column produces them;
// a huge digit-only TEXT is still bounded work) and are compared as text by their prefix only.
// Comparison cost must not grow with the total characters of a result (AGENTS.md), since
// sortedRowOrder runs synchronously on the main thread
const MAX_NUMERIC_CHARS = 64;
const MAX_COMPARE_CHARS = 10_000;

/// A value that the sort treats as a number: numbers, booleans (false < true) and numeric strings
/// (BIGINT / DECIMAL arrive as strings from the backend when they exceed 2^53, see db.rs json_i64).
function numericValue(v: unknown): number | null {
  if (typeof v === "number") return v;
  if (typeof v === "boolean") return v ? 1 : 0;
  if (
    typeof v === "string" &&
    v.length <= MAX_NUMERIC_CHARS &&
    NUMERIC_STRING.test(v)
  ) {
    return Number(v);
  }
  return null;
}

/// Split a plain decimal string into sign, integer digits (no leading zeros) and fraction digits
/// (no trailing zeros). "-0.0" becomes a non-negative zero.
function decimalParts(s: string): { neg: boolean; int: string; frac: string } {
  const neg = s.startsWith("-");
  const body = s.replace(/^[+-]/, "");
  const dot = body.indexOf(".");
  const int = (dot < 0 ? body : body.slice(0, dot)).replace(/^0+/, "");
  const frac = (dot < 0 ? "" : body.slice(dot + 1)).replace(/0+$/, "");
  const zero = int === "" && frac === "";
  return { neg: neg && !zero, int, frac };
}

/// Compare two plain decimal strings exactly (Number() would lose precision beyond 2^53
/// and merge e.g. "9007199254740993" with "9007199254740992.5").
function compareDecimalStrings(a: string, b: string): number {
  const pa = decimalParts(a);
  const pb = decimalParts(b);
  if (pa.neg !== pb.neg) return pa.neg ? -1 : 1;
  let magnitude: number;
  if (pa.int.length !== pb.int.length) {
    magnitude = Math.sign(pa.int.length - pb.int.length);
  } else if (pa.int !== pb.int) {
    magnitude = pa.int < pb.int ? -1 : 1;
  } else {
    // Same integer part: compare fractions left-aligned ("5" < "51", "5" == "50")
    const width = Math.max(pa.frac.length, pb.frac.length);
    const fa = pa.frac.padEnd(width, "0");
    const fb = pb.frac.padEnd(width, "0");
    magnitude = fa < fb ? -1 : fa > fb ? 1 : 0;
  }
  return pa.neg ? -magnitude : magnitude;
}

// Text key of a cell for comparison, cut to MAX_COMPARE_CHARS (cells beyond that compare
// equal on their prefix and keep the fetched order, which is acceptable for a visual sort)
const textOf = (v: unknown): string => {
  const text =
    typeof v === "string"
      ? v
      : typeof v === "object"
        ? JSON.stringify(v)
        : String(v);
  return text.length > MAX_COMPARE_CHARS ? text.slice(0, MAX_COMPARE_CHARS) : text;
};

/// Ascending comparison of two non-null cell values. Both numeric (number / boolean /
/// numeric string) -> numeric comparison, everything else -> locale-aware string comparison.
/// null / undefined are not handled here: `sortedRowOrder` always puts them last.
/// Bounded: numeric detection stops at MAX_NUMERIC_CHARS and text is compared on at most
/// MAX_COMPARE_CHARS, so a 100KB TEXT cell cannot make one comparison expensive.
export function compareCells(a: unknown, b: unknown): number {
  const na = numericValue(a);
  const nb = numericValue(b);
  if (na !== null && nb !== null) {
    if (
      typeof a === "string" &&
      typeof b === "string" &&
      DECIMAL_STRING.test(a) &&
      DECIMAL_STRING.test(b)
    ) {
      return compareDecimalStrings(a, b);
    }
    return na < nb ? -1 : na > nb ? 1 : 0;
  }
  // Mixed numeric / text: numbers before text, so that a column with a few text values
  // still groups its numbers together instead of interleaving them lexicographically
  if (na !== null) return -1;
  if (nb !== null) return 1;
  return textOf(a).localeCompare(textOf(b));
}

/// The display order of `rows` as original row indexes. Identity order when `sort` is null.
/// Stable (ties keep the fetched order), and null / undefined cells go last in both directions.
export function sortedRowOrder(
  rows: readonly (readonly unknown[])[],
  sort: SortState | null,
): number[] {
  const order = rows.map((_, i) => i);
  if (!sort || sort.colIndex < 0) return order;
  const { colIndex, direction } = sort;
  const sign = direction === "desc" ? -1 : 1;
  const nonNull: number[] = [];
  const nulls: number[] = [];
  for (const i of order) {
    (isNullish(rows[i]?.[colIndex]) ? nulls : nonNull).push(i);
  }
  // Array.prototype.sort is stable, so equal keys keep their fetched order
  nonNull.sort((i, j) => sign * compareCells(rows[i][colIndex], rows[j][colIndex]));
  return nonNull.concat(nulls);
}

/// The result with its rows in display order (for Copy / Export, which follow what is on screen).
/// Returns the same object when there is no sort, so the common path allocates nothing.
export function reorderRows<T extends Pick<QueryResult, "rows">>(
  result: T,
  sort: SortState | null,
): T {
  if (!sort) return result;
  const order = sortedRowOrder(result.rows, sort);
  return { ...result, rows: order.map((i) => result.rows[i]) };
}
