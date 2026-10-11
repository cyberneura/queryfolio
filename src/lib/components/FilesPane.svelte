<script lang="ts">
  import { toast } from "svelte-sonner";
  import appStore from "$lib/stores/app.svelte";
  import { setFileDragPayload } from "$lib/fileDrag";
  import { formatFileSize, formatModifiedAt, formatRelativeTime } from "$lib/fileMeta";

  interface Props {
    /// Switch to the HISTORY / TABLES tab (the tab state is held by +page.svelte)
    onShowHistory: () => void;
    onShowTables: () => void;
  }

  let { onShowHistory, onShowTables }: Props = $props();

  let creating = $state(false);
  let newFileName = $state("");
  /// The file whose three-dot menu is open
  let openMenuFile = $state<string | null>(null);
  /// The file waiting for Delete confirmation in the menu
  let confirmingDelete = $state<string | null>(null);
  /// The file being renamed and the input value
  let renamingFile = $state<string | null>(null);
  let renameValue = $state("");
  /// The file being dragged to the CONNECTIONS pane (to show the drag source dimmed)
  let draggingFile = $state<string | null>(null);

  /// Reference time for relative notation (`3 days ago`). Advanced every minute so the notation does not go stale even if left open
  let clock = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => {
      clock = Date.now();
    }, 60_000);
    return () => clearInterval(timer);
  });

  /// One line shown under the file name (`2026-09-15 17:30 · 3 days ago · 4 KB`).
  /// For a file whose modified time is unavailable, only the size
  const metaLine = (modifiedMs: number | null, size: number) =>
    modifiedMs === null
      ? formatFileSize(size)
      : `${formatModifiedAt(modifiedMs)} · ${formatRelativeTime(modifiedMs, clock)} · ${formatFileSize(size)}`;

  /// Query file extension per engine (with the dot, e.g. ".sql" / ".redis")
  const fileSuffix = $derived(`.${appStore.selectedFileExtension}`);

  // Default file name: YYYYMMDD-HHMM (the extension is added by the backend).
  // With the date first, the chronological order is visible from the name too and files are easy to find.
  // The list itself is in descending order of modified time (list_query_file_entries in query_files.rs),
  // but search results are in descending order of name (list_query_file_names), so make newer files
  // come first by name as well.
  // To avoid collisions for consecutive creations within the same minute, make the name unique by
  // appending -2, -3 ... if it duplicates an existing file. No zero padding is used, but the list side
  // compares digits as numbers, so -9 and -10 also stay in creation order.
  //
  // The number is **always the existing maximum + 1, without filling gaps**. The list is in descending
  // order, so reusing a deleted smaller number (or the unnumbered one) would make a newly created file
  // appear below an older file from the same minute.
  const defaultFileName = () => {
    const now = new Date();
    const pad = (n: number) => String(n).padStart(2, "0");
    const date = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}`;
    const time = `${pad(now.getHours())}${pad(now.getMinutes())}`;
    const base = `${date}-${time}`;
    // 0 = no file from the same minute yet / 1 = only the unnumbered one exists / n = exists up to -n
    let maxSeq = appStore.files.includes(`${base}${fileSuffix}`) ? 1 : 0;
    for (const fileName of appStore.files) {
      if (!fileName.startsWith(`${base}-`) || !fileName.endsWith(fileSuffix)) {
        continue;
      }
      const seq = fileName.slice(
        base.length + 1,
        fileName.length - fileSuffix.length,
      );
      // Ignore anything that is not a sequence number ("20260804-1200-draft")
      if (!/^\d+$/.test(seq)) {
        continue;
      }
      maxSeq = Math.max(maxSeq, Number(seq));
    }
    return maxSeq === 0 ? base : `${base}-${maxSeq + 1}`;
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

  // Normalize the name (guarantee the engine-specific extension). To match the same-name check with the backend.
  const normalize = (name: string) => {
    const trimmed = name.trim();
    return trimmed.toLowerCase().endsWith(fileSuffix)
      ? trimmed
      : `${trimmed}${fileSuffix}`;
  };

  const startRename = (fileName: string) => {
    closeMenu();
    renamingFile = fileName;
    // Let the user edit with the extension shown as is (so the user stays aware of .sql)
    renameValue = fileName;
  };

  const cancelRename = () => {
    renamingFile = null;
    renameValue = "";
  };

  // Commit only with Enter. For invalid input, show the reason in a toast and keep the input open.
  // Focus-out (blur) cancels without committing (to prevent accidental commits).
  const submitRename = async () => {
    const oldName = renamingFile;
    if (!oldName) {
      return;
    }
    const raw = renameValue.trim();
    // Empty or unchanged: cancel silently
    if (!raw || normalize(raw) === normalize(oldName)) {
      cancelRename();
      return;
    }
    if (raw.startsWith(".")) {
      toast.error("The name cannot start with a dot");
      return;
    }
    const normalized = normalize(raw);
    // Exclude the rename target itself (allow a rename that changes only the letter case)
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

  // Strip characters that cannot be used at the input stage (/ \). A leading dot such as .. is rejected on submit.
  const sanitizeRenameInput = (value: string) => {
    renameValue = value.replace(/[/\\]/g, "");
  };

  // Drag to the CONNECTIONS pane to move to another server. The move itself is done by the
  // drop target (ConnectionsPane) with appStore.moveFileToConnection.
  const startDrag = (e: DragEvent, fileName: string) => {
    // draggable is removed while renaming, but prevent it twice just in case
    // (so text selection in the input is not treated as a drag)
    if (!e.dataTransfer || renamingFile === fileName) {
      return;
    }
    const connection = appStore.selectedConnection;
    if (!connection) {
      return;
    }
    setFileDragPayload(e.dataTransfer, { connection, fileName });
    draggingFile = fileName;
    // Close the menu while dragging (to prevent it from staying open after the drop)
    closeMenu();
  };

  const handleNameClick = (fileName: string) => {
    // Clicking the name of an already open (selected) file again enters rename
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
    <!-- Do not show TABLES for engines without a table concept (redis, etc.) -->
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
      <!-- To give the rows with drag & drop handlers a proper ARIA role,
                 wrap only the list part with role="list" (the create form and empty message are not included) -->
      <div role="list">
        {#each appStore.fileEntries as { file_name: fileName, modified_ms, size } (fileName)}
          <div
            role="listitem"
            class="group relative flex items-center gap-1 pr-1 hover:bg-zinc-800 {appStore.selectedFile ===
            fileName
              ? 'bg-zinc-800 border-l-2 border-blue-400'
              : 'border-l-2 border-transparent'} {draggingFile === fileName
              ? 'opacity-50'
              : ''}"
            data-annotate="file-row-{fileName}"
            draggable={renamingFile !== fileName}
            ondragstart={(e) => startDrag(e, fileName)}
            ondragend={() => {
              draggingFile = null;
            }}
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
                class="min-w-0 flex-1 px-3 py-1 text-left"
                data-annotate="button-file-{fileName}"
                onclick={() => handleNameClick(fileName)}
              >
                <span class="block truncate text-sm text-zinc-200">
                  {fileName}
                  {#if appStore.selectedFile === fileName && appStore.dirty}
                    <span class="text-zinc-500" title="Unsaved">*</span>
                  {/if}
                </span>
                <span
                  class="block truncate text-[10px] leading-tight text-zinc-500"
                  data-annotate="file-meta-{fileName}"
                >
                  {metaLine(modified_ms, size)}
                </span>
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
              <!-- Transparent backdrop that closes the menu when clicking outside it -->
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
      </div>
    {/if}
  </div>
</div>
