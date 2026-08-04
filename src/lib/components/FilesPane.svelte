<script lang="ts">
  import { toast } from "svelte-sonner";
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    /// HISTORY / TABLES タブへの切り替え (タブ状態は +page.svelte が持つ)
    onShowHistory: () => void;
    onShowTables: () => void;
  }

  let { onShowHistory, onShowTables }: Props = $props();

  let creating = $state(false);
  let newFileName = $state("");
  /// 3 点メニューを開いているファイル
  let openMenuFile = $state<string | null>(null);
  /// メニュー内で Delete の確認待ちになっているファイル
  let confirmingDelete = $state<string | null>(null);
  /// リネーム入力中のファイルと入力値
  let renamingFile = $state<string | null>(null);
  let renameValue = $state("");

  /// エンジン別のクエリファイル拡張子 (ドット付き。例 ".sql" / ".redis")
  const fileSuffix = $derived(`.${appStore.selectedFileExtension}`);

  // デフォルトのファイル名: YYYYMMDD-HHMM (拡張子はバックエンドが付与)。
  // 日付が先頭にあると名前順ソートが時系列になり探しやすい。一覧は名前の降順
  // (query_files.rs の list_query_file_names) なので、新しいファイルほど上に出る。
  // 同一分内の連続作成で衝突しないよう、既存ファイルと重複する場合は
  // -2, -3 ... を付けて一意化する。ゼロ埋めはしないが、一覧側が数字を数値として
  // 比較するため -9 と -10 の並びも作成順どおりになる。
  const defaultFileName = () => {
    const now = new Date();
    const pad = (n: number) => String(n).padStart(2, "0");
    const date = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}`;
    const time = `${pad(now.getHours())}${pad(now.getMinutes())}`;
    const base = `${date}-${time}`;
    if (!appStore.files.includes(`${base}${fileSuffix}`)) {
      return base;
    }
    let n = 2;
    while (appStore.files.includes(`${base}-${n}${fileSuffix}`)) {
      n++;
    }
    return `${base}-${n}`;
  };

  const submitNewFile = async () => {
    const name = newFileName.trim();
    if (!name) {
      creating = false;
      return;
    }
    await appStore.createFile(name);
    newFileName = "";
    creating = false;
  };

  const closeMenu = () => {
    openMenuFile = null;
    confirmingDelete = null;
  };

  // 名前を正規化する (エンジン別拡張子を保証)。同名判定をバックエンドと揃えるため。
  const normalize = (name: string) => {
    const trimmed = name.trim();
    return trimmed.toLowerCase().endsWith(fileSuffix)
      ? trimmed
      : `${trimmed}${fileSuffix}`;
  };

  const startRename = (fileName: string) => {
    closeMenu();
    renamingFile = fileName;
    // 拡張子を隠さずそのまま編集させる (ユーザーが .sql を意識できる)
    renameValue = fileName;
  };

  const cancelRename = () => {
    renamingFile = null;
    renameValue = "";
  };

  // Enter でのみ確定する。無効な入力は理由をトーストで示し入力を開いたままにする。
  // フォーカスアウト (blur) は commit せずキャンセルする (誤コミット防止)。
  const submitRename = async () => {
    const oldName = renamingFile;
    if (!oldName) {
      return;
    }
    const raw = renameValue.trim();
    // 空・変更なしは黙ってキャンセル
    if (!raw || normalize(raw) === normalize(oldName)) {
      cancelRename();
      return;
    }
    if (raw.startsWith(".")) {
      toast.error("The name cannot start with a dot");
      return;
    }
    const normalized = normalize(raw);
    // リネーム対象自身は除外する (大文字小文字だけを変える改名を許可)
    if (
      appStore.files.some(
        (f) => f !== oldName && f.toLowerCase() === normalized.toLowerCase(),
      )
    ) {
      toast.error("A file with the same name already exists");
      return;
    }
    const result = await appStore.renameFile(oldName, raw);
    if (result) {
      cancelRename();
    } else {
      toast.error("Failed to rename the file", {
        description: appStore.errorMessage ?? undefined,
      });
    }
  };

  // 入力段階で使えない文字 (/ \) を取り除く。.. 等の先頭ドットは送信時に弾く。
  const sanitizeRenameInput = (value: string) => {
    renameValue = value.replace(/[/\\]/g, "");
  };

  const handleNameClick = (fileName: string) => {
    // 既に開いている (選択中) ファイルの名前を再クリックしたらリネームに入る
    if (appStore.selectedFile === fileName) {
      startRename(fileName);
    } else {
      void appStore.selectFile(fileName);
    }
  };

  const doDelete = async (fileName: string) => {
    closeMenu();
    await appStore.deleteFile(fileName);
  };

  const doCopyFullPath = async (fileName: string) => {
    closeMenu();
    const ok = await appStore.copyFilePath(fileName);
    if (ok) {
      toast.success("Copied the file path to the clipboard");
    } else {
      toast.error("Failed to copy the file path", {
        description: appStore.errorMessage ?? undefined,
      });
    }
  };
</script>

<div class="flex h-full w-full flex-col border-r border-zinc-700 bg-zinc-900">
  <div class="flex items-center gap-2 border-b border-zinc-700 px-3 py-2">
    <span class="text-xs font-semibold tracking-wide text-zinc-400">FILES</span>
    <button
      class="text-xs font-semibold tracking-wide text-zinc-600 hover:text-zinc-300"
      title="Show query history"
      data-annotate="tab-history"
      onclick={onShowHistory}
    >
      HISTORY
    </button>
    <!-- テーブルの概念が無いエンジン (redis 等) では TABLES を出さない -->
    {#if appStore.selectedCapabilities?.supports_tables ?? true}
      <button
        class="text-xs font-semibold tracking-wide text-zinc-600 hover:text-zinc-300"
        title="Show tables"
        data-annotate="tab-tables"
        onclick={onShowTables}
      >
        TABLES
      </button>
    {/if}
    <button
      class="ml-auto rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-700 hover:text-zinc-200 disabled:opacity-40"
      title="New query file"
      aria-label="New query file"
      data-annotate="button-create-file"
      disabled={!appStore.selectedConnection}
      onclick={() => {
        newFileName = defaultFileName();
        creating = true;
      }}
    >
      <i class="bi bi-plus-lg" aria-hidden="true"></i>
    </button>
  </div>
  <div class="min-h-0 flex-1 overflow-y-auto">
    {#if !appStore.selectedConnection}
      <p class="px-3 py-2 text-xs text-zinc-500">Select a connection</p>
    {:else}
      {#if creating}
        <form
          class="flex items-center gap-1 px-2 py-1.5"
          onsubmit={(e) => {
            e.preventDefault();
            void submitNewFile();
          }}
        >
          <!-- svelte-ignore a11y_autofocus -->
          <input
            class="w-full rounded border border-zinc-600 bg-zinc-800 px-1.5 py-0.5 text-xs text-zinc-200 outline-none focus:border-blue-400"
            placeholder="File name"
            data-annotate="input-new-file-name"
            autofocus
            onfocus={(e) => e.currentTarget.select()}
            bind:value={newFileName}
            onblur={() => {
              creating = false;
              newFileName = "";
            }}
          />
        </form>
      {/if}
      {#if appStore.files.length === 0 && !creating}
        <p class="px-3 py-2 text-xs text-zinc-500">
          Click + to create a query file
        </p>
      {/if}
      {#each appStore.files as fileName (fileName)}
        <div
          class="group relative flex items-center gap-1 pr-1 hover:bg-zinc-800 {appStore.selectedFile ===
          fileName
            ? 'bg-zinc-800 border-l-2 border-blue-400'
            : 'border-l-2 border-transparent'}"
        >
          {#if renamingFile === fileName}
            <form
              class="flex-1 px-2 py-1"
              onsubmit={(e) => {
                e.preventDefault();
                void submitRename();
              }}
            >
              <!-- svelte-ignore a11y_autofocus -->
              <input
                class="w-full rounded border border-zinc-600 bg-zinc-800 px-1.5 py-0.5 text-sm text-zinc-200 outline-none focus:border-blue-400"
                data-annotate="input-rename-{fileName}"
                autofocus
                value={renameValue}
                oninput={(e) => sanitizeRenameInput(e.currentTarget.value)}
                onfocus={(e) => e.currentTarget.select()}
                onblur={cancelRename}
                onkeydown={(e) => {
                  if (e.key === "Escape") {
                    e.preventDefault();
                    cancelRename();
                  }
                }}
              />
            </form>
          {:else}
            <button
              class="min-w-0 flex-1 truncate px-3 py-1.5 text-left text-sm text-zinc-200"
              data-annotate="button-file-{fileName}"
              onclick={() => handleNameClick(fileName)}
            >
              {fileName}
              {#if appStore.selectedFile === fileName && appStore.dirty}
                <span class="text-zinc-500" title="Unsaved">*</span>
              {/if}
            </button>
            <button
              class="shrink-0 rounded px-1 py-0.5 text-zinc-500 hover:bg-zinc-700 hover:text-zinc-200 {openMenuFile ===
              fileName
                ? 'block bg-zinc-700 text-zinc-200'
                : 'hidden group-hover:block'}"
              title="More actions"
              aria-label="More actions"
              aria-haspopup="menu"
              data-annotate="button-file-menu-{fileName}"
              onclick={() => {
                confirmingDelete = null;
                openMenuFile = openMenuFile === fileName ? null : fileName;
              }}
            >
              <i class="bi bi-three-dots-vertical" aria-hidden="true"></i>
            </button>
          {/if}

          {#if openMenuFile === fileName}
            <!-- メニュー外クリックで閉じる透明バックドロップ -->
            <button
              class="fixed inset-0 z-20 cursor-default"
              tabindex="-1"
              aria-label="Close menu"
              data-annotate="menu-backdrop-{fileName}"
              onclick={closeMenu}
            ></button>
            <div
              class="absolute right-1 top-full z-30 mt-0.5 min-w-32 rounded border border-zinc-700 bg-zinc-800 py-1 shadow-lg"
              role="menu"
            >
              {#if confirmingDelete === fileName}
                <div class="px-3 py-1 text-xs text-zinc-400">Delete this file?</div>
                <div class="flex gap-1 px-2 py-1">
                  <button
                    class="flex-1 rounded bg-red-700 px-2 py-1 text-xs text-red-100 hover:bg-red-600"
                    role="menuitem"
                    data-annotate="confirm-delete-{fileName}"
                    onclick={() => void doDelete(fileName)}
                  >
                    Delete
                  </button>
                  <button
                    class="flex-1 rounded bg-zinc-700 px-2 py-1 text-xs text-zinc-200 hover:bg-zinc-600"
                    role="menuitem"
                    data-annotate="cancel-delete-{fileName}"
                    onclick={() => {
                      confirmingDelete = null;
                    }}
                  >
                    Cancel
                  </button>
                </div>
              {:else}
                <button
                  class="flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm text-zinc-200 hover:bg-zinc-700"
                  role="menuitem"
                  data-annotate="menu-rename-{fileName}"
                  onclick={() => startRename(fileName)}
                >
                  <i class="bi bi-pencil" aria-hidden="true"></i>
                  Rename
                </button>
                <button
                  class="flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm text-zinc-200 hover:bg-zinc-700"
                  role="menuitem"
                  data-annotate="menu-copy-fullpath-{fileName}"
                  onclick={() => void doCopyFullPath(fileName)}
                >
                  <i class="bi bi-clipboard" aria-hidden="true"></i>
                  Copy full path
                </button>
                <button
                  class="flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm text-red-400 hover:bg-zinc-700"
                  role="menuitem"
                  data-annotate="menu-delete-{fileName}"
                  onclick={() => {
                    confirmingDelete = fileName;
                  }}
                >
                  <i class="bi bi-trash" aria-hidden="true"></i>
                  Delete
                </button>
              {/if}
            </div>
          {/if}
        </div>
      {/each}
    {/if}
  </div>
</div>
