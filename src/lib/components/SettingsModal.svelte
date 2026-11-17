<script lang="ts">
  // Settings window for the general (non-connection) settings in config.yml
  // (CYBERNEURA-DEV-1298). The rules (only the local file is written, override values are
  // read-only and never written back, comments are confirmed once, .bak before the first
  // overwrite) live in src-tauri/src/settings.rs; see the "Settings window" section of AGENTS.md.
  import { onMount } from "svelte";
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import { toast } from "svelte-sonner";
  import {
    getConfigInfo,
    getGeneralSettings,
    revealConfigFile,
    saveGeneralSettings,
    testConfigOverrideCommand,
    type ConfigInfo,
    type GeneralSettingsView,
  } from "$lib/api";
  import {
    DEFAULT_LIMIT,
    OP_COMMAND_EXAMPLE,
    OVERRIDE_YAML_TEMPLATE,
    REASONING_EFFORT_CHOICES,
    buildPatch,
    initialValues,
    isEmptyPatch,
    isOverridden,
    validate,
    type FormValues,
  } from "$lib/settingsForm";
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    onClose: () => void;
    /// Opens the text editor for config.yml (the escape hatch for everything this form does not cover)
    onEditAsText: () => void;
  }

  let { onClose, onEditAsText }: Props = $props();

  let view = $state<GeneralSettingsView | null>(null);
  let info = $state<ConfigInfo | null>(null);
  let loadError = $state<string | null>(null);
  let initial = $state<FormValues | null>(null);
  let values = $state<FormValues | null>(null);
  let saving = $state(false);
  let saveError = $state<string | null>(null);
  /// The file has comments that this save would drop; waiting for the user's answer
  let confirmComments = $state(false);
  let confirmDiscard = $state(false);
  let testing = $state(false);
  let testResult = $state<{ ok: boolean; text: string } | null>(null);

  const OVERRIDDEN_NOTE = "Overridden by 1Password (config_override_command)";
  /// config_override_command failed, so whether it provides this value is unknown (settings.rs
  /// OVERRIDE_UNKNOWN_KEYS). Locked until the command works again
  const OVERRIDE_UNKNOWN_NOTE =
    "Locked: config_override_command failed, so it is not known whether it provides this value. Fix the command, then reopen this window";

  const load = async () => {
    loadError = null;
    try {
      const [settings, configInfo] = await Promise.all([getGeneralSettings(), getConfigInfo()]);
      view = settings;
      info = configInfo;
      initial = initialValues(settings.local);
      values = initialValues(settings.local);
    } catch (e) {
      loadError = String(e);
    }
  };

  onMount(load);

  const overridden = $derived(view?.overridden ?? []);
  const locked = (field: string) => !view?.editable || isOverridden(overridden, field);
  const errors = $derived(values && initial ? validate(values, initial, overridden) : {});
  const patch = $derived(values && initial ? buildPatch(values, initial, overridden) : {});
  const dirty = $derived(!isEmptyPatch(patch));
  const invalidMessage = (field: string) =>
    view?.local.invalid.find((m) => m.field === field)?.message ?? null;

  const requestClose = () => {
    if (dirty && !saving) {
      confirmDiscard = true;
      return;
    }
    onClose();
  };

  const save = async (allowDropComments: boolean) => {
    if (!values || Object.keys(errors).length > 0 || saving) return;
    if (!dirty) {
      onClose();
      return;
    }
    saving = true;
    saveError = null;
    try {
      const outcome = await saveGeneralSettings(patch, allowDropComments);
      if (outcome.status === "needs_comment_confirmation") {
        confirmComments = true;
        return;
      }
      confirmComments = false;
      if (outcome.backup_created) {
        toast.info("Kept a copy of the previous config", { description: outcome.backup_created });
      }
      // Same as saving from the text editor: re-read the config so the change takes effect
      if (await appStore.reloadConnections()) {
        toast.success("Settings saved");
        onClose();
      } else {
        saveError = `Saved, but reloading failed: ${appStore.errorMessage ?? "unknown error"}`;
        await load();
      }
    } catch (e) {
      saveError = String(e);
    } finally {
      saving = false;
    }
  };

  const testCommand = async () => {
    if (!values || testing) return;
    const command = values.overrideCommand.trim();
    if (command === "") return;
    testing = true;
    testResult = null;
    try {
      const count = await testConfigOverrideCommand(command);
      testResult = {
        ok: true,
        text: `The command works. It overrides ${count} value${count === 1 ? "" : "s"}.`,
      };
    } catch (e) {
      testResult = { ok: false, text: String(e) };
    } finally {
      testing = false;
    }
  };

  const copy = async (text: string, what: string) => {
    try {
      await writeText(text);
      toast.success(`Copied ${what}`);
    } catch (e) {
      toast.error(`Could not copy: ${e}`);
    }
  };

  const reveal = async () => {
    try {
      await revealConfigFile();
    } catch (e) {
      toast.error(String(e));
    }
  };

  const reasoningChoices = $derived.by(() => {
    const current = values?.aiReasoningEffort;
    if (current === undefined || REASONING_EFFORT_CHOICES.some((c) => c.value === current)) {
      return REASONING_EFFORT_CHOICES;
    }
    return [...REASONING_EFFORT_CHOICES, { value: current, label: current }];
  });

  const inputClass =
    "w-full rounded border border-zinc-700 bg-zinc-950 px-2 py-1 font-mono text-xs text-zinc-200 disabled:cursor-not-allowed disabled:opacity-60";
  const smallButton =
    "shrink-0 whitespace-nowrap rounded border border-zinc-600 px-2 py-0.5 text-[11px] text-zinc-300 hover:bg-zinc-800 disabled:opacity-40";
</script>

<svelte:window
  onkeydown={(e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      if (confirmComments) confirmComments = false;
      else if (confirmDiscard) confirmDiscard = false;
      else requestClose();
    }
  }}
/>

{#snippet resetButton(field: string, onReset: () => void, disabled: boolean)}
  <button
    type="button"
    class={smallButton}
    data-annotate={`button-reset-${field}`}
    title="Remove this key from config.yml and use the default"
    disabled={disabled || locked(field)}
    onclick={onReset}
  >
    Reset to default
  </button>
{/snippet}

{#snippet fieldNotes(field: string)}
  {#if isOverridden(overridden, field)}
    <p class="text-[11px] text-amber-400" data-annotate={`text-overridden-${field}`}>
      <i class="bi bi-shield-lock"></i>
      {view?.override_error ? OVERRIDE_UNKNOWN_NOTE : OVERRIDDEN_NOTE}
    </p>
  {/if}
  {#if invalidMessage(field)}
    <p class="text-[11px] text-amber-400">{invalidMessage(field)}</p>
  {/if}
  {#if errors[field]}
    <p class="text-[11px] text-red-400" data-annotate={`text-error-${field}`}>{errors[field]}</p>
  {/if}
{/snippet}

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-settings-modal"
  data-modal
  onclick={(e) => {
    if (e.target === e.currentTarget) requestClose();
  }}
>
  <div
    class="flex max-h-[85vh] w-[640px] flex-col overflow-hidden rounded-lg border border-zinc-700 bg-zinc-900 shadow-xl"
  >
    <div class="flex items-center justify-between border-b border-zinc-800 px-4 py-3">
      <h2 class="text-sm font-semibold text-zinc-200">Settings</h2>
      <div class="flex gap-2">
        <button
          class={smallButton}
          data-annotate="button-settings-reveal"
          title="Show config.yml in the file manager"
          onclick={reveal}
        >
          <i class="bi bi-folder2-open"></i> Show config file
        </button>
        <button
          class={smallButton}
          data-annotate="button-settings-edit-as-text"
          title="Edit config.yml as text (keeps your comments)"
          disabled={dirty}
          onclick={onEditAsText}
        >
          <i class="bi bi-file-earmark-code"></i> Edit as text
        </button>
      </div>
    </div>

    <!-- disabled while saving: edits typed after the request was sent would not be in it, and
         the window closes when the save succeeds -->
    <fieldset class="flex min-w-0 flex-col gap-4 overflow-auto px-4 py-3 text-xs" disabled={saving}>
      {#if loadError}
        <pre class="whitespace-pre-wrap font-mono text-red-400">{loadError}</pre>
      {:else if !view || !values}
        <p class="text-zinc-500">Loading...</p>
      {:else}
        <div class="flex flex-col gap-1 text-zinc-400">
          <p>
            These settings are saved to <span class="font-mono text-zinc-300"
              >{view.config_path}</span
            >. Connections are edited in config.yml for now.
          </p>
          {#if info}
            <p>
              Config source: <span class="font-mono text-zinc-300" data-annotate="text-config-source"
                >{info.source}</span
              >
            </p>
          {/if}
          {#if view.readonly_reason}
            <p class="text-amber-400" data-annotate="text-settings-readonly">
              Read-only: {view.readonly_reason}
            </p>
          {/if}
          {#if view.override_error}
            <p class="text-amber-400">
              config_override_command failed, so it is not known which values it overrides:
              {view.override_error}
            </p>
          {/if}
          {#if view.editable && view.has_comments}
            <p class="text-zinc-500">
              config.yml has comments. Saving here rewrites the file without them (you will be
              asked first). Use "Edit as text" to keep them.
            </p>
          {/if}
        </div>

        <!-- Query results -->
        <section class="flex flex-col gap-1.5">
          <h3 class="font-semibold text-zinc-300">Automatic LIMIT (default_limit)</h3>
          <p class="text-zinc-500">
            Rows a SELECT without LIMIT returns at most. 0 turns it off. Default: {DEFAULT_LIMIT}.
          </p>
          <div class="flex items-center gap-2">
            <input
              class={`${inputClass} max-w-40`}
              inputmode="numeric"
              placeholder={String(DEFAULT_LIMIT)}
              data-annotate="input-default-limit"
              disabled={locked("default_limit")}
              value={isOverridden(overridden, "default_limit")
                ? String(view.effective_default_limit ?? "")
                : values.defaultLimit}
              oninput={(e) => {
                if (values) values.defaultLimit = e.currentTarget.value;
              }}
            />
            {@render resetButton(
              "default_limit",
              () => {
                if (values) values.defaultLimit = "";
              },
              values.defaultLimit === "",
            )}
          </div>
          {@render fieldNotes("default_limit")}
        </section>

        <!-- Query files -->
        <section class="flex flex-col gap-1.5">
          <h3 class="font-semibold text-zinc-300">Query file folder (sqlfiles_dir)</h3>
          <p class="text-zinc-500">
            Where saved queries live. A relative path is resolved against the config folder.
            Default: ~/.config/queryfolio/sqlfiles.
          </p>
          <div class="flex items-center gap-2">
            <input
              class={inputClass}
              placeholder="~/.config/queryfolio/sqlfiles"
              data-annotate="input-sqlfiles-dir"
              disabled={locked("sqlfiles_dir")}
              value={values.sqlfilesDir}
              oninput={(e) => {
                if (values) values.sqlfilesDir = e.currentTarget.value;
              }}
            />
            {@render resetButton(
              "sqlfiles_dir",
              () => {
                if (values) values.sqlfilesDir = "";
              },
              values.sqlfilesDir === "",
            )}
          </div>
          {#if view.effective_sqlfiles_dir}
            <p class="text-zinc-500">
              In use now: <span class="font-mono">{view.effective_sqlfiles_dir}</span>
            </p>
          {/if}
          {@render fieldNotes("sqlfiles_dir")}

          {#if view.folders.length > 0}
            <details class="text-zinc-400">
              <summary class="cursor-pointer text-zinc-500" data-annotate="toggle-folder-list">
                Folder of each connection ({view.folders.length})
              </summary>
              <p class="mt-1 text-zinc-500">
                Derived from host / engine / database / user unless the connection sets
                <span class="font-mono">folder_name</span> (edited with the connection).
              </p>
              <table class="mt-1 w-full font-mono text-[11px]" data-annotate="table-folders">
                <tbody>
                  {#each view.folders as folder (folder.connection)}
                    <tr>
                      <td class="pr-3 text-zinc-300">{folder.connection}</td>
                      <td class="text-zinc-400">
                        {folder.folder}
                        {#if folder.explicit}<span class="text-zinc-500">(folder_name)</span>{/if}
                      </td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            </details>
          {/if}
        </section>

        <!-- Templates -->
        <section class="flex flex-col gap-1.5">
          <h3 class="font-semibold text-zinc-300">Connection templates (server_templates)</h3>
          {#if view.templates.length === 0}
            <p class="text-zinc-500">
              None. A template holds settings shared by several connections (`template: name`).
            </p>
          {:else}
            <ul class="font-mono text-[11px] text-zinc-300" data-annotate="list-templates">
              {#each view.templates as template (template.name)}
                <li>
                  {template.name}
                  {#if template.engine}<span class="text-zinc-500">({template.engine})</span>{/if}
                </li>
              {/each}
            </ul>
            <p class="text-zinc-500">Edit templates in config.yml ("Edit as text").</p>
          {/if}
        </section>

        <!-- Secrets / override -->
        <section class="flex flex-col gap-1.5">
          <h3 class="font-semibold text-zinc-300">
            Keep secrets in 1Password (config_override_command)
          </h3>
          <p class="text-zinc-500">
            A command whose output (YAML) is merged over config.yml when the app starts, so API
            keys and passwords never have to be written in the file. It runs without a shell.
          </p>
          <div class="flex items-center gap-2">
            <input
              class={inputClass}
              placeholder={OP_COMMAND_EXAMPLE}
              data-annotate="input-override-command"
              disabled={locked("config_override_command")}
              value={values.overrideCommand}
              oninput={(e) => {
                if (values) values.overrideCommand = e.currentTarget.value;
                testResult = null;
              }}
            />
            <button
              type="button"
              class={smallButton}
              data-annotate="button-test-override-command"
              title="Run the command now and count the values it overrides (values are not shown)"
              disabled={testing ||
                values.overrideCommand.trim() === "" ||
                !!errors["config_override_command"]}
              onclick={testCommand}
            >
              {testing ? "Testing..." : "Test"}
            </button>
            {@render resetButton(
              "config_override_command",
              () => {
                if (values) values.overrideCommand = "";
                testResult = null;
              },
              values.overrideCommand === "",
            )}
          </div>
          {#if testResult}
            <p
              class={`whitespace-pre-wrap text-[11px] ${testResult.ok ? "text-emerald-400" : "text-red-400"}`}
              data-annotate="text-test-override-result"
            >
              {testResult.text}
            </p>
          {/if}
          {@render fieldNotes("config_override_command")}
        </section>

        <!-- AI -->
        <section class="flex flex-col gap-1.5">
          <h3 class="font-semibold text-zinc-300">AI (OpenAI)</h3>
          <p class="text-zinc-500">
            Used for SQL generation, explanations and the chat pane. Provider:
            <span class="font-mono">{view.local.ai.provider ?? "openai"}</span>
            (the only one supported).
          </p>
          {@render fieldNotes("ai")}

          <span class="text-zinc-400">API key</span>
          {#if isOverridden(overridden, "ai.api_key")}
            {@render fieldNotes("ai.api_key")}
          {:else}
            <div class="flex flex-col gap-1.5 rounded border border-zinc-800 p-2">
              <label class="flex items-center gap-2 text-zinc-300">
                <input
                  type="radio"
                  name="api-key-mode"
                  data-annotate="radio-api-key-op"
                  disabled={locked("ai.api_key")}
                  checked={values.apiKeyMode === "op"}
                  onchange={() => {
                    if (values) values.apiKeyMode = "op";
                  }}
                />
                Get it from 1Password (recommended)
              </label>
              {#if values.apiKeyMode === "op"}
                <div class="ml-5 flex flex-col gap-1.5 text-zinc-500">
                  <p>
                    1. In 1Password, save this YAML in a field (put your key in place of
                    <span class="font-mono">sk-...</span>):
                  </p>
                  <div class="flex items-start gap-2">
                    <pre
                      class="flex-1 rounded bg-zinc-950 px-2 py-1 font-mono text-[11px] text-zinc-300">{OVERRIDE_YAML_TEMPLATE}</pre>
                    <button
                      type="button"
                      class={smallButton}
                      data-annotate="button-copy-override-yaml"
                      onclick={() => copy(OVERRIDE_YAML_TEMPLATE, "the YAML")}
                    >
                      Copy
                    </button>
                  </div>
                  <p>2. Set the command above to read that field, then press Test:</p>
                  <div class="flex items-center gap-2">
                    <code class="flex-1 rounded bg-zinc-950 px-2 py-1 font-mono text-[11px] text-zinc-300"
                      >{OP_COMMAND_EXAMPLE}</code
                    >
                    <button
                      type="button"
                      class={smallButton}
                      data-annotate="button-copy-op-command"
                      onclick={() => copy(OP_COMMAND_EXAMPLE, "the command")}
                    >
                      Copy
                    </button>
                    <button
                      type="button"
                      class={smallButton}
                      data-annotate="button-use-op-command"
                      disabled={locked("config_override_command") ||
                        values.overrideCommand.trim() !== ""}
                      title="Put the example into the command field (replace the placeholders)"
                      onclick={() => {
                        if (values) values.overrideCommand = OP_COMMAND_EXAMPLE;
                      }}
                    >
                      Use
                    </button>
                  </div>
                  {#if view.local.ai.api_key_set}
                    <label class="flex items-center gap-2 text-zinc-300">
                      <input
                        type="checkbox"
                        data-annotate="checkbox-remove-stored-api-key"
                        checked={values.removeStoredApiKey}
                        onchange={(e) => {
                          if (values) values.removeStoredApiKey = e.currentTarget.checked;
                        }}
                      />
                      Remove the API key stored in config.yml when saving
                    </label>
                  {/if}
                </div>
              {/if}
              <label class="flex items-center gap-2 text-zinc-300">
                <input
                  type="radio"
                  name="api-key-mode"
                  data-annotate="radio-api-key-direct"
                  disabled={locked("ai.api_key")}
                  checked={values.apiKeyMode === "direct"}
                  onchange={() => {
                    if (values) values.apiKeyMode = "direct";
                  }}
                />
                Enter it here (stored as plain text in config.yml)
              </label>
              {#if values.apiKeyMode === "direct"}
                <div class="ml-5 flex flex-col gap-1">
                  <div class="flex items-center gap-2">
                    <input
                      class={inputClass}
                      type="password"
                      autocomplete="off"
                      placeholder={view.local.ai.api_key_set
                        ? "A key is stored. Type a new one to replace it"
                        : "sk-..."}
                      data-annotate="input-api-key"
                      disabled={locked("ai.api_key") || values.removeStoredApiKey}
                      value={values.apiKeyInput}
                      oninput={(e) => {
                        if (values) values.apiKeyInput = e.currentTarget.value;
                      }}
                    />
                    {#if view.local.ai.api_key_set}
                      <label class="flex shrink-0 items-center gap-1 text-zinc-400">
                        <input
                          type="checkbox"
                          data-annotate="checkbox-remove-api-key"
                          checked={values.removeStoredApiKey}
                          onchange={(e) => {
                            if (values) values.removeStoredApiKey = e.currentTarget.checked;
                          }}
                        />
                        Remove
                      </label>
                    {/if}
                  </div>
                  {@render fieldNotes("ai.api_key")}
                </div>
              {/if}
            </div>
          {/if}

          <span class="text-zinc-400">Model</span>
          <div class="flex items-center gap-2">
            <input
              class={inputClass}
              placeholder="(the app's default model)"
              data-annotate="input-ai-model"
              disabled={locked("ai.model")}
              value={values.aiModel}
              oninput={(e) => {
                if (values) values.aiModel = e.currentTarget.value;
              }}
            />
            {@render resetButton(
              "ai.model",
              () => {
                if (values) values.aiModel = "";
              },
              values.aiModel === "",
            )}
          </div>
          {@render fieldNotes("ai.model")}

          <span class="text-zinc-400">Base URL (for an OpenAI-compatible API)</span>
          <div class="flex items-center gap-2">
            <input
              class={inputClass}
              placeholder="https://api.openai.com/v1"
              data-annotate="input-ai-base-url"
              disabled={locked("ai.base_url")}
              value={values.aiBaseUrl}
              oninput={(e) => {
                if (values) values.aiBaseUrl = e.currentTarget.value;
              }}
            />
            {@render resetButton(
              "ai.base_url",
              () => {
                if (values) values.aiBaseUrl = "";
              },
              values.aiBaseUrl === "",
            )}
          </div>
          {@render fieldNotes("ai.base_url")}

          <span class="text-zinc-400">Reasoning effort for the chat (tool_reasoning_effort)</span>
          <div class="flex items-center gap-2">
            <select
              class={`${inputClass} max-w-72`}
              data-annotate="select-ai-reasoning-effort"
              disabled={locked("ai.tool_reasoning_effort")}
              value={values.aiReasoningEffort}
              onchange={(e) => {
                if (values) values.aiReasoningEffort = e.currentTarget.value;
              }}
            >
              {#each reasoningChoices as choice (choice.value)}
                <option value={choice.value}>{choice.label}</option>
              {/each}
            </select>
          </div>
          {@render fieldNotes("ai.tool_reasoning_effort")}
        </section>
      {/if}

      {#if saveError}
        <pre
          class="whitespace-pre-wrap font-mono text-red-400"
          data-annotate="text-settings-save-error">{saveError}</pre>
      {/if}
    </fieldset>

    <div class="flex flex-col gap-2 border-t border-zinc-800 px-4 py-3 text-xs">
      {#if confirmComments && view}
        <div class="flex flex-col gap-2 rounded border border-amber-700 bg-amber-950/40 p-2">
          <p class="text-amber-200">
            config.yml has comments. Saving from this window rewrites the file and removes them.
            {#if view.backup_exists}
              The copy made before the first save is at
            {:else}
              A copy of the current file is kept at
            {/if}
            <span class="font-mono">{view.backup_path}</span>.
          </p>
          <div class="flex justify-end gap-2">
            <button
              class="rounded border border-zinc-600 px-3 py-1 text-zinc-300 hover:bg-zinc-800"
              data-annotate="button-comments-cancel"
              onclick={() => (confirmComments = false)}
            >
              Cancel
            </button>
            <button
              class="rounded bg-amber-600 px-3 py-1 text-white hover:bg-amber-500"
              data-annotate="button-comments-save-anyway"
              disabled={saving}
              onclick={() => save(true)}
            >
              Save and remove comments
            </button>
          </div>
        </div>
      {:else if confirmDiscard}
        <div class="flex items-center justify-end gap-2">
          <span class="mr-auto text-amber-300">Discard your changes?</span>
          <button
            class="rounded border border-zinc-600 px-3 py-1 text-zinc-300 hover:bg-zinc-800"
            data-annotate="button-settings-keep-editing"
            onclick={() => (confirmDiscard = false)}
          >
            Keep editing
          </button>
          <button
            class="rounded bg-red-700 px-3 py-1 text-white hover:bg-red-600"
            data-annotate="button-settings-discard"
            onclick={onClose}
          >
            Discard
          </button>
        </div>
      {:else}
        <div class="flex justify-end gap-2">
          <button
            class="rounded border border-zinc-600 px-3 py-1 text-zinc-300 hover:bg-zinc-800"
            data-annotate="button-settings-close"
            onclick={requestClose}
          >
            {dirty ? "Cancel" : "Close"}
          </button>
          <button
            class="rounded bg-blue-600 px-3 py-1 text-white hover:bg-blue-500 disabled:opacity-40"
            data-annotate="button-settings-save"
            disabled={!view?.editable || !dirty || saving || Object.keys(errors).length > 0}
            onclick={() => save(false)}
          >
            {saving ? "Saving..." : "Save"}
          </button>
        </div>
      {/if}
    </div>
  </div>
</div>
