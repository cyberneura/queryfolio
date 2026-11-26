// Form logic of the Settings window (CYBERNEURA-DEV-1298): validation and turning the form into
// the save patch. Pure functions so that tests/settings-form.test.mts can call them directly.
// The backend (src-tauri/src/settings.rs) validates again; the checks here only exist to show
// the error next to the field before anything is sent. Keep the two in step.

import type { Change, LocalSettings, SettingsPatch } from "./api";

/// `default_limit` when the key is absent (settings.rs DEFAULT_LIMIT)
export const DEFAULT_LIMIT = 500;
/// Largest `default_limit` accepted (settings.rs MAX_DEFAULT_LIMIT)
export const MAX_DEFAULT_LIMIT = 1_000_000;

/// Example of a 1Password reference for config_override_command. The item holds a whole YAML
/// document (see OVERRIDE_YAML_TEMPLATE), not just the key.
export const OP_COMMAND_EXAMPLE = 'op read "op://<vault>/<item>/<field>"';

/// What to store in that 1Password field: YAML merged over config.yml.
export const OVERRIDE_YAML_TEMPLATE = "ai:\n  api_key: sk-...\n";

/// Choices for ai.tool_reasoning_effort. "default" = key absent (the app decides: `none` for the
/// official OpenAI endpoint), "" = never send it.
export const REASONING_EFFORT_CHOICES: { value: string; label: string }[] = [
  { value: "default", label: "Default (none for the OpenAI API)" },
  { value: "none", label: "none" },
  { value: "minimal", label: "minimal" },
  { value: "low", label: "low" },
  { value: "medium", label: "medium" },
  { value: "high", label: "high" },
  { value: "", label: "Never send" },
];

/// The editable values as the form holds them. Empty text = key absent (default).
export interface FormValues {
  defaultLimit: string;
  sqlfilesDir: string;
  overrideCommand: string;
  aiModel: string;
  aiBaseUrl: string;
  /// "default" = absent (see REASONING_EFFORT_CHOICES)
  aiReasoningEffort: string;
  /// "op" = keep the key in 1Password (recommended), "direct" = store it in config.yml
  apiKeyMode: "op" | "direct";
  /// A newly typed key (direct mode). Empty = keep whatever is stored
  apiKeyInput: string;
  /// Remove the key stored in config.yml on save
  removeStoredApiKey: boolean;
  /// Coding agent for the chat pane and the inline AI field: "" = none (the built-in
  /// assistant), "claude" or "codex" (agent.provider)
  agentProvider: string;
  /// agent.directory. Empty = each connection's query file folder
  agentDirectory: string;
  /// agent.auto_accept_edits (absent = true)
  agentAutoAcceptEdits: boolean;
}

/// Choices for agent.provider ("" = key absent)
export const AGENT_PROVIDER_CHOICES: { value: string; label: string }[] = [
  { value: "", label: "None (built-in assistant, OpenAI API)" },
  { value: "claude", label: "Claude Code" },
  { value: "codex", label: "Codex" },
];

export function initialValues(local: LocalSettings): FormValues {
  return {
    defaultLimit: local.default_limit === null ? "" : String(local.default_limit),
    sqlfilesDir: local.sqlfiles_dir ?? "",
    overrideCommand: local.config_override_command ?? "",
    aiModel: local.ai.model ?? "",
    aiBaseUrl: local.ai.base_url ?? "",
    aiReasoningEffort: local.ai.tool_reasoning_effort ?? "default",
    // Someone who already keeps the key in the file keeps seeing that; everyone else is
    // pointed at 1Password first
    apiKeyMode: local.ai.api_key_set ? "direct" : "op",
    apiKeyInput: "",
    removeStoredApiKey: false,
    agentProvider: normalizeAgentProvider(local.agent.provider),
    agentDirectory: local.agent.directory ?? "",
    agentAutoAcceptEdits: local.agent.auto_accept_edits ?? true,
  };
}

/// The provider as the form's select holds it. The file may spell Claude Code as
/// `claude-code` (agent.rs accepts it); anything unknown is shown as it is, so that saving
/// another field does not silently rewrite it.
export function normalizeAgentProvider(value: string | null): string {
  const provider = (value ?? "").trim().toLowerCase();
  if (provider === "" || provider === "none") return "";
  if (provider === "claude-code" || provider === "claude_code") return "claude";
  return provider;
}

export function validateAgentDirectory(text: string): string | null {
  const value = text.trim();
  if (value === "") return null;
  if (!singleLine(value)) return "The directory must be a single line";
  const absolute =
    value === "~" || value.startsWith("~/") || value.startsWith("/") || /^[A-Za-z]:[\\/]/.test(value);
  return absolute ? null : "Enter an absolute path (it may start with ~/)";
}

/// Whether `field` (dotted path) gets its value from config_override_command
/// (the field itself or a parent is overridden). Same rule as settings.rs is_overridden.
export function isOverridden(overridden: string[], field: string): boolean {
  return overridden.some((key) => key === field || field.startsWith(`${key}.`));
}

const singleLine = (value: string) => !/[\r\n\0]/.test(value);

export function validateDefaultLimit(text: string): string | null {
  const value = text.trim();
  if (value === "") return null;
  if (!/^\d+$/.test(value)) return "Enter a whole number (0 turns the automatic LIMIT off)";
  if (Number(value) > MAX_DEFAULT_LIMIT) {
    return `Use a number up to ${MAX_DEFAULT_LIMIT} (0 turns the automatic LIMIT off)`;
  }
  return null;
}

export function validateSqlfilesDir(text: string): string | null {
  return singleLine(text) ? null : "The folder must be a single line";
}

/// Mirrors shlex closely enough for the form: a quote that is never closed is the one mistake
/// the backend would reject.
export function validateCommand(text: string): string | null {
  const value = text.trim();
  if (value === "") return null;
  if (!singleLine(value)) return "The command must be a single line";
  let quote: string | null = null;
  for (let i = 0; i < value.length; i++) {
    const c = value[i];
    if (quote === null) {
      if (c === "\\") i++;
      else if (c === '"' || c === "'") quote = c;
    } else if (c === quote) {
      quote = null;
    } else if (quote === '"' && c === "\\") {
      i++;
    }
  }
  return quote === null ? null : "The command has an unclosed quote";
}

export function validateModel(text: string): string | null {
  const value = text.trim();
  if (value === "") return null;
  return /\s/.test(value) ? "A model name has no spaces" : null;
}

export function validateBaseUrl(text: string): string | null {
  const value = text.trim();
  if (value === "") return null;
  const rest = value.startsWith("https://")
    ? value.slice(8)
    : value.startsWith("http://")
      ? value.slice(7)
      : null;
  if (rest === null) return "The base URL starts with https:// (or http://)";
  if (/\s/.test(value)) return "The base URL has no spaces";
  if (rest === "" || rest.startsWith("/")) return "The base URL has no host";
  return null;
}

export function validateApiKey(text: string): string | null {
  const value = text.trim();
  if (value === "") return null;
  return /\s/.test(value) ? "The API key has no spaces" : null;
}

/// Errors by field, for the fields the user changed (overridden fields are skipped).
/// A value that was already in the file is not re-judged: the save only writes the fields that
/// changed, so an old out-of-range value must not block saving another field.
export function validate(
  values: FormValues,
  initial: FormValues,
  overridden: string[],
): Record<string, string> {
  const errors: Record<string, string> = {};
  const add = (field: string, changed: boolean, message: string | null) => {
    if (changed && message !== null && !isOverridden(overridden, field)) errors[field] = message;
  };
  const changed = (a: string, b: string) => a.trim() !== b.trim();
  add(
    "default_limit",
    changed(values.defaultLimit, initial.defaultLimit),
    validateDefaultLimit(values.defaultLimit),
  );
  add(
    "sqlfiles_dir",
    changed(values.sqlfilesDir, initial.sqlfilesDir),
    validateSqlfilesDir(values.sqlfilesDir),
  );
  add(
    "config_override_command",
    changed(values.overrideCommand, initial.overrideCommand),
    validateCommand(values.overrideCommand),
  );
  add("ai.model", changed(values.aiModel, initial.aiModel), validateModel(values.aiModel));
  add(
    "ai.base_url",
    changed(values.aiBaseUrl, initial.aiBaseUrl),
    validateBaseUrl(values.aiBaseUrl),
  );
  if (values.apiKeyMode === "direct") {
    add("ai.api_key", values.apiKeyInput.trim() !== "", validateApiKey(values.apiKeyInput));
  }
  add(
    "agent.directory",
    changed(values.agentDirectory, initial.agentDirectory),
    validateAgentDirectory(values.agentDirectory),
  );
  return errors;
}

function textChange(current: string, initial: string): Change<string> | undefined {
  const now = current.trim();
  if (now === initial.trim()) return undefined;
  return now === "" ? { op: "unset" } : { op: "set", value: now };
}

/// The save patch: only the fields that differ from what was loaded. Overridden fields are
/// never included (the backend would reject them anyway).
export function buildPatch(
  values: FormValues,
  initial: FormValues,
  overridden: string[],
): SettingsPatch {
  const patch: SettingsPatch = {};
  const editable = (field: string) => !isOverridden(overridden, field);

  if (editable("default_limit")) {
    const now = values.defaultLimit.trim();
    if (now !== initial.defaultLimit.trim()) {
      patch.default_limit = now === "" ? { op: "unset" } : { op: "set", value: Number(now) };
    }
  }
  if (editable("sqlfiles_dir")) {
    patch.sqlfiles_dir = textChange(values.sqlfilesDir, initial.sqlfilesDir);
  }
  if (editable("config_override_command")) {
    patch.config_override_command = textChange(values.overrideCommand, initial.overrideCommand);
  }
  if (editable("ai.model")) patch.ai_model = textChange(values.aiModel, initial.aiModel);
  if (editable("ai.base_url")) patch.ai_base_url = textChange(values.aiBaseUrl, initial.aiBaseUrl);
  if (editable("ai.tool_reasoning_effort") && values.aiReasoningEffort !== initial.aiReasoningEffort) {
    patch.ai_tool_reasoning_effort =
      values.aiReasoningEffort === "default"
        ? { op: "unset" }
        : { op: "set", value: values.aiReasoningEffort };
  }
  if (editable("ai.api_key")) {
    const typed = values.apiKeyInput.trim();
    if (values.removeStoredApiKey) {
      patch.ai_api_key = { op: "unset" };
    } else if (values.apiKeyMode === "direct" && typed !== "") {
      patch.ai_api_key = { op: "set", value: typed };
    }
  }
  if (editable("agent.provider")) {
    patch.agent_provider = textChange(values.agentProvider, initial.agentProvider);
  }
  if (editable("agent.directory")) {
    patch.agent_directory = textChange(values.agentDirectory, initial.agentDirectory);
  }
  if (
    editable("agent.auto_accept_edits") &&
    values.agentAutoAcceptEdits !== initial.agentAutoAcceptEdits
  ) {
    // true is the default: remove the key instead of writing it
    patch.agent_auto_accept_edits = values.agentAutoAcceptEdits
      ? { op: "unset" }
      : { op: "set", value: false };
  }
  for (const key of Object.keys(patch) as (keyof SettingsPatch)[]) {
    if (patch[key] === undefined) delete patch[key];
  }
  return patch;
}

export const isEmptyPatch = (patch: SettingsPatch) => Object.keys(patch).length === 0;
