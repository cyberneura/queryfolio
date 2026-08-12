<script lang="ts">
  import { onMount } from "svelte";
  import { getConfigInfo, type ConfigInfo } from "$lib/api";
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

  let info = $state<ConfigInfo | null>(null);
  let loadError = $state<string | null>(null);
  let reloadError = $state<string | null>(null);

  onMount(async () => {
    try {
      info = await getConfigInfo();
    } catch (e) {
      loadError = String(e);
    }
  });

  // 再読込に失敗した場合はモーダルを閉じず、エラーを表示して誤認を防ぐ
  const reload = async () => {
    reloadError = null;
    if (await appStore.reloadConnections()) {
      onClose();
      return;
    }
    reloadError = appStore.errorMessage ?? "Failed to reload";
    try {
      info = await getConfigInfo();
    } catch (e) {
      loadError = String(e);
    }
  };
</script>

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-config-modal"
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      onClose();
    }
  }}
>
  <!-- 他のモーダル (AiAnalysisModal / SearchModal / ConfigEditorModal) と同様に
       ビューポート内へ収める。loadError に長いメッセージが入ると縦にはみ出すが、
       fixed かつ中央揃えなので上端が画面外へ出て読めなくなる (ドキュメント自体は
       スクロールしないため戻せない。CYBERNEURA-DEV-421) -->
  <div
    class="flex max-h-[85vh] w-[560px] flex-col gap-3 overflow-auto rounded-lg border border-zinc-700 bg-zinc-900 p-4 shadow-xl"
  >
    <h2 class="text-sm font-semibold text-zinc-200">Settings</h2>

    <p class="text-xs text-zinc-400">
      Edit the YAML file directly, then press Reload to apply the changes.
    </p>

    {#if loadError}
      <pre class="whitespace-pre-wrap font-mono text-xs text-red-400">{loadError}</pre>
    {:else if info}
      <dl class="flex flex-col gap-2 text-xs">
        <div class="flex flex-col gap-0.5">
          <dt class="text-zinc-500">Config file</dt>
          <dd class="font-mono text-zinc-200" data-annotate="text-config-path">
            {info.config_path}
            {#if !info.config_exists}
              <span class="text-amber-400">(not created)</span>
            {/if}
          </dd>
        </div>
        <div class="flex flex-col gap-0.5">
          <dt class="text-zinc-500">Config source</dt>
          <dd class="font-mono text-zinc-200" data-annotate="text-config-source">
            {info.source}
          </dd>
        </div>
        <div class="flex flex-col gap-0.5">
          <dt class="text-zinc-500">Query file directory (sqlfiles_dir)</dt>
          <dd class="font-mono text-zinc-200" data-annotate="text-sqlfiles-dir">
            {info.sqlfiles_dir}
          </dd>
        </div>
      </dl>
    {:else}
      <p class="text-xs text-zinc-500">Loading...</p>
    {/if}

    {#if reloadError}
      <pre
        class="whitespace-pre-wrap font-mono text-xs text-red-400"
        data-annotate="text-reload-error">{reloadError}</pre>
    {/if}

    <div class="flex justify-end gap-2">
      <button
        class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
        data-annotate="button-config-close"
        onclick={onClose}
      >
        Close
      </button>
      <button
        class="rounded bg-blue-600 px-3 py-1 text-xs text-white hover:bg-blue-500"
        data-annotate="button-config-reload"
        onclick={reload}
      >
        Reload
      </button>
    </div>
  </div>
</div>
