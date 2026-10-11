<script lang="ts">
  import { toast } from "svelte-sonner";
  import appStore from "$lib/stores/app.svelte";
  import type { EngineCapabilities } from "$lib/api";

  interface Props {
    engine: string | null;
    /// Engine capability declaration (used to decide which UI to show). null means SQL-equivalent
    capabilities: EngineCapabilities | null;
    readonly: boolean;
    /// Handler for pressing the Explain button (+page.svelte extracts the statement at the
    /// editor cursor position and passes it to appStore.explainQuery)
    onExplain: () => void;
    /// Handler for pressing the Explain SQL button (+page.svelte extracts the statement at the
    /// editor cursor position and passes it to appStore.explainSql)
    onExplainSql: () => void;
    /// Handler for pressing the Format button (+page.svelte calls
    /// SqlEditor.formatCurrentStatement)
    onFormat: () => void;
    /// Whether multiple lines are selected. When true, the Replace Multiline button is shown
    showReplaceMultiline: boolean;
    /// Handler for pressing the Replace Multiline button (+page.svelte opens the pane)
    onReplaceMultiline: () => void;
  }

  let {
    engine,
    capabilities,
    readonly,
    onExplain,
    onExplainSql,
    onFormat,
    showReplaceMultiline,
    onReplaceMultiline,
  }: Props = $props();

  const isSqlite = $derived(
    ["sqlite", "sqlite3"].includes((engine ?? "").toLowerCase()),
  );

  /// Decide which capabilities to show (not yet fetched = null is treated as SQL-equivalent and shows everything)
  const supportsSchemas = $derived(capabilities?.supports_schemas ?? true);
  const supportsExplain = $derived(capabilities?.supports_explain ?? true);
  const supportsFormat = $derived(capabilities?.supports_format ?? true);
  const supportsAi = $derived(capabilities?.supports_ai ?? true);

  /// Visibility state and input content of the inline input field for AI generation
  let showAiInput = $state(false);
  let aiInstruction = $state("");
  let aiInputEl: HTMLInputElement | undefined = $state();

  const aiConfigured = $derived(appStore.aiInfo?.configured ?? false);

  /// Title of the AI button (explains how to configure when unset or on error)
  const aiButtonTitle = $derived(
    aiConfigured
      ? `Generate SQL with AI (${appStore.aiInfo?.model})`
      : appStore.aiError
        ? `AI is unavailable: ${appStore.aiError}`
        : "AI is not configured. Add an 'ai:' section (provider: openai, " +
          "api_key: ...) to config.yml or the override YAML.",
  );

  /// Title of the Explain SQL button (explains how to configure when unset or on error)
  const explainSqlButtonTitle = $derived(
    aiConfigured
      ? "Explain the SQL statement under the cursor with AI " +
          `(${appStore.aiInfo?.model}). Sends the SQL and schema info ` +
          "(table/column names), not your data."
      : appStore.aiError
        ? `AI is unavailable: ${appStore.aiError}`
        : "AI is not configured. Add an 'ai:' section (provider: openai, " +
          "api_key: ...) to config.yml or the override YAML.",
  );

  // Focus the input field once it is opened
  $effect(() => {
    if (showAiInput) {
      aiInputEl?.focus();
    }
  });

  const submitAiInstruction = async (e: SubmitEvent) => {
    e.preventDefault();
    if (appStore.aiGenerating) {
      return;
    }
    if (await appStore.generateSql(aiInstruction)) {
      aiInstruction = "";
      showAiInput = false;
    }
  };

  const onAiInputKeydown = (e: KeyboardEvent) => {
    if (e.key === "Escape" && !appStore.aiGenerating) {
      showAiInput = false;
    }
  };

  /// Resolve a conflicted tab: overwrite the disk with the local edits (force)
  const onOverwriteConflict = async () => {
    if (await appStore.overwriteActiveFileConflict()) {
      toast.success("Saved (overwrote the external change)");
    } else {
      toast.error("Failed to save the file", {
        description: appStore.errorMessage ?? undefined,
      });
    }
  };

  /// Resolve a conflicted tab: discard the local edits and reload the disk contents
  const onDiscardConflict = async () => {
    await appStore.discardActiveFileConflict();
  };

  const onSchemaChange = async (e: Event) => {
    const select = e.currentTarget as HTMLSelectElement;
    const schema = select.value;
    const previous = appStore.activeSchema;
    if (await appStore.changeActiveSchema(schema)) {
      if (schema !== previous) {
        toast.success(`Switched to ${schema}`);
      }
    } else {
      toast.error("Failed to switch the database", {
        description: appStore.errorMessage ?? undefined,
      });
      // If it fails, restore the display
      select.value = previous ?? "";
    }
  };
</script>

<!-- On a narrow window (e.g. when the chat pane is open), wrap the buttons that do not fit,
     to prevent the button row from overflowing and becoming invisible -->
<div
  class="flex shrink-0 flex-wrap items-center gap-x-2 gap-y-1 border-b border-zinc-700 bg-zinc-900 px-3 py-1"
>
  {#if engine}
    <span
      class="rounded bg-zinc-800 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-zinc-400"
      data-annotate="text-editor-engine"
    >
      {engine}
    </span>
  {/if}

  {#if readonly}
    <span
      class="rounded bg-yellow-500/15 px-1.5 py-0.5 text-[10px] tracking-wide text-yellow-400"
      title="Write statements are rejected (readonly: true in config)"
      data-annotate="badge-editor-readonly"
    >
      read-only
    </span>
  {/if}

  <!-- While in conflict with an external change, always show the resolution actions that can be reached (overwrite / discard).
         This is an escape hatch for the case where clicking the file list again becomes Rename and never reaches the reopen path. -->
  {#if appStore.activeFileConflicted}
    <span
      class="flex items-center gap-1 rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] tracking-wide text-amber-400"
      title="This file was changed on disk while you have unsaved edits"
      data-annotate="badge-editor-conflict"
    >
      <i class="bi bi-exclamation-triangle-fill" aria-hidden="true"></i> conflict
    </span>
    <button
      type="button"
      class="rounded border border-amber-500/50 bg-amber-500/15 px-2 py-0.5 text-xs text-amber-300 hover:bg-amber-500/25"
      data-annotate="button-conflict-overwrite"
      title="Save your edits, overwriting the external change on disk"
      onclick={onOverwriteConflict}
    >
      <i class="bi bi-save" aria-hidden="true"></i> Overwrite
    </button>
    <button
      type="button"
      class="rounded border border-zinc-600 bg-zinc-800 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-700"
      data-annotate="button-conflict-discard"
      title="Discard your unsaved edits and reload the file from disk"
      onclick={onDiscardConflict}
    >
      <i class="bi bi-arrow-counterclockwise" aria-hidden="true"></i> Discard
    </button>
  {/if}

  {#if supportsSchemas}
    <span class="text-xs text-zinc-500">Database:</span>
    {#if isSqlite || appStore.schemas.length <= 1}
      <span class="font-mono text-xs text-zinc-300" data-annotate="text-active-schema">
        {appStore.activeSchema ?? "(default)"}
      </span>
    {:else}
      <select
        class="max-w-64 rounded border border-zinc-600 bg-zinc-800 px-1.5 py-0.5 font-mono text-xs text-zinc-200 outline-none focus:border-blue-400"
        data-annotate="select-active-schema"
        value={appStore.activeSchema ?? ""}
        onchange={onSchemaChange}
      >
        {#if appStore.activeSchema && !appStore.schemas.includes(appStore.activeSchema)}
          <option value={appStore.activeSchema}>{appStore.activeSchema}</option>
        {/if}
        {#each appStore.schemas as schema (schema)}
          <option value={schema}>{schema}</option>
        {/each}
      </select>
    {/if}
  {/if}

  <div
    class="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-x-2 gap-y-1"
  >
    <!-- Shown only while multiple lines are selected. Opens the line-by-line bulk replace pane -->
    {#if showReplaceMultiline}
      <button
        type="button"
        class="rounded border border-zinc-600 bg-zinc-800 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-700"
        data-annotate="button-replace-multiline"
        title="Replace the selected lines with a template (e.g. KILL %%%;)"
        aria-label="Replace the selected lines with a template"
        onclick={onReplaceMultiline}
      >
        <i class="bi bi-body-text" aria-hidden="true"></i> Replace Multiline
      </button>
    {/if}
    <!-- Format the statement at the cursor position (enabled only when a file is open) -->
    {#if supportsFormat}
      <button
        type="button"
        class="rounded border border-zinc-600 bg-zinc-800 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50"
        data-annotate="button-format"
        title="Format the SQL statement under the cursor"
        aria-label="Format the SQL statement under the cursor"
        disabled={!appStore.selectedFile}
        onclick={onFormat}
      >
        <i class="bi bi-braces" aria-hidden="true"></i> Format
      </button>
    {/if}
    <!-- Run the statement at the cursor position with the engine-specific EXPLAIN (standalone feature, no AI needed) -->
    {#if supportsExplain}
      <button
        type="button"
        class="rounded border border-zinc-600 bg-zinc-800 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50"
        data-annotate="button-explain"
        title="Run EXPLAIN for the SELECT statement under the cursor"
        aria-label="Run EXPLAIN for the SELECT statement under the cursor"
        disabled={appStore.running}
        onclick={onExplain}
      >
        <i class="bi bi-diagram-3" aria-hidden="true"></i> Explain
      </button>
    {/if}
    <!-- Have the AI explain the statement at the cursor position in plain language (enabled only when AI is configured) -->
    {#if supportsAi}
      <button
        type="button"
        class="flex items-center gap-1 rounded border border-zinc-600 bg-zinc-800 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50"
        data-annotate="button-ai-explain-sql"
        title={explainSqlButtonTitle}
        disabled={!aiConfigured || appStore.aiExplaining}
        onclick={onExplainSql}
      >
        {#if appStore.aiExplaining}
          <!-- Spinner while the explanation is being generated -->
          <span
            class="inline-block size-3 animate-spin rounded-full border-2 border-zinc-300 border-t-transparent"
            data-annotate="spinner-ai-explaining"
          ></span>
          Explaining...
        {:else}
          <i class="bi bi-info-circle" aria-hidden="true"></i> Explain SQL
        {/if}
      </button>
    {/if}
    {#if !supportsAi}
      <!-- The AI UI is not shown for this engine -->
    {:else if showAiInput}
      <form
        class="flex min-w-0 items-center gap-1"
        onsubmit={submitAiInstruction}
      >
        <input
          bind:this={aiInputEl}
          bind:value={aiInstruction}
          class="w-72 max-w-full rounded border border-zinc-600 bg-zinc-800 px-2 py-0.5 text-xs text-zinc-200 outline-none placeholder:text-zinc-500 focus:border-blue-400"
          data-annotate="input-ai-instruction"
          placeholder="Describe the query in natural language..."
          disabled={appStore.aiGenerating}
          onkeydown={onAiInputKeydown}
        />
        <button
          type="submit"
          class="flex items-center gap-1 rounded border border-blue-500/50 bg-blue-500/15 px-2 py-0.5 text-xs text-blue-300 hover:bg-blue-500/25 disabled:cursor-not-allowed disabled:opacity-50"
          data-annotate="button-ai-generate"
          disabled={appStore.aiGenerating || !aiInstruction.trim()}
        >
          {#if appStore.aiGenerating}
            <!-- Spinner while generating -->
            <span
              class="inline-block size-3 animate-spin rounded-full border-2 border-blue-300 border-t-transparent"
              data-annotate="spinner-ai-generating"
            ></span>
            Generating...
          {:else}
            Generate
          {/if}
        </button>
        <button
          type="button"
          class="rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200 disabled:cursor-not-allowed disabled:opacity-50"
          data-annotate="button-ai-close"
          title="Close (Esc)"
          aria-label="Close (Esc)"
          disabled={appStore.aiGenerating}
          onclick={() => {
            showAiInput = false;
          }}
        >
          <i class="bi bi-x-lg" aria-hidden="true"></i>
        </button>
      </form>
    {:else}
      <button
        type="button"
        class="rounded border border-zinc-600 bg-zinc-800 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50"
        data-annotate="button-ai-toggle"
        title={aiButtonTitle}
        aria-label="Generate SQL with AI"
        disabled={!aiConfigured}
        onclick={() => {
          showAiInput = true;
        }}
      >
        <i class="bi bi-stars" aria-hidden="true"></i> AI
      </button>
    {/if}
  </div>
</div>
