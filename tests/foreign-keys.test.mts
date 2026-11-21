import { test } from "node:test";
import assert from "node:assert/strict";
import { foreignKeyLookupTable, referencedRowSql } from "../src/lib/foreignKeys.ts";
import { literalFromValue } from "../src/lib/editableResult.ts";

test("foreign keys are looked up only for a single-table SELECT", () => {
  assert.equal(foreignKeyLookupTable("mysql", "SELECT * FROM orders", ["id"]), "orders");
  assert.equal(
    foreignKeyLookupTable("postgres", "SELECT id, customer_id FROM Sales.Orders WHERE id = 1", [
      "id",
      "customer_id",
    ]),
    "sales.orders",
  );
  assert.equal(foreignKeyLookupTable("sqlite", "SELECT * FROM a JOIN b ON a.x = b.y", ["x"]), null);
  assert.equal(foreignKeyLookupTable("sqlite", "SELECT customer_id AS c FROM orders", ["c"]), null);
  assert.equal(foreignKeyLookupTable("sqlite", "SELECT id, id FROM orders", ["id", "id"]), null);
});

test("the referenced row is selected by its column with a dialect literal", () => {
  const fk = { column: "customer_id", ref_schema: "shop", ref_table: "customers", ref_column: "id" };
  assert.equal(
    referencedRowSql("mysql", fk, literalFromValue("mysql", 42)),
    "SELECT * FROM `shop`.`customers` WHERE `id` = 42",
  );
  assert.equal(
    referencedRowSql("postgres", { ...fk, ref_schema: "public", ref_column: "Code" }, literalFromValue("postgres", "A'1")),
    `SELECT * FROM "public"."customers" WHERE "Code" = 'A''1'`,
  );
  // SQLite main database: no schema part
  assert.equal(
    referencedRowSql("sqlite", { ...fk, ref_schema: null }, literalFromValue("sqlite", "x")),
    `SELECT * FROM "customers" WHERE "id" = 'x'`,
  );
  // Identifiers are quoted as stored, including characters that need escaping
  assert.equal(
    referencedRowSql("postgres", { ...fk, ref_schema: null, ref_table: 'we"ird.name' }, "1"),
    `SELECT * FROM "we""ird.name" WHERE "id" = 1`,
  );
});
