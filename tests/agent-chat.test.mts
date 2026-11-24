import { test } from "node:test";
import assert from "node:assert/strict";
import {
  chatPrompt,
  MAX_STATEMENT_CHARS,
  oncePrompt,
  schemaSummary,
} from "../src/lib/agentChat/context.ts";
import type { AgentEditorContext } from "../src/lib/agentChat/context.ts";
import { canDispatch, createQueued, mergeUp, takeNext } from "../src/lib/agentChat/queue.ts";

const context: AgentEditorContext = {
  connection: "reporting",
  engine: "postgres",
  database: "app",
  filePath: "/home/me/.config/queryfolio/sqlfiles/reporting/monthly.sql",
  statement: "SELECT * FROM users",
};

test("the chat prompt carries the connection, the open file and the statement", () => {
  const prompt = chatPrompt("Add a WHERE clause", context);
  assert.ok(prompt.startsWith("<queryfolio-context>"));
  assert.ok(prompt.includes("Connection: reporting (postgres), database: app"));
  assert.ok(prompt.includes(`Query file open in the editor: ${context.filePath}`));
  assert.ok(prompt.includes("```sql\nSELECT * FROM users\n```"));
  // The request comes last, after the closing tag
  assert.ok(prompt.endsWith("</queryfolio-context>\n\nAdd a WHERE clause"));
});

test("without a context the request is sent as is", () => {
  assert.equal(chatPrompt("hello", null), "hello");
});

test("missing parts are left out or said plainly", () => {
  const prompt = chatPrompt("hi", { ...context, database: null, filePath: null, statement: "  " });
  assert.ok(prompt.includes("Connection: reporting (postgres)\n"));
  assert.ok(prompt.includes("No query file is open in the editor."));
  assert.ok(!prompt.includes("Statement at the cursor"));
});

test("a statement cannot close its code fence and is clipped", () => {
  const tricky = chatPrompt("x", { ...context, statement: "SELECT '```'; ````" });
  assert.ok(tricky.includes("`````sql\nSELECT '```'; ````\n`````"));
  const long = chatPrompt("x", { ...context, statement: "a".repeat(MAX_STATEMENT_CHARS + 50) });
  assert.ok(long.includes(`${"a".repeat(MAX_STATEMENT_CHARS)}\n… (truncated)`));
  assert.ok(!long.includes("a".repeat(MAX_STATEMENT_CHARS + 1)));
});

test("the schema summary fits its budget and counts what it left out", () => {
  assert.equal(schemaSummary(null), "");
  assert.equal(schemaSummary({}), "");
  const map = { users: ["id", "name"], orders: ["id", "user_id"], big: ["x".repeat(100)] };
  assert.equal(schemaSummary(map), `users(id, name)\norders(id, user_id)\nbig(${"x".repeat(100)})`);
  assert.equal(schemaSummary(map, 40), "users(id, name)\norders(id, user_id)\n… and 1 more tables");
  // The schema goes into the prompt only when given (first message of a conversation)
  assert.ok(!chatPrompt("x", context).includes("Tables (name(columns))"));
  assert.ok(chatPrompt("x", context, schemaSummary(map)).includes("Tables (name(columns)):\nusers(id, name)"));
});

test("the one-shot prompt asks for an edit of the open file and a short reply", () => {
  const prompt = oncePrompt("  sort by created_at  ", context);
  assert.ok(prompt.includes(`Query file open in the editor: ${context.filePath}`));
  assert.ok(prompt.includes("Do not run anything."));
  assert.ok(prompt.endsWith("Instruction:\nsort by created_at"));
  // It never starts with a dash (it is passed as a command-line argument)
  assert.ok(/^[A-Za-z]/.test(prompt));
});

test("queued messages are sent in order, merged upward, and dropped when emptied", () => {
  const queue = [createQueued("one"), createQueued("  "), createQueued("two"), createQueued("three")];
  assert.equal(mergeUp(queue, 3), true);
  assert.equal(mergeUp(queue, 0), false);
  assert.equal(takeNext(queue), "one");
  assert.equal(takeNext(queue), "two\n\nthree");
  assert.equal(takeNext(queue), null);
  const state = { queued: 1, held: false, busy: false, ready: true, running: true, starting: false };
  assert.equal(canDispatch(state), true);
  assert.equal(canDispatch({ ...state, held: true }), false);
  assert.equal(canDispatch({ ...state, busy: true }), false);
});
