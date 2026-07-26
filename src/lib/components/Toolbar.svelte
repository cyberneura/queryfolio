<script lang="ts">
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    onRunCurrent: () => void;
    onOpenSearch: () => void;
    onOpenSettings: () => void;
    /// AI チャットペイン (右) の表示状態と切り替え
    chatOpen: boolean;
    onToggleChat: () => void;
  }

  let {
    onRunCurrent,
    onOpenSearch,
    onOpenSettings,
    chatOpen,
    onToggleChat,
  }: Props = $props();

  // 実行中は Run ボタンを Cancel ボタンに切り替え、
  // 実行中のタブ (接続単位で 1 つ) のクエリをキャンセルする
  const cancelRunningQuery = () => {
    const running = appStore.resultTabs.find((t) => t.running);
    if (running) {
      void appStore.cancelQuery(running.id);
    }
  };
</script>

<div
  class="flex shrink-0 items-center gap-2 border-b border-zinc-700 bg-zinc-900 px-3 py-1.5"
>
  <span class="text-sm font-semibold text-zinc-200">QueryFolio</span>
  {#if appStore.selectedConnection}
    <span class="text-xs text-zinc-500">
      {appStore.selectedConnection}
      {#if appStore.selectedFile}
        / {appStore.selectedFile}
      {/if}
    </span>
  {/if}

  <span class="ml-auto flex items-center gap-2">
    <!-- 検索 (接続・クエリファイル)。Cmd+K でも開く -->
    <button
      class="flex items-center gap-1 rounded border border-zinc-600 px-2 py-1 text-xs text-zinc-400 hover:bg-zinc-800"
      title="Search connections and query files (Cmd+K)"
      aria-label="Search"
      data-annotate="button-open-search"
      onclick={onOpenSearch}
    >
      <i class="bi bi-search" aria-hidden="true"></i>
      <span class="text-zinc-500">⌘K</span>
    </button>
    <!--
      Writable スイッチ。OFF (既定) では SELECT/SHOW 等の副作用の無い文しか
      実行できない (バックエンドが強制)。config で readonly: true の接続では
      スイッチでは解除できないためロック表示にする。
    -->
    {#if appStore.selectedConnectionReadonly}
      <span
        class="flex items-center gap-1 rounded border border-zinc-700 px-2 py-1 text-xs text-zinc-500"
        title="This connection is read-only (readonly: true in config)"
        data-annotate="writable-locked"
      >
        <i class="bi bi-lock-fill" aria-hidden="true"></i> Read-only
      </span>
    {:else}
      <button
        class="flex items-center gap-1 rounded border px-2 py-1 text-xs transition-colors {appStore.writable
          ? 'border-amber-500 bg-amber-600/20 text-amber-300 hover:bg-amber-600/30'
          : 'border-zinc-600 text-zinc-400 hover:bg-zinc-800'}"
        title={appStore.writable
          ? "Writable: write statements (INSERT/UPDATE/DELETE etc.) are allowed. Click to switch to read-only."
          : "Read-only: only SELECT/SHOW and other side-effect-free statements run. Click to allow writes."}
        aria-pressed={appStore.writable}
        data-annotate="toggle-writable"
        onclick={() => appStore.toggleWritable()}
      >
        {#if appStore.writable}
          <i class="bi bi-unlock-fill" aria-hidden="true"></i> Writable
        {:else}
          <i class="bi bi-lock-fill" aria-hidden="true"></i> Read-only
        {/if}
      </button>
    {/if}
    {#if appStore.running}
      <button
        class="rounded bg-red-800 px-3 py-1 text-xs text-white hover:bg-red-700"
        title="Cancel the running query"
        aria-label="Cancel the running query"
        data-annotate="button-cancel-query-toolbar"
        onclick={cancelRunningQuery}
      >
        <i class="bi bi-stop-fill" aria-hidden="true"></i> Cancel
      </button>
    {:else}
      <button
        class="rounded bg-green-700 px-3 py-1 text-xs text-white hover:bg-green-600 disabled:opacity-40"
        title="Run the statement under the cursor (Cmd+Enter)"
        data-annotate="button-run-query"
        disabled={!appStore.selectedConnection}
        onclick={onRunCurrent}
      >
        <i class="bi bi-play-fill" aria-hidden="true"></i> Run
      </button>
    {/if}
    <!-- AI チャットペイン (右) の開閉 -->
    <button
      class="flex items-center gap-1 rounded border px-2 py-1 text-xs transition-colors {chatOpen
        ? 'border-blue-500 bg-blue-600/20 text-blue-300 hover:bg-blue-600/30'
        : 'border-zinc-600 text-zinc-400 hover:bg-zinc-800'}"
      title="Toggle the AI chat pane"
      aria-label="Toggle the AI chat pane"
      aria-pressed={chatOpen}
      data-annotate="toggle-chat-pane"
      onclick={onToggleChat}
    >
      <i class="bi bi-chat-dots" aria-hidden="true"></i> Chat
    </button>
    <button
      class="rounded border border-zinc-600 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
      title="Settings"
      aria-label="Settings"
      data-annotate="button-open-settings"
      onclick={onOpenSettings}
    >
      <i class="bi bi-gear" aria-hidden="true"></i>
    </button>
  </span>
</div>
