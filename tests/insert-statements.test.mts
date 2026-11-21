import { test } from "node:test";
import assert from "node:assert/strict";
import {
  insertLiteral,
  insertTarget,
  toInsertStatements as build,
} from "../src/lib/insertStatements.ts";

const toInsertStatements = (...args: Parameters<typeof build>): string => {
  const built = build(...args);
  assert.ok(built.ok, built.ok ? "" : built.reason);
  return built.text;
};

test("only a single-table SELECT can be written as INSERT statements", () => {
  assert.deepEqual(insertTarget("mysql", "SELECT * FROM users WHERE id = 1", ["id"]), {
    ok: true,
    engine: "mysql",
    table: "users",
  });
  assert.deepEqual(insertTarget("postgres", "select id, name from App.Users", ["id", "name"]), {
    ok: true,
    engine: "postgres",
    // PostgreSQL folds unquoted identifiers to lowercase (same as cell editing)
    table: "app.users",
  });
  for (const sql of [
    "SELECT * FROM a JOIN b ON a.id = b.a_id",
    "SELECT count(*) FROM users",
    "SELECT id AS user_id FROM users",
    "SELECT u.id FROM users u",
    "SELECT status FROM users GROUP BY status",
    "WITH x AS (SELECT 1) SELECT * FROM x",
    "SHOW TABLES",
  ]) {
    const t = insertTarget("mysql", sql, ["id"]);
    assert.equal(t.ok, false, sql);
  }
  const dup = insertTarget("sqlite", "SELECT id, id FROM t", ["id", "id"]);
  assert.equal(dup.ok, false);
});

test("values are converted per dialect", () => {
  assert.equal(insertLiteral("mysql", null), "NULL");
  assert.equal(insertLiteral("postgres", undefined), "NULL");
  assert.equal(insertLiteral("mysql", 42), "42");
  assert.equal(insertLiteral("mysql", -1.5), "-1.5");
  assert.equal(insertLiteral("postgres", true), "TRUE");
  assert.equal(insertLiteral("sqlite", false), "0");
  assert.equal(insertLiteral("mysql", true), "1");
  // Quotes inside strings are doubled; MySQL also doubles the backslash
  assert.equal(insertLiteral("postgres", "O'Reilly \\ x"), "'O''Reilly \\ x'");
  assert.equal(insertLiteral("mysql", "O'Reilly \\ x"), "'O''Reilly \\\\ x'");
  // Dates and big integers arrive as strings
  assert.equal(insertLiteral("postgres", "2026-11-21 10:00:00"), "'2026-11-21 10:00:00'");
  assert.equal(insertLiteral("mysql", "9007199254740993"), "'9007199254740993'");
  // JSON cells are written as their JSON text
  assert.equal(insertLiteral("postgres", { a: [1, "x'y"] }), `'{"a":[1,"x''y"]}'`);
  assert.equal(insertLiteral("mysql", [1, 2]), "'[1,2]'");
});

test("one statement per row with dialect-quoted identifiers", () => {
  const columns = ["id", "name", "note"];
  const rows = [
    [1, "Alice", null],
    [2, "B'ob", "multi\nline"],
  ];
  assert.equal(
    toInsertStatements("mysql", "users", columns, rows),
    "INSERT INTO `users` (`id`, `name`, `note`) VALUES (1, 'Alice', NULL);\n" +
      "INSERT INTO `users` (`id`, `name`, `note`) VALUES (2, 'B''ob', 'multi\nline');",
  );
  assert.equal(
    toInsertStatements("postgres", "app.users", ["id", "Na\"me"], [[1, "x"]]),
    'INSERT INTO "app"."users" ("id", "Na""me") VALUES (1, \'x\');',
  );
  assert.equal(toInsertStatements("sqlite", "t", ["a"], []), "");
});

test("a range writes only the selected rows and columns", () => {
  const columns = ["id", "name", "age"];
  const rows = [
    [1, "a", 10],
    [2, "b", 20],
    [3, "c", 30],
  ];
  assert.equal(
    toInsertStatements("sqlite", "t", columns, rows, {
      rowStart: 1,
      rowEnd: 2,
      colStart: 1,
      colEnd: 2,
    }),
    'INSERT INTO "t" ("name", "age") VALUES (\'b\', 20);\n' +
      'INSERT INTO "t" ("name", "age") VALUES (\'c\', 30);',
  );
});

test("PostgreSQL array cells are refused; other dialects write arrays as JSON", () => {
  const rows = [[1, ["happy", null, "sad"]]];
  const pg = build("postgres", "items", ["id", "moods"], rows);
  assert.equal(pg.ok, false);
  assert.match(pg.ok ? "" : pg.reason, /"moods"/);
  // Leaving the array column out of the selection works
  assert.equal(
    toInsertStatements("postgres", "items", ["id", "moods"], rows, {
      rowStart: 0,
      rowEnd: 0,
      colStart: 0,
      colEnd: 0,
    }),
    'INSERT INTO "items" ("id") VALUES (1);',
  );
  // Objects (JSON) are still fine on PostgreSQL
  assert.equal(
    toInsertStatements("postgres", "items", ["meta"], [[{ a: 1 }]]),
    `INSERT INTO "items" ("meta") VALUES ('{"a":1}');`,
  );
  assert.equal(
    toInsertStatements("mysql", "items", ["tags"], [[["a", "b"]]]),
    "INSERT INTO `items` (`tags`) VALUES ('[\"a\",\"b\"]');",
  );
});
