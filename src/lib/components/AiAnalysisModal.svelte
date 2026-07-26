<script lang="ts">
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import { splitMarkdownSegments } from "$lib/markdown";

  interface Props {
    /// モーダルの見出し (EXPLAIN 解説と選択 SQL 解説で使い回すため
    /// 差し替え可能にする。省略時は EXPLAIN 解説用の見出し)
    title?: string;
    /// AI 解説の Markdown テキスト
    text: string;
    onClose: () => void;
  }

  let { title = "AI Plan Analysis", text, onClose }: Props = $props();

  let copied = $state(false);

  // Markdown の表示区間 (完全なレンダラは持たず、コードブロックの
  // 装飾 + テキストの pre-wrap 表示のみ行う。ChatPane と共用)
  const segments = $derived(splitMarkdownSegments(text));

  const copy = async () => {
    // navigator.clipboard は Tauri 2 で OS のパーミッションプロンプトが
    // 出ることがあるため、公式プラグイン経由で書き込む
    await writeText(text);
    copied = true;
    setTimeout(() => {
      copied = false;
    }, 1500);
  };
</script>

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-ai-analysis-modal"
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      onClose();
    }
  }}
>
  <div
    class="flex max-h-[85vh] w-[720px] max-w-[90vw] flex-col gap-3 rounded-lg border border-zinc-700 bg-zinc-900 p-4 shadow-xl"
  >
    <h2 class="text-sm font-semibold text-zinc-200">{title}</h2>

    <div
      class="flex min-h-0 flex-col gap-2 overflow-y-auto"
      data-annotate="text-ai-analysis"
    >
      {#each segments as segment, i (i)}
        {#if segment.type === "code"}
          <pre
            class="overflow-x-auto rounded border border-zinc-700 bg-zinc-950 p-2 font-mono text-xs leading-relaxed text-emerald-300">{segment.content}</pre>
        {:else}
          <p class="whitespace-pre-wrap text-xs leading-relaxed text-zinc-300">
            {segment.content}
          </p>
        {/if}
      {/each}
    </div>

    <div class="flex justify-end gap-2">
      <button
        class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
        data-annotate="button-ai-analysis-copy"
        onclick={copy}
      >
        {copied ? "Copied!" : "Copy"}
      </button>
      <button
        class="rounded bg-blue-600 px-3 py-1 text-xs text-white hover:bg-blue-500"
        data-annotate="button-ai-analysis-close"
        onclick={onClose}
      >
        Close
      </button>
    </div>
  </div>
</div>
