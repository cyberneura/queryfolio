import { test } from "node:test";
import assert from "node:assert/strict";
import {
  buildSearchIndex,
  cellSearchText,
  filterRowOrder,
  MAX_CELL_SEARCH_CHARS,
  rowMatches,
  rowSearchText,
  visibleRowOrder,
  visibleRows,
} from "../src/lib/resultFilter.ts";
import { filterTerms } from "../src/lib/paneFilter.ts";

const rows: unknown[][] = [
  [1, "Alice", "alice@example.com", null],
  [2, "Bob", "bob@example.org", { role: "admin", tags: ["a", "b"] }],
  [3, "Carol", "carol@example.com", { role: "user" }],
  [4, "alice cooper", "ac@example.net", "NULLABLE"],
];

test("an empty or whitespace-only query keeps every row in the fetched order", () => {
  assert.deepEqual(visibleRowOrder(rows, null, ""), [0, 1, 2, 3]);
  assert.deepEqual(visibleRowOrder(rows, null, "   "), [0, 1, 2, 3]);
});

test("matching is a case-insensitive substring match in any cell", () => {
  assert.deepEqual(visibleRowOrder(rows, null, "ALICE"), [0, 3]);
  assert.deepEqual(visibleRowOrder(rows, null, "example.org"), [1]);
  assert.deepEqual(visibleRowOrder(rows, null, "3"), [2]);
});

test("every whitespace-separated term has to match, each in any cell", () => {
  assert.deepEqual(visibleRowOrder(rows, null, "alice .com"), [0]);
  assert.deepEqual(visibleRowOrder(rows, null, "  example   bob "), [1]);
  assert.deepEqual(visibleRowOrder(rows, null, "alice bob"), []);
});

test("NULL cells are matched as the text NULL", () => {
  // Row 0 has a real NULL, row 3 a string that contains the letters
  assert.deepEqual(visibleRowOrder(rows, null, "null"), [0, 3]);
  assert.equal(cellSearchText(null), "NULL");
  assert.equal(cellSearchText(undefined), "NULL");
});

test("JSON cells are matched on their compact serialization", () => {
  assert.deepEqual(visibleRowOrder(rows, null, "admin"), [1]);
  assert.deepEqual(visibleRowOrder(rows, null, '"role":"user"'), [2]);
  assert.equal(cellSearchText({ a: [1, "x"] }), '{"a":[1,"x"]}');
});

test("a term does not match across two neighboring cells", () => {
  const joined: unknown[][] = [["ab", "cd"], ["abcd", ""]];
  assert.deepEqual(visibleRowOrder(joined, null, "bc"), [1]);
});

test("numbers and booleans are matched as shown", () => {
  const values: unknown[][] = [[1.5, true], [20, false]];
  assert.deepEqual(visibleRowOrder(values, null, "1.5"), [0]);
  assert.deepEqual(visibleRowOrder(values, null, "false"), [1]);
});

test("the filter keeps the sorted order and returns original row indexes", () => {
  const sort = { colIndex: 1, direction: "desc" } as const;
  // Sorted by name descending: Carol(2), Bob(1), alice cooper(3), Alice(0) under localeCompare
  const all = visibleRowOrder(rows, sort, "");
  const filtered = visibleRowOrder(rows, sort, "example.com");
  assert.deepEqual(
    filtered,
    all.filter((i) => i === 0 || i === 2),
  );
  assert.deepEqual(visibleRowOrder(rows, { colIndex: 0, direction: "desc" }, "alice"), [3, 0]);
});

test("a cached search index gives the same result as building it on the fly", () => {
  const index = buildSearchIndex(rows);
  assert.equal(index.length, rows.length);
  for (const query of ["alice", "example .com", "null", "nothing-matches"]) {
    assert.deepEqual(
      visibleRowOrder(rows, null, query, index),
      visibleRowOrder(rows, null, query),
    );
  }
});

test("filterRowOrder returns the same array when there are no terms", () => {
  const order = [2, 0, 1];
  assert.equal(filterRowOrder(order, buildSearchIndex(rows), []), order);
  assert.deepEqual(filterRowOrder(order, buildSearchIndex(rows), filterTerms("bob")), [1]);
});

test("visibleRows returns the same object without sort and filter, else the rows on screen", () => {
  const result = { columns: ["id", "name", "email", "extra"], rows };
  assert.equal(visibleRows(result, null, ""), result);
  const filtered = visibleRows(result, { colIndex: 0, direction: "desc" }, "alice");
  assert.deepEqual(filtered.rows, [rows[3], rows[0]]);
  assert.equal(filtered.columns, result.columns);
  // The source is never changed
  assert.equal(result.rows.length, 4);
});

test("long cells are searched within a bounded prefix", () => {
  const long = "x".repeat(MAX_CELL_SEARCH_CHARS) + "needle";
  assert.equal(cellSearchText(long).length, MAX_CELL_SEARCH_CHARS);
  assert.deepEqual(visibleRowOrder([[long], ["needle"]], null, "needle"), [1]);
  const big = { text: "y".repeat(MAX_CELL_SEARCH_CHARS * 3) };
  assert.ok(cellSearchText(big).length <= MAX_CELL_SEARCH_CHARS);
  // No per-row limit: a short cell after many long ones is still found
  const wide = [...Array.from({ length: 200 }, () => "z".repeat(MAX_CELL_SEARCH_CHARS * 2)), "tail"];
  assert.ok(rowSearchText(wide).length <= 201 * (MAX_CELL_SEARCH_CHARS + 1));
  assert.deepEqual(visibleRowOrder([wide], null, "tail"), [0]);
});

test("building the index of a large result with huge cells stays fast", () => {
  const huge = "a".repeat(100_000);
  const many = Array.from({ length: 500 }, (_, i) => [i, huge, huge, huge]);
  const started = performance.now();
  const index = buildSearchIndex(many);
  const order = filterRowOrder(
    many.map((_, i) => i),
    index,
    filterTerms("499"),
  );
  const elapsed = performance.now() - started;
  assert.deepEqual(order, [499]);
  assert.ok(elapsed < 1000, `took ${elapsed} ms`);
});

test("rowMatches needs every term", () => {
  assert.equal(rowMatches("abc\ndef", ["ab", "ef"]), true);
  assert.equal(rowMatches("abc\ndef", ["ab", "zz"]), false);
  assert.equal(rowMatches("abc", []), true);
});
