import { test } from "node:test";
import assert from "node:assert/strict";
import {
  compareCells,
  nextSort,
  reorderRows,
  sortedRowOrder,
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
