<script lang="ts">
  import * as api from "$lib/api";
  import type { QueryHistoryEntry } from "$lib/api";
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    /// Switch to the FILES / TABLES tab (+page.svelte owns the tab state)
    onShowFiles: () => void;
    onShowTables: () => void;
  }

  let { onShowFiles, onShowTables }: Props = $props();

  /// Debounce time for incremental search
  const SEARCH_DEBOUNCE_MS = 250;
  /// Number of history entries fetched at once
  const FETCH_LIMIT = 200;

  let search = $state("");
  let entries = $state<QueryHistoryEntry[]>([]);
  let loading = $state(false);
  let loadError = $state<string | null>(null);

  const load = async (connection: string | null, searchText: string) => {
    if (!connection) {
      entries = [];
      loadError = null;
      return;
    }
    loading = true;
    try {
      entries = await api.listQueryHistory(
        connection,
        searchText || undefined,
        FETCH_LIMIT,
      );
      loadError = null;
    } catch (e) {
      loadError = typeof e === "string" ? e : String(e);
      entries = [];
    } finally {
      loading = false;
    }
  };

  // Reload when the connection or search term changes (also runs on first mount).
  // Also subscribe to query execution completion (changes in running) to pick up the history just after execution.
  // Search is incremental, so debounce it to limit the number of calls.
  $effect(() => {
    const connection = appStore.selectedConnection;
    const searchText = search.trim();
    void appStore.running;
    const timer = setTimeout(() => {
      void load(connection, searchText);
    }, SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  });

  /// Turn a history timestamp (ISO 8601) into a short local format
  const formatTime = (iso: string): string => {
    const date = new Date(iso);
    if (Number.isNaN(date.getTime())) {
      return iso;
    }
    const pad = (n: number) => String(n).padStart(2, "0");
    return (
      `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
      ` ${pad(date.getHours())}:${pad(date.getMinutes())}`
    );
  };

  /// Return the first line of the SQL for list display
  const firstLine = (sql: string): string => sql.trimStart().split("\n")[0];
</script>

<div class="flex h-full w-full flex-col border-r border-zinc-700 bg-zinc-900">
  <div class="flex items-center gap-2 border-b border-zinc-700 px-3 py-2">
    <button
      class="text-xs font-semibold tracking-wide text-zinc-600 hover:text-zinc-300"
      title="Show query files"
      data-annotate="tab-files"
      onclick={onShowFiles}
    >
      FILES
    </button>
    <span class="text-xs font-semibold tracking-wide text-zinc-400">HISTORY</span>
    <!-- Engines with no table concept (redis etc.) do not show TABLES -->
    {#if appStore.selectedCapabilities?.supports_tables ?? true}
      <button
        class="text-xs font-semibold tracking-wide text-zinc-600 hover:text-zinc-300"
        title="Show tables"
        data-annotate="tab-tables"
        onclick={onShowTables}
      >
        TABLES
      </button>
    {/if}
    <button
      class="ml-auto rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-700 hover:text-zinc-200 disabled:opacity-40"
      title="Reload history"
      aria-label="Reload history"
      data-annotate="button-reload-history"
      disabled={!appStore.selectedConnection || loading}
      onclick={() => void load(appStore.selectedConnection, search.trim())}
    >
      <i class="bi bi-arrow-clockwise" aria-hidden="true"></i>
    </button>
  </div>
  <div class="border-b border-zinc-700 px-2 py-1.5">
    <input
      class="w-full rounded border border-zinc-600 bg-zinc-800 px-1.5 py-0.5 text-xs text-zinc-200 outline-none focus:border-blue-400"
      placeholder="Search history"
      data-annotate="input-history-search"
      bind:value={search}
    />
  </div>
  <div class="min-h-0 flex-1 overflow-y-auto">
    {#if !appStore.selectedConnection}
      <p class="px-3 py-2 text-xs text-zinc-500">Select a connection</p>
    {:else if loadError}
      <p class="px-3 py-2 text-xs text-red-400">{loadError}</p>
    {:else if entries.length === 0}
      <p class="px-3 py-2 text-xs text-zinc-500">
        {loading
          ? "Loading..."
          : search.trim()
            ? "No matching history"
            : "No query history yet"}
      </p>
    {:else}
      {#each entries as entry, index (index)}
        <button
          class="block w-full border-b border-zinc-800 px-3 py-1.5 text-left hover:bg-zinc-800"
          title={entry.sql}
          data-annotate="button-history-entry-{index}"
          onclick={() => appStore.insertSqlSnippet(entry.sql)}
        >
          <span class="flex items-center gap-1 text-[10px] text-zinc-500">
            <span
              class={entry.success ? "text-green-500" : "text-red-500"}
              title={entry.success ? "Succeeded" : "Failed"}
            >
              ●
            </span>
            <span>{formatTime(entry.time)}</span>
            <span class="ml-auto">
              {entry.row_count !== null ? `${entry.row_count} rows` : "error"}
            </span>
          </span>
          <span class="block truncate text-xs text-zinc-300">
            {firstLine(entry.sql)}
          </span>
        </button>
      {/each}
    {/if}
  </div>
</div>
