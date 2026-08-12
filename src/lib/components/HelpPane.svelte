<script lang="ts">
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import { helpForEngine } from "$lib/help";
  import { parseHelpDoc, parseInline } from "$lib/help/render";

  interface Props {
    /// 選択中の接続のエンジン (未選択なら null)
    engine: string | null;
    /// ペインを閉じる (ツールバーのトグルと同じ状態を切る)
    onClose: () => void;
    /// 例文をエディタへ挿入する
    onInsert: (text: string) => void;
  }

  let { engine, onClose, onInsert }: Props = $props();

  /// Copy を押したコードブロックの添字
  let copiedIndex = $state<number | null>(null);

  const doc = $derived(helpForEngine(engine));
  const blocks = $derived(doc ? parseHelpDoc(doc) : []);

  const copy = async (index: number, text: string) => {
    try {
      await writeText(text);
      copiedIndex = index;
      setTimeout(() => {
        if (copiedIndex === index) {
          copiedIndex = null;
        }
      }, 1200);
    } catch {
      // クリップボードが使えない環境でも表示は壊さない
      copiedIndex = null;
    }
  };
</script>

<div
  class="flex h-full w-full flex-col border-l border-zinc-700 bg-zinc-900"
  data-annotate="pane-help"
>
  <div
    class="flex shrink-0 items-center gap-2 border-b border-zinc-700 px-3 py-1"
  >
    <span class="text-xs font-semibold tracking-wide text-zinc-400">HELP</span>
    {#if engine}
      <span class="text-xs text-zinc-500">{engine}</span>
    {/if}
    <button
      type="button"
      class="ml-auto rounded px-1 text-zinc-500 hover:bg-zinc-700 hover:text-zinc-200"
      title="Close the help pane"
      aria-label="Close the help pane"
      data-annotate="button-help-close"
      onclick={onClose}
    >
      <i class="bi bi-x" aria-hidden="true"></i>
    </button>
  </div>

  <div class="min-h-0 flex-1 overflow-y-auto px-3 py-2 text-zinc-300">
    {#if !engine}
      <p class="text-sm text-zinc-600">
        Select a connection to see how to query it.
      </p>
    {:else if blocks.length === 0}
      <p class="text-sm text-zinc-600">
        No help is available for this data source yet.
      </p>
    {:else}
      {#each blocks as block, i (i)}
        {#if block.type === "heading"}
          {#if block.level === 1}
            <h2 class="mt-1 mb-2 text-sm font-semibold text-zinc-100">
              {block.text}
            </h2>
          {:else}
            <h3
              class="mt-4 mb-1.5 border-b border-zinc-800 pb-1 text-xs font-semibold tracking-wide text-zinc-300"
            >
              {block.text}
            </h3>
          {/if}
        {:else if block.type === "code"}
          <div class="mb-2 flex flex-col gap-1">
            <pre
              class="overflow-x-auto rounded border border-zinc-700 bg-zinc-950 p-2 font-mono text-xs leading-relaxed text-emerald-300">{block.content}</pre>
            <div class="flex justify-end gap-1">
              <button
                type="button"
                class="rounded border border-zinc-600 px-1.5 py-0.5 text-[10px] text-zinc-300 hover:bg-zinc-800"
                data-annotate="button-help-copy-code"
                onclick={() => copy(i, block.content)}
              >
                {copiedIndex === i ? "Copied!" : "Copy"}
              </button>
              <button
                type="button"
                class="rounded border border-zinc-600 px-1.5 py-0.5 text-[10px] text-zinc-300 hover:bg-zinc-800"
                title="Insert into the editor"
                data-annotate="button-help-insert-code"
                onclick={() => onInsert(block.content)}
              >
                Insert
              </button>
            </div>
          </div>
        {:else if block.type === "list"}
          <ul class="mb-2 list-disc pl-4 text-xs leading-relaxed">
            {#each block.items as item, j (j)}
              <li class="mb-0.5">
                {#each parseInline(item) as span, k (k)}
                  {#if span.type === "code"}
                    <code class="rounded bg-zinc-800 px-1 font-mono text-[11px] text-emerald-300"
                      >{span.text}</code
                    >
                  {:else if span.type === "strong"}
                    <strong class="font-semibold text-zinc-100">{span.text}</strong>
                  {:else}{span.text}{/if}
                {/each}
              </li>
            {/each}
          </ul>
        {:else}
          <p class="mb-2 text-xs leading-relaxed">
            {#each parseInline(block.text) as span, k (k)}
              {#if span.type === "code"}
                <code class="rounded bg-zinc-800 px-1 font-mono text-[11px] text-emerald-300"
                  >{span.text}</code
                >
              {:else if span.type === "strong"}
                <strong class="font-semibold text-zinc-100">{span.text}</strong>
              {:else}{span.text}{/if}
            {/each}
          </p>
        {/if}
      {/each}
    {/if}
  </div>
</div>
