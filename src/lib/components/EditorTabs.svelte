<script lang="ts">
  import appStore from "$lib/stores/app.svelte";
  import type { EditorTab } from "$lib/stores/app.svelte";

  // Show the file name without the trailing query-file extension for a
  // compact label (extension varies by engine: .sql / .redis / .es).
  const tabLabel = (tab: EditorTab): string =>
    tab.file.replace(/\.(sql|redis|es)$/i, "");

  // Tabs are global across connections, so disambiguate with the connection
  // name whenever more than one connection has open tabs.
  const multipleConnections = $derived(
    new Set(appStore.editorTabs.map((t) => t.connection)).size > 1,
  );

  const tabTooltip = (tab: EditorTab): string =>
    `${tab.connection} / ${tab.file}${tab.dirty ? " (unsaved)" : ""}`;
</script>

<!-- 多段タブ: 開いているエディタが増えたら折り返して複数段で表示する -->
<div
  class="flex shrink-0 flex-wrap items-end gap-1 border-b border-zinc-700 bg-zinc-950 px-2 pt-1"
  data-annotate="editor-tabs"
>
  {#each appStore.editorTabs as tab (tab.id)}
    {@const active = tab.id === appStore.activeEditorTabId}
    <!-- アクティブタブは下辺のボーダーをコンテンツ側に食い込ませて
         「タブが本文とつながっている」見た目にする (-mb-px) -->
    <div
      class="group flex shrink-0 items-center gap-1 rounded-t-md border border-b-0 px-2.5 py-1 text-xs {active
        ? '-mb-px border-zinc-600 bg-zinc-800 text-zinc-100 shadow-[inset_0_-2px_0_0_#3b82f6]'
        : 'border-zinc-800 bg-zinc-900 text-zinc-400 hover:bg-zinc-800/70 hover:text-zinc-200'}"
    >
      <button
        class="flex max-w-56 items-baseline gap-1 truncate font-mono"
        title={tabTooltip(tab)}
        data-annotate="button-editor-tab-{tab.id}"
        onclick={() => appStore.activateEditorTab(tab.id)}
      >
        {#if multipleConnections}
          <span
            class="shrink-0 text-[10px] {active
              ? 'text-sky-300'
              : 'text-zinc-500'}">{tab.connection}/</span
          >
        {/if}
        <span class="truncate">{tabLabel(tab)}</span>
        {#if tab.dirty}
          <span
            class="shrink-0 {active ? 'text-sky-300' : 'text-zinc-400'}"
            title="Unsaved">●</span
          >
        {/if}
      </button>
      <button
        class="rounded px-0.5 text-zinc-500 opacity-70 hover:bg-zinc-700 hover:text-zinc-100 group-hover:opacity-100 {active
          ? 'opacity-100'
          : ''}"
        title="Close this tab"
        aria-label="Close this tab"
        data-annotate="button-editor-tab-close-{tab.id}"
        onclick={() => appStore.closeEditorTab(tab.id)}
      >
        <i class="bi bi-x-lg text-[10px]" aria-hidden="true"></i>
      </button>
    </div>
  {/each}
</div>
