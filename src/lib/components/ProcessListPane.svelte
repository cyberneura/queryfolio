<script lang="ts">
  import { onDestroy } from "svelte";
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import * as api from "$lib/api";
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    /// The connection whose server processes are listed (null = none selected)
    connection: string | null;
    /// Close the pane (flips the same state as the toolbar toggle)
    onClose: () => void;
  }

  let { connection, onClose }: Props = $props();

  const SHOW_IDLE_KEY = "queryfolio.processList.showIdle";
  const AUTO_REFRESH_KEY = "queryfolio.processList.autoRefresh";
  const AUTO_REFRESH_MS = 3000;

  function loadFlag(key: string): boolean {
    try {
      return localStorage.getItem(key) === "1";
    } catch {
      return false;
    }
  }

  function saveFlag(key: string, value: boolean) {
    try {
      localStorage.setItem(key, value ? "1" : "0");
    } catch {
      // Keep the toggle itself working even when localStorage is unavailable
    }
  }

  let processes = $state<api.ProcessInfo[]>([]);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let fetchedAt = $state<Date | null>(null);
  /// Id of the process whose details are shown. Kept across refreshes
  let selectedId = $state<string | null>(null);
  let showIdle = $state(loadFlag(SHOW_IDLE_KEY));
  let autoRefresh = $state(loadFlag(AUTO_REFRESH_KEY));
  let copied = $state(false);
  /// Bumped on connection change (and on destroy), so a slow response for the previous
  /// connection never overwrites the current one
  let generation = 0;
  /// The request in flight for the current generation. Every refresh path (opening, the
  /// button, auto refresh) shares it, so the pane never has two fetches running for one
  /// connection
  let inFlight: Promise<void> | null = null;

  const capabilities = $derived(
    appStore.connections.find((c) => c.name === connection)?.capabilities ??
      null,
  );
  const supported = $derived(capabilities?.supports_process_list === true);
  const visible = $derived(
    showIdle ? processes : processes.filter((p) => !p.idle),
  );
  const activeCount = $derived(processes.filter((p) => !p.idle).length);
  const selected = $derived(
    processes.find((p) => p.id === selectedId) ?? null,
  );

  function refresh(): Promise<void> {
    if (inFlight) {
      return inFlight;
    }
    const request = fetchProcesses();
    inFlight = request;
    void request.finally(() => {
      if (inFlight === request) {
        inFlight = null;
      }
    });
    return request;
  }

  async function fetchProcesses() {
    const target = connection;
    if (target === null || !supported) {
      return;
    }
    const gen = generation;
    loading = true;
    try {
      const result = await api.listProcesses(target);
      if (gen !== generation) return;
      processes = result;
      error = null;
      fetchedAt = new Date();
    } catch (e) {
      if (gen !== generation) return;
      error = String(e);
    } finally {
      if (gen === generation) {
        loading = false;
      }
    }
  }

  // Fetch when the pane opens and whenever the connection changes
  $effect(() => {
    void connection;
    void supported;
    // A new generation: the old request's response is dropped and no longer shared
    generation++;
    inFlight = null;
    processes = [];
    selectedId = null;
    error = null;
    fetchedAt = null;
    loading = false;
    void refresh();
  });

  // Auto refresh. setTimeout after each response (rather than setInterval) so that ticks do
  // not pile up on a slow server; refresh() joins a fetch that is already in flight
  let timer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    if (!autoRefresh || !supported || connection === null) return;
    let stopped = false;
    const tick = async () => {
      await refresh();
      if (!stopped) {
        timer = setTimeout(tick, AUTO_REFRESH_MS);
      }
    };
    timer = setTimeout(tick, AUTO_REFRESH_MS);
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  });
  onDestroy(() => {
    generation++;
    clearTimeout(timer);
  });

  function formatDuration(secs: number | null): string {
    if (secs === null) return "-";
    if (secs < 60) return `${secs}s`;
    const m = Math.floor(secs / 60);
    if (m < 60) return `${m}m ${String(secs % 60).padStart(2, "0")}s`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h}h ${String(m % 60).padStart(2, "0")}m`;
    return `${Math.floor(h / 24)}d ${String(h % 24).padStart(2, "0")}h`;
  }

  /// Long-running active processes are highlighted so the heavy ones stand out
  function timeClass(p: api.ProcessInfo): string {
    if (p.idle || p.time_secs === null) return "text-zinc-500";
    if (p.time_secs >= 60) return "text-red-400";
    if (p.time_secs >= 10) return "text-amber-400";
    return "text-zinc-300";
  }

  async function copyStatement(text: string) {
    await writeText(text);
    copied = true;
    setTimeout(() => {
      copied = false;
    }, 1500);
  }
</script>

<div
  class="flex h-full w-full flex-col border-l border-zinc-700 bg-zinc-900"
  data-annotate="pane-process-list"
>
  <div
    class="flex shrink-0 items-center gap-2 border-b border-zinc-700 px-3 py-1"
  >
    <span class="text-xs font-semibold tracking-wide text-zinc-300">
      <i class="bi bi-activity" aria-hidden="true"></i> Processes
    </span>
    {#if connection}
      <span
        class="truncate rounded bg-zinc-800 px-1.5 py-0.5 text-[10px] text-zinc-400"
        title={connection}
        data-annotate="text-process-list-connection"
      >
        {connection}
      </span>
    {/if}
    <div class="ml-auto flex shrink-0 items-center gap-1">
      <button
        type="button"
        class="rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200 disabled:cursor-not-allowed disabled:opacity-50"
        data-annotate="button-process-list-refresh"
        title="Refresh the process list"
        aria-label="Refresh the process list"
        disabled={!supported || loading}
        onclick={() => void refresh()}
      >
        <i
          class="bi bi-arrow-clockwise inline-block"
          class:animate-spin={loading}
          aria-hidden="true"
        ></i>
      </button>
      <button
        type="button"
        class="rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200"
        data-annotate="button-process-list-close"
        title="Close the process list pane"
        aria-label="Close the process list pane"
        onclick={onClose}
      >
        <i class="bi bi-x-lg" aria-hidden="true"></i>
      </button>
    </div>
  </div>

  {#if connection === null}
    <p class="px-3 py-2 text-xs leading-relaxed text-zinc-500">
      Select a connection first.
    </p>
  {:else if !supported}
    <p
      class="px-3 py-2 text-xs leading-relaxed text-zinc-500"
      data-annotate="text-process-list-unsupported"
    >
      The process list is available for MySQL and PostgreSQL connections.
    </p>
  {:else}
    <div
      class="flex shrink-0 items-center gap-3 border-b border-zinc-800 px-3 py-1 text-[11px] text-zinc-400"
    >
      <label class="flex items-center gap-1">
        <input
          type="checkbox"
          class="accent-blue-500"
          data-annotate="toggle-process-list-show-idle"
          bind:checked={showIdle}
          onchange={(e) => saveFlag(SHOW_IDLE_KEY, e.currentTarget.checked)}
        />
        Show idle
      </label>
      <label
        class="flex items-center gap-1"
        title="Refresh every {AUTO_REFRESH_MS / 1000} seconds"
      >
        <input
          type="checkbox"
          class="accent-blue-500"
          data-annotate="toggle-process-list-auto-refresh"
          bind:checked={autoRefresh}
          onchange={(e) => saveFlag(AUTO_REFRESH_KEY, e.currentTarget.checked)}
        />
        Auto refresh
      </label>
      <span class="ml-auto text-zinc-500" data-annotate="text-process-count">
        {activeCount} active / {processes.length} total
      </span>
    </div>

    {#if error}
      <p
        class="mx-3 my-2 whitespace-pre-wrap rounded border border-red-500/40 bg-red-500/10 px-2 py-1 text-xs leading-relaxed text-red-300"
        data-annotate="text-process-list-error"
      >
        {error}
      </p>
    {/if}

    <div
      class="min-h-0 flex-1 overflow-y-auto"
      data-annotate="list-processes"
    >
      {#if fetchedAt !== null && visible.length === 0}
        <p class="px-3 py-2 text-xs text-zinc-500">
          {processes.length === 0
            ? "No processes."
            : "No active processes. Check Show idle to see idle connections."}
        </p>
      {/if}
      {#each visible as process (process.id)}
        <button
          type="button"
          class="flex w-full flex-col gap-0.5 border-b border-zinc-800 px-3 py-1 text-left font-mono text-[11px] hover:bg-zinc-800 {selectedId ===
          process.id
            ? 'bg-zinc-800'
            : ''} {process.idle ? 'opacity-60' : ''}"
          aria-pressed={selectedId === process.id}
          data-annotate="item-process"
          onclick={() => {
            selectedId = selectedId === process.id ? null : process.id;
          }}
        >
          <span class="flex items-center gap-2">
            <span class="w-14 shrink-0 truncate text-right {timeClass(process)}">
              {formatDuration(process.time_secs)}
            </span>
            <span class="shrink-0 text-zinc-500">#{process.id}</span>
            <span class="truncate text-zinc-300">{process.command ?? "-"}</span>
            {#if process.state}
              <span class="truncate text-amber-300/80">{process.state}</span>
            {/if}
            <span class="ml-auto shrink-0 truncate text-zinc-500">
              {process.user ?? ""}{process.db ? `@${process.db}` : ""}
            </span>
          </span>
          {#if process.info}
            <span class="truncate pl-16 text-zinc-400">{process.info}</span>
          {/if}
        </button>
      {/each}
    </div>

    {#if selected}
      <div
        class="flex max-h-[45%] shrink-0 flex-col border-t border-zinc-700"
        data-annotate="detail-process"
      >
        <div
          class="flex shrink-0 items-center gap-2 border-b border-zinc-800 px-3 py-1 text-[11px] text-zinc-400"
        >
          <span class="font-semibold text-zinc-300">Process #{selected.id}</span>
          <span class="ml-auto flex items-center gap-1">
            {#if selected.info}
              <button
                type="button"
                class="rounded border border-zinc-600 px-1.5 py-0.5 text-[10px] text-zinc-300 hover:bg-zinc-800"
                data-annotate="button-process-copy-statement"
                onclick={() => copyStatement(selected.info ?? "")}
              >
                {copied ? "Copied!" : "Copy SQL"}
              </button>
            {/if}
            <button
              type="button"
              class="rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200"
              title="Close the details"
              aria-label="Close the details"
              data-annotate="button-process-detail-close"
              onclick={() => {
                selectedId = null;
              }}
            >
              <i class="bi bi-x" aria-hidden="true"></i>
            </button>
          </span>
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto px-3 py-2">
          {#if selected.info}
            <pre
              class="mb-2 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded border border-zinc-700 bg-zinc-950 p-2 font-mono text-[11px] leading-relaxed text-emerald-300">{selected.info}</pre>
          {/if}
          <table class="w-full font-mono text-[11px]">
            <tbody>
              {#each selected.fields as field (field.name)}
                <tr class="align-top">
                  <th
                    class="w-28 py-0.5 pr-2 text-left font-normal text-zinc-500"
                  >
                    {field.name}
                  </th>
                  <td class="break-all py-0.5 text-zinc-300">
                    {#if field.value === null}
                      <span class="text-zinc-600">NULL</span>
                    {:else}
                      {field.value.length > 300
                        ? `${field.value.slice(0, 300)}…`
                        : field.value}
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      </div>
    {/if}

    {#if fetchedAt}
      <div
        class="shrink-0 border-t border-zinc-800 px-3 py-0.5 text-[10px] text-zinc-600"
      >
        Updated {fetchedAt.toLocaleTimeString()}
      </div>
    {/if}
  {/if}
</div>
