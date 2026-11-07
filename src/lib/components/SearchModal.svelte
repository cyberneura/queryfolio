<script lang="ts">
  import appStore from "$lib/stores/app.svelte";
  import * as api from "$lib/api";
  import type { FileSearchHit, TableInfo } from "$lib/api";
  import {
    buildSearchItems,
    matchConnections,
    matchLoadedTables,
    type SearchItem,
  } from "$lib/searchItems";

  interface Props {
    onClose: () => void;
    /// Show the TABLES pane (the left pane tab is held by +page.svelte). Called when a
    /// table candidate is chosen, so the revealed table is visible
    onOpenTable?: () => void;
  }

  let { onClose, onOpenTable }: Props = $props();

  let query = $state("");
  let fileHits = $state<FileSearchHit[]>([]);
  /// The candidate selected with the keyboard (a flat index into items)
  let activeIndex = $state(0);
  let inputEl: HTMLInputElement | undefined = $state();
  /// Generation number of the async search. Used so an old response does not overwrite newer results
  let searchGeneration = 0;

  /// Connection filtering (see $lib/searchItems.ts)
  const connMatches = $derived(matchConnections(appStore.connections, query));

  /// Tables already loaded by the TABLES pane for the selected connection + database
  /// (CYBERNEURA-DEV-1208). Nothing is fetched for the search: a connection whose TABLES pane
  /// has not been opened simply has no table candidates
  const tableMatches = $derived<TableInfo[]>(
    matchLoadedTables(
      appStore.loadedTables,
      appStore.selectedConnection,
      appStore.activeSchema,
      query,
    ),
  );

  /// Flat candidate list for keyboard navigation (connections, then files, then tables).
  /// The display grouping and indices correspond to this order.
  const items = $derived<SearchItem[]>(
    buildSearchItems(connMatches, fileHits, tableMatches),
  );

  /// Run the file search debounced on query change. Files are searched only for the selected
  /// connection (to reach files in other connections, switch connections).
  $effect(() => {
    const q = query.trim();
    const connection = appStore.selectedConnection;
    const gen = ++searchGeneration;
    // Reset the selection to the top on every query change
    activeIndex = 0;
    // The moment the search term or connection changes, clear the old file results. While waiting for
    // debounce + invoke, this prevents showing or selecting (Enter/click) stale files that do not
    // match the current search term. Connection candidates are filtered synchronously, so they can stay.
    fileHits = [];
    if (!q || !connection) {
      return;
    }
    const timer = setTimeout(async () => {
      try {
        const hits = await api.searchQueryFiles(connection, q);
        if (gen === searchGeneration) {
          fileHits = hits;
        }
      } catch {
        // On search failure, empty the results (the modal stays open)
        if (gen === searchGeneration) {
          fileHits = [];
        }
      }
    }, 150);
    return () => clearTimeout(timer);
  });

  // Focus the input on mount
  $effect(() => {
    inputEl?.focus();
  });

  const activate = async (item: SearchItem) => {
    if (item.kind === "connection") {
      await appStore.selectConnection(item.name);
    } else if (item.kind === "file") {
      await appStore.selectFile(item.hit.file_name);
    } else {
      // Insert the name into the editor of the selected connection and show the table in the
      // TABLES pane (expanded). Same connection as the loaded list by construction of tableMatches
      const connection = appStore.selectedConnection;
      appStore.insertSqlSnippet(item.table.qualified_name);
      if (connection) {
        appStore.revealTable(
          connection,
          appStore.activeSchema || null,
          item.table.qualified_name,
        );
        onOpenTable?.();
      }
    }
    onClose();
  };

  const onKeydown = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (items.length) {
        activeIndex = (activeIndex + 1) % items.length;
      }
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (items.length) {
        activeIndex = (activeIndex - 1 + items.length) % items.length;
      }
    } else if (e.key === "Enter") {
      e.preventDefault();
      const item = items[activeIndex];
      if (item) {
        void activate(item);
      }
    }
  };
</script>

<div
  class="fixed inset-0 z-20 flex items-start justify-center bg-black/60 pt-[15vh]"
  role="presentation"
  data-annotate="backdrop-search-modal"
  data-modal
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      onClose();
    }
  }}
>
  <div
    class="flex max-h-[60vh] w-[560px] flex-col overflow-hidden rounded-lg border border-zinc-700 bg-zinc-900 shadow-xl"
  >
    <div class="flex items-center gap-2 border-b border-zinc-700 px-3 py-2">
      <i class="bi bi-search text-zinc-500" aria-hidden="true"></i>
      <input
        bind:this={inputEl}
        bind:value={query}
        onkeydown={onKeydown}
        type="text"
        placeholder="Search connections, query files and loaded tables…"
        class="w-full bg-transparent text-sm text-zinc-100 placeholder:text-zinc-500 focus:outline-none"
        data-annotate="search-input"
        autocomplete="off"
        spellcheck="false"
      />
    </div>

    <div class="min-h-0 flex-1 overflow-y-auto py-1">
      {#if items.length === 0}
        <p class="px-3 py-4 text-center text-xs text-zinc-500">
          {query.trim() ? "No matches" : "Type to search"}
        </p>
      {:else}
        {#if connMatches.length > 0}
          <p
            class="px-3 pt-1 pb-0.5 text-[10px] font-semibold tracking-wide text-zinc-500 uppercase"
          >
            Connections
          </p>
          {#each connMatches as conn, i (conn.name)}
            <button
              class="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs {activeIndex ===
              i
                ? 'bg-zinc-700/60'
                : 'hover:bg-zinc-800'}"
              data-annotate="search-result-connection-{conn.name}"
              onmouseenter={() => (activeIndex = i)}
              onclick={() => activate({ kind: "connection", name: conn.name, description: conn.description })}
            >
              <i class="bi bi-hdd-network text-zinc-400" aria-hidden="true"></i>
              <span class="text-zinc-100">{conn.name}</span>
              {#if conn.description}
                <span class="truncate text-zinc-500">{conn.description}</span>
              {/if}
            </button>
          {/each}
        {/if}

        {#if fileHits.length > 0}
          <p
            class="px-3 pt-1.5 pb-0.5 text-[10px] font-semibold tracking-wide text-zinc-500 uppercase"
          >
            Files{appStore.selectedConnection
              ? ` · ${appStore.selectedConnection}`
              : ""}
          </p>
          {#each fileHits as hit, j (hit.file_name)}
            {@const idx = connMatches.length + j}
            <button
              class="flex w-full flex-col gap-0.5 px-3 py-1.5 text-left text-xs {activeIndex ===
              idx
                ? 'bg-zinc-700/60'
                : 'hover:bg-zinc-800'}"
              data-annotate="search-result-file-{hit.file_name}"
              onmouseenter={() => (activeIndex = idx)}
              onclick={() => activate({ kind: "file", hit })}
            >
              <span class="flex items-center gap-2">
                <i class="bi bi-file-earmark-code text-zinc-400" aria-hidden="true"
                ></i>
                <span class="text-zinc-100">{hit.file_name}</span>
              </span>
              {#if hit.content_preview}
                <span class="truncate pl-5 font-mono text-[11px] text-zinc-500">
                  {hit.content_preview}
                </span>
              {/if}
            </button>
          {/each}
        {/if}

        {#if tableMatches.length > 0}
          <p
            class="px-3 pt-1.5 pb-0.5 text-[10px] font-semibold tracking-wide text-zinc-500 uppercase"
          >
            Tables{appStore.selectedConnection
              ? ` · ${appStore.selectedConnection}`
              : ""}{appStore.activeSchema ? ` · ${appStore.activeSchema}` : ""}
          </p>
          {#each tableMatches as table, k (table.qualified_name)}
            {@const idx = connMatches.length + fileHits.length + k}
            <button
              class="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs {activeIndex ===
              idx
                ? 'bg-zinc-700/60'
                : 'hover:bg-zinc-800'}"
              title="Insert the name into the editor and show it in TABLES"
              data-annotate="search-result-table-{table.qualified_name}"
              onmouseenter={() => (activeIndex = idx)}
              onclick={() => activate({ kind: "table", table })}
            >
              <i class="bi bi-table text-zinc-400" aria-hidden="true"></i>
              <span class="text-zinc-100">{table.qualified_name}</span>
              {#if table.kind !== "table"}
                <span
                  class="shrink-0 rounded bg-purple-500/15 px-1 text-[9px] uppercase tracking-wide text-purple-400"
                >
                  {table.kind}
                </span>
              {/if}
            </button>
          {/each}
        {/if}
      {/if}
    </div>

    <div
      class="flex items-center gap-3 border-t border-zinc-800 px-3 py-1 text-[10px] text-zinc-600"
    >
      <span><kbd>↑</kbd> <kbd>↓</kbd> navigate</span>
      <span><kbd>↵</kbd> open / insert</span>
      <span><kbd>esc</kbd> close</span>
    </div>
  </div>
</div>
