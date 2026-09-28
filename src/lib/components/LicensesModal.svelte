<script lang="ts">
  import { onMount } from "svelte";
  import { getThirdPartyNotices } from "$lib/api";

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

  let text = $state<string | null>(null);
  let loadError = $state<string | null>(null);

  onMount(async () => {
    try {
      text = await getThirdPartyNotices();
    } catch (e) {
      loadError = String(e);
    }
  });

  const onWindowKeydown = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    }
  };
</script>

<svelte:window onkeydown={onWindowKeydown} />

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-licenses-modal"
  data-modal
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      onClose();
    }
  }}
>
  <div
    class="flex h-[85vh] w-[760px] max-w-[90vw] flex-col gap-3 rounded-lg border border-zinc-700 bg-zinc-900 p-4 shadow-xl"
  >
    <h2 class="text-sm font-semibold text-zinc-200">Third-Party Licenses</h2>

    <p class="text-xs text-zinc-400">
      Queryfolio bundles the open source libraries listed below, under the licenses shown.
    </p>

    {#if loadError}
      <pre class="whitespace-pre-wrap font-mono text-xs text-red-400">{loadError}</pre>
    {:else if text !== null}
      <!-- 本文は数万行あるので、この要素だけをスクロールさせる
           (ドキュメント自体はスクロールしない。app.css 参照) -->
      <pre
        class="min-h-0 flex-1 select-text overflow-auto whitespace-pre-wrap rounded border border-zinc-700 bg-zinc-950 p-3 font-mono text-[11px] leading-relaxed text-zinc-300"
        data-annotate="text-licenses">{text}</pre>
    {:else}
      <p class="text-xs text-zinc-500">Loading...</p>
    {/if}

    <div class="flex justify-end gap-2">
      <button
        class="rounded bg-blue-600 px-3 py-1 text-xs text-white hover:bg-blue-500"
        data-annotate="button-licenses-close"
        onclick={onClose}
      >
        Close
      </button>
    </div>
  </div>
</div>
