import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ROW_DETAIL_MAX_CHARS,
  ROW_DETAIL_PRETTY_MAX_CHARS,
  rowDetailCellText,
  rowDetailDisplay,
  rowDetailFields,
  rowDetailTsv,
  stepRow,
} from "../src/lib/rowDetail.ts";

const columns = ["id", "user_name", "user_email", "created_at", "meta"];
const row: unknown[] = [7, "Alice", null, "2026-11-21 10:00:00", { a: 1 }];

test("all columns are listed in result order without a filter", () => {
  const fields = rowDetailFields(columns, row, "");
  assert.deepEqual(
    fields.map((f) => [f.colIndex, f.column, f.value]),
    [
      [0, "id", 7],
      [1, "user_name", "Alice"],
      [2, "user_email", null],
      [3, "created_at", "2026-11-21 10:00:00"],
      [4, "meta", { a: 1 }],
    ],
  );
});

test("the column filter is a case-insensitive match of every term on the column name", () => {
  assert.deepEqual(
    rowDetailFields(columns, row, "USER").map((f) => f.column),
    ["user_name", "user_email"],
  );
  assert.deepEqual(
    rowDetailFields(columns, row, "user mail").map((f) => f.column),
    ["user_email"],
  );
  // Values are not matched, only names
  assert.deepEqual(rowDetailFields(columns, row, "alice"), []);
  // The original column index is kept for the copy / inspect buttons
  assert.deepEqual(rowDetailFields(columns, row, "created").map((f) => f.colIndex), [3]);
});

test("NULL, scalars and JSON are displayed like the grid / cell inspector", () => {
  assert.deepEqual(rowDetailDisplay(null), {
    text: "NULL",
    cut: false,
    isNull: true,
    isJson: false,
  });
  assert.equal(rowDetailDisplay(42).text, "42");
  assert.equal(rowDetailDisplay(true).text, "true");
  const obj = rowDetailDisplay({ a: [1, 2] });
  assert.equal(obj.isJson, true);
  assert.equal(obj.text, JSON.stringify({ a: [1, 2] }, null, 2));
  const str = rowDetailDisplay(' {"k": "v"} ');
  assert.equal(str.isJson, true);
  assert.equal(str.text, '{\n  "k": "v"\n}');
  // Not JSON: as is
  const brace = rowDetailDisplay("{not json");
  assert.equal(brace.isJson, false);
  assert.equal(brace.text, "{not json");
});

test("long values are cut for display and huge JSON is not pretty-printed", () => {
  const long = "x".repeat(ROW_DETAIL_MAX_CHARS + 10);
  const shown = rowDetailDisplay(long);
  assert.equal(shown.cut, true);
  assert.equal(shown.text.length, ROW_DETAIL_MAX_CHARS + 1);
  assert.ok(shown.text.endsWith("…"));

  const big = { data: "y".repeat(ROW_DETAIL_PRETTY_MAX_CHARS) };
  const bigShown = rowDetailDisplay(big);
  assert.equal(bigShown.isJson, false);
  assert.equal(bigShown.cut, true);
  assert.ok(bigShown.text.startsWith('{"data":"yyy'));

  const bigString = `[${'"z",'.repeat(ROW_DETAIL_PRETTY_MAX_CHARS)}"z"]`;
  assert.equal(rowDetailDisplay(bigString).isJson, false);
});

test("the full cell text is what the copy buttons use", () => {
  assert.equal(rowDetailCellText(null), "NULL");
  assert.equal(rowDetailCellText({ a: 1 }), '{"a":1}');
  assert.equal(rowDetailCellText("x".repeat(5000)).length, 5000);
});

test("TSV has one column<TAB>value line per field, tabs and line breaks flattened", () => {
  const fields = rowDetailFields(
    ["a", "b\tc", "d", "e"],
    ["1", "x\ty", "line1\nline2\r\nline3", null],
    "",
  );
  assert.equal(rowDetailTsv(fields), "a\t1\nb c\tx y\nd\tline1 line2 line3\ne\tNULL");
  assert.equal(rowDetailTsv([]), "");
});

test("TSV guards against formula injection like the TSV export", () => {
  const fields = rowDetailFields(["=col", "v", "n"], ["=1+1", "@SUM(A1)", -1], "");
  assert.equal(rowDetailTsv(fields), "'=col\t'=1+1\nv\t'@SUM(A1)\nn\t-1");
});

test("stepping follows the display order and stops at both ends", () => {
  // Display order after a sort / filter: original rows 4, 0, 2
  const order = [4, 0, 2];
  assert.equal(stepRow(order, 4, 1), 0);
  assert.equal(stepRow(order, 0, 1), 2);
  assert.equal(stepRow(order, 2, 1), 2);
  assert.equal(stepRow(order, 0, -1), 4);
  assert.equal(stepRow(order, 4, -1), 4);
  // A row that is not on screen (filtered out)
  assert.equal(stepRow(order, 1, 1), null);
});
