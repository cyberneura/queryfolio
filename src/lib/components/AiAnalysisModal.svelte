<script lang="ts">
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import { splitMarkdownSegments } from "$lib/markdown";

  interface Props {
    /// The modal heading (replaceable, since it is reused for the EXPLAIN explanation and the
    /// selected-SQL explanation. When omitted, the heading for the EXPLAIN explanation)
    title?: string;
    /// Markdown text of the AI explanation
    text: string;
    onClose: () => void;
  }

  let { title = "AI Plan Analysis", text, onClose }: Props = $props();

  let copied = $state(false);

  // Markdown display segments (no full renderer; only code block decoration +
  // pre-wrap text display. Shared with ChatPane)
  const segments = $derived(splitMarkdownSegments(text));

  const copy = async () => {
    // navigator.clipboard may show an OS permission prompt in Tauri 2,
    // so write through the official plugin
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
  data-modal
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
