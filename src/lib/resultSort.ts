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

/// Serialize a JSON cell (object / array) to at most `max` characters, stopping as soon as
/// the budget is spent. `JSON.stringify` would walk a 100KB JSONB value in full for every
/// row; the sort only needs a bounded prefix. The output is the usual compact JSON up to the
/// cut, so prefixes of equal values stay equal and order between different values is kept
/// for everything that differs within the budget
export function stringifyBounded(value: unknown, max: number): string {
  const parts: string[] = [];
  let used = 0;
  const push = (text: string): boolean => {
    parts.push(text);
    used += text.length;
    return used < max;
  };
  const walk = (v: unknown): boolean => {
    if (v === null || v === undefined) return push("null");
    if (typeof v === "number" || typeof v === "boolean") return push(String(v));
    if (typeof v === "string") {
      // The string itself can be huge; cut it before escaping
      return push(JSON.stringify(v.length > max ? v.slice(0, max) : v));
    }
    if (Array.isArray(v)) {
      if (!push("[")) return false;
      for (let i = 0; i < v.length; i++) {
        if (i > 0 && !push(",")) return false;
        if (!walk(v[i])) return false;
      }
      return push("]");
    }
    if (typeof v === "object") {
      if (!push("{")) return false;
      let first = true;
      for (const [k, item] of Object.entries(v as Record<string, unknown>)) {
        if (item === undefined) continue;
        if (!first && !push(",")) return false;
        first = false;
        if (!push(JSON.stringify(k) + ":")) return false;
        if (!walk(item)) return false;
      }
      return push("}");
    }
    return push(String(v));
  };
  walk(value);
  const text = parts.join("");
  return text.length > max ? text.slice(0, max) : text;
}

/// The comparison key of one non-null cell, computed once per row by `sortedRowOrder`
/// (decorate-sort-undecorate), so serialization / numeric parsing is not repeated for every
/// comparison. `num` is set for numbers, booleans (false < true) and numeric strings (BIGINT /
/// DECIMAL arrive as strings from the backend when they exceed 2^53, see db.rs json_i64);
/// `decimal` keeps the exact digits of a plain decimal string; `text` is the bounded text key
interface CellKey {
  num: number | null;
  decimal: string | null;
  text: string;
}

function cellKey(v: unknown): CellKey {
  if (typeof v === "number") return { num: v, decimal: null, text: String(v) };
  if (typeof v === "boolean") return { num: v ? 1 : 0, decimal: null, text: String(v) };
  if (typeof v === "string") {
    const text = v.length > MAX_COMPARE_CHARS ? v.slice(0, MAX_COMPARE_CHARS) : v;
    if (v.length <= MAX_NUMERIC_CHARS && NUMERIC_STRING.test(v)) {
      return { num: Number(v), decimal: DECIMAL_STRING.test(v) ? v : null, text };
    }
    return { num: null, decimal: null, text };
  }
  if (typeof v === "object") {
    return { num: null, decimal: null, text: stringifyBounded(v, MAX_COMPARE_CHARS) };
  }
  return { num: null, decimal: null, text: String(v) };
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

function compareKeys(a: CellKey, b: CellKey): number {
  if (a.num !== null && b.num !== null) {
    if (a.decimal !== null && b.decimal !== null) {
      return compareDecimalStrings(a.decimal, b.decimal);
    }
    return a.num < b.num ? -1 : a.num > b.num ? 1 : 0;
  }
  // Mixed numeric / text: numbers before text, so that a column with a few text values
  // still groups its numbers together instead of interleaving them lexicographically
  if (a.num !== null) return -1;
  if (b.num !== null) return 1;
  return a.text.localeCompare(b.text);
}

/// Ascending comparison of two non-null cell values. Both numeric (number / boolean /
/// numeric string) -> numeric comparison, everything else -> locale-aware string comparison.
/// null / undefined are not handled here: `sortedRowOrder` always puts them last.
/// Bounded: numeric detection stops at MAX_NUMERIC_CHARS and text (including serialized
/// JSON cells) is compared on at most MAX_COMPARE_CHARS, so a 100KB cell cannot make one
/// comparison expensive. `sortedRowOrder` builds the keys once per row instead of calling this.
export function compareCells(a: unknown, b: unknown): number {
  return compareKeys(cellKey(a), cellKey(b));
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
  // One key per row (not per comparison): parsing / serialization happens N times, not N log N
  const keys = new Map<number, CellKey>();
  for (const i of order) {
    const v = rows[i]?.[colIndex];
    if (isNullish(v)) {
      nulls.push(i);
    } else {
      nonNull.push(i);
      keys.set(i, cellKey(v));
    }
  }
  // Array.prototype.sort is stable, so equal keys keep their fetched order
  nonNull.sort((i, j) => sign * compareKeys(keys.get(i)!, keys.get(j)!));
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
