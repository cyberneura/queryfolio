<script lang="ts">
  import * as api from "$lib/api";
  import type { ColumnInfo, TableInfo } from "$lib/api";
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    /// Switch to the FILES / HISTORY tab (tab state is held by +page.svelte)
    onShowFiles: () => void;
    onShowHistory: () => void;
  }

  let { onShowFiles, onShowHistory }: Props = $props();

  /// Debounce time that coalesces consecutive connection / schema changes
  const RELOAD_DEBOUNCE_MS = 150;
  /// Time to distinguish a single click (insert name) from a double click (insert SELECT).
  /// If shorter than the OS double-click interval (about 500ms by default on macOS),
  /// a double click would insert both the name and the SELECT,
  /// so use a value with some margin
  const CLICK_DELAY_MS = 500;

  /// Column fetch state of expanded tables (key is qualified_name)
  interface ExpandedEntry {
    /// null until fetching completes (shows Loading)
    columns: ColumnInfo[] | null;
    error: string | null;
  }

  let tables = $state<TableInfo[]>([]);
  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let expanded = $state<Record<string, ExpandedEntry>>({});

  let clickTimer: ReturnType<typeof setTimeout> | null = null;

  /// Generation number of the running load. Used so that only the result of the latest load
  /// is applied even if an old response resolves later, during consecutive connection /
  /// schema switches or repeated reloads
  let loadGeneration = 0;

  const toErrorMessage = (e: unknown): string =>
    typeof e === "string" ? e : String(e);

  const load = async (connection: string | null, refresh = false) => {
    const generation = ++loadGeneration;
    // When the connection or schema changes, the expanded state loses meaning, so reset it
    expanded = {};
    if (!connection) {
      tables = [];
      loadError = null;
      return;
    }
    loading = true;
    try {
      const result = await api.listTables(connection, refresh);
      // If a newer load has started, discard the old response
      if (generation !== loadGeneration) {
        return;
      }
      tables = result;
      loadError = null;
      // Opening the schema browser = a moment to use the connection. The tunnel is already
      // open from listTables, so also load the schema list and completion map now
      // (not fetched at connection selection time, since we do not connect on selection alone).
      void appStore.ensureConnectionResources(connection);
    } catch (e) {
      if (generation !== loadGeneration) {
        return;
      }
      loadError = toErrorMessage(e);
      tables = [];
    } finally {
      if (generation === loadGeneration) {
        loading = false;
      }
    }
  };

  /// Reload button: discards the cache and re-fetches the table list, and also
  /// updates the schema map for SQL completion (the refresh of list_tables also discards
  /// the backend column cache, so the re-fetch reflects it)
  const reload = async () => {
    await load(appStore.selectedConnection, true);
    void appStore.loadSchemaMap();
  };

  // Reload when the connection or active schema changes (also runs on first mount).
  // The tree update after a schema switch (changeActiveSchema) is also done via this subscription.
  // activeSchema changes several times right after a connection is selected, so debounce.
  $effect(() => {
    const connection = appStore.selectedConnection;
    void appStore.activeSchema;
    const timer = setTimeout(() => {
      void load(connection);
    }, RELOAD_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  });

  /// Lazy loading of tree expansion: fetch columns for the first time when expanded
  const toggleExpand = async (table: TableInfo) => {
    const key = table.qualified_name;
    const connection = appStore.selectedConnection;
    if (expanded[key]) {
      delete expanded[key];
      return;
    }
    if (!connection) {
      return;
    }
    expanded[key] = { columns: null, error: null };
    try {
      const columns = await api.listColumns(connection, key);
      // Do not apply if it was collapsed or the connection changed while fetching
      if (expanded[key] && appStore.selectedConnection === connection) {
        expanded[key] = { columns, error: null };
      }
    } catch (e) {
      if (expanded[key] && appStore.selectedConnection === connection) {
        expanded[key] = { columns: null, error: toErrorMessage(e) };
      }
    }
  };

  /// Single click: insert the table name into the editor.
  /// Wait a little before committing to distinguish it from a double click.
  const onTableClick = (table: TableInfo, event: MouseEvent) => {
    // Do not re-arm the timer on the 2nd click of a double click (detail > 1)
    // (the dblclick handler right after cancels the 1st click's timer and handles it)
    if (event.detail > 1) {
      return;
    }
    if (clickTimer) {
      clearTimeout(clickTimer);
    }
    clickTimer = setTimeout(() => {
      clickTimer = null;
      appStore.insertSqlSnippet(table.qualified_name);
    }, CLICK_DELAY_MS);
  };

  /// Double click: insert a query snippet into the editor (does not run it).
  /// The snippet matches the editor language / engine (a search request block for es,
  /// PartiQL without a LIMIT clause for dynamodb, TOP for mssql, and a SELECT statement
  /// with LIMIT otherwise)
  const onTableDblClick = (table: TableInfo) => {
    if (clickTimer) {
      clearTimeout(clickTimer);
      clickTimer = null;
    }
    if (appStore.selectedCapabilities?.editor_language === "es") {
      appStore.insertSqlSnippet(
        `GET /${table.qualified_name}/_search\n` +
          `{\n  "query": { "match_all": {} },\n  "size": 100\n}`,
      );
      return;
    }
    const engine = appStore.connections
      .find((c) => c.name === appStore.selectedConnection)
      ?.engine.toLowerCase();
    // Case-insensitive, same as the backend's parse_engine
    if (engine === "dynamodb") {
      // PartiQL has no LIMIT clause (the row count is limited by the backend's max_rows).
      // Table names may contain hyphens etc., so wrap them in double quotes
      appStore.insertSqlSnippet(`SELECT * FROM "${table.qualified_name}";`);
      return;
    }
    if (engine === "mssql" || engine === "sqlserver") {
      // T-SQL has no LIMIT clause (use TOP). The backend (qualified_name in engines/mssql.rs)
      // returns names containing spaces or dots wrapped in square brackets,
      // so they can be embedded as is
      appStore.insertSqlSnippet(`SELECT TOP 100 * FROM ${table.qualified_name};`);
      return;
    }
    appStore.insertSqlSnippet(
      `SELECT * FROM ${table.qualified_name} LIMIT 100;`,
    );
  };
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
    <button
      class="text-xs font-semibold tracking-wide text-zinc-600 hover:text-zinc-300"
      title="Show query history"
      data-annotate="tab-history"
      onclick={onShowHistory}
    >
      HISTORY
    </button>
    <span class="text-xs font-semibold tracking-wide text-zinc-400">TABLES</span>
    <button
      class="ml-auto rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-700 hover:text-zinc-200 disabled:opacity-40"
      title="Reload tables"
      aria-label="Reload tables"
      data-annotate="button-reload-tables"
      disabled={!appStore.selectedConnection || loading}
      onclick={() => void reload()}
    >
      <i class="bi bi-arrow-clockwise" aria-hidden="true"></i>
    </button>
  </div>
  <div class="min-h-0 flex-1 overflow-y-auto">
    {#if !appStore.selectedConnection}
      <p class="px-3 py-2 text-xs text-zinc-500">Select a connection</p>
    {:else if loadError}
      <p class="px-3 py-2 text-xs text-red-400">{loadError}</p>
    {:else if tables.length === 0}
      <p class="px-3 py-2 text-xs text-zinc-500">
        {loading ? "Loading..." : "No tables found"}
      </p>
    {:else}
      {#each tables as table (table.qualified_name)}
        <div class="group flex items-center hover:bg-zinc-800">
          <button
            class="shrink-0 px-1.5 py-1 text-[10px] text-zinc-500 hover:text-zinc-200"
            title={expanded[table.qualified_name]
              ? "Collapse columns"
              : "Expand columns"}
            aria-label={expanded[table.qualified_name]
              ? "Collapse columns"
              : "Expand columns"}
            data-annotate="button-toggle-table-{table.qualified_name}"
            onclick={() => void toggleExpand(table)}
          >
            <i
              class="bi {expanded[table.qualified_name]
                ? 'bi-chevron-down'
                : 'bi-chevron-right'}"
              aria-hidden="true"
            ></i>
          </button>
          <button
            class="flex min-w-0 flex-1 items-center gap-1 py-1 pr-2 text-left text-sm text-zinc-200"
            title="Click: insert the name / Double-click: insert a query snippet"
            data-annotate="button-table-{table.qualified_name}"
            onclick={(e) => onTableClick(table, e)}
            ondblclick={() => onTableDblClick(table)}
          >
            <span class="truncate">{table.qualified_name}</span>
            {#if table.kind !== "table"}
              <span
                class="shrink-0 rounded bg-purple-500/15 px-1 text-[9px] uppercase tracking-wide text-purple-400"
              >
                {table.kind}
              </span>
            {/if}
          </button>
        </div>
        {#if expanded[table.qualified_name]}
          {@const entry = expanded[table.qualified_name]}
          {#if entry.error}
            <p class="py-0.5 pl-6 pr-2 text-xs text-red-400">{entry.error}</p>
          {:else if !entry.columns}
            <p class="py-0.5 pl-6 pr-2 text-xs text-zinc-500">Loading...</p>
          {:else}
            {#each entry.columns as column (column.name)}
              <div
                class="flex items-baseline gap-1 py-0.5 pl-6 pr-2 text-xs"
                data-annotate="row-column-{table.qualified_name}-{column.name}"
              >
                <span class="truncate text-zinc-300">{column.name}</span>
                <span class="ml-auto shrink-0 text-[10px] text-zinc-500">
                  {column.data_type}{column.nullable ? "" : " NOT NULL"}
                </span>
              </div>
            {/each}
          {/if}
        {/if}
      {/each}
    {/if}
  </div>
</div>
