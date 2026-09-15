<script lang="ts">
  interface Props {
    /// 危険と判定した理由 (バックエンドの英語メッセージ)
    reason: string;
    onConfirm: () => void;
    onCancel: () => void;
  }

  let { reason, onConfirm, onCancel }: Props = $props();
</script>

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-dangerous-modal"
  data-modal
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      onCancel();
    }
  }}
>
  <div
    class="flex w-[480px] flex-col gap-3 rounded-lg border border-red-800 bg-zinc-900 p-4 shadow-xl"
  >
    <h2 class="flex items-center gap-2 text-sm font-semibold text-red-400">
      <i class="bi bi-exclamation-triangle-fill"></i>
      Dangerous statement
    </h2>

    <p class="text-xs text-zinc-300" data-annotate="text-dangerous-reason">
      {reason}
    </p>
    <p class="text-xs text-zinc-500">
      This action cannot be undone. Run it only if you are sure.
    </p>

    <div class="flex justify-end gap-2">
      <button
        class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
        data-annotate="button-dangerous-cancel"
        onclick={onCancel}
      >
        Cancel
      </button>
      <button
        class="rounded bg-red-600 px-3 py-1 text-xs text-white hover:bg-red-500"
        data-annotate="button-dangerous-confirm"
        onclick={onConfirm}
      >
        Run anyway
      </button>
    </div>
  </div>
</div>
