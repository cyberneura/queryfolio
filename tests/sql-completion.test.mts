import { test } from "node:test";
import assert from "node:assert/strict";
import {
  buildCompletionModel,
  completeSql,
  quoteIfNeeded,
  referencedTables,
  splitQualifiedKey,
} from "../src/lib/editor/sqlCompletion.ts";
import type { SqlCompletionDialect } from "../src/lib/editor/sqlCompletion.ts";

const MAP = {
  users: ["id", "name", "email", "created_at"],
  orders: ["id", "user_id", "status", "total"],
  "sales.orders": ["id", "region"],
  "sales.Invoices": ["id", "Amount"],
};

/// Complete at the `|` marker of `sql`
const complete = (
  sql: string,
  options: { dialect?: SqlCompletionDialect; explicit?: boolean; map?: Record<string, string[]> } = {},
) => {
  const pos = sql.indexOf("|");
  assert.notEqual(pos, -1, "the SQL needs a | marker");
  const doc = sql.slice(0, pos) + sql.slice(pos + 1);
  const model = buildCompletionModel(options.map ?? MAP, options.dialect ?? "postgres");
  const result = completeSql(model, doc, pos, options.explicit ?? false);
  return result && { ...result, typed: doc.slice(result.from, pos) };
};

const labels = (result: ReturnType<typeof complete>, type?: string) =>
  (result?.options ?? []).filter((o) => !type || o.type === type).map((o) => o.label);

test("schema map keys are split into schema and table", () => {
  assert.deepEqual(splitQualifiedKey("users", "mysql"), ["users"]);
  assert.deepEqual(splitQualifiedKey("sales.orders", "postgres"), ["sales", "orders"]);
  // Only SQL Server keys are bracketed; elsewhere quote characters are part of the name
  assert.deepEqual(splitQualifiedKey("we`ird", "mysql"), ["we`ird"]);
  assert.deepEqual(splitQualifiedKey("[dbo].[my.table]", "mssql"), ["dbo", "my.table"]);
  assert.deepEqual(splitQualifiedKey("[dbo].[a]]b]", "mssql"), ["dbo", "a]b"]);
});

test("columns of the referenced tables are offered without a qualifier", () => {
  const result = complete("SELECT na| FROM users");
  assert.equal(result?.typed, "na");
  assert.deepEqual(labels(result, "property"), ["id", "name", "email", "created_at"]);
  // Columns rank above tables outside a table position
  const name = result!.options.find((o) => o.label === "name")!;
  const table = result!.options.find((o) => o.label === "orders")!;
  assert.ok((name.boost ?? 0) > (table.boost ?? 0));
  // The table is written after the cursor: the whole statement is read, not only what precedes
  assert.deepEqual(labels(complete("SELECT * FROM users WHERE em|"), "property").includes("email"), true);
});

test("columns come from every table of a join, and a shared name lists its tables", () => {
  const result = complete("SELECT s| FROM users u JOIN orders o ON o.user_id = u.id");
  assert.deepEqual(labels(result, "property"), [
    "id", "name", "email", "created_at", "user_id", "status", "total",
  ]);
  assert.equal(result!.options.find((o) => o.label === "id")!.detail, "users, orders");
  assert.deepEqual(labels(result, "constant"), ["u", "o"]);
});

test("UPDATE, INSERT INTO and DELETE FROM name the table too", () => {
  assert.ok(labels(complete("UPDATE users SET na|"), "property").includes("name"));
  assert.ok(labels(complete("INSERT INTO orders (st|"), "property").includes("status"));
  assert.ok(labels(complete("DELETE FROM orders WHERE to|"), "property").includes("total"));
  assert.deepEqual(labels(complete("UPDATE users u SET u.|")), ["id", "name", "email", "created_at"]);
});

test("a qualifier resolves aliases, tables and schemas case-insensitively", () => {
  assert.deepEqual(labels(complete("SELECT u.| FROM Users AS u")), ["id", "name", "email", "created_at"]);
  assert.deepEqual(labels(complete("SELECT U.em| FROM users u")), ["id", "name", "email", "created_at"]);
  assert.deepEqual(labels(complete("SELECT USERS.| FROM users")), ["id", "name", "email", "created_at"]);
  // A schema lists its tables; schema.table lists the columns
  assert.deepEqual(labels(complete("SELECT * FROM sales.|")), ["orders", "Invoices"]);
  assert.deepEqual(labels(complete("SELECT sales.orders.| FROM sales.orders")), ["id", "region"]);
  // An alias of a qualified table
  assert.deepEqual(labels(complete("SELECT so.| FROM sales.orders so")), ["id", "region"]);
  // A bare table name used as the qualifier prefers the table the statement references
  assert.deepEqual(labels(complete("SELECT orders.| FROM sales.orders")), ["id", "region"]);
  // Unknown qualifier: nothing (the keyword source still answers)
  assert.equal(complete("SELECT nope.| FROM users"), null);
});

test("an alias hides a table of the same name", () => {
  assert.deepEqual(labels(complete("SELECT orders.| FROM users orders")), [
    "id", "name", "email", "created_at",
  ]);
});

test("tables rank first right after FROM / JOIN / INTO / UPDATE", () => {
  for (const sql of ["SELECT * FROM us|", "SELECT * FROM users JOIN or|", "INSERT INTO us|", "UPDATE us|"]) {
    const result = complete(sql);
    assert.deepEqual(labels(result, "property"), [], sql);
    assert.ok(labels(result, "type").includes("users"), sql);
    assert.equal(result!.options.find((o) => o.label === "users")!.boost, 20, sql);
  }
  // A table in another schema is inserted qualified, a schema is offered as a namespace
  const result = complete("SELECT * FROM inv|");
  assert.equal(result!.options.find((o) => o.label === "Invoices")!.apply, 'sales."Invoices"');
  assert.deepEqual(labels(result, "namespace"), ["sales"]);
});

test("comma-separated FROM lists and subqueries are read", () => {
  assert.deepEqual(
    referencedTables("SELECT * FROM users u, orders AS o WHERE 1", 0),
    [{ path: ["users"], alias: "u" }, { path: ["orders"], alias: "o" }],
  );
  assert.deepEqual(
    referencedTables("SELECT * FROM (SELECT * FROM sales.orders WHERE x) t JOIN users ON 1", 0),
    [{ path: ["sales", "orders"], alias: null }, { path: ["users"], alias: null }],
  );
  // Keywords after a table are not aliases
  assert.deepEqual(referencedTables("SELECT * FROM users WHERE id = 1", 0), [
    { path: ["users"], alias: null },
  ]);
  assert.deepEqual(referencedTables("SELECT * FROM users LEFT JOIN orders USING (id)", 0), [
    { path: ["users"], alias: null },
    { path: ["orders"], alias: null },
  ]);
});

test("only the statement around the cursor is read", () => {
  const result = complete("SELECT * FROM orders;\nSELECT na| FROM users;\nSELECT * FROM sales.orders");
  assert.deepEqual(labels(result, "property"), ["id", "name", "email", "created_at"]);
  assert.deepEqual(referencedTables("SELECT 1 FROM a; SELECT 2 FROM b", 20), [{ path: ["b"], alias: null }]);
});

test("nothing is offered inside strings, comments and numbers, or on an empty word", () => {
  assert.equal(complete("SELECT * FROM users WHERE name = 'na|'"), null);
  assert.equal(complete("SELECT * FROM users WHERE name = 'na|"), null);
  assert.equal(complete("SELECT * FROM users -- na|"), null);
  assert.equal(complete("SELECT * FROM users -- na|\nWHERE 1"), null);
  assert.equal(complete("SELECT * /* na| */ FROM users"), null);
  assert.equal(complete("SELECT * FROM users LIMIT 10|"), null);
  assert.equal(complete("SELECT | FROM users"), null);
  // An explicit request (Ctrl+Space) on an empty word lists everything
  assert.ok(labels(complete("SELECT | FROM users", { explicit: true }), "property").includes("email"));
  // Words in strings and comments are not table references
  assert.deepEqual(referencedTables("SELECT 'from orders' /* from orders */ FROM users -- from orders", 0), [
    { path: ["users"], alias: null },
  ]);
});

test("the text after a closed string or comment completes again", () => {
  assert.ok(labels(complete("SELECT * FROM users WHERE name = 'x' AND em|"), "property").includes("email"));
  assert.ok(labels(complete("SELECT /* c */na| FROM users"), "property").includes("name"));
});

test("names that need quoting are quoted for the dialect", () => {
  assert.equal(quoteIfNeeded("name", "postgres"), "name");
  assert.equal(quoteIfNeeded("Amount", "postgres"), '"Amount"');
  assert.equal(quoteIfNeeded("Amount", "mysql"), "Amount");
  assert.equal(quoteIfNeeded("order id", "mysql"), "`order id`");
  assert.equal(quoteIfNeeded("order id", "mssql"), "[order id]");
  assert.equal(quoteIfNeeded("日本語", "sqlite"), '"日本語"');
  const result = complete("SELECT am| FROM sales.Invoices");
  assert.equal(result!.options.find((o) => o.label === "Amount")!.apply, '"Amount"');
});

test("inside an identifier quote the content is replaced and nothing is re-quoted", () => {
  const closed = complete('SELECT "Am|" FROM sales."Invoices"');
  assert.equal(closed?.typed, "Am");
  assert.equal(closed!.options.find((o) => o.label === "Amount")!.apply, undefined);
  // The quote is still open: the rest of the statement is read as SQL, not as the identifier
  const open = complete('SELECT "Am| FROM sales."Invoices"');
  assert.equal(open?.typed, "Am");
  assert.ok(labels(open, "property").includes("Amount"));
  const mysql = complete("SELECT `na| FROM users", { dialect: "mysql" });
  assert.ok(labels(mysql, "property").includes("name"));
});

test("SQL Server bracketed keys complete by the bare table name", () => {
  const map = { "[dbo].[users]": ["id", "name"], "[sales].[order items]": ["id", "qty"] };
  const tables = complete("SELECT * FROM us|", { dialect: "mssql", map });
  assert.equal(tables!.options.find((o) => o.label === "users")!.apply, "[dbo].[users]");
  assert.deepEqual(labels(complete("SELECT na| FROM [dbo].[users]", { dialect: "mssql", map }), "property"), [
    "id", "name",
  ]);
  assert.deepEqual(labels(complete("SELECT na| FROM users", { dialect: "mssql", map }), "property"), [
    "id", "name",
  ]);
  assert.deepEqual(labels(complete("SELECT i.| FROM sales.[order items] i", { dialect: "mssql", map })), [
    "id", "qty",
  ]);
  assert.deepEqual(labels(complete("SELECT * FROM dbo.|", { dialect: "mssql", map })), ["users"]);
});

test("MySQL # comments and unicode identifiers", () => {
  assert.equal(complete("SELECT * FROM users # na|", { dialect: "mysql" }), null);
  const map = { 顧客: ["番号", "名前"] };
  const result = complete("SELECT 名| FROM 顧客", { dialect: "mysql", map });
  assert.equal(result?.typed, "名");
  assert.deepEqual(labels(result, "property"), ["番号", "名前"]);
});

test("no schema map, no completion", () => {
  assert.equal(completeSql(null, "SELECT na FROM users", 9, true), null);
  assert.equal(completeSql(buildCompletionModel({}, "mysql"), "SELECT na FROM users", 9, true), null);
});

test("statements that change the schema are recognized", async () => {
  const { changesSchema, sameSchemaMap } = await import("../src/lib/editor/sqlCompletion.ts");
  assert.equal(changesSchema("CREATE TABLE t (id int)"), true);
  assert.equal(changesSchema("-- add a column\n/* x */ alter table t add c int"), true);
  assert.equal(changesSchema("DROP VIEW v"), true);
  assert.equal(changesSchema("RENAME TABLE a TO b"), true);
  assert.equal(changesSchema("SELECT 'create table'"), false);
  assert.equal(changesSchema("INSERT INTO t VALUES (1)"), false);
  assert.equal(changesSchema("created_at"), false);
  assert.equal(sameSchemaMap({ a: ["x", "y"] }, { a: ["x", "y"] }), true);
  assert.equal(sameSchemaMap({ a: ["x", "y"] }, { a: ["y", "x"] }), false);
  assert.equal(sameSchemaMap({ a: ["x"] }, { a: ["x"], b: [] }), false);
  assert.equal(sameSchemaMap(null, {}), false);
  assert.equal(sameSchemaMap(null, null), true);
});

test("a qualifier that is not a schema of the map means the default schema", () => {
  // PostgreSQL leaves `public` out of the keys, DuckDB `main`; MySQL can qualify with the database
  assert.ok(labels(complete("SELECT na| FROM public.users"), "property").includes("name"));
  assert.deepEqual(labels(complete("SELECT public.users.| FROM public.users")), [
    "id", "name", "email", "created_at",
  ]);
  assert.ok(labels(complete("SELECT na| FROM mydb.users", { dialect: "mysql" }), "property").includes("name"));
  // A known schema without that table resolves to nothing rather than to the default schema
  assert.deepEqual(labels(complete("SELECT na| FROM sales.users"), "property"), []);
});

test("names that differ only in case or in where the dot falls stay distinct", () => {
  const map = { 'sales.Users': ["Upper"], "sales.users": ["lower"] };
  assert.deepEqual(labels(complete('SELECT x| FROM sales."Users"', { map }), "property"), ["Upper"]);
  assert.deepEqual(labels(complete("SELECT x| FROM sales.users", { map }), "property"), ["lower"]);
  // No exact match: every case-insensitive match
  assert.deepEqual(labels(complete("SELECT x| FROM sales.USERS", { map }), "property"), ["Upper", "lower"]);
  const dotted = { "[a.b].[c]": ["one"], "[a].[b.c]": ["two"] };
  assert.deepEqual(
    labels(complete("SELECT x| FROM [a.b].[c]", { map: dotted, dialect: "mssql" }), "property"),
    ["one"],
  );
  assert.deepEqual(
    labels(complete("SELECT x| FROM [a].[b.c]", { map: dotted, dialect: "mssql" }), "property"),
    ["two"],
  );
});

test("comments between the parts of a table reference are skipped", () => {
  assert.deepEqual(referencedTables("SELECT * FROM /* c */ users /* c */ AS u -- x\nWHERE 1", 0), [
    { path: ["users"], alias: "u" },
  ]);
  assert.ok(labels(complete("SELECT na| FROM /* the table */ users"), "property").includes("name"));
});

test("PostgreSQL dollar-quoted bodies are literals", () => {
  const sql = "SELECT na| FROM users WHERE x = $fn$ ; SELECT 1 FROM orders; $fn$";
  assert.deepEqual(labels(complete(sql), "property"), ["id", "name", "email", "created_at"]);
  assert.equal(complete("SELECT * FROM users WHERE x = $$ na| $$"), null);
  // Other dialects have no dollar quoting: `$` is part of a name
  assert.ok(labels(complete("SELECT na| FROM users WHERE a$$b = 1", { dialect: "mysql" }), "property").includes("name"));
});

test("reserved words and quote characters are escaped in the inserted text", () => {
  const map = { t: ["order", 'a"b', "plain"], "we`ird": ["x"] };
  const pg = complete("SELECT o| FROM t", { map });
  assert.equal(pg!.options.find((o) => o.label === "order")!.apply, '"order"');
  assert.equal(pg!.options.find((o) => o.label === 'a"b')!.apply, '"a""b"');
  assert.equal(pg!.options.find((o) => o.label === "plain")!.apply, undefined);
  // Inside quotes only the closing character is doubled
  const quoted = complete('SELECT "a|" FROM t', { map });
  assert.equal(quoted!.options.find((o) => o.label === 'a"b')!.apply, 'a""b');
  assert.equal(quoted!.options.find((o) => o.label === "order")!.apply, undefined);
  const mysql = complete("SELECT * FROM `we|`", { map, dialect: "mysql" });
  assert.equal(mysql!.options.find((o) => o.label === "we`ird")!.apply, "we``ird");
});

test("the caller can pass a window of the document", async () => {
  const { completionWindow, SCAN_WINDOW_CHARS } = await import("../src/lib/editor/sqlCompletion.ts");
  assert.deepEqual(completionWindow(50, 10), { from: 0, to: 50 });
  const length = SCAN_WINDOW_CHARS * 3;
  const pos = SCAN_WINDOW_CHARS * 2;
  assert.deepEqual(completionWindow(length, pos), { from: SCAN_WINDOW_CHARS, to: length });
  // `from` is relative to the text that was passed
  const model = buildCompletionModel(MAP, "postgres");
  const text = ";\nSELECT na FROM users";
  assert.equal(completeSql(model, text, text.indexOf("na") + 2, false)?.from, text.indexOf("na"));
});

test("a MySQL # comment before a DDL does not hide it", async () => {
  const { changesSchema } = await import("../src/lib/editor/sqlCompletion.ts");
  assert.equal(changesSchema("# migration\nALTER TABLE t ADD c int"), true);
});
