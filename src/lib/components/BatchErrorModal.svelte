<script lang="ts">
  interface Props {
    connection: string;
    /// 1-based position of the failed statement in the batch
    index: number;
    total: number;
    statement: string;
    error: string;
    onStop: () => void;
    onIgnoreErrors: () => void;
  }

  let {
    connection,
    index,
    total,
    statement,
    error,
    onStop,
    onIgnoreErrors,
  }: Props = $props();
</script>

<svelte:window
  onkeydown={(e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onStop();
    }
  }}
/>

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-batch-error-modal"
  data-modal
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      onStop();
    }
  }}
>
  <div
    class="flex w-[560px] max-w-[calc(100vw-32px)] flex-col gap-3 rounded-lg border border-red-800 bg-zinc-900 p-4 shadow-xl"
  >
    <h2 class="flex items-center gap-2 text-sm font-semibold text-red-400">
      <i class="bi bi-exclamation-octagon-fill"></i>
      Statement {index} of {total} failed
    </h2>
    <p class="text-xs text-zinc-400" data-annotate="text-batch-error-connection">
      on {connection}
    </p>

    <pre
      class="max-h-32 overflow-auto rounded bg-zinc-950 p-2 text-xs whitespace-pre-wrap text-zinc-300"
      data-annotate="text-batch-error-statement">{statement.trim()}</pre>
    <p
      class="max-h-32 overflow-auto text-xs whitespace-pre-wrap text-red-300"
      data-annotate="text-batch-error-message"
    >
      {error}
    </p>
    <p class="text-xs text-zinc-500">
      {total - index} statement{total - index === 1 ? "" : "s"} left. Ignoring errors runs
      all of them without stopping again.
    </p>

    <div class="flex justify-end gap-2">
      <button
        class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
        data-annotate="button-batch-stop"
        onclick={onStop}
      >
        Stop
      </button>
      <button
        class="rounded bg-red-600 px-3 py-1 text-xs text-white hover:bg-red-500"
        data-annotate="button-batch-ignore-errors"
        onclick={onIgnoreErrors}
      >
        Ignore errors and run all
      </button>
    </div>
  </div>
</div>
