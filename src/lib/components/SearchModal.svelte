<script lang="ts">
  import appStore from "$lib/stores/app.svelte";
  import * as api from "$lib/api";
  import type { FileSearchHit } from "$lib/api";

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

  let query = $state("");
  let fileHits = $state<FileSearchHit[]>([]);
  /// キーボードで選択中の候補 (items のフラットな添字)
  let activeIndex = $state(0);
  let inputEl: HTMLInputElement | undefined = $state();
  /// 非同期検索の世代番号。古い応答が新しい結果を上書きしないために使う
  let searchGeneration = 0;

  /// 接続の絞り込み。名前・説明の部分一致 (大小無視)。空クエリなら全件を出し、
  /// 接続の切り替え (ジャンプ) に使えるようにする。
  const connMatches = $derived.by(() => {
    const q = query.trim().toLowerCase();
    const list = appStore.connections;
    if (!q) {
      return list;
    }
    return list.filter(
      (c) =>
        c.name.toLowerCase().includes(q) ||
        (c.description ?? "").toLowerCase().includes(q),
    );
  });

  /// キーボード操作用のフラットな候補リスト (接続 → ファイルの順)。
  /// 表示のグループ分けと添字はこの順序に対応する。
  type Item =
    | { kind: "connection"; name: string; description: string | null }
    | { kind: "file"; hit: FileSearchHit };
  const items = $derived.by<Item[]>(() => [
    ...connMatches.map((c) => ({
      kind: "connection" as const,
      name: c.name,
      description: c.description,
    })),
    ...fileHits.map((hit) => ({ kind: "file" as const, hit })),
  ]);

  /// query 変更でファイル検索をデバウンス実行する。ファイルは選択中の接続の
  /// ものだけを対象にする (接続をまたぐファイルは接続の切り替えで辿る)。
  $effect(() => {
    const q = query.trim();
    const connection = appStore.selectedConnection;
    const gen = ++searchGeneration;
    // クエリ変更のたびに選択位置を先頭へ戻す
    activeIndex = 0;
    // 検索語・接続が変わった瞬間に古いファイル結果を消す。debounce + invoke
    // 待ちの間、現在の検索語と一致しない stale なファイルを表示・選択 (Enter/
    // クリック) できないようにする。接続の候補は同期的に絞り込むので残してよい。
    fileHits = [];
    if (!q || !connection) {
      return;
    }
    const timer = setTimeout(async () => {
      try {
        const hits = await api.searchQueryFiles(connection, q);
        if (gen === searchGeneration) {
          fileHits = hits;
        }
      } catch {
        // 検索失敗時は結果を空にする (モーダルは開いたまま)
        if (gen === searchGeneration) {
          fileHits = [];
        }
      }
    }, 150);
    return () => clearTimeout(timer);
  });

  // マウント時に入力へフォーカスする
  $effect(() => {
    inputEl?.focus();
  });

  const activate = async (item: Item) => {
    if (item.kind === "connection") {
      await appStore.selectConnection(item.name);
    } else {
      await appStore.selectFile(item.hit.file_name);
    }
    onClose();
  };

  const onKeydown = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (items.length) {
        activeIndex = (activeIndex + 1) % items.length;
      }
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (items.length) {
        activeIndex = (activeIndex - 1 + items.length) % items.length;
      }
    } else if (e.key === "Enter") {
      e.preventDefault();
      const item = items[activeIndex];
      if (item) {
        void activate(item);
      }
    }
  };
</script>

<div
  class="fixed inset-0 z-20 flex items-start justify-center bg-black/60 pt-[15vh]"
  role="presentation"
  data-annotate="backdrop-search-modal"
  data-modal
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      onClose();
    }
  }}
>
  <div
    class="flex max-h-[60vh] w-[560px] flex-col overflow-hidden rounded-lg border border-zinc-700 bg-zinc-900 shadow-xl"
  >
    <div class="flex items-center gap-2 border-b border-zinc-700 px-3 py-2">
      <i class="bi bi-search text-zinc-500" aria-hidden="true"></i>
      <input
        bind:this={inputEl}
        bind:value={query}
        onkeydown={onKeydown}
        type="text"
        placeholder="Search connections and query files…"
        class="w-full bg-transparent text-sm text-zinc-100 placeholder:text-zinc-500 focus:outline-none"
        data-annotate="search-input"
        autocomplete="off"
        spellcheck="false"
      />
    </div>

    <div class="min-h-0 flex-1 overflow-y-auto py-1">
      {#if items.length === 0}
        <p class="px-3 py-4 text-center text-xs text-zinc-500">
          {query.trim() ? "No matches" : "Type to search"}
        </p>
      {:else}
        {#if connMatches.length > 0}
          <p
            class="px-3 pt-1 pb-0.5 text-[10px] font-semibold tracking-wide text-zinc-500 uppercase"
          >
            Connections
          </p>
          {#each connMatches as conn, i (conn.name)}
            <button
              class="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs {activeIndex ===
              i
                ? 'bg-zinc-700/60'
                : 'hover:bg-zinc-800'}"
              data-annotate="search-result-connection-{conn.name}"
              onmouseenter={() => (activeIndex = i)}
              onclick={() => activate({ kind: "connection", name: conn.name, description: conn.description })}
            >
              <i class="bi bi-hdd-network text-zinc-400" aria-hidden="true"></i>
              <span class="text-zinc-100">{conn.name}</span>
              {#if conn.description}
                <span class="truncate text-zinc-500">{conn.description}</span>
              {/if}
            </button>
          {/each}
        {/if}

        {#if fileHits.length > 0}
          <p
            class="px-3 pt-1.5 pb-0.5 text-[10px] font-semibold tracking-wide text-zinc-500 uppercase"
          >
            Files{appStore.selectedConnection
              ? ` · ${appStore.selectedConnection}`
              : ""}
          </p>
          {#each fileHits as hit, j (hit.file_name)}
            {@const idx = connMatches.length + j}
            <button
              class="flex w-full flex-col gap-0.5 px-3 py-1.5 text-left text-xs {activeIndex ===
              idx
                ? 'bg-zinc-700/60'
                : 'hover:bg-zinc-800'}"
              data-annotate="search-result-file-{hit.file_name}"
              onmouseenter={() => (activeIndex = idx)}
              onclick={() => activate({ kind: "file", hit })}
            >
              <span class="flex items-center gap-2">
                <i class="bi bi-file-earmark-code text-zinc-400" aria-hidden="true"
                ></i>
                <span class="text-zinc-100">{hit.file_name}</span>
              </span>
              {#if hit.content_preview}
                <span class="truncate pl-5 font-mono text-[11px] text-zinc-500">
                  {hit.content_preview}
                </span>
              {/if}
            </button>
          {/each}
        {/if}
      {/if}
    </div>

    <div
      class="flex items-center gap-3 border-t border-zinc-800 px-3 py-1 text-[10px] text-zinc-600"
    >
      <span><kbd>↑</kbd> <kbd>↓</kbd> navigate</span>
      <span><kbd>↵</kbd> open</span>
      <span><kbd>esc</kbd> close</span>
    </div>
  </div>
</div>
