import { test } from "node:test";
import assert from "node:assert/strict";
import {
  buildSearchItems,
  matchConnections,
  matchLoadedTables,
  type LoadedTables,
} from "../src/lib/searchItems.ts";
import type { ConnectionInfo, FileSearchHit, TableInfo } from "../src/lib/api.ts";

const connection = (name: string, description: string | null = null): ConnectionInfo =>
  ({ name, description }) as unknown as ConnectionInfo;

const table = (qualified_name: string, schema: string | null = null): TableInfo => ({
  name: qualified_name.split(".").pop() ?? qualified_name,
  schema,
  kind: "table",
  qualified_name,
});

const loaded: LoadedTables = {
  connection: "prod",
  database: "shop",
  tables: [table("users"), table("user_logins"), table("orders"), table("sales.order_items", "sales")],
};

test("candidates are ordered connections, then files, then tables", () => {
  const hits = [{ file_name: "a.sql", content_preview: null }] as unknown as FileSearchHit[];
  const items = buildSearchItems([connection("c1"), connection("c2")], hits, [table("t")]);
  assert.deepEqual(
    items.map((i) => i.kind),
    ["connection", "connection", "file", "table"],
  );
  assert.equal(items[0].kind === "connection" && items[0].name, "c1");
  assert.equal(items[3].kind === "table" && items[3].table.qualified_name, "t");
});

test("connections match on name and description, and an empty query lists all of them", () => {
  const list = [connection("torico-blog", "MySQL production"), connection("local")];
  assert.deepEqual(matchConnections(list, "  ").map((c) => c.name), ["torico-blog", "local"]);
  assert.deepEqual(matchConnections(list, "PROD").map((c) => c.name), ["torico-blog"]);
  assert.deepEqual(matchConnections(list, "loc").map((c) => c.name), ["local"]);
});

test("tables come only from the loaded list of the selected connection and database", () => {
  const names = (tables: TableInfo[]) => tables.map((t) => t.qualified_name);
  assert.deepEqual(names(matchLoadedTables(loaded, "prod", "shop", "user")), [
    "users",
    "user_logins",
  ]);
  // Not loaded yet
  assert.deepEqual(matchLoadedTables(null, "prod", "shop", "user"), []);
  // Loaded for another connection
  assert.deepEqual(matchLoadedTables(loaded, "staging", "shop", "user"), []);
  // Loaded for another database of the same connection
  assert.deepEqual(matchLoadedTables(loaded, "prod", "other", "user"), []);
  // No connection selected
  assert.deepEqual(matchLoadedTables(loaded, null, "shop", "user"), []);
});

test("a connection without a database matches a list loaded with database null", () => {
  const noDb: LoadedTables = { ...loaded, database: null };
  assert.equal(matchLoadedTables(noDb, "prod", null, "orders").length, 1);
  assert.equal(matchLoadedTables(noDb, "prod", "", "orders").length, 1);
  assert.equal(matchLoadedTables(loaded, "prod", null, "orders").length, 0);
});

test("tables need a query; terms are whitespace-separated and match schema / name / qualified name", () => {
  assert.deepEqual(matchLoadedTables(loaded, "prod", "shop", ""), []);
  assert.deepEqual(matchLoadedTables(loaded, "prod", "shop", "   "), []);
  assert.deepEqual(
    matchLoadedTables(loaded, "prod", "shop", "sales item").map((t) => t.qualified_name),
    ["sales.order_items"],
  );
  assert.deepEqual(
    matchLoadedTables(loaded, "prod", "shop", "ORDER").map((t) => t.qualified_name),
    ["orders", "sales.order_items"],
  );
  assert.deepEqual(matchLoadedTables(loaded, "prod", "shop", "order nothing"), []);
});
