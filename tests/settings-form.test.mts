import { test } from "node:test";
import assert from "node:assert/strict";
import {
  buildPatch,
  initialValues,
  isEmptyPatch,
  isOverridden,
  validate,
  validateBaseUrl,
  validateCommand,
  validateDefaultLimit,
} from "../src/lib/settingsForm.ts";
import type { LocalSettings } from "../src/lib/api.ts";

const local = (over: Partial<LocalSettings> = {}): LocalSettings => ({
  default_limit: null,
  sqlfiles_dir: null,
  config_override_command: null,
  ai: {
    present: false,
    provider: null,
    model: null,
    base_url: null,
    tool_reasoning_effort: null,
    api_key_set: false,
  },
  agent: { provider: null, directory: null, auto_accept_edits: null, directories: 0 },
  invalid: [],
  ...over,
});

test("an untouched form saves nothing", () => {
  const initial = initialValues(local({ default_limit: 200, sqlfiles_dir: "~/q" }));
  assert.ok(isEmptyPatch(buildPatch({ ...initial }, initial, [])));
});

test("changed fields become set, emptied fields become unset", () => {
  const initial = initialValues(local({ default_limit: 200, sqlfiles_dir: "~/q" }));
  const patch = buildPatch(
    { ...initial, defaultLimit: "0", sqlfilesDir: "  ", aiModel: " gpt-x " },
    initial,
    [],
  );
  assert.deepEqual(patch, {
    default_limit: { op: "set", value: 0 },
    sqlfiles_dir: { op: "unset" },
    ai_model: { op: "set", value: "gpt-x" },
  });
});

test("overridden fields never reach the patch", () => {
  const initial = initialValues(local());
  const patch = buildPatch(
    { ...initial, apiKeyMode: "direct", apiKeyInput: "sk-1", aiModel: "m", defaultLimit: "9" },
    initial,
    ["ai", "default_limit"],
  );
  assert.deepEqual(patch, {});
});

test("the API key is only sent when typed or removed", () => {
  const initial = initialValues(local({ ai: { ...local().ai, present: true, api_key_set: true } }));
  assert.equal(initial.apiKeyMode, "direct");
  assert.ok(isEmptyPatch(buildPatch({ ...initial }, initial, [])));
  assert.deepEqual(buildPatch({ ...initial, apiKeyInput: " sk-2 " }, initial, []), {
    ai_api_key: { op: "set", value: "sk-2" },
  });
  // Switching to 1Password does not drop the stored key unless asked to
  assert.ok(isEmptyPatch(buildPatch({ ...initial, apiKeyMode: "op", apiKeyInput: "x" }, initial, [])));
  assert.deepEqual(
    buildPatch({ ...initial, apiKeyMode: "op", removeStoredApiKey: true }, initial, []),
    { ai_api_key: { op: "unset" } },
  );
});

test("reasoning effort distinguishes absent from never send", () => {
  const initial = initialValues(local());
  assert.equal(initial.aiReasoningEffort, "default");
  assert.deepEqual(buildPatch({ ...initial, aiReasoningEffort: "" }, initial, []), {
    ai_tool_reasoning_effort: { op: "set", value: "" },
  });
  const set = initialValues(local({ ai: { ...local().ai, tool_reasoning_effort: "" } }));
  assert.deepEqual(buildPatch({ ...set, aiReasoningEffort: "default" }, set, []), {
    ai_tool_reasoning_effort: { op: "unset" },
  });
});

test("validation matches the backend rules", () => {
  assert.equal(validateDefaultLimit(""), null);
  assert.equal(validateDefaultLimit("0"), null);
  assert.ok(validateDefaultLimit("-1"));
  assert.ok(validateDefaultLimit("1.5"));
  assert.ok(validateDefaultLimit("1000001"));
  assert.equal(validateCommand('op read "op://a/b/c"'), null);
  assert.ok(validateCommand('op read "op://a/b/c'));
  assert.equal(validateCommand("cat it\\'s"), null);
  assert.equal(validateBaseUrl("https://example.com/v1"), null);
  assert.ok(validateBaseUrl("example.com"));
  assert.ok(validateBaseUrl("https://"));
  const base = initialValues(local());
  const errors = validate({ ...base, defaultLimit: "x", aiBaseUrl: "ftp://x" }, base, ["ai.base_url"]);
  assert.deepEqual(Object.keys(errors), ["default_limit"]);
});

test("a value already in the file does not block saving another field", () => {
  const initial = initialValues(local({ default_limit: 2_000_000 }));
  assert.deepEqual(validate({ ...initial, sqlfilesDir: "~/q" }, initial, []), {});
  assert.ok(validate({ ...initial, defaultLimit: "3000000" }, initial, [])["default_limit"]);
});

test("override paths cover their children only", () => {
  assert.ok(isOverridden(["ai"], "ai.api_key"));
  assert.ok(isOverridden(["ai.api_key"], "ai.api_key"));
  assert.ok(!isOverridden(["ai.api_key"], "ai.model"));
  assert.ok(!isOverridden(["ai"], "aim"));
});

test("agent settings: only what changed is written, and true is the default", async () => {
  const { normalizeAgentProvider, validateAgentDirectory } = await import("../src/lib/settingsForm.ts");
  const initial = initialValues(local());
  assert.equal(initial.agentProvider, "");
  assert.equal(initial.agentAutoAcceptEdits, true);
  assert.deepEqual(buildPatch({ ...initial }, initial, []), {});
  assert.deepEqual(
    buildPatch(
      { ...initial, agentProvider: "claude", agentDirectory: " ~/repos/app ", agentAutoAcceptEdits: false },
      initial,
      [],
    ),
    {
      agent_provider: { op: "set", value: "claude" },
      agent_directory: { op: "set", value: "~/repos/app" },
      agent_auto_accept_edits: { op: "set", value: false },
    },
  );
  // Back to the defaults: the keys are removed, not written
  const stored = initialValues(
    local({ agent: { provider: "Claude-Code", directory: "/srv/app", auto_accept_edits: false, directories: 1 } }),
  );
  assert.equal(stored.agentProvider, "claude");
  assert.deepEqual(
    buildPatch({ ...stored, agentProvider: "", agentDirectory: "", agentAutoAcceptEdits: true }, stored, []),
    {
      agent_provider: { op: "unset" },
      agent_directory: { op: "unset" },
      agent_auto_accept_edits: { op: "unset" },
    },
  );
  // An overridden section is never part of the patch
  assert.deepEqual(buildPatch({ ...initial, agentProvider: "codex" }, initial, ["agent"]), {});
  assert.equal(normalizeAgentProvider(null), "");
  assert.equal(normalizeAgentProvider("none"), "");
  assert.equal(normalizeAgentProvider("gemini"), "gemini");
  assert.equal(validateAgentDirectory(""), null);
  assert.equal(validateAgentDirectory("~/repos/app"), null);
  assert.equal(validateAgentDirectory("C:\\repos\\app"), null);
  assert.notEqual(validateAgentDirectory("repos/app"), null);
  assert.deepEqual(validate({ ...initial, agentDirectory: "repos/app" }, initial, []), {
    "agent.directory": "Enter an absolute path (it may start with ~/)",
  });
});
