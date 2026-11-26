<script lang="ts">
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import {
    rowDetailCellText,
    rowDetailDisplay,
    rowDetailFields,
    rowDetailTsv,
  } from "$lib/rowDetail";

  interface Props {
    columns: string[];
    row: unknown[];
    /// Position of the row on screen (0-based; shown 1-based)
    displayIndex: number;
    /// Number of rows on screen (after the filter)
    total: number;
    onprev: () => void;
    onnext: () => void;
    onclose: () => void;
    /// Open one value in the cell inspector
    oninspect: (colIndex: number) => void;
    /// The root element, so that the results pane can tell whether the focus is in the panel
    panelEl?: HTMLDivElement | null;
  }

  let {
    columns,
    row,
    displayIndex,
    total,
    onprev,
    onnext,
    onclose,
    oninspect,
    panelEl = $bindable(null),
  }: Props = $props();

  // Column-name filter of the panel (kept while moving between rows)
  let query = $state("");
  const fields = $derived(rowDetailFields(columns, row, query));

  // Temporary "copied" feedback: a column index, or "all" for the TSV of the panel
  let copied = $state<number | "all" | null>(null);
  const flashCopied = (key: number | "all") => {
    copied = key;
    setTimeout(() => {
      if (copied === key) copied = null;
    }, 1500);
  };

  // navigator.clipboard can trigger an OS permission prompt in Tauri 2, so use the plugin
  const copyValue = async (colIndex: number) => {
    await writeText(rowDetailCellText(row[colIndex]));
    flashCopied(colIndex);
  };
  const copyAll = async () => {
    await writeText(rowDetailTsv(fields));
    flashCopied("all");
  };
</script>

<div
  class="flex w-96 shrink-0 flex-col border-l border-zinc-700 bg-zinc-900"
  data-annotate="panel-row-detail"
  bind:this={panelEl}
>
  <!-- Header: row number, previous / next, close -->
  <div
    class="flex shrink-0 items-center gap-2 border-b border-zinc-700 px-3 py-1.5 text-xs text-zinc-400"
  >
    <span class="font-semibold text-zinc-300" data-annotate="text-row-detail-position">
      Row {displayIndex + 1} of {total}
    </span>
    <span class="flex items-center gap-0.5">
      <button
        class="rounded px-1 text-zinc-500 hover:bg-zinc-700 hover:text-zinc-200 disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent"
        title="Previous row (↑)"
        aria-label="Previous row"
        data-annotate="button-row-detail-prev"
        disabled={displayIndex <= 0}
        onclick={onprev}
      >
        <i class="bi bi-chevron-up" aria-hidden="true"></i>
      </button>
      <button
        class="rounded px-1 text-zinc-500 hover:bg-zinc-700 hover:text-zinc-200 disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent"
        title="Next row (↓)"
        aria-label="Next row"
        data-annotate="button-row-detail-next"
        disabled={displayIndex >= total - 1}
        onclick={onnext}
      >
        <i class="bi bi-chevron-down" aria-hidden="true"></i>
      </button>
    </span>
    <button
      class="ml-auto shrink-0 rounded px-1 text-zinc-500 hover:bg-zinc-700 hover:text-zinc-200"
      title="Close the row detail (Esc / Space)"
      aria-label="Close the row detail"
      data-annotate="button-row-detail-close"
      onclick={onclose}
    >
      <i class="bi bi-x" aria-hidden="true"></i>
    </button>
  </div>

  <!-- Column filter + copy of the whole panel -->
  <div
    class="flex shrink-0 items-center gap-1 border-b border-zinc-700 px-3 py-1 text-xs text-zinc-400"
  >
    <span class="relative inline-flex min-w-0 flex-1 items-center">
      <i
        class="bi bi-search pointer-events-none absolute left-1.5 text-[10px] text-zinc-500"
        aria-hidden="true"
      ></i>
      <input
        class="w-full rounded border border-zinc-600 bg-zinc-800 py-0.5 pr-1 pl-5 text-xs text-zinc-200 outline-none placeholder:text-zinc-500 focus:border-blue-400"
        type="text"
        spellcheck="false"
        autocomplete="off"
        placeholder="Filter columns"
        aria-label="Filter the columns by name"
        title="Show only the columns whose name contains every word (case-insensitive)"
        data-annotate="input-row-detail-filter"
        bind:value={query}
        onkeydown={(e) => {
          if (e.isComposing || e.keyCode === 229) {
            return;
          }
          if (e.key === "Escape" && query !== "") {
            // First Esc clears the filter; the next one closes the panel
            e.preventDefault();
            e.stopPropagation();
            query = "";
          }
        }}
      />
    </span>
    <button
      class="shrink-0 rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
      title="Copy the columns shown as 'column<TAB>value' lines"
      data-annotate="button-row-detail-copy-tsv"
      onclick={copyAll}
    >
      {copied === "all" ? "Copied!" : "Copy TSV"}
    </button>
  </div>

  <!-- Body: one column per entry -->
  <div class="min-h-0 flex-1 overflow-auto" data-annotate="list-row-detail">
    {#each fields as field (field.colIndex)}
      {@const display = rowDetailDisplay(field.value)}
      <div class="group border-b border-zinc-800 px-3 py-1 text-xs">
        <div class="flex items-center gap-1">
          <span
            class="min-w-0 truncate font-mono font-semibold text-zinc-400"
            title={field.column}
          >
            {field.column}
          </span>
          {#if display.isJson}
            <span
              class="shrink-0 rounded bg-zinc-700 px-1 py-px text-[10px] font-semibold text-zinc-300"
            >
              JSON
            </span>
          {/if}
          <button
            class="ml-auto shrink-0 rounded px-1 text-zinc-500 opacity-0 group-hover:opacity-100 hover:bg-zinc-700 hover:text-zinc-200 focus:opacity-100"
            title="Open this value in the cell inspector"
            aria-label="Open {field.column} in the cell inspector"
            data-annotate="button-row-detail-inspect-{field.colIndex}"
            onclick={() => oninspect(field.colIndex)}
          >
            <i class="bi bi-arrows-angle-expand" aria-hidden="true"></i>
          </button>
          <button
            class="shrink-0 rounded px-1 hover:bg-zinc-700 {copied === field.colIndex
              ? 'text-emerald-400'
              : 'text-zinc-500 hover:text-zinc-200'}"
            title="Copy this value"
            aria-label="Copy the value of {field.column}"
            data-annotate="button-row-detail-copy-{field.colIndex}"
            onclick={() => copyValue(field.colIndex)}
          >
            <i
              class="bi {copied === field.colIndex ? 'bi-clipboard-check' : 'bi-clipboard'}"
              aria-hidden="true"
            ></i>
          </button>
        </div>
        <!-- Clicking the value opens it in the cell inspector (full value, JSON highlighting) -->
        <button
          class="block w-full cursor-pointer text-left"
          title={display.cut
            ? "Cut for display. Click to see the full value in the cell inspector"
            : "Click to open in the cell inspector"}
          data-annotate="button-row-detail-value-{field.colIndex}"
          onclick={() => oninspect(field.colIndex)}
        >
          <pre
            class="font-mono break-all whitespace-pre-wrap {display.isNull
              ? 'text-zinc-600 italic'
              : 'text-zinc-200'}">{display.text}</pre>
        </button>
      </div>
    {:else}
      <p class="px-3 py-2 text-xs text-zinc-500" data-annotate="text-row-detail-no-match">
        No columns match the filter.
      </p>
    {/each}
  </div>
</div>
