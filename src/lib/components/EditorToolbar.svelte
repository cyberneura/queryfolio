<script lang="ts">
  import { toast } from "svelte-sonner";
  import appStore from "$lib/stores/app.svelte";
  import type { EngineCapabilities } from "$lib/api";

  interface Props {
    engine: string | null;
    /// エンジンの能力宣言 (UI の出し分けに使う)。null は SQL 相当
    capabilities: EngineCapabilities | null;
    readonly: boolean;
    /// Explain ボタン押下時の処理 (+page.svelte がエディタの
    /// カーソル位置の文を取り出して appStore.explainQuery に渡す)
    onExplain: () => void;
    /// Explain SQL ボタン押下時の処理 (+page.svelte がエディタの
    /// カーソル位置の文を取り出して appStore.explainSql に渡す)
    onExplainSql: () => void;
    /// Format ボタン押下時の処理 (+page.svelte が
    /// SqlEditor.formatCurrentStatement を呼ぶ)
    onFormat: () => void;
    /// 複数行選択中か。true のとき Replace Multiline ボタンを表示する
    showReplaceMultiline: boolean;
    /// Replace Multiline ボタン押下時の処理 (+page.svelte がペインを開く)
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

  /// capability の出し分け (未取得 = null は SQL 相当として全部出す)
  const supportsSchemas = $derived(capabilities?.supports_schemas ?? true);
  const supportsExplain = $derived(capabilities?.supports_explain ?? true);
  const supportsFormat = $derived(capabilities?.supports_format ?? true);
  const supportsAi = $derived(capabilities?.supports_ai ?? true);

  /// AI 生成のインライン入力欄の表示状態と入力内容
  let showAiInput = $state(false);
  let aiInstruction = $state("");
  let aiInputEl: HTMLInputElement | undefined = $state();

  const aiConfigured = $derived(appStore.aiInfo?.configured ?? false);

  /// AI ボタンの title (未設定・エラー時は設定方法を案内する)
  const aiButtonTitle = $derived(
    aiConfigured
      ? `Generate SQL with AI (${appStore.aiInfo?.model})`
      : appStore.aiError
        ? `AI is unavailable: ${appStore.aiError}`
        : "AI is not configured. Add an 'ai:' section (provider: openai, " +
          "api_key: ...) to config.yml or the override YAML.",
  );

  /// Explain SQL ボタンの title (未設定・エラー時は設定方法を案内する)
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

  // 入力欄を開いたらフォーカスする
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

  /// 衝突タブの解消: 手元の編集でディスクを上書き保存する (force)
  const onOverwriteConflict = async () => {
    if (await appStore.overwriteActiveFileConflict()) {
      toast.success("Saved (overwrote the external change)");
    } else {
      toast.error("Failed to save the file", {
        description: appStore.errorMessage ?? undefined,
      });
    }
  };

  /// 衝突タブの解消: 手元の編集を破棄してディスクの内容を読み直す
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
      // 失敗したら表示を元に戻す
      select.value = previous ?? "";
    }
  };
</script>

<div
  class="flex shrink-0 items-center gap-2 border-b border-zinc-700 bg-zinc-900 px-3 py-1"
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

  <!-- 外部変更との衝突中は、常に到達可能な解消手段 (上書き / 破棄) を出す。
       ファイル一覧の再クリックが Rename になり reopen 経路に届かないケースの逃げ道。 -->
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

  <div class="ml-auto flex min-w-0 items-center gap-2">
    <!-- 複数行選択中のみ表示。行単位の一括置換ペインを開く -->
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
    <!-- カーソル位置の文を整形する (ファイルが開いているときのみ有効) -->
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
    <!-- カーソル位置の文をエンジン別 EXPLAIN で実行する (AI 不要の単体機能) -->
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
    <!-- カーソル位置の文を AI に平易に解説させる (AI 設定済みのときのみ有効) -->
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
          <!-- 解説の生成中スピナー -->
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
      <!-- AI 系 UI はこのエンジンでは出さない -->
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
            <!-- 生成中スピナー -->
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
