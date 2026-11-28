import { test } from "node:test";
import assert from "node:assert/strict";
import {
  definitionFor,
  fillParams,
  findParams,
  invalidParamInput,
  paramEngineOf,
  paramInputSql,
  paramNames,
  parseDefinitions,
  quoteParamString,
} from "../src/lib/queryParams.ts";

const names = (sql: string, engine: Parameters<typeof findParams>[1] = "postgres") =>
  findParams(sql, engine).map((u) => u.name);

test("finds :name placeholders in order", () => {
  assert.deepEqual(names("SELECT * FROM t WHERE id = :id AND d >= :since OR id = :id"), ["id", "since", "id"]);
  assert.deepEqual(paramNames(findParams("SELECT :a, :b, :a", "mysql")), ["a", "b"]);
  assert.deepEqual(names("SELECT (:x)", "sqlite"), ["x"]);
});

test("ignores casts, assignments, slices and quoted / commented text", () => {
  assert.deepEqual(names("SELECT x::int, '2026-01-01'::date"), []);
  assert.deepEqual(names("SELECT @v := 1", "mysql"), []);
  assert.deepEqual(names("SELECT arr[1:2], arr[a:b]"), []);
  assert.deepEqual(names("SELECT ':no', \":no\", `:no` -- :no\n /* :no */ , :yes", "mysql"), ["yes"]);
  assert.deepEqual(names("SELECT 'it''s :no', :yes"), ["yes"]);
  assert.deepEqual(names("SELECT 'a\\' :no', :yes", "mysql"), ["yes"]);
  assert.deepEqual(names("SELECT 1 # :no\n, :yes", "mysql"), ["yes"]);
  assert.deepEqual(names("SELECT $$ :no $$, $f$ :no $f$, :yes"), ["yes"]);
  assert.deepEqual(names("SELECT [:no], :yes", "mssql"), ["yes"]);
  // `#` is not a comment and `[` not a quote outside their dialects
  assert.deepEqual(names("SELECT a #> :p"), ["p"]);
  // Unclosed string swallows the rest
  assert.deepEqual(names("SELECT 'open :no"), []);
});

test("parseDefinitions reads -- @set lines with their offsets", () => {
  const doc = "-- @set id = 42\nSELECT 1;\n  --@set name='O''Brien';\n-- @set empty =\n-- not @set x = 1\n";
  const defs = parseDefinitions(doc, "postgres");
  assert.deepEqual(
    defs.map((d) => [d.name, d.value, d.at]),
    [
      ["id", "42", 0],
      ["name", "'O''Brien'", 26],
    ],
  );
});

test("definitionFor: the last definition above, otherwise the first in the file", () => {
  const doc = "-- @set n = 1\nSELECT :n;\n-- @set n = 2\nSELECT :n;\n";
  const defs = parseDefinitions(doc, "postgres");
  assert.equal(definitionFor(defs, "n", doc.indexOf("SELECT :n;"))?.value, "1");
  assert.equal(definitionFor(defs, "n", doc.lastIndexOf("SELECT"))?.value, "2");
  const below = parseDefinitions("SELECT :m;\n-- @set m = 'x'\n", "postgres");
  assert.equal(definitionFor(below, "m", 0)?.value, "'x'");
  assert.equal(definitionFor(below, "other", 0), null);
});

test("quoting per engine", () => {
  assert.equal(quoteParamString("postgres", "O'Brien"), "'O''Brien'");
  assert.equal(quoteParamString("mssql", "日本"), "N'日本'");
  assert.equal(paramInputSql("sqlite", { value: "42", raw: false }), "'42'");
  assert.equal(paramInputSql("sqlite", { value: "NULL", raw: true }), "NULL");
});

test("fillParams replaces every occurrence, or reports what is missing", () => {
  const sql = "SELECT * FROM t WHERE a = :a AND b = :b AND c = :a::text";
  const uses = findParams(sql, "postgres");
  assert.deepEqual(fillParams(sql, uses, { a: "1" }), { ok: false, missing: ["b"] });
  assert.deepEqual(fillParams(sql, uses, { a: "1", b: "'x'" }), {
    ok: true,
    sql: "SELECT * FROM t WHERE a = 1 AND b = 'x' AND c = 1::text",
  });
});

test("paramEngineOf / invalidParamInput", () => {
  assert.equal(paramEngineOf("MariaDB"), "mysql");
  assert.equal(paramEngineOf("sqlserver"), "mssql");
  assert.equal(paramEngineOf("dynamodb"), null);
  assert.equal(paramEngineOf("redis"), null);
  assert.ok(invalidParamInput({ value: "a\u0000", raw: false }));
  assert.ok(invalidParamInput({ value: " ", raw: true }));
  assert.equal(invalidParamInput({ value: "", raw: false }), null);
});

test("string literals do not depend on the session's string settings", () => {
  // Without a backslash the plain form is read the same in every mode
  assert.equal(quoteParamString("mysql", "O'Brien"), "'O''Brien'");
  // With one: hex for MySQL (NO_BACKSLASH_ESCAPES), E'' for PostgreSQL (standard_conforming_strings)
  assert.equal(quoteParamString("mysql", String.raw`a\b'`), "CONVERT(X'615c6227' USING utf8mb4)");
  assert.equal(quoteParamString("postgres", String.raw`a\b'`), String.raw`E'a\\b'''`);
  assert.equal(quoteParamString("sqlite", String.raw`a\b`), String.raw`'a\b'`);
});

test("dialect rules", () => {
  // PostgreSQL E'' strings, nested comments, slices with an omitted bound
  assert.deepEqual(names(String.raw`SELECT E'it\'s :no', :yes`), ["yes"]);
  assert.deepEqual(names(String.raw`SELECT 'x\', :yes`), ["yes"]); // plain string: backslash is literal
  assert.deepEqual(names("/* a /* :no */ :no */ SELECT :yes"), ["yes"]);
  assert.deepEqual(names("SELECT arr[:n]"), []);
  // SQL Server `]]`, SQLite brackets
  assert.deepEqual(names("SELECT [a]]:no], :yes", "mssql"), ["yes"]);
  assert.deepEqual(names("SELECT [a:no], :yes", "sqlite"), ["yes"]);
  // MySQL: `--` needs whitespace to be a comment
  assert.deepEqual(names("SELECT 1--:n", "mysql"), ["n"]);
  assert.deepEqual(names("SELECT 1 -- :no\n, :yes", "mysql"), ["yes"]);
});

test("-- @set only counts as a real line comment", () => {
  const doc = "SELECT $$\n-- @set n = 9\n$$;\n/*\n-- @set m = 1\n*/\n-- @set k = 2\n";
  assert.deepEqual(parseDefinitions(doc, "postgres").map((d) => d.name), ["k"]);
});

test("fillParams ignores inherited properties", () => {
  const uses = findParams("SELECT :constructor", "postgres");
  assert.deepEqual(fillParams("SELECT :constructor", uses, {}), { ok: false, missing: ["constructor"] });
});
