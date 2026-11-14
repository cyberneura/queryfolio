import { test } from "node:test";
import assert from "node:assert/strict";
import { escapeMarkdownCell, toMarkdown, toMarkdownRange } from "../src/lib/export.ts";
import { reorderRows } from "../src/lib/resultSort.ts";

// Only the fields the converters read
const result = (columns: string[], rows: unknown[][]) =>
  ({ columns, rows }) as unknown as Parameters<typeof toMarkdown>[0];

test("a result becomes a GFM table with a header and a separator row", () => {
  const text = toMarkdown(result(["id", "name"], [[1, "Alice"], [2, "Bob"]]));
  assert.equal(
    text,
    ["| id | name |", "| ---: | --- |", "| 1 | Alice |", "| 2 | Bob |"].join("\n"),
  );
});

test("pipes, backslashes and line breaks cannot break the table", () => {
  assert.equal(escapeMarkdownCell("a|b"), "a\\|b");
  assert.equal(escapeMarkdownCell("C:\\dir"), "C:\\\\dir");
  assert.equal(escapeMarkdownCell("x\\|y"), "x\\\\\\|y");
  assert.equal(escapeMarkdownCell("l1\nl2\r\nl3\rl4"), "l1<br>l2<br>l3<br>l4");
  const text = toMarkdown(result(["v"], [["a|b\nc"]]));
  assert.equal(text.split("\n").length, 3);
  assert.equal(text.split("\n")[2], "| a\\|b<br>c |");
});

test("HTML in a cell is neutralized and the only tag left is our own <br>", () => {
  assert.equal(
    escapeMarkdownCell("<script>alert(1)</script>"),
    "&lt;script&gt;alert(1)&lt;/script&gt;",
  );
  // A <br> typed in the data is escaped; the one made from a line break is not
  assert.equal(escapeMarkdownCell("a<br>b\nc"), "a&lt;br&gt;b<br>c");
  // An entity in the data stays the text it was, not the character it names
  assert.equal(escapeMarkdownCell("&lt; & co"), "&amp;lt; &amp; co");
});

test("inline Markdown in a cell is shown as text, not rendered", () => {
  assert.equal(escapeMarkdownCell("**bold** and *it*"), "\\*\\*bold\\*\\* and \\*it\\*");
  assert.equal(escapeMarkdownCell("[label](https://example.com)"), "[label\\](https://example.com)");
  assert.equal(escapeMarkdownCell("![img](https://example.com/x.png)"), "![img\\](https://example.com/x.png)");
  // A backtick must not turn the entities / <br> into literal code-span text
  assert.equal(escapeMarkdownCell("`<b>`\nx"), "\\`&lt;b&gt;\\`<br>x");
  assert.equal(escapeMarkdownCell("[a][b] [1,[2]] x]"), "[a\\][b] [1,[2]] x]");
  assert.equal(escapeMarkdownCell("~~gone~~"), "\\~\\~gone\\~\\~");
});

test("underscores are escaped only where they could start emphasis", () => {
  assert.equal(escapeMarkdownCell("user_id"), "user_id");
  assert.equal(escapeMarkdownCell("a_1_b"), "a_1_b");
  assert.equal(escapeMarkdownCell("_private_"), "\\_private\\_");
  assert.equal(escapeMarkdownCell("__init__ x_"), "\\_\\_init\\_\\_ x\\_");
  assert.equal(escapeMarkdownCell("日本_語"), "日本\\_語");
});

test("NULL is written as NULL and JSON cells as one-line JSON", () => {
  const text = toMarkdown(
    result(["a", "b"], [[null, { k: [1, "x|y"], n: null }], [undefined, ["p\nq"]]]),
  );
  const lines = text.split("\n");
  assert.equal(lines[2], '| NULL | {"k":[1,"x\\|y"],"n":null} |');
  // The line break inside the JSON string is already \n in the serialization; its backslash
  // is escaped
  assert.equal(lines[3], '| NULL | ["p\\\\nq"] |');
});

test("the formula-injection guard of CSV / TSV is not applied", () => {
  const text = toMarkdown(result(["=cmd"], [["=1+1"], ["-x"], ["@y"]]));
  assert.deepEqual(text.split("\n"), ["| =cmd |", "| --- |", "| =1+1 |", "| -x |", "| @y |"]);
});

test("numeric columns are right-aligned, NULLs do not change that, mixed columns are not", () => {
  const text = toMarkdown(
    result(
      ["int", "decimal_string", "mixed", "all_null", "bool", "empty_string"],
      [
        [1, "9007199254740993", 1, null, true, ""],
        [null, "-1.50", "x", null, false, ""],
        [3.5, "1e3", 2, null, true, ""],
      ],
    ),
  );
  assert.equal(text.split("\n")[1], "| ---: | ---: | --- | --- | --- | --- |");
});

test("headers are escaped too, and an empty result still has its header", () => {
  const text = toMarkdown(result(["a|b", "<i>"], []));
  assert.equal(text, ["| a\\|b | &lt;i&gt; |", "| --- | --- |"].join("\n"));
});

test("a range gives only the selected cells, always with the header row", () => {
  const source = result(
    ["id", "name", "note"],
    [[1, "Alice", "x"], [2, "Bob", "y"], [3, "Carol", "z"]],
  );
  const range = { rowStart: 1, rowEnd: 2, colStart: 1, colEnd: 2 };
  const expected = ["| name | note |", "| --- | --- |", "| Bob | y |", "| Carol | z |"].join("\n");
  assert.equal(toMarkdownRange(source, range, true), expected);
  assert.equal(toMarkdownRange(source, range, false), expected);
  // Alignment is decided from the selected rows only
  const numbers = result(["v"], [["text"], [1], [2]]);
  assert.equal(
    toMarkdownRange(numbers, { rowStart: 1, rowEnd: 2, colStart: 0, colEnd: 0 }, true),
    ["| v |", "| ---: |", "| 1 |", "| 2 |"].join("\n"),
  );
});

test("the table follows the sorted order of the rows on screen", () => {
  const source = result(["id", "name"], [[2, "Bob"], [3, "Carol"], [1, "Alice"]]);
  const sorted = reorderRows(source, { colIndex: 0, direction: "desc" });
  assert.deepEqual(toMarkdown(sorted).split("\n").slice(2), [
    "| 3 | Carol |",
    "| 2 | Bob |",
    "| 1 | Alice |",
  ]);
});

test("a huge digit-only cell does not make alignment detection slow", () => {
  const big = "9".repeat(40_000) + "x";
  const started = performance.now();
  toMarkdown(result(["v"], Array.from({ length: 200 }, () => [big])));
  assert.ok(performance.now() - started < 1000);
});
