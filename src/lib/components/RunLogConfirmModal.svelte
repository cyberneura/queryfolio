<script lang="ts">
  import type { RunLogChoice } from "$lib/runLog";

  interface Props {
    /// 書き戻そうとしている結果の行数
    rows: number;
    /// 「一部だけ書く」を選んだ時に書く行数
    limit: number;
    onChoose: (choice: RunLogChoice) => void;
  }

  let { rows, limit, onChoose }: Props = $props();
</script>

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-run-log-modal"
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      onChoose("cancel");
    }
  }}
>
  <div
    class="flex w-[520px] flex-col gap-3 rounded-lg border border-zinc-600 bg-zinc-900 p-4 shadow-xl"
  >
    <h2 class="flex items-center gap-2 text-sm font-semibold text-zinc-200">
      <i class="bi bi-journal-text"></i>
      Write the result into the editor?
    </h2>

    <p class="text-xs text-zinc-300" data-annotate="text-run-log-rows">
      This statement is marked with 📝, so the result is about to be written
      below it as a comment. The result has {rows.toLocaleString()} rows, which
      is more than {limit.toLocaleString()}.
    </p>
    <p class="text-xs text-zinc-500">
      The result is still shown in the table below either way.
    </p>

    <div class="flex justify-end gap-2">
      <button
        class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
        data-annotate="button-run-log-cancel"
        onclick={() => onChoose("cancel")}
      >
        Don't write
      </button>
      <button
        class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
        data-annotate="button-run-log-write-limited"
        onclick={() => onChoose("limited")}
      >
        Write {limit.toLocaleString()} rows
      </button>
      <button
        class="rounded bg-blue-600 px-3 py-1 text-xs text-white hover:bg-blue-500"
        data-annotate="button-run-log-write-all"
        onclick={() => onChoose("all")}
      >
        Write all {rows.toLocaleString()} rows
      </button>
    </div>
  </div>
</div>
