<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { open, save } from "@tauri-apps/plugin-dialog";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";
  import { toast } from "svelte-sonner";
  import * as api from "$lib/api";
  import type { DumpInfo, DumpRequest, DumpTable } from "$lib/api";

  interface Props {
    connection: string;
    /// Database to dump. null = the connection's active schema
    database: string | null;
    /// null = the whole database
    table: DumpTable | null;
    onClose: () => void;
  }

  let { connection, database, table, onClose }: Props = $props();

  /// The tool binary chosen last time is remembered per tool, as are the extra arguments
  /// (both are about this machine / these servers, not about one dump)
  const toolPathKey = (tool: string) => `queryfolio.dump.toolPath.${tool}`;
  const extraArgsKey = (tool: string) => `queryfolio.dump.extraArgs.${tool}`;
  const CUSTOM = "__custom__";

  const INSTALL_HINTS: Record<api.DumpTool, string> = {
    mysqldump: "brew install mysql-client",
    pg_dump: "brew install libpq",
  };

  let info = $state<DumpInfo | null>(null);
  let loadError = $state<string | null>(null);
  /// Value of the tool selector: a detected path, or CUSTOM
  let toolChoice = $state("");
  let customToolPath = $state("");
  let content = $state<DumpRequest["content"]>("all");
  let format = $state<DumpRequest["format"]>("plain");
  let singleTransaction = $state(true);
  let routines = $state(false);
  let addDrop = $state(true);
  let noOwner = $state(false);
  let extraArgs = $state("");
  let preview = $state("");
  let previewError = $state<string | null>(null);

  let running = $state(false);
  let dumpId: string | null = null;
  let progressBytes = $state(0);
  let runError = $state<string | null>(null);
  let outcome = $state<api.DumpOutcome | null>(null);
  let unlisten: UnlistenFn | null = null;
  let closed = false;

  const isPg = $derived(info?.tool === "pg_dump");
  const toolPath = $derived(toolChoice === CUSTOM ? customToolPath.trim() : toolChoice);
  const targetDatabase = $derived(database ?? info?.default_database ?? null);
  const targetLabel = $derived(
    table
      ? `${table.schema ? `${table.schema}.` : ""}${table.name}`
      : (targetDatabase ?? "(default database)"),
  );

  const buildRequest = (outputPath: string): DumpRequest => ({
    toolPath,
    database,
    table,
    content,
    format: isPg ? format : "plain",
    singleTransaction,
    routines,
    // pg_dump rejects --clean with --data-only, and there is nothing to drop in a data-only dump
    addDrop: addDrop && content !== "data",
    noOwner: noOwner && format === "plain",
    extraArgs,
    outputPath,
  });

  const formatBytes = (bytes: number) => {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
    return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
  };

  onMount(async () => {
    unlisten = await listen<api.DumpProgress>("dump-progress", (event) => {
      if (event.payload.dumpId === dumpId) {
        progressBytes = event.payload.bytes;
      }
    });
    if (closed) {
      unlisten();
    }
    try {
      const result = await api.getDumpInfo(connection);
      info = result;
      const remembered = localStorage.getItem(toolPathKey(result.tool));
      if (remembered && result.tools.some((t) => t.path === remembered)) {
        toolChoice = remembered;
      } else if (remembered) {
        toolChoice = CUSTOM;
        customToolPath = remembered;
      } else if (result.tools.length > 0) {
        toolChoice = result.tools[0].path;
      } else {
        toolChoice = CUSTOM;
      }
      extraArgs = localStorage.getItem(extraArgsKey(result.tool)) ?? "";
    } catch (e) {
      loadError = String(e);
    }
  });

  onDestroy(() => {
    closed = true;
    unlisten?.();
    // Closing the window mid-dump is prevented, but if the component goes away anyway
    // (e.g. the connection list was reloaded), do not leave the tool running
    if (running && dumpId) {
      void api.cancelDump(dumpId);
    }
  });

  // Command preview, refreshed as the options change
  let previewGeneration = 0;
  $effect(() => {
    if (!info || !toolPath) {
      preview = "";
      previewError = null;
      return;
    }
    const request = buildRequest("<output file>");
    const generation = ++previewGeneration;
    const timer = setTimeout(async () => {
      try {
        const line = await api.previewDumpCommand(connection, request);
        if (generation === previewGeneration) {
          preview = line;
          previewError = null;
        }
      } catch (e) {
        if (generation === previewGeneration) {
          preview = "";
          previewError = String(e);
        }
      }
    }, 150);
    return () => clearTimeout(timer);
  });

  const browseTool = async () => {
    const picked = await open({ multiple: false, directory: false });
    if (typeof picked === "string") {
      toolChoice = CUSTOM;
      customToolPath = picked;
    }
  };

  const timestamp = () => {
    const d = new Date();
    const pad = (n: number) => String(n).padStart(2, "0");
    return (
      `${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-` +
      `${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}`
    );
  };

  const defaultFileName = () => {
    const base = [targetDatabase, table?.name].filter(Boolean).join("-") || "dump";
    // Keep the name usable on every OS
    const safe = base.replace(/[\\/:*?"<>|\s]+/g, "_");
    const ext = isPg && format === "custom" ? "dump" : "sql";
    return `${safe}-${timestamp()}.${ext}`;
  };

  const startDump = async () => {
    if (!info || running || !toolPath) {
      return;
    }
    const ext = isPg && format === "custom" ? "dump" : "sql";
    let outputPath: string | null;
    try {
      outputPath = await save({
        defaultPath: defaultFileName(),
        filters: [{ name: ext.toUpperCase(), extensions: [ext] }],
      });
    } catch (e) {
      toast.error(`Dump failed: ${e}`);
      return;
    }
    if (!outputPath || closed) {
      return;
    }
    localStorage.setItem(toolPathKey(info.tool), toolPath);
    localStorage.setItem(extraArgsKey(info.tool), extraArgs);

    const id = `dump-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    dumpId = id;
    running = true;
    progressBytes = 0;
    runError = null;
    outcome = null;
    try {
      const result = await api.runDump(connection, id, buildRequest(outputPath));
      outcome = result;
      toast.success(`Dumped ${targetLabel} (${formatBytes(result.bytes)})`);
    } catch (e) {
      const message = String(e);
      if (message.includes("Query cancelled")) {
        toast.info("Dump cancelled");
      } else {
        runError = message;
      }
    } finally {
      running = false;
      dumpId = null;
    }
  };

  const cancel = () => {
    if (dumpId) {
      void api.cancelDump(dumpId);
    }
  };

  const close = () => {
    if (!running) {
      onClose();
    }
  };

  const onKeydown = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  };
</script>

<svelte:window onkeydown={onKeydown} />

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-dump-modal"
  data-modal
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      close();
    }
  }}
>
  <div
    class="flex max-h-[90vh] w-[560px] flex-col gap-3 overflow-y-auto rounded-lg border border-zinc-700 bg-zinc-900 p-4 shadow-xl"
  >
    <h2 class="flex items-center gap-2 text-sm font-semibold text-zinc-200">
      <i class="bi bi-download" aria-hidden="true"></i>
      Dump {table ? "table" : "database"}
      <span class="truncate font-mono text-zinc-400">{targetLabel}</span>
    </h2>

    {#if loadError}
      <p class="text-xs text-red-400" data-annotate="text-dump-load-error">{loadError}</p>
    {:else if !info}
      <p class="text-xs text-zinc-500">Looking for dump tools...</p>
    {:else}
      <div class="flex flex-col gap-1">
        <label class="text-xs text-zinc-400" for="dump-tool">{info.tool} binary</label>
        <div class="flex gap-2">
          <select
            id="dump-tool"
            class="min-w-0 flex-1 rounded border border-zinc-700 bg-zinc-800 px-2 py-1 text-xs text-zinc-200"
            data-annotate="select-dump-tool"
            disabled={running}
            bind:value={toolChoice}
          >
            {#each info.tools as tool (tool.path)}
              <option value={tool.path}>
                {tool.path}{tool.version ? ` (${tool.version})` : ""}
              </option>
            {/each}
            <option value={CUSTOM}>Other...</option>
          </select>
          <button
            class="shrink-0 rounded border border-zinc-600 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-800 disabled:opacity-40"
            data-annotate="button-dump-browse-tool"
            disabled={running}
            onclick={() => void browseTool()}
          >
            Browse...
          </button>
        </div>
        {#if toolChoice === CUSTOM}
          <input
            class="rounded border border-zinc-700 bg-zinc-800 px-2 py-1 font-mono text-xs text-zinc-200"
            placeholder="/path/to/{info.tool}"
            data-annotate="input-dump-tool-path"
            disabled={running}
            bind:value={customToolPath}
          />
        {/if}
        {#if info.tools.length === 0}
          <p class="text-[11px] text-amber-400">
            {info.tool} was not found. Install it (for example
            <code class="font-mono">{INSTALL_HINTS[info.tool]}</code>) or choose the
            binary.
          </p>
        {:else if isPg}
          <p class="text-[11px] text-zinc-500">
            pg_dump must be the same or a newer major version than the server. The newest one
            found is listed first.
          </p>
        {/if}
      </div>

      <div class="grid grid-cols-2 gap-3">
        <div class="flex flex-col gap-1">
          <label class="text-xs text-zinc-400" for="dump-content">Content</label>
          <select
            id="dump-content"
            class="rounded border border-zinc-700 bg-zinc-800 px-2 py-1 text-xs text-zinc-200"
            data-annotate="select-dump-content"
            disabled={running}
            bind:value={content}
          >
            <option value="all">Structure and data</option>
            <option value="schema">Structure only</option>
            <option value="data">Data only</option>
          </select>
        </div>
        {#if isPg}
          <div class="flex flex-col gap-1">
            <label class="text-xs text-zinc-400" for="dump-format">Format</label>
            <select
              id="dump-format"
              class="rounded border border-zinc-700 bg-zinc-800 px-2 py-1 text-xs text-zinc-200"
              data-annotate="select-dump-format"
              disabled={running}
              bind:value={format}
            >
              <option value="plain">Plain SQL (.sql)</option>
              <option value="custom">Custom archive (.dump)</option>
            </select>
            <p class="text-[11px] text-zinc-500">
              Restore with {format === "custom" ? "pg_restore" : "psql"}
            </p>
          </div>
        {/if}
      </div>

      <div class="flex flex-col gap-1 text-xs text-zinc-300">
        {#if !isPg}
          <label class="flex items-center gap-2">
            <input
              type="checkbox"
              data-annotate="checkbox-dump-single-transaction"
              disabled={running}
              bind:checked={singleTransaction}
            />
            Consistent snapshot without locking (--single-transaction, InnoDB)
          </label>
          {#if !table}
            <label class="flex items-center gap-2">
              <input
                type="checkbox"
                data-annotate="checkbox-dump-routines"
                disabled={running}
                bind:checked={routines}
              />
              Include stored routines and events (--routines --events)
            </label>
          {/if}
        {/if}
        {#if !isPg || format === "plain"}
          <label class="flex items-center gap-2">
            <input
              type="checkbox"
              data-annotate="checkbox-dump-add-drop"
              disabled={running || content === "data"}
              bind:checked={addDrop}
            />
            Add DROP statements before CREATE ({isPg
              ? "--clean --if-exists"
              : "--add-drop-table"})
          </label>
        {/if}
        {#if isPg && format === "plain"}
          <label class="flex items-center gap-2">
            <input
              type="checkbox"
              data-annotate="checkbox-dump-no-owner"
              disabled={running}
              bind:checked={noOwner}
            />
            Omit owners and privileges (--no-owner --no-privileges)
          </label>
        {:else if isPg}
          <p class="text-[11px] text-zinc-500">
            DROP statements and owners of an archive are chosen when restoring (pg_restore
            --clean --if-exists / --no-owner --no-privileges).
          </p>
        {/if}
      </div>

      <div class="flex flex-col gap-1">
        <label class="text-xs text-zinc-400" for="dump-extra-args">Extra arguments</label>
        <input
          id="dump-extra-args"
          class="rounded border border-zinc-700 bg-zinc-800 px-2 py-1 font-mono text-xs text-zinc-200"
          placeholder={isPg ? "--exclude-table-data=logs" : "--column-statistics=0"}
          data-annotate="input-dump-extra-args"
          disabled={running}
          bind:value={extraArgs}
        />
        {#if !isPg}
          <p class="text-[11px] text-zinc-500">
            mysqldump 8 against MySQL 5.7 / MariaDB fails with "Unknown table
            'COLUMN_STATISTICS'": add <code class="font-mono">--column-statistics=0</code>.
          </p>
        {/if}
      </div>

      <div class="flex flex-col gap-1">
        <span class="text-xs text-zinc-400">
          Command{info.via_ssh_tunnel ? " (through a dedicated SSH tunnel)" : ""}
        </span>
        {#if previewError}
          <p class="text-xs text-red-400" data-annotate="text-dump-preview-error">
            {previewError}
          </p>
        {:else}
          <pre
            class="max-h-32 overflow-auto whitespace-pre-wrap break-all rounded bg-zinc-950 px-2 py-1 font-mono text-[11px] text-zinc-400"
            data-annotate="text-dump-preview">{preview}</pre>
        {/if}
      </div>

      {#if running}
        <p class="flex items-center gap-2 text-xs text-zinc-300" data-annotate="text-dump-progress">
          <i class="bi bi-arrow-repeat animate-spin" aria-hidden="true"></i>
          Dumping... {formatBytes(progressBytes)}
        </p>
      {/if}
      {#if runError}
        <pre
          class="max-h-40 overflow-auto whitespace-pre-wrap break-all rounded border border-red-900 bg-red-950/40 px-2 py-1 font-mono text-[11px] text-red-300"
          data-annotate="text-dump-error">{runError}</pre>
      {/if}
      {#if outcome}
        <div class="flex flex-col gap-1 text-xs" data-annotate="text-dump-done">
          <p class="text-emerald-400">
            Saved {formatBytes(outcome.bytes)} to
            <span class="break-all font-mono">{outcome.path}</span>
          </p>
          {#if outcome.warnings}
            <pre
              class="max-h-32 overflow-auto whitespace-pre-wrap break-all rounded bg-zinc-950 px-2 py-1 font-mono text-[11px] text-amber-300">{outcome.warnings}</pre>
          {/if}
        </div>
      {/if}
    {/if}

    <div class="flex justify-end gap-2">
      {#if outcome}
        <button
          class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
          data-annotate="button-dump-reveal"
          onclick={() => outcome && void revealItemInDir(outcome.path)}
        >
          Show in folder
        </button>
      {/if}
      {#if running}
        <button
          class="rounded border border-red-700 px-3 py-1 text-xs text-red-300 hover:bg-red-950"
          data-annotate="button-dump-cancel"
          onclick={cancel}
        >
          Cancel dump
        </button>
      {:else}
        <button
          class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
          data-annotate="button-dump-close"
          onclick={close}
        >
          Close
        </button>
      {/if}
      <button
        class="rounded bg-blue-600 px-3 py-1 text-xs text-white hover:bg-blue-500 disabled:opacity-40"
        data-annotate="button-dump-start"
        disabled={!info || running || !toolPath || previewError !== null}
        onclick={() => void startDump()}
      >
        {outcome || runError ? "Dump again..." : "Dump..."}
      </button>
    </div>
  </div>
</div>
