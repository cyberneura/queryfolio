import { test } from "node:test";
import assert from "node:assert/strict";
import { mysqlKillMayDamageTable, targetChangedReason } from "../src/lib/processSignal.ts";
import type { ProcessInfo } from "../src/lib/api.ts";

function proc(over: Partial<ProcessInfo> & { extra?: Record<string, string | null> } = {}): ProcessInfo {
  const { extra = {}, ...rest } = over;
  return {
    id: "42",
    user: "app",
    host: "10.0.0.1:5000",
    db: "main",
    command: "active",
    time_secs: 10,
    state: null,
    info: "SELECT pg_sleep(30)",
    idle: false,
    fields: Object.entries(extra).map(([name, value]) => ({ name, value })),
    ...rest,
  };
}

test("same session and statement passes", () => {
  const a = proc({ extra: { backend_start: "t0", query_start: "q1" } });
  const b = proc({ time_secs: 12, extra: { backend_start: "t0", query_start: "q1" } });
  assert.equal(targetChangedReason(a, b, "cancel_query"), null);
  assert.equal(targetChangedReason(a, b, "kill_connection"), null);
});

test("an ended session is reported", () => {
  assert.match(targetChangedReason(proc(), undefined, "kill_connection") ?? "", /no longer listed/);
});

test("a reused PostgreSQL pid (other backend_start) is a different session", () => {
  const a = proc({ extra: { backend_start: "t0" } });
  const b = proc({ extra: { backend_start: "t1" } });
  assert.match(targetChangedReason(a, b, "kill_connection") ?? "", /different session/);
});

test("cancel refuses when the statement changed", () => {
  const a = proc({ extra: { query_start: "q1" } });
  // Other text
  assert.ok(targetChangedReason(a, proc({ info: "UPDATE t SET x = 1", extra: { query_start: "q1" } }), "cancel_query"));
  // Same text run again (PostgreSQL query_start)
  assert.ok(targetChangedReason(a, proc({ extra: { query_start: "q2" } }), "cancel_query"));
  // MySQL: same text, Time went back to 0
  assert.ok(targetChangedReason(proc({ time_secs: 20 }), proc({ time_secs: 1 }), "cancel_query"));
  // Kill does not care about the statement
  assert.equal(targetChangedReason(a, proc({ info: "SELECT 1", extra: { query_start: "q9" } }), "kill_connection"), null);
});

test("mysqlKillMayDamageTable", () => {
  assert.equal(mysqlKillMayDamageTable("REPAIR TABLE t"), true);
  assert.equal(mysqlKillMayDamageTable("  /* x */ optimize table t"), true);
  assert.equal(mysqlKillMayDamageTable("-- c\nALTER TABLE t ADD c int"), true);
  assert.equal(mysqlKillMayDamageTable("SELECT 'repair table'"), false);
  assert.equal(mysqlKillMayDamageTable(null), false);
});
