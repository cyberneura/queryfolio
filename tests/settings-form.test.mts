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
