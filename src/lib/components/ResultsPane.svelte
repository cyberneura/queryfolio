<script lang="ts">
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import { save } from "@tauri-apps/plugin-dialog";
  import { toast } from "svelte-sonner";
  import * as api from "$lib/api";
  import appStore, { isExplainSql } from "$lib/stores/app.svelte";
  import type { ResultTab } from "$lib/stores/app.svelte";
  import {
    toCsv,
    toCsvRange,
    toJson,
    toJsonRange,
    toTsv,
    toTsvRange,
    type CellRange,
  } from "$lib/export";
  import {
    singleTableSelectTable,
    buildUpdateStatements,
    normalizeEngine,
    normalizeTableName,
    type NormalizedEngine,
    type CellEdit,
  } from "$lib/editableResult";
  import CellInspector from "./CellInspector.svelte";
  import AiAnalysisModal from "./AiAnalysisModal.svelte";

  // Copy / Export / Cmd+C コピーで共有する出力フォーマット。
  type CopyFormat = "csv" | "tsv" | "json";
  const COPY_FORMAT_KEY = "queryfolio.results.copyFormat";
  const isCopyFormat = (v: string): v is CopyFormat =>
    v === "csv" || v === "tsv" || v === "json";
  function loadCopyFormat(): CopyFormat {
    try {
      const v = localStorage.getItem(COPY_FORMAT_KEY);
      if (v && isCopyFormat(v)) {
        return v;
      }
    } catch {
      // localStorage が使えなくても既定値で継続する
    }
    return "tsv";
  }
  let copyFormat = $state<CopyFormat>(loadCopyFormat());
  $effect(() => {
    try {
      localStorage.setItem(COPY_FORMAT_KEY, copyFormat);
    } catch {
      // localStorage が使えなくても動作は継続する
    }
  });

  // Copy / Export ボタンの一時的なフィードバック表示
  let copiedWhole = $state(false);
  let exported = $state(false);

  // Export の文字コード選択メニュー (分割ボタンの ▼)。
  // 本体のボタンは UTF-8 なので、ここには UTF-8 以外だけを並べる。
  const EXPORT_ENCODINGS: { value: api.ExportEncoding; label: string }[] = [
    { value: "cp932", label: "CP932" },
    { value: "euc-jp", label: "EUC-JP" },
  ];
  let exportMenuOpen = $state(false);

  const activeTab = $derived(appStore.activeResultTab);

  /// 結果セットを持たない文 (INSERT / UPDATE / DELETE / DDL 等) の実行結果を、
  /// DB コンソール風の生の実行メッセージとして組み立てる。最終行には
  /// [ No result set ] のラベルを置き、表形式の結果が無かったことを明示する。
  function noResultSetText(result: api.QueryResult): string {
    const lines: string[] = [];
    if (result.affected_rows !== null) {
      const n = result.affected_rows;
      lines.push(`Query OK, ${n} row${n === 1 ? "" : "s"} affected`);
    } else {
      // fetch 系だが列も行も得られなかった稀なケース (describe 不可の SHOW 等)
      lines.push("Query executed. No rows returned.");
    }
    lines.push(`Elapsed: ${result.elapsed_ms} ms`);
    lines.push("");
    lines.push("[ No result set ]");
    return lines.join("\n");
  }

  /// Analyze with AI ボタンの表示条件: EXPLAIN 由来のタブに結果があり、
  /// AI が設定済みであること
  const canAnalyzePlan = $derived(
    activeTab !== null &&
      activeTab.result !== null &&
      isExplainSql(activeTab.sql) &&
      (appStore.aiInfo?.configured ?? false),
  );

  // インスペクタで表示中のセル。どのタブのセルかを tabId で覚えておき、
  // タブ切替・タブクローズ時に別タブのセルを表示し続けないようにする
  let selectedCell = $state<{
    tabId: number;
    rowIndex: number;
    colIndex: number;
  } | null>(null);

  // 結果テーブルの矩形選択。anchor から focus までを選択範囲とする。
  // mode: cell = 単一セル基点の矩形, row = 行まるごと, col = 列まるごと。
  // tabId で対象タブを覚え、タブ切替・再実行で範囲外になったら破棄する。
  let selection = $state<{
    tabId: number;
    mode: "cell" | "row" | "col";
    anchorRow: number;
    anchorCol: number;
    focusRow: number;
    focusCol: number;
  } | null>(null);

  // ドラッグ選択の状態 ($state 不要: レンダリングに使わない内部フラグ)
  let dragging = false;
  let dragMode: "cell" | "row" | "col" | null = null;
  // ドラッグでセルをまたいだか。単純クリックとの区別に使い、
  // ドラッグ後にセルインスペクタを誤って開閉しないようにする
  let dragMoved = false;

  // キーボード操作 (Cmd+C / Cmd+A) を結果グリッドに限定するためのフォーカス先
  let gridEl = $state<HTMLDivElement | null>(null);

  // Cmd+C コピーにヘッダ行を含めるか。localStorage に保存する
  const COPY_HEADERS_KEY = "queryfolio.results.copyWithHeaders";
  let copyWithHeaders = $state(loadCopyWithHeaders());
  function loadCopyWithHeaders(): boolean {
    try {
      return localStorage.getItem(COPY_HEADERS_KEY) === "1";
    } catch {
      return false;
    }
  }
  $effect(() => {
    try {
      localStorage.setItem(COPY_HEADERS_KEY, copyWithHeaders ? "1" : "0");
    } catch {
      // localStorage が使えなくても動作は継続する
    }
  });

  // 選択範囲コピー時の一時的なフィードバック表示
  let selectionCopied = $state(false);

  // タブ切替 (クローズによる切替を含む) でインスペクタ・選択を閉じる
  $effect(() => {
    void appStore.activeTabId;
    selectedCell = null;
    selection = null;
  });

  // 選択範囲を結果サイズにクランプして正規化する (min/max を確定)。
  // アクティブタブ・結果が無い、または別タブの選択なら null。
  const selectedRange = $derived.by<CellRange | null>(() => {
    const tab = activeTab;
    if (!selection || !tab || selection.tabId !== tab.id) {
      return null;
    }
    const result = tab.result;
    if (!result) {
      return null;
    }
    const maxRow = result.rows.length - 1;
    const maxCol = result.columns.length - 1;
    if (maxRow < 0 || maxCol < 0) {
      return null;
    }
    const clampR = (n: number) => Math.min(maxRow, Math.max(0, n));
    const clampC = (n: number) => Math.min(maxCol, Math.max(0, n));
    let rowStart: number;
    let rowEnd: number;
    let colStart: number;
    let colEnd: number;
    if (selection.mode === "col") {
      rowStart = 0;
      rowEnd = maxRow;
    } else {
      rowStart = clampR(Math.min(selection.anchorRow, selection.focusRow));
      rowEnd = clampR(Math.max(selection.anchorRow, selection.focusRow));
    }
    if (selection.mode === "row") {
      colStart = 0;
      colEnd = maxCol;
    } else {
      colStart = clampC(Math.min(selection.anchorCol, selection.focusCol));
      colEnd = clampC(Math.max(selection.anchorCol, selection.focusCol));
    }
    return { rowStart, rowEnd, colStart, colEnd };
  });

  const isCellSelected = (rowIndex: number, colIndex: number): boolean => {
    const r = selectedRange;
    return (
      r !== null &&
      rowIndex >= r.rowStart &&
      rowIndex <= r.rowEnd &&
      colIndex >= r.colStart &&
      colIndex <= r.colEnd
    );
  };

  // 行ヘッダ (#) / 列ヘッダのハイライト条件
  const isRowHeaderSelected = (rowIndex: number): boolean => {
    const r = selectedRange;
    const cols = activeTab?.result?.columns.length ?? 0;
    return (
      r !== null &&
      r.colStart === 0 &&
      r.colEnd === cols - 1 &&
      rowIndex >= r.rowStart &&
      rowIndex <= r.rowEnd
    );
  };
  const isColHeaderSelected = (colIndex: number): boolean => {
    const r = selectedRange;
    const rows = activeTab?.result?.rows.length ?? 0;
    return (
      r !== null &&
      r.rowStart === 0 &&
      r.rowEnd === rows - 1 &&
      colIndex >= r.colStart &&
      colIndex <= r.colEnd
    );
  };

  const beginSelect = (
    mode: "cell" | "row" | "col",
    rowIndex: number,
    colIndex: number,
    e: PointerEvent,
  ) => {
    if (!activeTab?.result || e.button !== 0) {
      return;
    }
    dragging = true;
    dragMode = mode;
    dragMoved = false;
    selection = {
      tabId: activeTab.id,
      mode,
      anchorRow: rowIndex,
      anchorCol: colIndex,
      focusRow: rowIndex,
      focusCol: colIndex,
    };
    // ドラッグ後 (クリックが発火しない) でも Cmd+C が届くようにグリッドへフォーカス
    gridEl?.focus();
  };

  const extendSelect = (
    mode: "cell" | "row" | "col",
    rowIndex: number,
    colIndex: number,
    e: PointerEvent,
  ) => {
    if (!dragging) {
      return;
    }
    // pointerup をウインドウ外で取りこぼした場合の保険。ボタンが押されて
    // いない pointerenter (ただのホバー) が来たらドラッグを終了する
    if (e.buttons === 0) {
      endDrag();
      return;
    }
    if (dragMode !== mode || !selection) {
      return;
    }
    if (selection.focusRow !== rowIndex || selection.focusCol !== colIndex) {
      dragMoved = true;
    }
    selection = { ...selection, focusRow: rowIndex, focusCol: colIndex };
  };

  const endDrag = () => {
    dragging = false;
    dragMode = null;
  };

  const copySelection = async () => {
    const range = selectedRange;
    const result = activeTab?.result;
    if (!range || !result) {
      return;
    }
    const text =
      copyFormat === "csv"
        ? toCsvRange(result, range, copyWithHeaders)
        : copyFormat === "tsv"
          ? toTsvRange(result, range, copyWithHeaders)
          : toJsonRange(result, range, copyWithHeaders);
    await writeText(text);
    selectionCopied = true;
    setTimeout(() => {
      selectionCopied = false;
    }, 1500);
  };

  // 結果グリッドにフォーカスがあるか。SQL エディタ等からのキー操作を
  // 横取りしないための共通ガード。セル入力 (編集 input / 読み取り専用ビューの
  // textarea) にフォーカスがある間も、入力内の通常のキー操作を優先して false
  const isGridKeyTarget = (): boolean => {
    if (
      document.activeElement instanceof HTMLInputElement ||
      document.activeElement instanceof HTMLTextAreaElement
    ) {
      return false;
    }
    return gridEl !== null && gridEl.contains(document.activeElement);
  };

  // テーブル全体を選択する。選択できる結果が無ければ false (既定動作に任せる)。
  // mode は "cell" のままでよい: 行/列ヘッダのハイライトは
  // isRowHeaderSelected / isColHeaderSelected が mode ではなく確定後の範囲が
  // 全行・全列に及ぶかで判定するため、全体を覆う矩形なら両方点灯する
  const selectAll = (): boolean => {
    const tab = activeTab;
    const result = tab?.result;
    if (
      !tab ||
      !result ||
      result.rows.length === 0 ||
      result.columns.length === 0
    ) {
      return false;
    }
    selection = {
      tabId: tab.id,
      mode: "cell",
      anchorRow: 0,
      anchorCol: 0,
      focusRow: result.rows.length - 1,
      focusCol: result.columns.length - 1,
    };
    return true;
  };

  // Cmd+C (Ctrl+C) で選択範囲を CSV コピー、Cmd+A (Ctrl+A) でテーブル全体を選択。
  // どちらも結果グリッドにフォーカスがある時だけ処理する
  const handleWindowKeydown = (e: KeyboardEvent) => {
    if (!(e.metaKey || e.ctrlKey)) {
      return;
    }
    const key = e.key.toLowerCase();
    if (key !== "c" && key !== "a") {
      return;
    }
    if (!isGridKeyTarget()) {
      return;
    }
    if (key === "a") {
      if (selectAll()) {
        e.preventDefault();
      }
      return;
    }
    if (!selectedRange) {
      return;
    }
    e.preventDefault();
    void copySelection();
  };

  // インスペクタに渡す値。再実行などで結果が入れ替わり
  // 選択位置が範囲外になった場合は null (= 非表示) にする
  const inspectedCell = $derived.by(() => {
    const tab = activeTab;
    if (!selectedCell || !tab || selectedCell.tabId !== tab.id) {
      return null;
    }
    const result = tab.result;
    if (!result) {
      return null;
    }
    const row = result.rows[selectedCell.rowIndex];
    if (!row || selectedCell.colIndex >= result.columns.length) {
      return null;
    }
    return {
      value: row[selectedCell.colIndex],
      column: result.columns[selectedCell.colIndex],
      rowIndex: selectedCell.rowIndex,
    };
  });

  // セルクリックで選択してインスペクタを開く。選択中セルの再クリックで閉じる
  const selectCell = (rowIndex: number, colIndex: number) => {
    if (!activeTab) {
      return;
    }
    if (
      selectedCell &&
      selectedCell.tabId === activeTab.id &&
      selectedCell.rowIndex === rowIndex &&
      selectedCell.colIndex === colIndex
    ) {
      selectedCell = null;
      return;
    }
    selectedCell = { tabId: activeTab.id, rowIndex, colIndex };
  };

  // セルの click。直前がドラッグ選択だった場合はインスペクタ開閉を抑制する
  const onCellClick = (rowIndex: number, colIndex: number) => {
    if (dragMoved) {
      dragMoved = false;
      return;
    }
    // 選択中セルの再クリック (インスペクタを閉じる操作) では、
    // pointerdown で張られた 1 セルの矩形選択も一緒に解除して
    // ハイライトが残らないようにする
    const closingInspector = isSelectedCell(rowIndex, colIndex);
    selectCell(rowIndex, colIndex);
    if (closingInspector) {
      selection = null;
    }
  };

  const isSelectedCell = (rowIndex: number, colIndex: number): boolean =>
    selectedCell !== null &&
    selectedCell.tabId === activeTab?.id &&
    selectedCell.rowIndex === rowIndex &&
    selectedCell.colIndex === colIndex;

  // アクティブセルのコピーアイコンの一時的なフィードバック表示 (`${row}:${col}`)
  let copiedCellKey = $state<string | null>(null);

  // アクティブセルの値をコピーする (NULL / オブジェクトの文字列化はセル表示と同じ)
  const copyCellValue = async (rowIndex: number, colIndex: number) => {
    const result = activeTab?.result;
    if (!result) {
      return;
    }
    await writeText(cellText(result.rows[rowIndex][colIndex]));
    copiedCellKey = `${rowIndex}:${colIndex}`;
    setTimeout(() => {
      copiedCellKey = null;
    }, 1500);
  };

  // セル背景色: インスペクタで開いているセルを最優先で強調し、
  // 次に矩形選択の範囲を淡く強調する
  const cellBgClass = (rowIndex: number, colIndex: number): string => {
    if (isSelectedCell(rowIndex, colIndex)) {
      return "bg-sky-800/60";
    }
    if (isCellSelected(rowIndex, colIndex)) {
      return "bg-sky-900/40";
    }
    return "";
  };

  // 結果テーブル全体を選択中フォーマットで文字列化する (Copy / Export 共通)。
  const serializeResult = (
    format: CopyFormat,
    result: api.QueryResult | null | undefined,
  ): string | null => {
    if (!result) {
      return null;
    }
    return format === "csv"
      ? toCsv(result)
      : format === "tsv"
        ? toTsv(result)
        : toJson(result);
  };

  // Copy / Export で出力する結果を用意する。
  //
  // 結果テーブルの表示は設定の default_limit で絞られているが、Copy / Export では
  // その制限を無視して全件を出す。取り直しが要らない (default_limit が付いて
  // いなかった) 場合は表示中の結果をそのまま使う。
  // 取り直し中は Copy / Export を二重に走らせない。
  let preparingOutput = $state(false);
  const resultForOutput = async (): Promise<api.QueryResult | null> => {
    const tab = activeTab;
    if (!tab?.result) {
      return null;
    }
    let full: api.QueryResult | null = null;
    preparingOutput = true;
    try {
      full = await appStore.fetchResultWithoutDefaultLimit(tab);
    } catch (e) {
      // 取り直しに失敗したら黙って表示中の結果を出さない
      // (件数が違うものを気付かず出力するほうが危険)
      toast.error(`Failed to fetch the full result: ${e}`);
      return null;
    } finally {
      preparingOutput = false;
    }
    const result = full ?? tab.result;
    if (result.truncated) {
      toast.warning(
        `The output was truncated at ${result.row_count.toLocaleString()} rows.`,
      );
    }
    return result;
  };

  // Copy ボタン: 結果テーブル全体を選択中フォーマットでクリップボードへ。
  const copyResult = async () => {
    if (preparingOutput) {
      return;
    }
    const text = serializeResult(copyFormat, await resultForOutput());
    if (text === null) {
      return;
    }
    // navigator.clipboard は Tauri 2 で OS のパーミッションプロンプトが
    // 出ることがあるため、公式プラグイン経由で書き込む
    await writeText(text);
    copiedWhole = true;
    setTimeout(() => {
      copiedWhole = false;
    }, 1500);
  };

  // Export ボタン: 結果テーブル全体を選択中フォーマットでファイルへ保存する。
  // ネイティブ保存ダイアログで選ばせたパスへ Rust 側で書き込む。
  // encoding は分割ボタンの選択 (既定は UTF-8)。
  const exportResult = async (encoding: api.ExportEncoding = "utf-8") => {
    if (!activeTab?.result || preparingOutput) {
      return;
    }
    const ext = copyFormat;
    let path: string | null;
    try {
      path = await save({
        defaultPath: `result.${ext}`,
        filters: [{ name: ext.toUpperCase(), extensions: [ext] }],
      });
    } catch (e) {
      toast.error(`Export failed: ${e}`);
      return;
    }
    if (!path) {
      // ユーザーがダイアログをキャンセルした
      return;
    }
    // 取り直しとシリアライズはパス確定後に行う (キャンセル時の無駄な処理を避ける)
    const text = serializeResult(copyFormat, await resultForOutput());
    if (text === null) {
      return;
    }
    try {
      await api.writeExportFile(path, text, encoding);
      exported = true;
      setTimeout(() => {
        exported = false;
      }, 1500);
    } catch (e) {
      toast.error(`Export failed: ${e}`);
    }
  };

  const cellText = (value: unknown): string => {
    if (value === null || value === undefined) {
      return "NULL";
    }
    if (typeof value === "object") {
      return JSON.stringify(value);
    }
    return String(value);
  };

  // タブ見出し用に SQL を 1 行・短縮表示にする
  const tabLabel = (tab: ResultTab): string => {
    const compact = tab.sql.replace(/\s+/g, " ").trim();
    if (!compact) {
      return "Query";
    }
    return compact.length > 24 ? `${compact.slice(0, 24)}…` : compact;
  };

  const formatTime = (epochMs: number): string => {
    const d = new Date(epochMs);
    const pad = (n: number) => String(n).padStart(2, "0");
    return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  };

  const tabTooltip = (tab: ResultTab): string =>
    `${tab.connection}${tab.schema ? ` / ${tab.schema}` : ""} at ${formatTime(tab.executedAt)}\n${tab.sql.trim()}`;

  const aiConfigured = $derived(appStore.aiInfo?.configured ?? false);

  /// アクティブなタブの接続が AI 機能に対応しているか (redis 等は非対応)。
  /// Fix with AI は SQL 修正プロンプト前提なので、非対応エンジンでは出さない
  const activeTabSupportsAi = $derived.by(() => {
    const conn = activeTab?.connection;
    if (!conn) return true;
    const info = appStore.connections.find((c) => c.name === conn);
    return info?.capabilities.supports_ai ?? true;
  });

  /// Fix with AI ボタンの title (未設定・エラー時は設定方法を案内する)。
  /// DB エラーメッセージには値が含まれ得るため、送信内容を明示する
  const aiFixButtonTitle = $derived(
    aiConfigured
      ? `Ask AI to fix this SQL (${appStore.aiInfo?.model}). ` +
          "Sends the failed SQL, the database error message, and " +
          "table/column names to the AI provider (never the query results)."
      : appStore.aiError
        ? `AI is unavailable: ${appStore.aiError}`
        : "AI is not configured. Add an 'ai:' section (provider: openai, " +
          "api_key: ...) to config.yml or the override YAML.",
  );

  // ------- 結果セルの編集 (ダブルクリック → 保留 → Preview/Edit/Submit/Cancel) -------

  /// あるタブの編集可否と対象テーブル / 主キー / 編集可能列。
  interface EditContext {
    table: string;
    pkColumns: string[];
    editableColumns: Set<string>;
  }

  // tabId ごとにキャッシュ。値 null = 判定済みで編集不可、キー無し = 未判定。
  let editContexts = $state(new Map<number, EditContext | null>());
  // tabId -> (`${rowIndex}:${column}` -> 編集内容)
  let pendingEdits = $state(new Map<number, Map<string, CellEdit>>());
  // インライン編集中のセル (1 つ)。readonly = true は編集不可セルを
  // 読み取り専用入力で開いている状態 (全文の選択・コピー用。書き込みはしない)
  let editingCell = $state<{
    tabId: number;
    rowIndex: number;
    colIndex: number;
    readonly: boolean;
  } | null>(null);
  let editingValue = $state("");
  // Preview モーダルに出す UPDATE 文 (非表示中は null)
  let previewStatements = $state<string[] | null>(null);
  // 結果が再取得された (executedAt 変化) タブの編集状態を破棄するための記録
  const seenExecutedAt = new Map<number, number>();

  const activeEngine = $derived.by<NormalizedEngine | null>(() => {
    const conn = activeTab?.connection;
    if (!conn) return null;
    const info = appStore.connections.find((c) => c.name === conn);
    return info ? normalizeEngine(info.engine) : null;
  });

  /// 編集はアクティブ接続かつ Writable ON かつ config readonly でない時のみ。
  /// セル編集非対応のエンジン (capabilities.supports_editable_cells = false、
  /// redis 等) では常に不可。
  /// さらにタブの実行時スキーマが現在のアクティブスキーマと一致する時だけ許可する。
  /// 生成する UPDATE はスキーマ未修飾で「接続の現在のアクティブスキーマ」に対して
  /// 走るため、実行後にスキーマを切り替えていると別スキーマの同名テーブルを
  /// 更新してしまう。スキーマ不一致時は編集不可にしてこれを防ぐ。
  const canEditActiveConnection = $derived(
    activeTab !== null &&
      activeTab.connection === appStore.selectedConnection &&
      (appStore.selectedCapabilities?.supports_editable_cells ?? true) &&
      appStore.writable &&
      !appStore.selectedConnectionReadonly &&
      activeTab.schema === appStore.activeSchema,
  );

  const activeEditContext = $derived(
    activeTab ? (editContexts.get(activeTab.id) ?? null) : null,
  );
  const activePending = $derived(
    activeTab ? (pendingEdits.get(activeTab.id) ?? null) : null,
  );
  const pendingCount = $derived(activePending ? activePending.size : 0);
  // 適用中 (または同一接続でクエリ実行中) は Submit を無効化し、二重 Submit や
  // 並列実行を防ぐ (submitCellEdits 側の isConnectionRunning ガードと二重の防御)。
  const submitDisabled = $derived(
    activeTab === null || appStore.isConnectionRunning(activeTab.connection),
  );

  // 結果の入れ替わりで編集状態を破棄し、編集可能な状況なら editContext を求める
  $effect(() => {
    const tab = activeTab;
    if (!tab) return;
    const seen = seenExecutedAt.get(tab.id);
    if (seen !== tab.executedAt) {
      seenExecutedAt.set(tab.id, tab.executedAt);
      if (pendingEdits.has(tab.id)) {
        pendingEdits.delete(tab.id);
        pendingEdits = new Map(pendingEdits);
      }
      if (editContexts.has(tab.id)) {
        editContexts.delete(tab.id);
        editContexts = new Map(editContexts);
      }
      if (editingCell?.tabId === tab.id) editingCell = null;
    }
    if (canEditActiveConnection && tab.result && !editContexts.has(tab.id)) {
      void deriveEditContext(tab);
    }
  });

  async function deriveEditContext(tab: ResultTab) {
    const result = tab.result;
    if (!result) return;
    const rawTable = singleTableSelectTable(tab.sql);
    // 実テーブルに合わせてエンジン別に表名を正規化する (PG は小文字化)。
    // 正規化名を PK / カラム照会と生成 UPDATE の両方で一貫して使う。
    const info = appStore.connections.find((c) => c.name === tab.connection);
    const engine = info ? normalizeEngine(info.engine) : null;
    const table = rawTable && engine ? normalizeTableName(engine, rawTable) : null;
    let ctx: EditContext | null = null;
    if (table) {
      try {
        const [pk, cols] = await Promise.all([
          api.getPrimaryKeys(tab.connection, table),
          api.listColumns(tab.connection, table),
        ]);
        const colNames = new Set(cols.map((c) => c.name));
        // 列名に重複があると行/列の対応が曖昧なため編集不可 (安全側)
        const hasDup = result.columns.length !== new Set(result.columns).size;
        const pkPresent =
          pk.length > 0 && pk.every((k) => result.columns.includes(k));
        if (!hasDup && pkPresent) {
          const editable = new Set(
            result.columns.filter((c) => colNames.has(c) && !pk.includes(c)),
          );
          if (editable.size > 0) {
            ctx = { table, pkColumns: pk, editableColumns: editable };
          }
        }
      } catch {
        ctx = null; // 取得失敗は編集不可に倒す
      }
    }
    // 応答が古い (結果が入れ替わった) なら捨てる
    const current = appStore.resultTabs.find((t) => t.id === tab.id);
    if (!current || current.executedAt !== tab.executedAt) return;
    editContexts.set(tab.id, ctx);
    editContexts = new Map(editContexts);
  }

  const editText = (v: unknown): string =>
    v === null || v === undefined ? "" : String(v);

  const isColumnEditable = (colIndex: number): boolean => {
    if (!canEditActiveConnection) return false;
    const ctx = activeEditContext;
    const col = activeTab?.result?.columns[colIndex];
    return !!ctx && col != null && ctx.editableColumns.has(col);
  };

  // その行の主キー値がすべて非 NULL か。NULL を含む主キーは行を一意に
  // 同定できず (特に SQLite は複合 / 非整数 PK に NULL を許し、WHERE pk IS NULL が
  // 複数行に当たる)、UPDATE が意図しない行にも及ぶため編集不可にする。
  const rowPkComplete = (rowIndex: number): boolean => {
    const ctx = activeEditContext;
    const result = activeTab?.result;
    if (!ctx || !result) return false;
    return ctx.pkColumns.every((pk) => {
      const ci = result.columns.indexOf(pk);
      return ci >= 0 && result.rows[rowIndex]?.[ci] != null;
    });
  };

  // オブジェクト (JSON / blob) セルはインライン編集の対象外にする
  const isCellEditable = (rowIndex: number, colIndex: number): boolean => {
    if (!isColumnEditable(colIndex)) return false;
    if (!rowPkComplete(rowIndex)) return false;
    const v = activeTab?.result?.rows[rowIndex]?.[colIndex];
    return typeof v !== "object" || v === null;
  };

  const pendingInput = (rowIndex: number, colIndex: number): string | null => {
    const col = activeTab?.result?.columns[colIndex];
    if (!col || !activePending) return null;
    return activePending.get(`${rowIndex}:${col}`)?.input ?? null;
  };

  const isEditingCell = (rowIndex: number, colIndex: number): boolean =>
    editingCell !== null &&
    editingCell.tabId === activeTab?.id &&
    editingCell.rowIndex === rowIndex &&
    editingCell.colIndex === colIndex;

  const beginCellEdit = (rowIndex: number, colIndex: number) => {
    if (!activeTab?.result) return;
    if (!isCellEditable(rowIndex, colIndex)) {
      // 編集不可セルは読み取り専用入力で開き、全文を選択・コピーしやすくする
      editingCell = { tabId: activeTab.id, rowIndex, colIndex, readonly: true };
      editingValue = cellText(activeTab.result.rows[rowIndex][colIndex]);
      return;
    }
    const col = activeTab.result.columns[colIndex];
    const existing = activePending?.get(`${rowIndex}:${col}`);
    const original = activeTab.result.rows[rowIndex][colIndex];
    editingCell = { tabId: activeTab.id, rowIndex, colIndex, readonly: false };
    editingValue = existing ? existing.input : editText(original);
  };

  const commitCellEdit = () => {
    const ec = editingCell;
    // 読み取り専用ビューは閉じるだけで保留編集には登録しない
    if (
      !ec ||
      ec.readonly ||
      !activeTab ||
      ec.tabId !== activeTab.id ||
      !activeTab.result
    ) {
      editingCell = null;
      return;
    }
    const col = activeTab.result.columns[ec.colIndex];
    const original = activeTab.result.rows[ec.rowIndex][ec.colIndex];
    const key = `${ec.rowIndex}:${col}`;
    const map = new Map(pendingEdits.get(activeTab.id) ?? []);
    // 元の表示と同じに戻したら保留を解除する
    if (editingValue === editText(original)) {
      map.delete(key);
    } else {
      map.set(key, {
        rowIndex: ec.rowIndex,
        column: col,
        original,
        input: editingValue,
      });
    }
    if (map.size > 0) pendingEdits.set(activeTab.id, map);
    else pendingEdits.delete(activeTab.id);
    pendingEdits = new Map(pendingEdits);
    editingCell = null;
  };

  const cancelCellEdit = () => {
    editingCell = null;
  };

  const onEditKeydown = (e: KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      commitCellEdit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancelCellEdit();
    }
  };

  const clearPending = () => {
    const tab = activeTab;
    if (!tab) return;
    if (pendingEdits.has(tab.id)) {
      pendingEdits.delete(tab.id);
      pendingEdits = new Map(pendingEdits);
    }
    editingCell = null;
  };

  const buildActiveStatements = (): string[] => {
    const tab = activeTab;
    const ctx = activeEditContext;
    const eng = activeEngine;
    const pending = activePending;
    if (!tab?.result || !ctx || !eng || !pending || pending.size === 0) {
      return [];
    }
    return buildUpdateStatements(
      eng,
      ctx.table,
      ctx.pkColumns,
      tab.result.columns,
      tab.result.rows,
      [...pending.values()],
    );
  };

  const openPreview = () => {
    const stmts = buildActiveStatements();
    if (stmts.length > 0) previewStatements = stmts;
  };

  // 生成した UPDATE をエディタへ貼り、保留は解除する (以降は手動で実行する想定)。
  // insertSqlSnippet は「現在選択中の接続」のエディタへ挿入するため、結果タブの
  // 接続と選択中接続が違う時は貼らない (別接続の同名テーブルへ A の UPDATE を
  // 流し込む事故を防ぐ。Submit の接続ガードと揃える)。
  const editInEditor = () => {
    const tab = activeTab;
    if (!tab) return;
    if (tab.connection !== appStore.selectedConnection) {
      toast.warning(
        `These edits are for '${tab.connection}'. Switch to that connection to paste them into the editor.`,
      );
      return;
    }
    // 生成 UPDATE はスキーマ未修飾。実行時からスキーマを切り替えていると、
    // 貼り付けた UPDATE が別スキーマの同名テーブルに走り得るため貼らない
    // (Submit の tab.schema !== activeSchema ガードと揃える)。
    if (tab.schema !== appStore.activeSchema) {
      toast.warning(
        "The active schema changed since these edits were made. Cancel them and re-run the query.",
      );
      return;
    }
    const stmts = buildActiveStatements();
    if (stmts.length === 0) return;
    const text = stmts.map((s) => `${s};`).join("\n");
    if (appStore.insertSqlSnippet(text)) {
      clearPending();
      previewStatements = null;
    }
  };

  const submitEdits = async () => {
    const tab = activeTab;
    const stmts = buildActiveStatements();
    if (!tab || stmts.length === 0) return;
    const ok = await appStore.submitCellEdits(tab.id, stmts);
    if (ok) {
      // 成功時は再実行で結果が入れ替わり effect が保留を破棄するが、明示的にも消す
      clearPending();
      previewStatements = null;
    }
  };

  const cancelAllEdits = () => {
    clearPending();
    previewStatements = null;
  };
</script>

<!-- ドラッグ選択はセルの pointerenter で追跡するため、
     pointer capture は使わず window で終了を拾う -->
<svelte:window
  onpointerup={endDrag}
  onpointercancel={endDrag}
  onkeydown={handleWindowKeydown}
/>

<div class="flex h-full min-h-0 flex-col bg-zinc-900">
  <!-- タブバー -->
  <div
    class="flex shrink-0 items-start gap-3 border-b border-zinc-700 px-3 py-1 text-xs text-zinc-400"
  >
    <span class="mt-0.5 font-semibold tracking-wide">RESULTS</span>
    {#if appStore.resultTabs.length > 0}
      <!-- 多段タブ: タブが増えたら折り返して複数段で表示する -->
      <div class="flex min-w-0 flex-1 flex-wrap items-center gap-1">
        {#each appStore.resultTabs as tab (tab.id)}
          <div
            class="flex shrink-0 items-center gap-0.5 rounded-t border-t border-r border-l px-1 py-0.5 {tab.id ===
            appStore.activeTabId
              ? 'border-zinc-600 bg-zinc-800 text-zinc-200'
              : 'border-transparent text-zinc-500 hover:bg-zinc-800/60 hover:text-zinc-300'}"
          >
            <button
              class="max-w-48 truncate font-mono"
              title={tabTooltip(tab)}
              data-annotate="button-result-tab-{tab.id}"
              onclick={() => appStore.selectResultTab(tab.id)}
            >
              {tabLabel(tab)}
            </button>
            <button
              class="rounded px-0.5 hover:bg-zinc-700 {tab.pinned
                ? 'text-amber-400'
                : 'text-zinc-500 hover:text-zinc-300'}"
              title={tab.pinned ? "Unpin this tab" : "Pin this tab"}
              aria-label={tab.pinned ? "Unpin this tab" : "Pin this tab"}
              data-annotate="button-result-tab-pin-{tab.id}"
              onclick={() => appStore.toggleResultTabPin(tab.id)}
            >
              <i
                class="bi {tab.pinned ? 'bi-pin-fill' : 'bi-pin-angle'}"
                aria-hidden="true"
              ></i>
            </button>
            <button
              class="rounded px-0.5 text-zinc-500 hover:bg-zinc-700 hover:text-zinc-200 disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-zinc-500"
              title={tab.running
                ? "Cannot close while the query is running"
                : "Close this tab"}
              aria-label={tab.running
                ? "Cannot close while the query is running"
                : "Close this tab"}
              data-annotate="button-result-tab-close-{tab.id}"
              disabled={tab.running}
              onclick={() => appStore.closeResultTab(tab.id)}
            >
              <i class="bi bi-x" aria-hidden="true"></i>
            </button>
          </div>
        {/each}
      </div>
    {/if}
  </div>

  <!-- アクティブタブの実行情報 -->
  {#if activeTab}
    <div
      class="flex shrink-0 items-center gap-3 border-b border-zinc-700 px-3 py-1.5 text-xs text-zinc-400"
    >
      <span
        class="max-w-40 truncate font-mono text-zinc-300"
        title={activeTab.connection}
        data-annotate="text-result-connection"
      >
        {activeTab.connection}
      </span>
      {#if activeTab.schema}
        <span
          class="max-w-40 truncate font-mono"
          title={activeTab.schema}
          data-annotate="text-result-schema"
        >
          {activeTab.schema}
        </span>
      {/if}
      <span data-annotate="text-result-executed-at">
        {formatTime(activeTab.executedAt)}
      </span>
      {#if activeTab.running}
        <span class="text-blue-400">Running...</span>
      {:else if activeTab.cancelled}
        <span class="text-amber-400" data-annotate="text-result-cancelled">
          Cancelled
        </span>
      {:else if activeTab.result}
        {@const result = activeTab.result}
        {#if result.affected_rows !== null}
          <span data-annotate="text-affected-rows">
            {result.affected_rows} rows affected
          </span>
        {:else}
          <span data-annotate="text-row-count">{result.row_count} rows</span>
          {#if result.applied_limit !== null}
            <span
              class="text-zinc-500"
              title="LIMIT was added automatically (default_limit in config.yml)"
              data-annotate="text-applied-limit"
            >
              LIMIT {result.applied_limit} (auto)
            </span>
          {/if}
          {#if result.truncated}
            <span class="text-amber-400" title="Truncated at the row limit">
              (truncated)
            </span>
          {/if}
        {/if}
        <span>{result.elapsed_ms} ms</span>
      {/if}
      <span class="ml-auto flex items-center gap-1">
        {#if activeTab.running}
          <button
            class="rounded border border-red-800 bg-red-900/40 px-1.5 py-0.5 text-red-300 hover:bg-red-800 hover:text-red-100"
            title="Cancel the running query"
            aria-label="Cancel the running query"
            data-annotate="button-cancel-query"
            onclick={() => appStore.cancelQuery(activeTab.id)}
          >
            <i class="bi bi-x-circle" aria-hidden="true"></i> Cancel
          </button>
        {/if}
        <button
          class="rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200 disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent"
          title="Run this tab's SQL again on {activeTab.connection}"
          aria-label="Run this tab's SQL again"
          data-annotate="button-rerun-tab"
          disabled={appStore.isConnectionRunning(activeTab.connection)}
          onclick={() => appStore.rerunTab(activeTab.id)}
        >
          <i class="bi bi-arrow-repeat" aria-hidden="true"></i> Re-run
        </button>
        {#if canAnalyzePlan}
          <!-- EXPLAIN の実行計画を AI に解説させる (AI 設定済みのタブのみ) -->
          <button
            class="flex items-center gap-1 rounded border border-blue-500/50 bg-blue-500/15 px-1.5 py-0.5 text-blue-300 hover:bg-blue-500/25 disabled:cursor-not-allowed disabled:opacity-50"
            title="Explain the plan with AI ({appStore.aiInfo?.model})"
            data-annotate="button-analyze-plan"
            disabled={appStore.aiAnalyzing}
            onclick={() => appStore.analyzeExplainTab(activeTab.id)}
          >
            {#if appStore.aiAnalyzing}
              <!-- 解説中スピナー -->
              <span
                class="inline-block size-3 animate-spin rounded-full border-2 border-blue-300 border-t-transparent"
                data-annotate="spinner-ai-analyzing"
              ></span>
              Analyzing...
            {:else}
              <i class="bi bi-stars" aria-hidden="true"></i> Analyze with AI
            {/if}
          </button>
        {/if}
        {#if activeTab.result && activeTab.result.columns.length > 0}
          {#if selectionCopied}
            <span class="text-emerald-400" data-annotate="text-selection-copied">
              Copied
            </span>
          {/if}
          <label
            class="flex cursor-pointer items-center gap-1 select-none hover:text-zinc-200"
            title="Include column headers when copying a cell selection with Cmd+C (Ctrl+C)"
          >
            <input
              type="checkbox"
              class="cursor-pointer accent-sky-600"
              data-annotate="checkbox-copy-with-headers"
              bind:checked={copyWithHeaders}
            />
            Copy with headers
          </label>
          <!-- 出力フォーマット。Copy / Export / Cmd+C コピーで共通に使う -->
          <select
            class="cursor-pointer rounded border border-zinc-700 bg-zinc-800 px-1 py-0.5 uppercase hover:bg-zinc-700 hover:text-zinc-200"
            title="Output format for Copy / Export and Cmd+C (Ctrl+C)"
            data-annotate="select-copy-format"
            bind:value={copyFormat}
          >
            {#each ["tsv", "csv", "json"] as const as format (format)}
              <option value={format}>{format.toUpperCase()}</option>
            {/each}
          </select>
          <button
            class="rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
            title="Copy the whole result to the clipboard in the selected format"
            data-annotate="button-copy-result"
            onclick={copyResult}
          >
            {copiedWhole ? "Copied!" : "Copy"}
          </button>
          <!-- Export は分割ボタン。本体は UTF-8、▼ で文字コードを選ぶ -->
          <span class="relative inline-flex">
            <button
              class="rounded-l border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
              title="Export the whole result to a file in the selected format (UTF-8)"
              data-annotate="button-export-result"
              onclick={() => void exportResult("utf-8")}
            >
              {exported ? "Exported!" : "Export"}
            </button>
            <button
              class="-ml-px rounded-r border border-zinc-700 px-1 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
              title="Export with a different character encoding"
              aria-label="Export encoding options"
              aria-haspopup="menu"
              data-annotate="button-export-encoding-menu"
              onclick={() => (exportMenuOpen = !exportMenuOpen)}
            >
              <i class="bi bi-caret-down-fill text-[0.6rem]"></i>
            </button>
            {#if exportMenuOpen}
              <!-- 外側クリックで閉じるための背面レイヤー -->
              <button
                class="fixed inset-0 z-20 cursor-default"
                aria-label="Close menu"
                data-annotate="export-menu-backdrop"
                onclick={() => (exportMenuOpen = false)}
              ></button>
              <div
                class="absolute right-0 top-full z-30 mt-0.5 min-w-44 rounded border border-zinc-700 bg-zinc-800 py-1 shadow-lg"
                role="menu"
              >
                {#each EXPORT_ENCODINGS as option (option.value)}
                  <button
                    class="block w-full px-3 py-1 text-left hover:bg-zinc-700 hover:text-zinc-200"
                    role="menuitem"
                    data-annotate="menu-export-as-{option.value}"
                    onclick={() => {
                      exportMenuOpen = false;
                      void exportResult(option.value);
                    }}
                  >
                    Export as {option.label}
                  </button>
                {/each}
              </div>
            {/if}
          </span>
        {/if}
      </span>
    </div>
  {/if}

  <!-- 保留中のセル編集バー (未確定の編集がある時のみ) -->
  {#if pendingCount > 0}
    <div
      class="flex shrink-0 items-center gap-3 border-b border-amber-700/60 bg-amber-950/40 px-3 py-1.5 text-xs text-amber-200"
      data-annotate="bar-pending-edits"
    >
      <span class="font-semibold">
        {pendingCount} pending edit{pendingCount === 1 ? "" : "s"}
      </span>
      <span class="ml-auto flex items-center gap-1">
        <button
          class="rounded border border-amber-600/60 px-1.5 py-0.5 hover:bg-amber-800/50"
          title="Preview the UPDATE statements"
          data-annotate="button-edits-preview"
          onclick={openPreview}
        >
          <i class="bi bi-eye" aria-hidden="true"></i> Preview
        </button>
        <button
          class="rounded border border-amber-600/60 px-1.5 py-0.5 hover:bg-amber-800/50"
          title="Paste the UPDATE statements into the editor (does not run them)"
          data-annotate="button-edits-edit"
          onclick={editInEditor}
        >
          <i class="bi bi-pencil" aria-hidden="true"></i> Edit
        </button>
        <button
          class="rounded border border-emerald-600/60 bg-emerald-900/40 px-1.5 py-0.5 text-emerald-200 hover:bg-emerald-800/50 disabled:cursor-default disabled:opacity-40 disabled:hover:bg-emerald-900/40"
          title="Run the UPDATE statements in one transaction"
          data-annotate="button-edits-submit"
          disabled={submitDisabled}
          onclick={submitEdits}
        >
          <i class="bi bi-check2" aria-hidden="true"></i> Submit
        </button>
        <button
          class="rounded border border-zinc-600 px-1.5 py-0.5 text-zinc-300 hover:bg-zinc-700"
          title="Discard all pending edits"
          data-annotate="button-edits-cancel"
          onclick={cancelAllEdits}
        >
          <i class="bi bi-x" aria-hidden="true"></i> Cancel
        </button>
      </span>
    </div>
  {/if}

  <div class="flex min-h-0 flex-1">
    <!-- tabindex/bind: セル選択後の Cmd+C コピー・Cmd+A 全選択を結果グリッドに
         限定する (window の keydown で gridEl 内にフォーカスがある時だけ処理) -->
    <div
      class="min-h-0 flex-1 overflow-auto focus:outline-none"
      tabindex="-1"
      bind:this={gridEl}
    >
      {#if appStore.errorMessage}
        <pre
          class="whitespace-pre-wrap px-3 py-2 font-mono text-xs text-red-400"
          data-annotate="text-error-message">{appStore.errorMessage}</pre>
      {:else if activeTab?.running}
        <p class="px-3 py-2 text-xs text-blue-400">Running...</p>
      {:else if activeTab?.error}
        <div class="px-3 py-2">
          <div class="flex items-start gap-2">
            <pre
              class="min-w-0 flex-1 whitespace-pre-wrap font-mono text-xs text-red-400"
              data-annotate="text-error-message">{activeTab.error}</pre>
            {#if activeTabSupportsAi}
              <button
                class="flex shrink-0 items-center gap-1 rounded border border-blue-500/50 bg-blue-500/15 px-2 py-0.5 text-xs text-blue-300 hover:bg-blue-500/25 disabled:cursor-not-allowed disabled:opacity-50"
                title={aiFixButtonTitle}
                data-annotate="button-ai-fix"
                disabled={!aiConfigured || activeTab.fixing}
                onclick={() => appStore.fixSqlWithAi(activeTab.id)}
              >
                {#if activeTab.fixing}
                  <!-- 修正案の生成中スピナー -->
                  <span
                    class="inline-block size-3 animate-spin rounded-full border-2 border-blue-300 border-t-transparent"
                    data-annotate="spinner-ai-fixing"
                  ></span>
                  Fixing...
                {:else}
                  <i class="bi bi-magic" aria-hidden="true"></i> Fix with AI
                {/if}
              </button>
            {/if}
          </div>

          <!-- AI の修正案 (元の SQL と並べて表示。Apply までは実行しない) -->
          {#if activeTab.fixSuggestion}
            <div
              class="mt-2 rounded border border-zinc-700 bg-zinc-800/40"
              data-annotate="panel-ai-fix-suggestion"
            >
              <div
                class="flex items-center gap-2 border-b border-zinc-700 px-2 py-1 text-xs text-zinc-400"
              >
                <span class="font-semibold">AI fix suggestion</span>
                <span class="ml-auto flex items-center gap-1">
                  <button
                    class="rounded border border-blue-500/50 bg-blue-500/15 px-1.5 py-0.5 text-blue-300 hover:bg-blue-500/25"
                    title="Insert the suggested SQL into the editor (does not run it)"
                    data-annotate="button-ai-fix-apply"
                    onclick={() => appStore.applyFixSuggestion(activeTab.id)}
                  >
                    Apply to editor
                  </button>
                  <button
                    class="rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
                    title="Discard the suggestion"
                    data-annotate="button-ai-fix-dismiss"
                    onclick={() => appStore.dismissFixSuggestion(activeTab.id)}
                  >
                    Dismiss
                  </button>
                </span>
              </div>
              <div class="p-2 text-xs">
                <p class="mb-1 text-zinc-500">Original SQL:</p>
                <pre
                  class="mb-2 overflow-x-auto rounded bg-zinc-900 px-2 py-1 font-mono text-zinc-400"
                  data-annotate="text-ai-fix-original">{activeTab.sql.trim()}</pre>
                <p class="mb-1 text-zinc-500">Suggested SQL:</p>
                <pre
                  class="overflow-x-auto rounded bg-zinc-900 px-2 py-1 font-mono text-emerald-300"
                  data-annotate="text-ai-fix-suggested">{activeTab.fixSuggestion}</pre>
              </div>
            </div>
          {/if}
        </div>
      {:else if activeTab?.cancelled}
        <p
          class="px-3 py-2 text-xs text-amber-400"
          data-annotate="text-query-cancelled"
        >
          Query cancelled
        </p>
      {:else if activeTab?.result && activeTab.result.columns.length > 0}
        {@const result = activeTab.result}
        <table class="min-w-full border-collapse font-mono text-xs select-none">
          <!-- WKWebView は border-collapse テーブルの thead / tr に付けた
               背景・sticky を描画しないため (下の行が透けて見える)、各 th に
               直接 sticky と不透明背景を付ける。選択時の色味は不透明な th の
               上に重なるよう内側の button 側に載せる -->
          <thead>
            <tr>
              <th
                class="sticky top-0 z-10 border-b border-r border-zinc-700 bg-zinc-800 px-2 py-1 text-right font-normal text-zinc-500"
              >
                #
              </th>
              {#each result.columns as column, colIndex (colIndex)}
                <!-- ヘッダクリックでその列を選択。ドラッグで複数列に拡張 -->
                <th
                  class="sticky top-0 z-10 border-b border-r border-zinc-700 bg-zinc-800 p-0 text-left font-semibold {isColHeaderSelected(
                    colIndex,
                  )
                    ? 'text-zinc-100'
                    : 'text-zinc-300'}"
                >
                  <button
                    class="block w-full cursor-pointer px-2 py-1 text-left {isColHeaderSelected(
                      colIndex,
                    )
                      ? 'bg-sky-800/50'
                      : ''}"
                    title="Click to select this column (drag to select more)"
                    data-annotate="button-result-col-header-{colIndex}"
                    onpointerdown={(e) => beginSelect("col", 0, colIndex, e)}
                    onpointerenter={(e) => extendSelect("col", 0, colIndex, e)}
                  >
                    {column}
                  </button>
                </th>
              {/each}
            </tr>
          </thead>
          <tbody>
            {#each result.rows as row, rowIndex (rowIndex)}
              <tr class="hover:bg-zinc-800/60">
                <!-- 行番号クリックでその行を選択。ドラッグで複数行に拡張 -->
                <td
                  class="border-b border-r border-zinc-800 p-0 text-right {isRowHeaderSelected(
                    rowIndex,
                  )
                    ? 'bg-sky-800/50 text-zinc-300'
                    : 'text-zinc-600'}"
                >
                  <button
                    class="block w-full cursor-pointer px-2 py-0.5 text-right"
                    title="Click to select this row (drag to select more)"
                    data-annotate="button-result-row-header-{rowIndex}"
                    onpointerdown={(e) => beginSelect("row", rowIndex, 0, e)}
                    onpointerenter={(e) => extendSelect("row", rowIndex, 0, e)}
                  >
                    {rowIndex + 1}
                  </button>
                </td>
                {#each row as value, colIndex (colIndex)}
                  {@const pending = pendingInput(rowIndex, colIndex)}
                  {@const editable = isCellEditable(rowIndex, colIndex)}
                  <!-- クリックでセルインスペクタ、ドラッグで矩形選択、
                       ダブルクリックで編集 (編集不可セルは読み取り専用ビュー)。
                       truncate のためボタン/入力をセル全面に敷く -->
                  <td
                    class="relative max-w-96 border-b border-r border-zinc-800 p-0 {pending !==
                    null
                      ? 'bg-amber-900/40'
                      : cellBgClass(rowIndex, colIndex)}"
                  >
                    {#if isEditingCell(rowIndex, colIndex)}
                      {#if editingCell?.readonly}
                        <!-- 読み取り専用ビュー: 全文を選択済みで開き、コピーだけできる。
                             input は値サニタイズで改行を除去するため textarea を使う -->
                        <!-- svelte-ignore a11y_autofocus -->
                        <textarea
                          class="block w-full resize-none bg-zinc-950 px-2 py-0.5 font-mono text-xs text-zinc-200 ring-1 ring-sky-500 outline-none"
                          data-annotate="input-result-cell-view-{rowIndex}-{colIndex}"
                          value={editingValue}
                          rows="1"
                          readonly
                          autofocus
                          onfocus={(e) => e.currentTarget.select()}
                          onkeydown={onEditKeydown}
                          onblur={cancelCellEdit}
                        ></textarea>
                      {:else}
                        <!-- svelte-ignore a11y_autofocus -->
                        <input
                          class="block w-full bg-zinc-950 px-2 py-0.5 font-mono text-xs text-amber-100 ring-1 ring-amber-500 outline-none"
                          data-annotate="input-result-cell-{rowIndex}-{colIndex}"
                          bind:value={editingValue}
                          autofocus
                          onkeydown={onEditKeydown}
                          onblur={commitCellEdit}
                        />
                      {/if}
                    {:else}
                      <button
                        class="block w-full truncate px-2 py-0.5 text-left {editable
                          ? 'cursor-cell'
                          : 'cursor-pointer'} {pending !== null
                          ? 'text-amber-200'
                          : value === null
                            ? 'italic text-zinc-600'
                            : 'text-zinc-200'}"
                        title={editable
                          ? "Double-click to edit"
                          : cellText(value)}
                        data-annotate="button-result-cell-{rowIndex}-{colIndex}"
                        onpointerdown={(e) =>
                          beginSelect("cell", rowIndex, colIndex, e)}
                        onpointerenter={(e) =>
                          extendSelect("cell", rowIndex, colIndex, e)}
                        onclick={() => onCellClick(rowIndex, colIndex)}
                        ondblclick={() => beginCellEdit(rowIndex, colIndex)}
                      >
                        {pending !== null ? pending : cellText(value)}
                      </button>
                      {#if isSelectedCell(rowIndex, colIndex)}
                        {@const copied =
                          copiedCellKey === `${rowIndex}:${colIndex}`}
                        <!-- アクティブセルのコピーアイコン。button の入れ子は
                             不正なため td 直下に重ねて配置する -->
                        <button
                          class="absolute top-1/2 right-0.5 -translate-y-1/2 rounded bg-zinc-800/90 px-1 {copied
                            ? 'text-emerald-400'
                            : 'text-zinc-400 hover:text-zinc-100'}"
                          title="Copy this cell value"
                          aria-label="Copy this cell value"
                          data-annotate="button-copy-cell-{rowIndex}-{colIndex}"
                          onclick={() => copyCellValue(rowIndex, colIndex)}
                        >
                          <i
                            class="bi {copied
                              ? 'bi-clipboard-check'
                              : 'bi-clipboard'}"
                            aria-hidden="true"
                          ></i>
                        </button>
                      {/if}
                    {/if}
                  </td>
                {/each}
              </tr>
            {/each}
          </tbody>
        </table>
      {:else if activeTab?.result}
        {@const result = activeTab.result}
        <pre
          class="px-3 py-2 font-mono text-xs whitespace-pre-wrap text-zinc-400"
          data-annotate="text-no-result-set">{noResultSetText(result)}</pre>
      {:else}
        <p class="px-3 py-2 text-xs text-zinc-500">
          Press Cmd+Enter (Ctrl+Enter) to run the SQL statement under the cursor
        </p>
      {/if}
    </div>

    <!-- セルインスペクタ (セル選択中のみ表示) -->
    {#if inspectedCell}
      <CellInspector
        value={inspectedCell.value}
        column={inspectedCell.column}
        rowIndex={inspectedCell.rowIndex}
        onclose={() => (selectedCell = null)}
      />
    {/if}
  </div>
</div>

<!-- AI による実行計画解説のモーダル -->
{#if appStore.aiAnalysis !== null}
  <AiAnalysisModal
    text={appStore.aiAnalysis}
    onClose={() => appStore.closeAiAnalysis()}
  />
{/if}

<!-- セル編集の UPDATE プレビュー -->
{#if previewStatements !== null}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-6"
    data-annotate="modal-edits-preview"
  >
    <div
      class="flex max-h-[80vh] w-full max-w-3xl flex-col rounded border border-zinc-700 bg-zinc-900 shadow-xl"
    >
      <div
        class="flex items-center gap-2 border-b border-zinc-700 px-3 py-2 text-sm text-zinc-300"
      >
        <span class="font-semibold">
          SQL to run ({previewStatements.length} statement{previewStatements.length ===
          1
            ? ""
            : "s"}, one transaction)
        </span>
        <button
          class="ml-auto rounded px-1.5 py-0.5 text-zinc-400 hover:bg-zinc-700 hover:text-zinc-200"
          title="Close"
          aria-label="Close"
          data-annotate="button-edits-preview-close"
          onclick={() => (previewStatements = null)}
        >
          <i class="bi bi-x-lg" aria-hidden="true"></i>
        </button>
      </div>
      <pre
        class="min-h-0 flex-1 overflow-auto px-3 py-2 font-mono text-xs text-zinc-200"
        data-annotate="text-edits-preview-sql">{previewStatements
          .map((s) => `${s};`)
          .join("\n")}</pre>
      <div
        class="flex items-center justify-end gap-1 border-t border-zinc-700 px-3 py-2 text-xs"
      >
        <button
          class="rounded border border-zinc-700 px-2 py-0.5 text-zinc-300 hover:bg-zinc-700"
          data-annotate="button-edits-preview-to-editor"
          onclick={editInEditor}
        >
          Paste to editor
        </button>
        <button
          class="rounded border border-emerald-600/60 bg-emerald-900/40 px-2 py-0.5 text-emerald-200 hover:bg-emerald-800/50 disabled:cursor-default disabled:opacity-40 disabled:hover:bg-emerald-900/40"
          data-annotate="button-edits-preview-submit"
          disabled={submitDisabled}
          onclick={submitEdits}
        >
          Submit
        </button>
      </div>
    </div>
  </div>
{/if}
