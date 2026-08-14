<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { toast } from "svelte-sonner";
  import { ensureConfigFile, frontendReady } from "$lib/api";
  import type { OpenTarget } from "$lib/api";
  import {
    RUN_LOG_CONFIRM_ROWS,
    formatRunLogBlock,
    formatRunLogTimestamp,
    runLogBody,
  } from "$lib/runLog";
  import type { RunTarget } from "$lib/runLog";
  import appStore from "$lib/stores/app.svelte";
  import Toolbar from "$lib/components/Toolbar.svelte";
  import EditorToolbar from "$lib/components/EditorToolbar.svelte";
  import ConnectionsPane from "$lib/components/ConnectionsPane.svelte";
  import FilesPane from "$lib/components/FilesPane.svelte";
  import HistoryPane from "$lib/components/HistoryPane.svelte";
  import TablesPane from "$lib/components/TablesPane.svelte";
  import SqlEditor from "$lib/components/SqlEditor.svelte";
  import EditorTabs from "$lib/components/EditorTabs.svelte";
  import ReplaceMultilinePane from "$lib/components/ReplaceMultilinePane.svelte";
  import ResultsPane from "$lib/components/ResultsPane.svelte";
  import ConfigInfoModal from "$lib/components/ConfigInfoModal.svelte";
  import ConfigEditorModal from "$lib/components/ConfigEditorModal.svelte";
  import AiAnalysisModal from "$lib/components/AiAnalysisModal.svelte";
  import DangerousConfirmModal from "$lib/components/DangerousConfirmModal.svelte";
  import RunLogConfirmModal from "$lib/components/RunLogConfirmModal.svelte";
  import SearchModal from "$lib/components/SearchModal.svelte";
  import ChatPane from "$lib/components/ChatPane.svelte";
  import HelpPane from "$lib/components/HelpPane.svelte";
  import PaneDivider from "$lib/components/PaneDivider.svelte";

  let showSettings = $state(false);
  let showSearch = $state(false);
  /// 設定エディタ。null = 閉じている
  let configEditorMode = $state<"config" | "source" | null>(null);
  /// 設定エディタに未保存の変更があるか (モード切替で巻き添え破棄しないため)
  let configEditorDirty = $state(false);

  /// メニューから設定エディタを開く。表示中のエディタに未保存の変更がある状態で
  /// 別のモードへ切り替えると #key による作り直しで編集が消えるため、それを断る。
  function openConfigEditor(mode: "config" | "source") {
    if (configEditorMode !== null && configEditorMode !== mode && configEditorDirty) {
      // source モードには Save が無いため、できない操作を案内しないよう文言を分ける
      toast.warning(
        configEditorMode === "config"
          ? "Save or discard your changes first"
          : "Discard your edits first (they cannot be saved)",
      );
      return;
    }
    configEditorMode = mode;
  }

  /// モーダルを 1 つでも開いているか (キーボードはモーダルのものとみなす)。
  /// aiAnalysis (EXPLAIN の AI 解説) はこのファイルではなく ResultsPane が
  /// 描画するが、画面を覆うのは同じなのでここで見る。
  const isModalOpen = () =>
    showSearch ||
    showSettings ||
    configEditorMode !== null ||
    appStore.aiAnalysis !== null ||
    appStore.aiExplanation !== null ||
    appStore.dangerousConfirmReason !== null ||
    runLogConfirm !== null;

  /// グローバルショートカット。
  /// - Cmd+K (mac) / Ctrl+K で検索モーダルを開閉する
  /// - Ctrl+Tab / Ctrl+Shift+Tab でエディタタブを履歴順に切り替える
  function handleGlobalKeydown(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      showSearch = !showSearch;
      return;
    }
    // Ctrl+Tab は Ctrl 単独の時だけ拾う (Cmd+Tab は OS のアプリ切替、
    // Alt+Tab は Windows のウインドウ切替なので、修飾が増えたら手を出さない)。
    // preventDefault が要る: 既定はフォーカス移動で、押すたびにフォーカスが
    // エディタから外れてしまう。
    // モーダルが開いている間は無視する (見えない裏でタブが移り、閉じたら別の
    // ファイルになっている、という状態を作らない)。
    if (e.key === "Tab" && e.ctrlKey && !e.metaKey && !e.altKey && !isModalOpen()) {
      e.preventDefault();
      void appStore.cycleEditorTab(e.shiftKey ? -1 : 1);
    }
  }

  /// Ctrl を離したら巡回を終える (そこで初めて、選んだタブが履歴の先頭になる)。
  function handleGlobalKeyup(e: KeyboardEvent) {
    if (e.key === "Control") {
      void appStore.endEditorTabCycle();
    }
  }

  /// ウインドウがフォーカスを失うと Ctrl の keyup は届かない (Cmd+Tab で
  /// アプリを切り替えた時など)。巡回状態を持ち越すと、次に Ctrl+Tab を押した時に
  /// 古い巡回の続きから進んでしまうので、ここでも終わらせる。
  function handleWindowBlur() {
    void appStore.endEditorTabCycle();
  }
  let editor: SqlEditor | undefined = $state();

  /// 大量の行をエディタへ書き戻す前の確認ダイアログ。null = 出していない
  let runLogConfirm = $state<{
    rows: number;
    resolve: (ok: boolean) => void;
  } | null>(null);

  /// 書き戻してよいかを尋ね、応答 (true = 書く) を待つ。
  /// 未応答のものが残っていれば却下してから差し替える (危険文の確認と同じ)
  const confirmRunLog = (rows: number): Promise<boolean> =>
    new Promise((resolve) => {
      runLogConfirm?.resolve(false);
      runLogConfirm = { rows, resolve };
    });

  function resolveRunLogConfirm(ok: boolean) {
    const pending = runLogConfirm;
    runLogConfirm = null;
    pending?.resolve(ok);
  }

  /// エディタからの実行。`-- 📝 <label>` が付いた文は、実行後にその下へ
  /// 結果を TSV のブロックコメント (Run and Log) として書き戻す。
  /// 結果テーブルへの表示は書き戻しの有無に関わらず通常どおり行われる。
  async function runStatement(target: RunTarget) {
    // 実行を開始したエディタタブを控える。SqlEditor は
    // {#key appStore.activeEditorTabId} でタブ切替のたびに作り直されるため、
    // editor 参照は常に「今開いているファイル」を指す。target の範囲照合だけ
    // では、たまたま同じ位置に同じ SQL がある別ファイルへ書いてしまう
    const tabId = appStore.activeEditorTabId;
    // アクティブスキーマ (database) も控える。実行中に Database 欄を
    // 切り替えられると、タブも SQL も変わらないまま切替前のスキーマの結果が
    // ファイルに残り、後から読んだ人には現在のスキーマの結果に見える
    const schema = appStore.activeSchema;
    const result = await appStore.runQuery(target.sql);
    if (!result || target.logLabel === null) {
      return;
    }
    // 見出しに入れるのは実行が終わった時刻。下の確認ダイアログを開いたまま
    // にされると承認した時刻になってしまうので、待つ前に採る
    const executedAt = formatRunLogTimestamp(new Date());
    // 大量の行はエディタを埋めてしまうので、書き戻す前に確認する
    if (
      result.rows.length >= RUN_LOG_CONFIRM_ROWS &&
      !(await confirmRunLog(result.rows.length))
    ) {
      return;
    }
    // ラベルは書き戻す直前に本文から取り直したものを使う (SqlEditor が渡す)
    const buildBlock = (label: string) =>
      formatRunLogBlock(label, executedAt, runLogBody(result));
    // `\c` / `USE` は実行そのものが切替なので、その文が切り替えた先は
    // 「変わっていない」とみなす (この結果は切替後のスキーマのもの)
    const expectedSchema = result.switched_schema ?? schema;
    const outcome =
      appStore.activeEditorTabId === tabId &&
      appStore.activeSchema === expectedSchema
        ? (editor?.writeRunLog(target, buildBlock) ?? "stale")
        : "stale";
    switch (outcome) {
      case "stale":
        // 実行中に編集・タブ切替が起きて対象がズレた場合。無関係な位置へ
        // 書き込むより、書かずに知らせる方が安全
        toast.warning(
          "The editor changed while the query was running — the log was not written.",
        );
        break;
      case "unmarked":
        // 実行中にマーカーを消した = 書き戻しの取り消しなので黙って従う
        break;
      case "broken":
        toast.warning(
          "The existing log block is missing its closing */ — the log was not written.",
        );
        break;
    }
  }

  // Replace Multiline: エディタの複数行選択状態と、右側の置換ペインの表示。
  // ペインを開いた時点の選択範囲を snapshot し、差し込み時に範囲がズレて
  // いないか照合してから置換する (ファイル切替・編集での誤挿入を防ぐ)
  let hasMultilineSelection = $state(false);
  let showReplacePane = $state(false);
  let replaceInitialLines = $state("");
  let replaceSnapshot: { from: number; to: number; text: string } | null = null;
  // ペインを開き直すたびに増やし、#key で再マウントして Lines を作り直す
  let replaceOpenToken = $state(0);

  function openReplacePane() {
    const snap = editor?.getMainSelection();
    if (!snap) {
      return;
    }
    replaceSnapshot = snap;
    replaceInitialLines = snap.text;
    replaceOpenToken += 1;
    showReplacePane = true;
  }

  function applyReplace(result: string) {
    const snap = replaceSnapshot;
    const ok =
      snap != null &&
      (editor?.replaceRangeIfMatches(
        snap.from,
        snap.to,
        snap.text,
        result,
      ) ??
        false);
    if (ok) {
      showReplacePane = false;
    } else {
      // 選択範囲がズレた (ファイル切替や編集) 場合は破壊せずに知らせる
      toast.error("The editor selection changed — nothing was replaced.", {
        description: "Use Copy to grab the result instead.",
      });
    }
  }

  // タブ切替・クローズで選択追跡状態と置換ペインをリセットする。
  // 依存はアクティブタブ ID にする: 同名ファイルを別接続で開いている場合、
  // selectedFile (ファイル名) は変わらないままタブだけ切り替わり得るため、
  // selectedFile 依存だと stale なスナップショットが残り新タブへ誤適用される
  $effect(() => {
    void appStore.activeEditorTabId;
    hasMultilineSelection = false;
    showReplacePane = false;
    replaceSnapshot = null;
  });
  /// 左ペイン 2 列目のタブ (クエリファイル一覧 / クエリ履歴 / テーブル一覧)
  let leftPaneTab = $state<"files" | "history" | "tables">("files");

  // ペインのレイアウト。区切り線のドラッグで変更し localStorage に保存する
  const LAYOUT_PREFIX = "queryfolio.layout.";
  const SIDEBAR_MIN = 140;
  const SIDEBAR_MAX = 500;
  /// AI チャットペインは本文が長いので、サイドバーより広い範囲を許す
  const CHAT_MIN = 240;
  const CHAT_MAX = 720;
  const HELP_MIN = 260;
  const HELP_MAX = 720;
  const EDITOR_FRAC_MIN = 0.15;
  const EDITOR_FRAC_MAX = 0.85;

  function loadLayoutValue(key: string, fallback: number): number {
    try {
      const raw = localStorage.getItem(LAYOUT_PREFIX + key);
      if (raw === null) return fallback;
      const n = Number(raw);
      return Number.isFinite(n) ? n : fallback;
    } catch {
      return fallback;
    }
  }

  function saveLayoutValue(key: string, value: number) {
    try {
      localStorage.setItem(LAYOUT_PREFIX + key, String(value));
    } catch {
      // localStorage が使えなくてもレイアウト変更自体は機能させる
    }
  }

  function clamp(value: number, min: number, max: number): number {
    return Math.min(max, Math.max(min, value));
  }

  /// 接続一覧ペインの幅 (px)。デフォルトは従来の w-56 = 224px
  let connectionsWidth = $state(
    clamp(loadLayoutValue("connectionsWidth", 224), SIDEBAR_MIN, SIDEBAR_MAX),
  );
  /// 2 列目 (Files / History / Tables) ペインの幅 (px)
  let sidebarWidth = $state(
    clamp(loadLayoutValue("sidebarWidth", 224), SIDEBAR_MIN, SIDEBAR_MAX),
  );
  /// エディタが占める縦の割合。デフォルトは従来の flex 3:2 = 0.6
  let editorFrac = $state(
    clamp(loadLayoutValue("editorFrac", 0.6), EDITOR_FRAC_MIN, EDITOR_FRAC_MAX),
  );
  // editorFrac の px 換算用。列全体 (ツールバー込み) ではなく
  // 分割対象 2 ペインの実高さの合計を使うと、ドラッグがカーソルに正確に追従する
  let editorPaneEl: HTMLDivElement | undefined = $state();
  let resultsPaneEl: HTMLDivElement | undefined = $state();

  /// AI チャットペイン (右) の幅 (px)
  let chatWidth = $state(
    clamp(loadLayoutValue("chatWidth", 360), CHAT_MIN, CHAT_MAX),
  );
  /// AI チャットペインを開いているか (表示状態も次回起動へ引き継ぐ)
  let showChat = $state(loadLayoutValue("chatOpen", 0) === 1);
  let helpWidth = $state(
    clamp(loadLayoutValue("helpWidth", 380), HELP_MIN, HELP_MAX),
  );
  let showHelp = $state(loadLayoutValue("helpOpen", 0) === 1);

  // ドラッグ開始時の基準サイズ。PaneDivider は開始位置からの累積 delta を
  // 渡すので、基準 + delta で計算するとクランプ飽和後もポインタと同期する
  let dragBaseConnections = 0;
  let dragBaseSidebar = 0;
  let dragBaseEditorFrac = 0;
  let dragBaseChat = 0;
  let dragBaseHelp = 0;

  const selectedConnectionInfo = $derived(
    appStore.connections.find((c) => c.name === appStore.selectedConnection) ??
      null,
  );
  const selectedEngine = $derived(selectedConnectionInfo?.engine ?? null);
  const selectedCapabilities = $derived(
    selectedConnectionInfo?.capabilities ?? null,
  );

  // TABLES ペインを開いたままテーブル非対応エンジン (redis 等) の接続へ
  // 切り替えた場合は FILES へ戻す (TablesPane が listTables を呼ばないように)
  $effect(() => {
    if (
      leftPaneTab === "tables" &&
      selectedCapabilities &&
      !selectedCapabilities.supports_tables
    ) {
      leftPaneTab = "files";
    }
  });

  onMount(() => {
    // 開いているクエリファイルが外部で変更されたら自動リロード / マージする
    appStore.startFileWatcher();

    // メニューの Reload config file からの通知を受けて再読込する
    const unlistenPromise = listen("menu-reload-config", async () => {
      if (await appStore.reloadConnections()) {
        toast.success("Config reloaded");
      } else {
        toast.error("Failed to reload the config", {
          description: appStore.errorMessage ?? undefined,
        });
      }
    });

    // メニューの Edit config.yml / View override config yaml からの通知
    const unlistenEditPromise = listen("menu-edit-config", () => {
      openConfigEditor("config");
    });
    const unlistenEditSourcePromise = listen("menu-view-override-config", () => {
      openConfigEditor("source");
    });

    // 開く指定を直列で処理するキュー。openFileByTarget は selectConnection を呼び、
    // ストアの世代ガードが後発の接続切替で先行分をキャンセルするため、複数を並行で
    // 走らせると別接続のファイルが黙って飛ばされ得る。Promise チェーンで 1 件ずつ
    // 順に開く (1 件の失敗でチェーンが止まらないよう catch する。個別の失敗は
    // openFileByTarget が errorMessage で表示する)。
    let openQueue: Promise<void> = Promise.resolve();
    const enqueueOpen = (connection: string, fileName: string) => {
      openQueue = openQueue
        .then(() => appStore.openFileByTarget(connection, fileName))
        .catch(() => {});
    };

    // 実行中に queryfolio:// deep link / CLI で開くよう要求された時の通知。
    // バックエンドが保存領域配下かを検証済みの接続 / ファイル名を届ける。
    // 1 イベントに複数 URL・近接した複数回起動でも直列に開く。
    const unlistenOpenFilePromise = listen<OpenTarget>(
      "open-query-file",
      (event) => {
        enqueueOpen(event.payload.connection, event.payload.fileName);
      },
    );
    const unlistenOpenFileErrPromise = listen<string>(
      "open-query-file-error",
      (event) => {
        toast.error("Failed to open the file", {
          description: event.payload,
        });
      },
    );

    void (async () => {
      // frontend_ready を呼ぶと backend が ready=true にしてイベント直送に切り替わる。
      // その前に open-query-file / -error の listener が実際に installed される
      // (listen の Promise が解決する) のを待たないと、間に届いた指定を取りこぼす。
      await unlistenOpenFilePromise;
      await unlistenOpenFileErrPromise;
      try {
        const createdPath = await ensureConfigFile();
        if (createdPath) {
          toast.info("Created a config file", {
            description: `Edit ${createdPath} to add your connections`,
          });
        }
      } catch (e) {
        toast.error("Failed to create the config file", {
          description: String(e),
        });
      }
      await appStore.loadConnections();
      // listener が installed 済みになったので frontend_ready を呼んで「準備完了」を
      // 知らせ、起動時指定 + 起動中に溜まった開く対象をまとめて受け取って開く。
      // 以降の指定は open-query-file イベントで直接届く (取りこぼさない)。
      try {
        const { targets, errors } = await frontendReady();
        // ライブイベントと同じキューに載せて直列に開く (ready 直後に届くライブ
        // イベントとの並行実行を避ける)。
        for (const target of targets) {
          enqueueOpen(target.connection, target.fileName);
        }
        // 起動時指定の解決に失敗した分はトーストで知らせる (GUI 起動では
        // stderr が見えず、握り潰すとユーザーの明示的な指定が無反応になる)。
        for (const message of errors) {
          toast.error("Failed to open the requested file", {
            description: message,
          });
        }
      } catch (e) {
        toast.error("Failed to open the requested file", {
          description: String(e),
        });
      }
    })();

    return () => {
      appStore.stopFileWatcher();
      void unlistenPromise.then((unlisten) => unlisten());
      void unlistenEditPromise.then((unlisten) => unlisten());
      void unlistenEditSourcePromise.then((unlisten) => unlisten());
      void unlistenOpenFilePromise.then((unlisten) => unlisten());
      void unlistenOpenFileErrPromise.then((unlisten) => unlisten());
    };
  });
</script>

<svelte:window
  onkeydown={handleGlobalKeydown}
  onkeyup={handleGlobalKeyup}
  onblur={handleWindowBlur}
/>

<!-- overflow-hidden: 内側のペインがはみ出しても、アプリの枠から外へ広げない
     (html/body 側の overflow: hidden と対で効かせる。CYBERNEURA-DEV-421) -->
<div class="flex h-screen flex-col overflow-hidden bg-zinc-950 text-zinc-200">
  <Toolbar
    onRunCurrent={() => editor?.runCurrentStatement()}
    onOpenSearch={() => {
      showSearch = true;
    }}
    onOpenSettings={() => {
      showSettings = true;
    }}
    helpOpen={showHelp}
    onToggleHelp={() => {
      showHelp = !showHelp;
      saveLayoutValue("helpOpen", showHelp ? 1 : 0);
    }}
    chatOpen={showChat}
    onToggleChat={() => {
      showChat = !showChat;
      saveLayoutValue("chatOpen", showChat ? 1 : 0);
    }}
  />

  <!-- overflow-x-auto: 接続一覧 / サイドバー / チャットは shrink-0 の固定幅なので、
       ウインドウを狭めると中央のエディタが 0 幅まで潰れた先で右端がはみ出す。
       ドキュメントをスクロールさせない代わりに、この行の中で横スクロールできるように
       しておかないと右側のペインへ到達できなくなる (CYBERNEURA-DEV-421)。
       縦は各ペインが内側にスクロール領域を持つので抑止する (片方だけ指定すると
       もう片方が auto に計算されるため、明示的に hidden を置く) -->
  <div class="flex min-h-0 flex-1 overflow-x-auto overflow-y-hidden">
    <div class="shrink-0" style="width: {connectionsWidth}px">
      <ConnectionsPane />
    </div>
    <PaneDivider
      direction="vertical"
      annotate="pane-divider-connections"
      onDragStart={() => {
        dragBaseConnections = connectionsWidth;
      }}
      onDrag={(delta) => {
        connectionsWidth = clamp(
          dragBaseConnections + delta,
          SIDEBAR_MIN,
          SIDEBAR_MAX,
        );
      }}
      onDragEnd={() => saveLayoutValue("connectionsWidth", connectionsWidth)}
    />
    <div class="shrink-0" style="width: {sidebarWidth}px">
      {#if leftPaneTab === "files"}
        <FilesPane
          onShowHistory={() => {
            leftPaneTab = "history";
          }}
          onShowTables={() => {
            leftPaneTab = "tables";
          }}
        />
      {:else if leftPaneTab === "history"}
        <HistoryPane
          onShowFiles={() => {
            leftPaneTab = "files";
          }}
          onShowTables={() => {
            leftPaneTab = "tables";
          }}
        />
      {:else}
        <TablesPane
          onShowFiles={() => {
            leftPaneTab = "files";
          }}
          onShowHistory={() => {
            leftPaneTab = "history";
          }}
        />
      {/if}
    </div>
    <PaneDivider
      direction="vertical"
      annotate="pane-divider-sidebar"
      onDragStart={() => {
        dragBaseSidebar = sidebarWidth;
      }}
      onDrag={(delta) => {
        sidebarWidth = clamp(dragBaseSidebar + delta, SIDEBAR_MIN, SIDEBAR_MAX);
      }}
      onDragEnd={() => saveLayoutValue("sidebarWidth", sidebarWidth)}
    />

    <div class="flex min-w-0 flex-1 flex-col">
      {#if appStore.selectedConnection}
        <EditorToolbar
          engine={selectedEngine}
          capabilities={selectedCapabilities}
          readonly={selectedConnectionInfo?.readonly ?? false}
          onExplain={() =>
            appStore.explainQuery(editor?.getCurrentStatement() ?? "")}
          onExplainSql={() =>
            appStore.explainSql(editor?.getCurrentStatement() ?? "")}
          onFormat={() => editor?.formatCurrentStatement()}
          showReplaceMultiline={hasMultilineSelection &&
            appStore.selectedFile !== null}
          onReplaceMultiline={openReplacePane}
        />
      {/if}
      <div
        class="flex min-h-0 basis-0 flex-col border-b border-zinc-700"
        style="flex-grow: {editorFrac}"
        bind:this={editorPaneEl}
      >
        {#if appStore.editorTabs.length > 0}
          <EditorTabs />
        {/if}
        <div class="min-h-0 flex-1">
          {#if appStore.selectedFile}
            <!-- エディタと Replace Multiline ペインを横並びにする -->
            <div class="flex h-full min-h-0">
              <div class="min-w-0 flex-1">
                <!-- タブ切替でエディタを作り直し、タブ間で undo 履歴・
                     カーソルが混ざらないようにする -->
                {#key appStore.activeEditorTabId}
                  <SqlEditor
                    bind:this={editor}
                    content={appStore.editorContent}
                    engine={selectedEngine}
                    editorLanguage={selectedCapabilities?.editor_language ?? null}
                    schemaMap={appStore.schemaMap}
                    onChange={(content) => appStore.updateEditorContent(content)}
                    onRun={(target) => void runStatement(target)}
                    onSelectionChange={(info) => {
                      hasMultilineSelection = info.hasMultilineSelection;
                    }}
                  />
                {/key}
              </div>
              {#if showReplacePane}
                <div class="w-96 shrink-0 border-l border-zinc-700">
                  {#key replaceOpenToken}
                    <ReplaceMultilinePane
                      initialLines={replaceInitialLines}
                      onReplace={applyReplace}
                      onClose={() => {
                        showReplacePane = false;
                      }}
                    />
                  {/key}
                </div>
              {/if}
            </div>
          {:else}
            <div class="flex h-full items-center justify-center">
              <p class="text-sm text-zinc-600">
                Select or create a query file
              </p>
            </div>
          {/if}
        </div>
      </div>
      <PaneDivider
        direction="horizontal"
        annotate="pane-divider-results"
        onDragStart={() => {
          dragBaseEditorFrac = editorFrac;
        }}
        onDrag={(delta) => {
          const height =
            (editorPaneEl?.clientHeight ?? 0) +
            (resultsPaneEl?.clientHeight ?? 0);
          if (height <= 0) return;
          editorFrac = clamp(
            dragBaseEditorFrac + delta / height,
            EDITOR_FRAC_MIN,
            EDITOR_FRAC_MAX,
          );
        }}
        onDragEnd={() => saveLayoutValue("editorFrac", editorFrac)}
      />
      <div
        class="min-h-0 basis-0"
        style="flex-grow: {1 - editorFrac}"
        bind:this={resultsPaneEl}
      >
        <ResultsPane />
      </div>
    </div>

    <!-- AI チャットペイン。エディタ / 結果の右側に縦いっぱいで並ぶ -->
    {#if showChat}
      <PaneDivider
        direction="vertical"
        annotate="pane-divider-chat"
        onDragStart={() => {
          dragBaseChat = chatWidth;
        }}
        onDrag={(delta) => {
          // 右端のペインなので、ドラッグ方向と幅の増減は逆になる
          chatWidth = clamp(dragBaseChat - delta, CHAT_MIN, CHAT_MAX);
        }}
        onDragEnd={() => saveLayoutValue("chatWidth", chatWidth)}
      />
      <div class="shrink-0" style="width: {chatWidth}px">
        <ChatPane
          supportsAi={selectedCapabilities?.supports_ai ?? false}
          onClose={() => {
            showChat = false;
            saveLayoutValue("chatOpen", 0);
          }}
          onInsert={(sql) => appStore.insertSqlSnippet(sql)}
        />
      </div>
    {/if}

    <!-- ヘルプペイン。チャットペインのさらに右 (最も右) に並ぶ -->
    {#if showHelp}
      <PaneDivider
        direction="vertical"
        annotate="pane-divider-help"
        onDragStart={() => {
          dragBaseHelp = helpWidth;
        }}
        onDrag={(delta) => {
          // 右端のペインなので、ドラッグ方向と幅の増減は逆になる
          helpWidth = clamp(dragBaseHelp - delta, HELP_MIN, HELP_MAX);
        }}
        onDragEnd={() => saveLayoutValue("helpWidth", helpWidth)}
      />
      <div class="shrink-0" style="width: {helpWidth}px">
        <HelpPane
          engine={selectedEngine}
          onClose={() => {
            showHelp = false;
            saveLayoutValue("helpOpen", 0);
          }}
          onInsert={(text) => appStore.insertSqlSnippet(text)}
        />
      </div>
    {/if}
  </div>
</div>

{#if showSearch}
  <SearchModal
    onClose={() => {
      showSearch = false;
    }}
  />
{/if}

{#if showSettings}
  <ConfigInfoModal
    onClose={() => {
      showSettings = false;
    }}
  />
{/if}

<!-- 設定ファイルのエディタ (メニューから開く)。mode で保存できる config と、
     編集はできるが保存できない source を切り替える。
     モーダル表示中でもネイティブメニューは操作できるため、mode が切り替わったら
     #key で作り直す (読み込み直しがマウント時に確定するため) -->
{#if configEditorMode !== null}
  {#key configEditorMode}
    <ConfigEditorModal
      mode={configEditorMode}
      onDirtyChange={(dirty) => {
        configEditorDirty = dirty;
      }}
      onClose={() => {
        configEditorMode = null;
        configEditorDirty = false;
      }}
    />
  {/key}
{/if}

<!-- AI による選択 SQL 解説のモーダル (EXPLAIN 解説モーダルを見出し違いで再利用) -->
{#if appStore.aiExplanation !== null}
  <AiAnalysisModal
    title="AI SQL Explanation"
    text={appStore.aiExplanation}
    onClose={() => appStore.closeAiExplanation()}
  />
{/if}

<!-- 危険な文 (allow_dangerous_statements 有効な接続) の実行前確認モーダル -->
{#if appStore.dangerousConfirmReason !== null}
  <DangerousConfirmModal
    reason={appStore.dangerousConfirmReason}
    onConfirm={() => appStore.confirmDangerous()}
    onCancel={() => appStore.cancelDangerous()}
  />
{/if}

<!-- 📝 マーカー付きの文で、行数の多い結果をエディタへ書き戻す前の確認 -->
{#if runLogConfirm !== null}
  <RunLogConfirmModal
    rows={runLogConfirm.rows}
    onConfirm={() => resolveRunLogConfirm(true)}
    onCancel={() => resolveRunLogConfirm(false)}
  />
{/if}
