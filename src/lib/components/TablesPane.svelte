<script lang="ts">
  import * as api from "$lib/api";
  import type { ColumnInfo, TableInfo } from "$lib/api";
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    /// FILES / HISTORY タブへの切り替え (タブ状態は +page.svelte が持つ)
    onShowFiles: () => void;
    onShowHistory: () => void;
  }

  let { onShowFiles, onShowHistory }: Props = $props();

  /// 接続・スキーマ切替の連続変化をまとめるデバウンス時間
  const RELOAD_DEBOUNCE_MS = 150;
  /// シングルクリック (名前挿入) とダブルクリック (SELECT 挿入) の判別時間。
  /// OS のダブルクリック間隔 (macOS デフォルト約 500ms) より短いと
  /// ダブルクリック時に名前と SELECT の両方が挿入されてしまうため、
  /// 余裕を持たせた値にする
  const CLICK_DELAY_MS = 500;

  /// 展開中テーブルのカラム取得状態 (キーは qualified_name)
  interface ExpandedEntry {
    /// 取得完了までは null (Loading 表示)
    columns: ColumnInfo[] | null;
    error: string | null;
  }

  let tables = $state<TableInfo[]>([]);
  let loading = $state(false);
  let loadError = $state<string | null>(null);
  let expanded = $state<Record<string, ExpandedEntry>>({});

  let clickTimer: ReturnType<typeof setTimeout> | null = null;

  /// 実行中の load の世代番号。接続・スキーマの連続切替やリロード連打で
  /// 古い応答が後から解決しても、最新の load の結果だけを反映するために使う
  let loadGeneration = 0;

  const toErrorMessage = (e: unknown): string =>
    typeof e === "string" ? e : String(e);

  const load = async (connection: string | null, refresh = false) => {
    const generation = ++loadGeneration;
    // 接続やスキーマが変わったら展開状態は意味を失うためリセットする
    expanded = {};
    if (!connection) {
      tables = [];
      loadError = null;
      return;
    }
    loading = true;
    try {
      const result = await api.listTables(connection, refresh);
      // より新しい load が始まっていたら、古い応答は捨てる
      if (generation !== loadGeneration) {
        return;
      }
      tables = result;
      loadError = null;
      // スキーマブラウザを開いた = 接続を使う契機。listTables で既にトンネルは
      // 開いているので、この機会にスキーマ一覧・補完マップも取り込んでおく
      // (接続選択だけでは張らない方針のため、選択時点では未取得)。
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

  /// リロードボタン: キャッシュを破棄してテーブル一覧を再取得し、
  /// SQL 補完用のスキーママップも追従させる (list_tables の refresh が
  /// バックエンドのカラムキャッシュも破棄するため、再取得で反映される)
  const reload = async () => {
    await load(appStore.selectedConnection, true);
    void appStore.loadSchemaMap();
  };

  // 接続・アクティブスキーマの変化で再読込する (初回マウント時も走る)。
  // スキーマ切替 (changeActiveSchema) 後のツリー更新もこの購読で行われる。
  // 接続選択直後は activeSchema が複数回変化するためデバウンスする。
  $effect(() => {
    const connection = appStore.selectedConnection;
    void appStore.activeSchema;
    const timer = setTimeout(() => {
      void load(connection);
    }, RELOAD_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  });

  /// ツリー展開の遅延ロード: 展開時に初めてカラムを取得する
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
      // 取得中に折りたたまれた・接続が変わった場合は反映しない
      if (expanded[key] && appStore.selectedConnection === connection) {
        expanded[key] = { columns, error: null };
      }
    } catch (e) {
      if (expanded[key] && appStore.selectedConnection === connection) {
        expanded[key] = { columns: null, error: toErrorMessage(e) };
      }
    }
  };

  /// シングルクリック: テーブル名をエディタに挿入。
  /// ダブルクリックと区別するため少し待ってから確定する。
  const onTableClick = (table: TableInfo, event: MouseEvent) => {
    // ダブルクリックの 2 打目 (detail > 1) ではタイマーを張り直さない
    // (直後の dblclick ハンドラが 1 打目のタイマーを取り消して処理する)
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

  /// ダブルクリック: クエリスニペットをエディタに挿入する (実行はしない)。
  /// スニペットはエディタ言語・エンジンに合わせる (es は検索リクエスト
  /// ブロック、dynamodb は LIMIT 句の無い PartiQL、それ以外は SELECT 文)
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
    // バックエンドの parse_engine と同じく大文字小文字を区別しない
    if (engine === "dynamodb") {
      // PartiQL に LIMIT 句は無い (行数はバックエンドの max_rows で抑える)。
      // テーブル名はハイフン等を含み得るためダブルクォートで括る
      appStore.insertSqlSnippet(`SELECT * FROM "${table.qualified_name}";`);
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
