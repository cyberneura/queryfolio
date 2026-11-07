import { test } from "node:test";
import assert from "node:assert/strict";
import {
  compareCells,
  nextSort,
  reorderRows,
  sortedRowOrder,
  stringifyBounded,
} from "../src/lib/resultSort.ts";

const column = (values: unknown[]): unknown[][] => values.map((v) => [v]);

test("nextSort cycles ascending -> descending -> off and restarts on another column", () => {
  assert.deepEqual(nextSort(null, 2), { colIndex: 2, direction: "asc" });
  assert.deepEqual(nextSort({ colIndex: 2, direction: "asc" }, 2), {
    colIndex: 2,
    direction: "desc",
  });
  assert.equal(nextSort({ colIndex: 2, direction: "desc" }, 2), null);
  assert.deepEqual(nextSort({ colIndex: 2, direction: "desc" }, 0), {
    colIndex: 0,
    direction: "asc",
  });
});

test("null and undefined go last in both directions", () => {
  const rows = column([3, null, 1, undefined, 2]);
  assert.deepEqual(sortedRowOrder(rows, { colIndex: 0, direction: "asc" }), [2, 4, 0, 1, 3]);
  assert.deepEqual(sortedRowOrder(rows, { colIndex: 0, direction: "desc" }), [0, 4, 2, 1, 3]);
});

test("numbers and numeric strings are compared numerically, text lexically after numbers", () => {
  const rows = column(["10", 9, "2.5", "abc", "ABD", 1e3, "-1"]);
  const order = sortedRowOrder(rows, { colIndex: 0, direction: "asc" });
  assert.deepEqual(
    order.map((i) => rows[i][0]),
    ["-1", "2.5", 9, "10", 1e3, "abc", "ABD"],
  );
});

test("big integer strings beyond 2^53 are compared exactly", () => {
  const a = "9007199254740993";
  const b = "9007199254740992";
  assert.equal(compareCells(a, b), 1);
  assert.equal(compareCells(b, a), -1);
  assert.equal(compareCells("-9007199254740993", "-9007199254740992"), -1);
  assert.equal(compareCells("0012", "12"), 0);
  assert.equal(compareCells("-0", "0"), 0);
  assert.equal(compareCells("123456789012345678901", "99"), 1);
});

test("big decimal strings keep their order and integers mix with them exactly", () => {
  assert.equal(compareCells("9007199254740992.5", "9007199254740993"), -1);
  assert.equal(compareCells("9007199254740993", "9007199254740992.5"), 1);
  assert.equal(compareCells("0.10", "0.1"), 0);
  assert.equal(compareCells("1.5", "1.50000000000000001"), -1);
  assert.equal(compareCells("-1.5", "-1.25"), -1);
  assert.equal(compareCells("-0.0", "0"), 0);
  assert.equal(compareCells(".5", "0.25"), 1);
  // Exponent notation and numbers fall back to Number() (no exact path for those)
  assert.equal(compareCells("1e3", "999"), 1);
  assert.equal(compareCells(1000, "1e3"), 0);
});

test("a long digit run followed by a non-digit is text and does not backtrack", () => {
  const pathological = "1".repeat(40_000) + "x";
  const started = performance.now();
  // Text (not numeric): sorts after numbers
  assert.equal(compareCells(pathological, 5), 1);
  assert.equal(compareCells(pathological, "text"), -1);
  // Over MAX_NUMERIC_CHARS, even a pure digit run is compared as text
  assert.equal(compareCells("9".repeat(100), 5), 1);
  // The 100KB TEXT case: linear and cut to the compare prefix
  const huge = "a".repeat(100_000);
  assert.equal(compareCells(huge, huge + "b"), 0);
  assert.ok(performance.now() - started < 200, "comparison must stay cheap");
});

test("stringifyBounded produces compact JSON and stops at the budget", () => {
  assert.equal(stringifyBounded({ a: 1, b: [true, null, "x"] }, 1000), '{"a":1,"b":[true,null,"x"]}');
  assert.equal(stringifyBounded({ a: undefined, b: 2 }, 1000), '{"b":2}');
  const cut = stringifyBounded({ k: "y".repeat(50_000), z: 1 }, 100);
  assert.equal(cut.length, 100);
  assert.ok(cut.startsWith('{"k":"yyy'));
  // Equal prefixes compare equal, differences inside the budget keep their order
  assert.equal(compareCells({ id: 1, blob: "q".repeat(100_000) }, { id: 2, blob: "q".repeat(100_000) }), -1);
});

test("sorting 500 large JSON cells is bounded work (keys are built once per row)", () => {
  const payload = "p".repeat(100_000);
  // Objects compare by their JSON text, so zero-pad the varying field to make the expected
  // order unambiguous ("001" < "500")
  const rows = Array.from({ length: 500 }, (_, i) => [
    { n: String(500 - i).padStart(3, "0"), payload },
  ]);
  const started = performance.now();
  const order = sortedRowOrder(rows, { colIndex: 0, direction: "asc" });
  assert.equal(order[0], 499);
  assert.equal(order[499], 0);
  assert.ok(performance.now() - started < 1000, "sort must stay cheap");
});

test("booleans sort as 0 / 1 and objects by their JSON text", () => {
  assert.equal(compareCells(false, true), -1);
  assert.equal(compareCells(true, 1), 0);
  assert.equal(compareCells({ a: 1 }, { a: 2 }), -1);
  assert.equal(compareCells([1], "[1]"), 0);
});

test("the sort is stable: equal keys keep the fetched order", () => {
  const rows = [
    ["b", 1],
    ["a", 2],
    ["b", 3],
    ["a", 4],
  ];
  assert.deepEqual(sortedRowOrder(rows, { colIndex: 0, direction: "asc" }), [1, 3, 0, 2]);
  assert.deepEqual(sortedRowOrder(rows, { colIndex: 0, direction: "desc" }), [0, 2, 1, 3]);
});

test("no sort returns the identity order and reorderRows returns the same object", () => {
  const result = { rows: column([2, 1]) };
  assert.deepEqual(sortedRowOrder(result.rows, null), [0, 1]);
  assert.equal(reorderRows(result, null), result);
  const sorted = reorderRows(result, { colIndex: 0, direction: "asc" });
  assert.deepEqual(sorted.rows, [[1], [2]]);
  // The original rows are untouched
  assert.deepEqual(result.rows, [[2], [1]]);
});

test("a column index outside the row yields nulls only, so the order is unchanged", () => {
  const rows = column([2, 1, 3]);
  assert.deepEqual(sortedRowOrder(rows, { colIndex: 5, direction: "asc" }), [0, 1, 2]);
});
