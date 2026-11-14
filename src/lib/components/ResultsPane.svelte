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
    MARKDOWN_MAX_CELL_CHARS,
    toMarkdownTable,
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
  import { nextSort, sortedRowOrder, type SortState } from "$lib/resultSort";
  import {
    buildSearchIndex,
    filterRowOrder,
    MAX_CELL_SEARCH_CHARS,
    visibleRows,
  } from "$lib/resultFilter";
  import { filterTerms } from "$lib/paneFilter";
  import CellInspector from "./CellInspector.svelte";
  import AiAnalysisModal from "./AiAnalysisModal.svelte";

  interface Props {
    /// Open the process list pane for the given connection
    onOpenProcessList?: (connection: string) => void;
  }

  let { onOpenProcessList }: Props = $props();

  // Output format shared by Copy / Export / Cmd+C copy.
  const COPY_FORMATS = ["tsv", "csv", "json", "markdown"] as const;
  type CopyFormat = (typeof COPY_FORMATS)[number];
  // File extension (and save dialog filter name) of each format
  const EXPORT_FILE_TYPES: Record<CopyFormat, { ext: string; name: string }> = {
    tsv: { ext: "tsv", name: "TSV" },
    csv: { ext: "csv", name: "CSV" },
    json: { ext: "json", name: "JSON" },
    markdown: { ext: "md", name: "Markdown" },
  };
  const COPY_FORMAT_KEY = "queryfolio.results.copyFormat";
  const isCopyFormat = (v: string): v is CopyFormat =>
    (COPY_FORMATS as readonly string[]).includes(v);
  function loadCopyFormat(): CopyFormat {
    try {
      const v = localStorage.getItem(COPY_FORMAT_KEY);
      if (v && isCopyFormat(v)) {
        return v;
      }
    } catch {
      // Continue with the default even if localStorage is unavailable
    }
    return "tsv";
  }
  let copyFormat = $state<CopyFormat>(loadCopyFormat());
  $effect(() => {
    try {
      localStorage.setItem(COPY_FORMAT_KEY, copyFormat);
    } catch {
      // Behavior continues even if localStorage is unavailable
    }
  });

  // Temporary feedback display for the Copy / Export buttons
  let copiedWhole = $state(false);
  let exported = $state(false);

  // Character-encoding selection menu for Export (the ▼ of the split button).
  // The main button is UTF-8, so only non-UTF-8 encodings are listed here.
  const EXPORT_ENCODINGS: { value: api.ExportEncoding; label: string }[] = [
    { value: "cp932", label: "CP932" },
    { value: "euc-jp", label: "EUC-JP" },
  ];
  let exportMenuOpen = $state(false);

  const activeTab = $derived(appStore.activeResultTab);

  // ---- Client-side sort of the fetched rows (CYBERNEURA-DEV-1206) ----
  // The sort belongs to one result (tab + executedAt): switching tabs or re-running
  // leaves it behind, so a stale state never applies to another result. `result.rows` is
  // never reordered; `rowOrder` maps display positions to original row indexes, and
  // everything that identifies a row's data (cell edits, pending edits, the inspector) keeps
  // using the original index. The rectangular selection is a visual range, so it stays in
  // display positions (see $lib/resultSort.ts).
  let sort = $state<(SortState & { tabId: number; executedAt: number }) | null>(
    null,
  );
  const activeSort = $derived.by<SortState | null>(() => {
    const tab = activeTab;
    if (!sort || !tab || sort.tabId !== tab.id || sort.executedAt !== tab.executedAt) {
      return null;
    }
    return { colIndex: sort.colIndex, direction: sort.direction };
  });

  // ---- Client-side filter of the fetched rows (CYBERNEURA-DEV-1266) ----
  // One filter per result tab, kept while the tab shows the same result: like the sort it is
  // keyed by executedAt, so a re-run drops it. Unlike the sort it survives switching tabs
  // (coming back to a tab shows the rows that were being looked at).
  // `input` is what is in the text box, `query` the debounced value the rows are filtered by.
  interface TabFilter {
    executedAt: number;
    input: string;
    query: string;
  }
  const FILTER_DEBOUNCE_MS = 150;
  let filters = $state<Record<number, TabFilter>>({});
  const filterTimers = new Map<number, ReturnType<typeof setTimeout>>();
  let filterInputEl = $state<HTMLInputElement | null>(null);
  const activeFilter = $derived.by<TabFilter | null>(() => {
    const tab = activeTab;
    const filter = tab ? filters[tab.id] : undefined;
    return tab && filter && filter.executedAt === tab.executedAt ? filter : null;
  });
  const filterOpen = $derived(activeFilter !== null);
  const filterQuery = $derived(activeFilter?.query ?? "");
  const activeFilterTerms = $derived(filterTerms(filterQuery));
  const filterActive = $derived(activeFilterTerms.length > 0);
  // The search text of every row, built once per result while the filter is open and reused
  // for every keystroke (it does not depend on the query)
  const searchIndex = $derived.by(() =>
    filterOpen ? buildSearchIndex(activeTab?.result?.rows ?? []) : undefined,
  );
  const dropFilter = (tabId: number) => {
    const timer = filterTimers.get(tabId);
    if (timer !== undefined) {
      clearTimeout(timer);
      filterTimers.delete(tabId);
    }
    delete filters[tabId];
  };
  // Open the filter of the active tab (or focus it when it is already open)
  const openFilter = () => {
    const tab = activeTab;
    if (!tab?.result || tab.result.columns.length === 0) {
      return;
    }
    if (activeFilter) {
      filterInputEl?.focus();
      filterInputEl?.select();
      return;
    }
    filters[tab.id] = { executedAt: tab.executedAt, input: "", query: "" };
  };
  // Close the filter of the active tab and show all rows again
  const closeFilter = () => {
    const tab = activeTab;
    if (!tab) {
      return;
    }
    const hadFocus = document.activeElement === filterInputEl;
    dropFilter(tab.id);
    if (hadFocus) {
      // Keep the keyboard on the result pane (Cmd+F / Cmd+C / Cmd+A are handled only there)
      gridEl?.focus({ preventScroll: true });
    }
  };
  const onFilterInput = (value: string) => {
    const tab = activeTab;
    const filter = activeFilter;
    if (!tab || !filter) {
      return;
    }
    filter.input = value;
    const tabId = tab.id;
    const executedAt = tab.executedAt;
    clearTimeout(filterTimers.get(tabId));
    // Debounced: filtering walks every fetched row, which is too much to do per keystroke
    // on a result fetched without default_limit
    filterTimers.set(
      tabId,
      setTimeout(() => {
        filterTimers.delete(tabId);
        const current = filters[tabId];
        if (current && current.executedAt === executedAt) {
          current.query = current.input;
        }
      }, FILTER_DEBOUNCE_MS),
    );
  };
  // Forget the filters of closed tabs and of replaced results (re-run), so that they neither
  // pile up nor come back
  $effect(() => {
    const live = new Map(appStore.resultTabs.map((tab) => [tab.id, tab.executedAt]));
    for (const [key, filter] of Object.entries(filters)) {
      const tabId = Number(key);
      if (live.get(tabId) !== filter.executedAt) {
        dropFilter(tabId);
      }
    }
  });
  // Focus the input when the filter is opened by the button or Cmd+F. Coming back to a tab
  // whose filter is open must not steal the focus, so this is requested explicitly
  let filterFocusPending = false;
  $effect(() => {
    if (filterInputEl && filterFocusPending) {
      filterFocusPending = false;
      filterInputEl.focus();
    }
  });
  const openFilterAndFocus = () => {
    filterFocusPending = !activeFilter;
    openFilter();
  };

  // All fetched rows in sorted order. Kept apart from the filter so that typing in the filter
  // does not sort again
  const sortedOrder = $derived(
    sortedRowOrder(activeTab?.result?.rows ?? [], activeSort),
  );
  // Display position -> original row index: the rows that pass the filter, in sorted order
  const rowOrder = $derived(
    filterActive && searchIndex
      ? filterRowOrder(sortedOrder, searchIndex, activeFilterTerms)
      : sortedOrder,
  );
  // Original row index -> display position (the inverse of rowOrder)
  const rowToDisplay = $derived.by(() => {
    const map = new Map<number, number>();
    rowOrder.forEach((rowIndex, displayIndex) => map.set(rowIndex, displayIndex));
    return map;
  });
  // The result in display order, for the operations that follow what is on screen
  // (Cmd+C of a selection). Same object as the result when neither sorted nor filtered
  const displayedResult = $derived.by(() => {
    const result = activeTab?.result;
    if (!result) {
      return null;
    }
    if (!activeSort && !filterActive) {
      return result;
    }
    return { ...result, rows: rowOrder.map((i) => result.rows[i]) };
  });
  const toggleSort = (colIndex: number) => {
    const tab = activeTab;
    if (!tab?.result) {
      return;
    }
    const next = nextSort(activeSort, colIndex);
    sort = next ? { ...next, tabId: tab.id, executedAt: tab.executedAt } : null;
  };
  const sortDirectionOf = (colIndex: number): "asc" | "desc" | null =>
    activeSort?.colIndex === colIndex ? activeSort.direction : null;

  /// Build the execution result of a statement without a result set (INSERT / UPDATE / DELETE / DDL etc.)
  /// as a raw execution message in the style of a DB console. The last line carries the
  /// [ No result set ] label to make clear that there was no tabular result.
  function noResultSetText(result: api.QueryResult): string {
    const lines: string[] = [];
    if (result.affected_rows !== null) {
      const n = result.affected_rows;
      lines.push(`Query OK, ${n} row${n === 1 ? "" : "s"} affected`);
    } else {
      // A rare case that is fetch-type but yields neither columns nor rows (SHOW etc. that cannot be described)
      lines.push("Query executed. No rows returned.");
    }
    lines.push(`Elapsed: ${result.elapsed_ms} ms`);
    lines.push("");
    lines.push("[ No result set ]");
    return lines.join("\n");
  }

  /// Display condition for the Analyze with AI button: an EXPLAIN-derived tab has a result and
  /// AI is configured
  const canAnalyzePlan = $derived(
    activeTab !== null &&
      activeTab.result !== null &&
      activeTab.batch === null &&
      isExplainSql(activeTab.sql) &&
      (appStore.aiInfo?.configured ?? false),
  );

  // The cell shown in the inspector. We remember which tab's cell it is via tabId,
  // so that we do not keep showing another tab's cell after a tab switch / tab close
  let selectedCell = $state<{
    tabId: number;
    rowIndex: number;
    colIndex: number;
  } | null>(null);

  // Rectangular selection of the result table. The range runs from anchor to focus.
  // mode: cell = rectangle based on a single cell, row = whole rows, col = whole columns.
  // We remember the target tab via tabId and discard the selection if it falls out of range after a tab switch or re-execution.
  let selection = $state<{
    tabId: number;
    mode: "cell" | "row" | "col";
    anchorRow: number;
    anchorCol: number;
    focusRow: number;
    focusCol: number;
  } | null>(null);

  // Drag-selection state (no $state needed: internal flags not used for rendering)
  let dragging = false;
  let dragMode: "cell" | "row" | "col" | null = null;
  // Whether the drag crossed cells. Used to tell it apart from a simple click,
  // so the cell inspector is not opened/closed by mistake after a drag
  let dragMoved = false;

  // Focus target used to limit keyboard operations (Cmd+C / Cmd+A) to the result grid
  let gridEl = $state<HTMLDivElement | null>(null);

  // ---- Row virtualization of the result table ----
  // Putting all rows x all columns in the DOM makes layout cost grow in proportion to the cell count,
  // and even typing in the SQL editor in the same document stutters (measured: 500 rows x
  // 60 columns = 31,000 cells costs 17ms per keystroke, 2,000 cells 1ms, 0 cells 0ms).
  // In addition, every cell subscribes to the selection via cellBgClass / isSelectedCell,
  // so the effects of all cells are re-evaluated on every drag selection. We render only the visible range +
  // overscan to keep the cell count in the low thousands.
  const ROW_OVERSCAN = 10;
  const DEFAULT_ROW_HEIGHT = 21;
  /// Upper limit of the column width (in characters). Equivalent to the previous max-w-96 (384px)
  const MAX_COL_CHARS = 48;
  /// Estimated column width for object values (in characters). JSON.stringify-ing all rows
  /// just to decide the width would defeat the point of virtualization, so we do not measure
  const OBJECT_WIDTH_CHARS = 24;
  /// Width per cell other than characters: padding (px-2 = 8px * 2) + 1px border + 3px margin
  const CELL_CHROME_PX = 20;

  let rowHeight = $state(DEFAULT_ROW_HEIGHT);
  let gridScrollTop = $state(0);
  let gridViewportHeight = $state(0);

  /// Return the display width in a monospace font as "how many half-width characters". CSS ch is the half-width (width of 0)
  /// advance, so using `String.length` as ch as-is makes full-width characters such as Japanese
  /// take half the width and get truncated instead of stretched under table-layout: fixed.
  /// An approximation that counts East Asian Wide / Fullwidth ranges as 2 is enough.
  /// Always write the ranges with \u escapes. Writing them as character literals can yield
  /// unintended code points even when they look the same (a character meant as CJK Compatibility Ideograph U+F900 became
  /// U+8C48, so the range swallowed the surrogate area and all characters in the supplementary planes
  /// were counted as width 2). Iterating by code point with the u flag + for...of also
  /// handles emoji and CJK Extension B and beyond explicitly
  const WIDE_CHAR =
    /[\u1100-\u115F\u2E80-\u303E\u3041-\u33FF\u3400-\u4DBF\u4E00-\u9FFF\uA000-\uA4CF\uAC00-\uD7A3\uF900-\uFAFF\uFE10-\uFE19\uFE30-\uFE6F\uFF00-\uFF60\uFFE0-\uFFE6\u{1F300}-\u{1FAFF}\u{20000}-\u{3FFFD}]/u;
  /// Stop once limit is reached. The caller caps the result at MAX_COL_CHARS, so
  /// the value is exactly equivalent, but without stopping, the cost is proportional to the **total character count** rather than the cell count,
  /// causing second-scale freezes on long TEXT columns (measured: 500 rows x 3 columns x 100KB: 1,971ms ->
  /// 1ms with the cutoff). db.rs does not truncate cell character counts on the sqlx path,
  /// so a 100KB TEXT or a long JSON string reaches here as is
  const displayWidth = (s: string, limit = Infinity): number => {
    let w = 0;
    for (const ch of s) {
      w += WIDE_CHAR.test(ch) ? 2 : 1;
      if (w >= limit) {
        return limit;
      }
    }
    return w;
  };

  // The range of rows to render, and the heights of the top/bottom spacers that preserve the scroll amount.
  // The sticky header height is offset by "distance from the content top" and "amount hidden by the header",
  // so it does not enter the computation of start
  const rowWindow = $derived.by(() => {
    // The rows on screen (fewer than result.rows while a filter is active)
    const total = rowOrder.length;
    const h = rowHeight > 0 ? rowHeight : DEFAULT_ROW_HEIGHT;
    if (total === 0) {
      return { start: 0, end: 0, padTop: 0, padBottom: 0 };
    }
    const visible = Math.ceil((gridViewportHeight || 600) / h);
    // start must always stay within the last row. Right after switching to a result with fewer rows,
    // gridScrollTop can remain larger than the new total height, and without clamping,
    // start > end renders 0 rows (only the column header and a huge blank area)
    const start = Math.min(
      Math.max(0, total - 1),
      Math.max(0, Math.floor(gridScrollTop / h) - ROW_OVERSCAN),
    );
    const end = Math.min(total, start + visible + ROW_OVERSCAN * 2);
    return {
      start,
      end,
      padTop: start * h,
      padBottom: (total - end) * h,
    };
  });

  // Column widths. With virtualization, the auto-layout column widths would be decided only by the "cells being rendered",
  // and widths would shift on every scroll, so we use table-layout: fixed + fixed widths
  // computed from the actual data. Since the font is monospace, widths can be derived directly from
  // character counts in ch units (no DOM measurement needed). To padding (px-2 = 8px * 2) + 1px border = 17px,
  // we add 3px of margin so that subpixel rounding does not cut a one-character value like `y…`.
  //
  // table-layout: fixed is **effective only when the table's width is not auto**, so
  // `min-width: 100%` leaves it in auto layout (the col widths become mere hints
  // and widths shift on the rows being rendered). On the other hand, `width: 100%` would compress columns when the pane is narrow
  // until they fall below the specified widths. So we explicitly give "the larger of 100% and the sum of column widths"
  // as the width: horizontal scroll when narrow, and a spacer column absorbs the surplus when wide.
  const columnWidths = $derived.by<{
    rowNum: string;
    cols: string[];
    table: string;
  } | null>(
    () => {
      const result = activeTab?.result;
      if (!result || result.columns.length === 0) {
        return null;
      }
      const lens = result.columns.map((c) => displayWidth(c, MAX_COL_CHARS));
      for (const row of result.rows) {
        for (let j = 0; j < lens.length; j++) {
          const v = row[j];
          const len =
            v === null || v === undefined
              ? 4 // "NULL"
              : typeof v === "object"
                ? OBJECT_WIDTH_CHARS
                : displayWidth(String(v), MAX_COL_CHARS);
          if (len > lens[j]) {
            lens[j] = len;
          }
        }
      }
      const capped = lens.map((n) => Math.min(n, MAX_COL_CHARS));
      const rowNumChars = String(result.rows.length).length;
      const width = (chars: number) => `calc(${chars}ch + ${CELL_CHROME_PX}px)`;
      // Sum of all columns (including the row-number column). The spacer column counts as 0
      const totalChars = capped.reduce((a, b) => a + b, rowNumChars);
      const totalPad = (capped.length + 1) * CELL_CHROME_PX;
      return {
        rowNum: width(rowNumChars),
        cols: capped.map(width),
        table: `max(100%, calc(${totalChars}ch + ${totalPad}px))`,
      };
    },
  );

  // Track the scroll amount and viewport height. Row height can drift due to theme changes etc.,
  // so re-measure it from the actually rendered rows
  const syncGridMetrics = () => {
    const el = gridEl;
    if (!el) {
      return;
    }
    gridScrollTop = el.scrollTop;
    gridViewportHeight = el.clientHeight;
    // offsetHeight is rounded to an integer, so with a fractional row height the spacer and
    // the actual row positions drift apart little by little. getBoundingClientRect keeps the fraction
    const row = el.querySelector<HTMLElement>("tbody tr[data-row-index]");
    const h = row?.getBoundingClientRect().height ?? 0;
    if (h > 0 && h !== rowHeight) {
      rowHeight = h;
    }
  };

  // When a row is unmounted, focus moves to body, and Cmd+C /
  // Cmd+A stop working even though the selection remains (because handleWindowKeydown decides by gridEl.contains(activeElement)).
  // **It cannot be restored inside the scroll handler**: at that point the row is still in the DOM,
  // focus is not on body yet, and Svelte discards the row afterward. Moreover,
  // a one-shot scroll such as PageDown produces no further scroll event.
  // So we restore it in a $effect that runs after DOM updates, after looking at the change of the rendering window.
  // When focus was intentionally moved outside the grid (editor, chat input, modal),
  // activeElement does not become body, so we do not steal the focus
  $effect(() => {
    void rowWindow;
    if (selection && gridEl && document.activeElement === document.body) {
      gridEl.focus({ preventScroll: true });
    }
  });

  $effect(() => {
    const el = gridEl;
    if (!el) {
      return;
    }
    syncGridMetrics();
    const observer = new ResizeObserver(syncGridMetrics);
    observer.observe(el);
    return () => observer.disconnect();
  });


  // Whether the Cmd+C copy includes the header row. Saved to localStorage
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
      // Behavior continues even if localStorage is unavailable
    }
  });

  // Temporary feedback display when copying the selection
  let selectionCopied = $state(false);

  // Close the inspector, the selection and the sort on tab switch (including switches caused by closing).
  // The sort is also keyed by tab + executedAt (activeSort), but without clearing it here
  // it would come back when switching to the same tab again
  $effect(() => {
    void appStore.activeTabId;
    selectedCell = null;
    selection = null;
    sort = null;
  });

  // When showing a different result, return to the top (computing the row window with the previous result's
  // scroll amount would render out of range for a result with a different row count). **Not only on tab switch but also
  // on re-execution in the same tab**: prepareTargetTab reuses tabs without a pin,
  // so activeTabId does not change even if the SQL is rewritten and run again
  $effect(() => {
    void appStore.activeTabId;
    void activeTab?.executedAt;
    // Also when the filter changes the rows on screen (the scroll amount of the previous row set
    // means nothing for the new one)
    void filterQuery;
    if (gridEl) {
      gridEl.scrollTop = 0;
      // Also reset the horizontal position. Otherwise, when switching to another result after scrolling a horizontally wide result to the right,
      // the # column and the leftmost column stay off-screen and the display starts from a column in the middle
      gridEl.scrollLeft = 0;
    }
    gridScrollTop = 0;
  });

  // The rectangular selection is a range of display positions and the inspector shows one row:
  // when the filter changes which rows are on screen, both would point at other rows
  // (or at a hidden one), so drop them
  $effect(() => {
    void filterQuery;
    selectedCell = null;
    selection = null;
  });

  // Clamp the selection to the result size and normalize it (fix min/max).
  // null if there is no active tab / result, or the selection belongs to another tab.
  const selectedRange = $derived.by<CellRange | null>(() => {
    const tab = activeTab;
    if (!selection || !tab || selection.tabId !== tab.id) {
      return null;
    }
    const result = tab.result;
    if (!result) {
      return null;
    }
    // Display positions: the last row on screen, not the last fetched row
    const maxRow = rowOrder.length - 1;
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

  // Highlight conditions for the row header (#) / column header
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
    const rows = rowOrder.length;
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
    // Focus the grid so that Cmd+C still reaches it even after a drag (when no click fires)
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
    // Safety net for when pointerup is missed outside the window. If a pointerenter
    // arrives with no button pressed (just a hover), end the drag
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
    // The selection is a range of display positions, so copy from the rows in display order
    const result = displayedResult;
    if (!range || !result) {
      return;
    }
    const text =
      copyFormat === "markdown"
        ? markdownText(result, range)
        : { csv: toCsvRange, tsv: toTsvRange, json: toJsonRange }[copyFormat](
            result,
            range,
            copyWithHeaders,
          );
    await writeText(text);
    selectionCopied = true;
    setTimeout(() => {
      selectionCopied = false;
    }, 1500);
  };

  // Whether the result grid has focus. A common guard so that key operations from the SQL editor etc.
  // are not hijacked. Also returns false while a cell input (edit input / read-only view
  // textarea) has focus, giving priority to normal key operations within the input
  const isGridKeyTarget = (): boolean => {
    if (
      document.activeElement instanceof HTMLInputElement ||
      document.activeElement instanceof HTMLTextAreaElement
    ) {
      return false;
    }
    return gridEl !== null && gridEl.contains(document.activeElement);
  };

  // Select the whole table. Returns false if there is no selectable result (leave it to the default behavior).
  // mode can stay "cell": the row/column header highlight is decided by
  // whether the fixed range after clamping covers all rows / all columns in isRowHeaderSelected / isColHeaderSelected, not by mode,
  // so a rectangle covering everything lights up both
  const selectAll = (): boolean => {
    const tab = activeTab;
    const result = tab?.result;
    if (
      !tab ||
      !result ||
      rowOrder.length === 0 ||
      result.columns.length === 0
    ) {
      return false;
    }
    selection = {
      tabId: tab.id,
      mode: "cell",
      anchorRow: 0,
      anchorCol: 0,
      focusRow: rowOrder.length - 1,
      focusCol: result.columns.length - 1,
    };
    return true;
  };

  // Cmd+C (Ctrl+C) copies the selection as CSV, Cmd+A (Ctrl+A) selects the whole table.
  // Both are handled only while the result grid has focus
  const handleWindowKeydown = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      // Esc on the grid closes the filter (Esc in the filter input is handled by the input).
      // Something else that already handled the key (a modal, the cell editor) wins
      if (filterOpen && !e.defaultPrevented && isGridKeyTarget()) {
        e.preventDefault();
        closeFilter();
      }
      return;
    }
    if (!(e.metaKey || e.ctrlKey)) {
      return;
    }
    const key = e.key.toLowerCase();
    if (key === "f") {
      // Cmd+F (Ctrl+F) opens the row filter only while the result pane has the focus (the grid
      // or the filter input itself). In the SQL editor the same key is CodeMirror's search,
      // which handles it there; this handler must not react to it
      if (
        !e.altKey &&
        !e.shiftKey &&
        (isGridKeyTarget() || document.activeElement === filterInputEl)
      ) {
        e.preventDefault();
        openFilterAndFocus();
      }
      return;
    }
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

  // Value passed to the inspector. null (= hidden) when the result is replaced, e.g. by re-execution,
  // and the selection position falls out of range
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
      // The inspector shows the row number as seen on screen (the display position when sorted)
      rowIndex: rowToDisplay.get(selectedCell.rowIndex) ?? selectedCell.rowIndex,
    };
  });

  // Clicking a cell selects it and opens the inspector. Clicking the selected cell again closes it
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

  // Cell click. If the previous action was a drag selection, suppress opening/closing the inspector
  // rowIndex is the original row index: the inspector follows the data, not the position on screen
  const onCellClick = (rowIndex: number, colIndex: number) => {
    if (dragMoved) {
      dragMoved = false;
      return;
    }
    // Re-clicking the selected cell (the action that closes the inspector) also
    // clears the single-cell rectangular selection set up on pointerdown,
    // so that no highlight remains
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

  // Temporary feedback display for the active cell's copy icon (`${row}:${col}`)
  let copiedCellKey = $state<string | null>(null);

  // Copy the active cell's value (stringification of NULL / objects is the same as the cell display)
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

  // Cell background color: the cell open in the inspector is emphasized first,
  // then the rectangular selection range is lightly emphasized.
  // rowIndex = original row index (inspector), displayIndex = position on screen (selection)
  const cellBgClass = (
    rowIndex: number,
    displayIndex: number,
    colIndex: number,
  ): string => {
    if (isSelectedCell(rowIndex, colIndex)) {
      return "bg-sky-800/60";
    }
    if (isCellSelected(displayIndex, colIndex)) {
      return "bg-sky-900/40";
    }
    return "";
  };

  // The Markdown table of a result (or of a range of it). Unlike the other formats it cuts
  // long cells (see MARKDOWN_MAX_CELL_CHARS in $lib/export.ts), so say so when that happened
  const markdownText = (result: api.QueryResult, range?: CellRange): string => {
    const { text, truncatedCells } = toMarkdownTable(result, range);
    if (truncatedCells > 0) {
      toast.warning(
        `${truncatedCells.toLocaleString()} cell(s) longer than ${MARKDOWN_MAX_CELL_CHARS.toLocaleString()} characters were cut in the Markdown table. Use CSV / TSV / JSON for the full values.`,
      );
    }
    return text;
  };

  // Stringify the whole result table in the selected format (shared by Copy / Export).
  const serializeResult = (
    format: CopyFormat,
    result: api.QueryResult | null | undefined,
  ): string | null => {
    if (!result) {
      return null;
    }
    return format === "markdown"
      ? markdownText(result)
      : { csv: toCsv, tsv: toTsv, json: toJson }[format](result);
  };

  // Prepare the result to output for Copy / Export.
  //
  // The result table display is limited by the setting default_limit, but Copy / Export
  // ignore that limit and output all rows. If no refetch is needed (default_limit was not
  // applied), use the currently displayed result as is.
  // While refetching, do not run Copy / Export twice.
  let preparingOutput = $state(false);
  const resultForOutput = async (): Promise<api.QueryResult | null> => {
    const tab = activeTab;
    if (!tab?.result) {
      return null;
    }
    // Take the sort of the result being output now: the refetch below awaits, and the tab or
    // the sort can change meanwhile (the output must follow what was on screen when it started)
    const sortAtStart = activeSort;
    const filterAtStart = filterQuery;
    let full: api.QueryResult | null = null;
    preparingOutput = true;
    try {
      full = await appStore.fetchResultWithoutDefaultLimit(tab);
    } catch (e) {
      // If the refetch fails, do not silently output the displayed result
      // (unknowingly outputting something with a different row count is more dangerous)
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
    // Copy / Export follow what is on screen: the rows that pass the filter, in the sorted order.
    // A refetched full result is filtered by the same terms and sorted by the same column
    // (still "within the fetched rows": whatever was fetched is what gets filtered and sorted)
    return visibleRows(result, sortAtStart, filterAtStart);
  };

  // Copy button: copy the whole result table to the clipboard in the selected format.
  const copyResult = async () => {
    if (preparingOutput) {
      return;
    }
    const text = serializeResult(copyFormat, await resultForOutput());
    if (text === null) {
      return;
    }
    // navigator.clipboard can trigger an OS permission prompt in Tauri 2,
    // so write via the official plugin
    await writeText(text);
    copiedWhole = true;
    setTimeout(() => {
      copiedWhole = false;
    }, 1500);
  };

  // Export button: save the whole result table to a file in the selected format.
  // Rust writes to the path chosen in the native save dialog.
  // encoding is the split button's selection (UTF-8 by default).
  const exportResult = async (encoding: api.ExportEncoding = "utf-8") => {
    if (!activeTab?.result || preparingOutput) {
      return;
    }
    const { ext, name } = EXPORT_FILE_TYPES[copyFormat];
    let path: string | null;
    try {
      path = await save({
        defaultPath: `result.${ext}`,
        filters: [{ name, extensions: [ext] }],
      });
    } catch (e) {
      toast.error(`Export failed: ${e}`);
      return;
    }
    if (!path) {
      // The user canceled the dialog
      return;
    }
    // Do the refetch and serialization after the path is confirmed (avoids wasted work on cancel)
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

  // Show the SQL on one line, shortened, for tab titles
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

  /// Whether the active tab's connection supports AI features (not supported for redis etc.).
  /// Fix with AI assumes an SQL-fix prompt, so it is not shown for unsupported engines
  const activeTabSupportsAi = $derived.by(() => {
    const conn = activeTab?.connection;
    if (!conn) return true;
    const info = appStore.connections.find((c) => c.name === conn);
    return info?.capabilities.supports_ai ?? true;
  });

  /// Title of the Fix with AI button (guides to the setup when unconfigured / on error).
  /// DB error messages can contain values, so state explicitly what is sent
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

  // ------- Editing result cells (double-click -> pending -> Preview/Edit/Submit/Cancel) -------

  /// Whether a tab is editable, plus the target table / primary key / editable columns.
  interface EditContext {
    table: string;
    pkColumns: string[];
    editableColumns: Set<string>;
  }

  // Cached per tabId. Value null = judged not editable, no key = not yet judged.
  let editContexts = $state(new Map<number, EditContext | null>());
  // tabId -> (`${rowIndex}:${column}` -> edit content)
  let pendingEdits = $state(new Map<number, Map<string, CellEdit>>());
  // The cell being edited inline (one). readonly = true means a non-editable cell is
  // opened in a read-only input (for selecting/copying the full text; nothing is written)
  let editingCell = $state<{
    tabId: number;
    rowIndex: number;
    colIndex: number;
    readonly: boolean;
  } | null>(null);
  let editingValue = $state("");
  // UPDATE statements shown in the Preview modal (null while hidden)
  let previewStatements = $state<string[] | null>(null);
  // Record for discarding the edit state of a tab whose result was refetched (executedAt changed)
  const seenExecutedAt = new Map<number, number>();

  const activeEngine = $derived.by<NormalizedEngine | null>(() => {
    const conn = activeTab?.connection;
    if (!conn) return null;
    const info = appStore.connections.find((c) => c.name === conn);
    return info ? normalizeEngine(info.engine) : null;
  });

  /// Editing is allowed only when the connection is active, Writable is ON, and config is not readonly.
  /// Always disallowed for engines that do not support cell editing (capabilities.supports_editable_cells = false,
  /// e.g. redis).
  /// Further, it is allowed only when the tab's execution-time schema matches the current active schema.
  /// The generated UPDATE is unqualified by schema and runs against "the connection's current active schema",
  /// so if the schema was switched after execution, it would update a same-named table in a different schema.
  /// On schema mismatch we make it non-editable to prevent this.
  const canEditActiveConnection = $derived(
    activeTab !== null &&
      activeTab.connection === appStore.selectedConnection &&
      (appStore.selectedCapabilities?.supports_editable_cells ?? true) &&
      appStore.writable &&
      !appStore.selectedConnectionReadonly &&
      activeTab.schema === appStore.activeSchema,
  );

  // A batch tab is never editable: its result is the per-statement summary built on the frontend
  const activeEditContext = $derived(
    activeTab && activeTab.batch === null
      ? (editContexts.get(activeTab.id) ?? null)
      : null,
  );
  const activePending = $derived(
    activeTab ? (pendingEdits.get(activeTab.id) ?? null) : null,
  );
  const pendingCount = $derived(activePending ? activePending.size : 0);
  // Disable Submit while applying (or while a query is running on the same connection) to prevent
  // double Submit and parallel execution (a double defense with the isConnectionRunning guard on the submitCellEdits side).
  const submitDisabled = $derived(
    activeTab === null || appStore.isConnectionRunning(activeTab.connection),
  );

  // Discard the edit state when the result is replaced, and request editContext if editing is possible
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
    if (
      canEditActiveConnection &&
      tab.result &&
      tab.batch === null &&
      !editContexts.has(tab.id)
    ) {
      void deriveEditContext(tab);
    }
  });

  async function deriveEditContext(tab: ResultTab) {
    const result = tab.result;
    if (!result) return;
    const rawTable = singleTableSelectTable(tab.sql);
    // Normalize the table name per engine to match the actual table (PG lowercases).
    // The normalized name is used consistently for both the PK / column lookup and the generated UPDATE.
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
        // Duplicate column names make the row/column mapping ambiguous, so not editable (safe side)
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
        ctx = null; // A fetch failure falls back to not editable
      }
    }
    // Discard if the response is stale (the result was replaced)
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

  // Whether all primary key values of the row are non-NULL. A primary key containing NULL cannot
  // uniquely identify the row (SQLite in particular allows NULL in composite / non-integer PKs, and WHERE pk IS NULL can match
  // multiple rows) and the UPDATE would affect unintended rows too, so make it non-editable.
  const rowPkComplete = (rowIndex: number): boolean => {
    const ctx = activeEditContext;
    const result = activeTab?.result;
    if (!ctx || !result) return false;
    return ctx.pkColumns.every((pk) => {
      const ci = result.columns.indexOf(pk);
      return ci >= 0 && result.rows[rowIndex]?.[ci] != null;
    });
  };

  // Object (JSON / blob) cells are excluded from inline editing
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
      // Open a non-editable cell in a read-only input to make it easy to select and copy the full text
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

  // Commit the edited value into a pending edit. **The target tab is looked up from
  // editingCell.tabId, not activeTab**: the triggers for committing include switching to another result tab
  // while still editing, and at that point activeTab is already the destination,
  // so looking at activeTab would fail to commit and the input would silently vanish
  const commitCellEdit = () => {
    const ec = editingCell;
    const tab = ec ? appStore.resultTabs.find((t) => t.id === ec.tabId) : null;
    // The read-only view is just closed and not registered as a pending edit
    if (!ec || ec.readonly || !tab || !tab.result) {
      editingCell = null;
      return;
    }
    const col = tab.result.columns[ec.colIndex];
    const row = tab.result.rows[ec.rowIndex];
    // If the result was replaced and the row is gone, do not commit (avoids out-of-range access)
    if (!row || col === undefined) {
      editingCell = null;
      return;
    }
    const original = row[ec.colIndex];
    const key = `${ec.rowIndex}:${col}`;
    const map = new Map(pendingEdits.get(tab.id) ?? []);
    // If it is restored to the same as the original display, release the pending edit
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
    if (map.size > 0) pendingEdits.set(tab.id, map);
    else pendingEdits.delete(tab.id);
    pendingEdits = new Map(pendingEdits);
    editingCell = null;
  };

  const cancelCellEdit = () => {
    editingCell = null;
  };

  // When the row being edited leaves the virtualization render range, commit it on the spot.
  // Committing relies on the input's blur, but **blur does not fire when the focused element is
  // removed from the DOM**, so if a row is scrolled out of range and another cell is then edited,
  // editingValue is overwritten and the input is silently lost
  $effect(() => {
    const ec = editingCell;
    if (!ec) {
      return;
    }
    // If switched to another tab, the input also disappears from the DOM, so commit likewise
    // (commitCellEdit pushes to the tab of editingCell.tabId, so it is correct even after the switch)
    // editingCell.rowIndex is the original index; the render window is in display positions
    const displayIndex = rowToDisplay.get(ec.rowIndex) ?? -1;
    if (
      ec.tabId !== activeTab?.id ||
      displayIndex < rowWindow.start ||
      displayIndex >= rowWindow.end
    ) {
      commitCellEdit();
    }
  });

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

  // Paste the generated UPDATE into the editor and release the pending edits (assuming they are run manually afterward).
  // insertSqlSnippet inserts into the editor of "the currently selected connection", so when the result tab's
  // connection differs from the selected connection, do not paste (prevents the accident of pouring A's UPDATE
  // into a same-named table on another connection. Aligned with Submit's connection guard).
  const editInEditor = () => {
    const tab = activeTab;
    if (!tab) return;
    if (tab.connection !== appStore.selectedConnection) {
      toast.warning(
        `These edits are for '${tab.connection}'. Switch to that connection to paste them into the editor.`,
      );
      return;
    }
    // The generated UPDATE is unqualified by schema. If the schema was switched since execution,
    // the pasted UPDATE could run against a same-named table in a different schema, so do not paste
    // (aligned with Submit's tab.schema !== activeSchema guard).
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
      // On success, re-execution replaces the result and the effect discards the pending edits, but clear them explicitly too
      clearPending();
      previewStatements = null;
    }
  };

  const cancelAllEdits = () => {
    clearPending();
    previewStatements = null;
  };
</script>

<!-- Drag selection is tracked via the cell's pointerenter, so
     pointer capture is not used and the end is caught on window -->
<svelte:window
  onpointerup={endDrag}
  onpointercancel={endDrag}
  onkeydown={handleWindowKeydown}
/>

<div class="flex h-full min-h-0 flex-col bg-zinc-900">
  <!-- Tab bar -->
  <div
    class="flex shrink-0 items-start gap-3 border-b border-zinc-700 px-3 py-1 text-xs text-zinc-400"
  >
    <span class="mt-0.5 font-semibold tracking-wide">RESULTS</span>
    {#if appStore.resultTabs.length > 0}
      <!-- Multi-row tabs: when tabs increase, wrap and show them in multiple rows -->
      <div class="flex min-w-0 flex-1 flex-wrap items-center gap-1">
        {#each appStore.resultTabs as tab (tab.id)}
          {@const color = appStore.connectionColorOf(tab.connection)}
          <div
            class="flex shrink-0 items-center gap-0.5 rounded-t border-t border-r border-l px-1 py-0.5 {tab.id ===
            appStore.activeTabId
              ? 'border-zinc-600 bg-zinc-800 text-zinc-200'
              : 'border-transparent text-zinc-500 hover:bg-zinc-800/60 hover:text-zinc-300'}"
          >
            {#if color}
              <!-- Dot in the connection's color (see ConnectionInfo.color) -->
              <span
                class="inline-block size-2 shrink-0 rounded-full"
                style:background-color={color}
                title={tab.connection}
                data-annotate="result-tab-color-{tab.id}"
                aria-hidden="true"
              ></span>
            {/if}
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

  <!-- Execution info of the active tab -->
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
        <span class="text-blue-400">
          Running...{#if activeTab.batch && activeTab.result}
            {@const done = activeTab.result.rows.filter(
              (row) => row[1] !== "running" && row[1] !== "not run",
            ).length}
            ({done}/{activeTab.batch.length})
          {/if}
        </span>
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
          {#if filterActive}
            <span
              data-annotate="text-row-count"
              title="Rows matching the filter / fetched rows"
            >
              {rowOrder.length} of {result.rows.length} rows
            </span>
          {:else}
            <span data-annotate="text-row-count">{result.row_count} rows</span>
          {/if}
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
        {#if onOpenProcessList && appStore.connections.find((c) => c.name === activeTab.connection)?.capabilities.supports_process_list}
          <!-- Show the server's processes (useful while this query is running) -->
          <button
            class="rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
            title="Show the process list of {activeTab.connection}"
            aria-label="Show the process list"
            data-annotate="button-open-process-list"
            onclick={() => onOpenProcessList(activeTab.connection)}
          >
            <i class="bi bi-activity" aria-hidden="true"></i> Processes
          </button>
        {/if}
        {#if canAnalyzePlan}
          <!-- Have AI explain the EXPLAIN plan (only for tabs where AI is configured) -->
          <button
            class="flex items-center gap-1 rounded border border-blue-500/50 bg-blue-500/15 px-1.5 py-0.5 text-blue-300 hover:bg-blue-500/25 disabled:cursor-not-allowed disabled:opacity-50"
            title="Explain the plan with AI ({appStore.aiInfo?.model})"
            data-annotate="button-analyze-plan"
            disabled={appStore.aiAnalyzing}
            onclick={() => appStore.analyzeExplainTab(activeTab.id)}
          >
            {#if appStore.aiAnalyzing}
              <!-- Spinner while explaining -->
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
          <!-- Filter the fetched rows (nothing is sent to the database) -->
          {#if activeFilter}
            <span class="relative inline-flex items-center">
              <i
                class="bi bi-search pointer-events-none absolute left-1.5 text-[10px] text-zinc-500"
                aria-hidden="true"
              ></i>
              <input
                bind:this={filterInputEl}
                class="w-44 rounded border border-zinc-600 bg-zinc-800 py-0.5 pl-5 pr-5 text-xs text-zinc-200 outline-none placeholder:text-zinc-500 focus:border-blue-400"
                type="text"
                spellcheck="false"
                autocomplete="off"
                placeholder="Filter rows"
                aria-label="Filter the fetched rows"
                title="Show only the fetched rows that contain every word (case-insensitive, in any cell). The query is not re-run. Cells are matched on their first {MAX_CELL_SEARCH_CHARS.toLocaleString()} characters"
                data-annotate="input-filter-result"
                value={activeFilter.input}
                oninput={(e) => onFilterInput(e.currentTarget.value)}
                onkeydown={(e) => {
                  // WebKit can deliver the key that commits an IME composition after
                  // compositionend, with isComposing already false; keyCode 229 still marks it
                  if (e.isComposing || e.keyCode === 229) {
                    return;
                  }
                  if (e.key === "Escape") {
                    e.preventDefault();
                    closeFilter();
                  }
                }}
              />
              <button
                class="absolute right-1 text-zinc-500 hover:text-zinc-200"
                title="Close the filter and show all rows (Esc)"
                aria-label="Close the filter"
                data-annotate="button-close-filter-result"
                onclick={closeFilter}
              >
                <i class="bi bi-x" aria-hidden="true"></i>
              </button>
            </span>
          {:else}
            <button
              class="rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
              title="Filter the fetched rows (Cmd+F / Ctrl+F while the result has the focus)"
              aria-label="Filter the fetched rows"
              data-annotate="button-filter-result"
              onclick={openFilterAndFocus}
            >
              <i class="bi bi-search" aria-hidden="true"></i>
            </button>
          {/if}
          {#if selectionCopied}
            <span class="text-emerald-400" data-annotate="text-selection-copied">
              Copied
            </span>
          {/if}
          <label
            class="flex cursor-pointer items-center gap-1 select-none hover:text-zinc-200"
            title="Include column headers when copying a cell selection with Cmd+C (Ctrl+C). CSV / TSV only: JSON always has keys and a Markdown table always has its header row"
          >
            <input
              type="checkbox"
              class="cursor-pointer accent-sky-600"
              data-annotate="checkbox-copy-with-headers"
              bind:checked={copyWithHeaders}
            />
            Copy with headers
          </label>
          <!-- Output format. Shared by Copy / Export / Cmd+C copy -->
          <select
            class="cursor-pointer rounded border border-zinc-700 bg-zinc-800 px-1 py-0.5 uppercase hover:bg-zinc-700 hover:text-zinc-200"
            title="Output format for Copy / Export and Cmd+C (Ctrl+C)"
            data-annotate="select-copy-format"
            bind:value={copyFormat}
          >
            {#each COPY_FORMATS as format (format)}
              <option value={format}>{format.toUpperCase()}</option>
            {/each}
          </select>
          <button
            class="rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
            title={filterActive
              ? "Copy the rows matching the filter to the clipboard in the selected format"
              : "Copy the whole result to the clipboard in the selected format"}
            data-annotate="button-copy-result"
            onclick={copyResult}
          >
            {copiedWhole ? "Copied!" : "Copy"}
          </button>
          <!-- Export is a split button. The main button is UTF-8, and ▼ selects the character encoding -->
          <span class="relative inline-flex">
            <button
              class="rounded-l border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
              title={filterActive
                ? "Export the rows matching the filter to a file in the selected format (UTF-8)"
                : "Export the whole result to a file in the selected format (UTF-8)"}
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
              <!-- Backdrop layer for closing on outside click -->
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

  <!-- Pending cell edit bar (only when there are uncommitted edits) -->
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
    <!-- tabindex/bind: limit Cmd+C copy and Cmd+A select-all after cell selection to the result grid
         (the window keydown handles it only when focus is inside gridEl) -->
    <div
      class="min-h-0 flex-1 overflow-auto focus:outline-none"
      tabindex="-1"
      bind:this={gridEl}
      onscroll={syncGridMetrics}
    >
      {#if appStore.errorMessage}
        <pre
          class="whitespace-pre-wrap px-3 py-2 font-mono text-xs text-red-400"
          data-annotate="text-error-message">{appStore.errorMessage}</pre>
      {:else if activeTab?.running && activeTab.batch === null}
        <!-- A batch keeps its per-statement summary visible while it runs -->
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
                  <!-- Spinner while generating the fix suggestion -->
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

          <!-- AI fix suggestion (shown side by side with the original SQL. Not executed until Apply) -->
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
        <table
          class="table-fixed border-separate font-mono text-xs select-none"
          style="border-spacing:0; width:{columnWidths?.table ?? '100%'}"
        >
          <!-- Use fixed widths so column widths do not move even as the rendered rows change with virtualization -->
          {#if columnWidths}
            <colgroup>
              <col style="width:{columnWidths.rowNum}" />
              {#each columnWidths.cols as width, colIndex (colIndex)}
                <col style="width:{width}" />
              {/each}
              <!-- Spacer column with no width specified. In table-layout: fixed the surplus space goes to
                   columns with no specified width, so this prevents each column from being
                   stretched when the table is narrower than its parent (avoids the # column growing extremely wide) -->
              <col />
            </colgroup>
          {/if}
          <!-- WKWebView does not paint the background / sticky set on the thead / tr of a
               border-collapse table (rows below show through), so sticky and an opaque background are put
               directly on each th. The selection tint is placed on the inner button so that
               it overlays the opaque th -->
          <thead>
            <tr>
              <th
                class="sticky top-0 z-10 border-b border-r border-zinc-700 bg-zinc-800 px-2 py-1 text-right font-normal text-zinc-500"
              >
                #
              </th>
              {#each result.columns as column, colIndex (colIndex)}
                {@const sortDirection = sortDirectionOf(colIndex)}
                <!-- Clicking a header selects that column. Dragging extends it to multiple columns.
                     The arrow at the right edge (or Alt+click) sorts the fetched rows by the column:
                     ascending -> descending -> off. Sorting never touches the selection behavior -->
                <th
                  class="group sticky top-0 z-10 border-b border-r border-zinc-700 bg-zinc-800 p-0 text-left font-semibold {isColHeaderSelected(
                    colIndex,
                  )
                    ? 'text-zinc-100'
                    : 'text-zinc-300'}"
                >
                  <div
                    class="flex items-stretch {isColHeaderSelected(colIndex)
                      ? 'bg-sky-800/50'
                      : ''}"
                  >
                    <button
                      class="min-w-0 flex-1 cursor-pointer truncate px-2 py-1 text-left"
                      title="{column} — Click to select this column (drag to select more). Alt+click to sort"
                      data-annotate="button-result-col-header-{colIndex}"
                      onpointerdown={(e) => {
                        if (e.altKey) {
                          // Alt+click sorts instead of selecting. Stop here so no drag selection starts
                          e.preventDefault();
                          toggleSort(colIndex);
                          return;
                        }
                        beginSelect("col", 0, colIndex, e);
                      }}
                      onpointerenter={(e) => extendSelect("col", 0, colIndex, e)}
                    >
                      {column}
                    </button>
                    <button
                      class="shrink-0 cursor-pointer px-1 py-1 text-[0.65rem] {sortDirection
                        ? 'text-sky-300'
                        : 'text-zinc-500 opacity-0 group-hover:opacity-100'}"
                      title="Sort by {column} ({sortDirection === 'asc'
                        ? 'ascending; click for descending'
                        : sortDirection === 'desc'
                          ? 'descending; click to turn off'
                          : 'ascending'}) — sorted within the fetched rows{result.truncated ||
                      result.applied_limit !== null
                        ? ', not the whole table'
                        : ''}"
                      aria-label="Sort by {column}"
                      data-annotate="button-result-col-sort-{colIndex}"
                      onclick={() => toggleSort(colIndex)}
                    >
                      <i
                        class="bi {sortDirection === 'asc'
                          ? 'bi-sort-up-alt'
                          : sortDirection === 'desc'
                            ? 'bi-sort-down'
                            : 'bi-arrow-down-up'}"
                      ></i>
                    </button>
                  </div>
                </th>
              {/each}
              <!-- Spacer column (corresponds to the last col of the colgroup) -->
              <th
                class="sticky top-0 z-10 border-b border-zinc-700 bg-zinc-800 p-0"
              ></th>
            </tr>
          </thead>
          <tbody>
            <!-- Top/bottom spacers reserve the height of unrendered rows and preserve the scroll amount -->
            {#if rowWindow.padTop > 0}
              <tr aria-hidden="true">
                <td
                  colspan={result.columns.length + 2}
                  style="height:{rowWindow.padTop}px"
                ></td>
              </tr>
            {/if}
            <!-- displayIndex = position on screen (selection, row number, data-annotate);
                 rowIndex = original index into result.rows (the data: edits, inspector, pending) -->
            {#each rowOrder.slice(rowWindow.start, rowWindow.end) as rowIndex, windowIndex (rowIndex)}
              {@const displayIndex = rowWindow.start + windowIndex}
              {@const row = result.rows[rowIndex]}
              <tr class="hover:bg-zinc-800/60" data-row-index={displayIndex}>
                <!-- Clicking a row number selects that row. Dragging extends it to multiple rows -->
                <td
                  class="border-b border-r border-zinc-800 p-0 text-right {isRowHeaderSelected(
                    displayIndex,
                  )
                    ? 'bg-sky-800/50 text-zinc-300'
                    : 'text-zinc-600'}"
                >
                  <button
                    class="block w-full cursor-pointer px-2 py-0.5 text-right"
                    title="Click to select this row (drag to select more)"
                    data-annotate="button-result-row-header-{displayIndex}"
                    onpointerdown={(e) => beginSelect("row", displayIndex, 0, e)}
                    onpointerenter={(e) => extendSelect("row", displayIndex, 0, e)}
                  >
                    {displayIndex + 1}
                  </button>
                </td>
                {#each row as value, colIndex (colIndex)}
                  {@const pending = pendingInput(rowIndex, colIndex)}
                  {@const editable = isCellEditable(rowIndex, colIndex)}
                  <!-- Click opens the cell inspector, drag does rectangular selection,
                       double-click edits (non-editable cells get a read-only view).
                       For truncate, the button/input is laid over the whole cell -->
                  <td
                    class="relative border-b border-r border-zinc-800 p-0 {pending !==
                    null
                      ? 'bg-amber-900/40'
                      : cellBgClass(rowIndex, displayIndex, colIndex)}"
                  >
                    {#if isEditingCell(rowIndex, colIndex)}
                      {#if editingCell?.readonly}
                        <!-- Read-only view: opens with the full text selected, only copying is possible.
                             input strips newlines through value sanitizing, so use a textarea -->
                        <!-- svelte-ignore a11y_autofocus -->
                        <textarea
                          class="block w-full resize-none bg-zinc-950 px-2 py-0.5 font-mono text-xs text-zinc-200 ring-1 ring-sky-500 outline-none"
                          data-annotate="input-result-cell-view-{displayIndex}-{colIndex}"
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
                          data-annotate="input-result-cell-{displayIndex}-{colIndex}"
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
                        data-annotate="button-result-cell-{displayIndex}-{colIndex}"
                        onpointerdown={(e) =>
                          beginSelect("cell", displayIndex, colIndex, e)}
                        onpointerenter={(e) =>
                          extendSelect("cell", displayIndex, colIndex, e)}
                        onclick={() => onCellClick(rowIndex, colIndex)}
                        ondblclick={() => beginCellEdit(rowIndex, colIndex)}
                      >
                        {pending !== null ? pending : cellText(value)}
                      </button>
                      {#if isSelectedCell(rowIndex, colIndex)}
                        {@const copied =
                          copiedCellKey === `${rowIndex}:${colIndex}`}
                        <!-- Copy icon of the active cell. Nesting a button is
                             invalid, so it is overlaid directly under the td -->
                        <button
                          class="absolute top-1/2 right-0.5 -translate-y-1/2 rounded bg-zinc-800/90 px-1 {copied
                            ? 'text-emerald-400'
                            : 'text-zinc-400 hover:text-zinc-100'}"
                          title="Copy this cell value"
                          aria-label="Copy this cell value"
                          data-annotate="button-copy-cell-{displayIndex}-{colIndex}"
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
                <td class="border-b border-zinc-800"></td>
              </tr>
            {/each}
            {#if rowWindow.padBottom > 0}
              <tr aria-hidden="true">
                <td
                  colspan={result.columns.length + 2}
                  style="height:{rowWindow.padBottom}px"
                ></td>
              </tr>
            {/if}
          </tbody>
        </table>
        {#if filterActive && rowOrder.length === 0}
          <p
            class="px-3 py-2 text-xs text-zinc-500"
            data-annotate="text-filter-no-match"
          >
            No fetched rows match the filter.
          </p>
        {/if}
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

    <!-- Cell inspector (shown only while a cell is selected) -->
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

<!-- Modal for the AI explanation of the execution plan -->
{#if appStore.aiAnalysis !== null}
  <AiAnalysisModal
    text={appStore.aiAnalysis}
    onClose={() => appStore.closeAiAnalysis()}
  />
{/if}

<!-- UPDATE preview for cell edits -->
{#if previewStatements !== null}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-6"
    data-annotate="modal-edits-preview"
    data-modal
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
      <!--
        The SQL wraps at line ends and the height fits the content (CYBERNEURA-DEV-578).
        With flex-1, even a short SQL stretched the modal up to 80vh, and a long SQL
        was hidden behind the horizontal scrollbar and unreadable.
        max-h-75 is Tailwind 4's dynamic spacing (0.25rem * 75), i.e. 300px.
        break-words is for long literals that have no wrap point.
      -->
      <pre
        class="max-h-75 min-h-0 overflow-auto px-3 py-2 font-mono text-xs break-words whitespace-pre-wrap text-zinc-200"
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
