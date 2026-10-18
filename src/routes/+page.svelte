<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { toast } from "svelte-sonner";
  import { ensureConfigFile, frontendReady } from "$lib/api";
  import type { OpenTarget } from "$lib/api";
  import {
    RUN_LOG_CONFIRM_ROWS,
    formatRunLogBlock,
    formatRunLogTimestamp,
    runLogBody,
  } from "$lib/runLog";
  import type { RunLogChoice, RunLogOutcome, RunTarget } from "$lib/runLog";
  import appStore from "$lib/stores/app.svelte";
  import Toolbar from "$lib/components/Toolbar.svelte";
  import EditorToolbar from "$lib/components/EditorToolbar.svelte";
  import ConnectionsPane from "$lib/components/ConnectionsPane.svelte";
  import FilesPane from "$lib/components/FilesPane.svelte";
  import HistoryPane from "$lib/components/HistoryPane.svelte";
  import TablesPane from "$lib/components/TablesPane.svelte";
  import SqlEditor from "$lib/components/SqlEditor.svelte";
  import EditorTabs from "$lib/components/EditorTabs.svelte";
  import ReplaceMultilinePane from "$lib/components/ReplaceMultilinePane.svelte";
  import ResultsPane from "$lib/components/ResultsPane.svelte";
  import ConfigInfoModal from "$lib/components/ConfigInfoModal.svelte";
  import ConfigEditorModal from "$lib/components/ConfigEditorModal.svelte";
  import LicensesModal from "$lib/components/LicensesModal.svelte";
  import AiAnalysisModal from "$lib/components/AiAnalysisModal.svelte";
  import DangerousConfirmModal from "$lib/components/DangerousConfirmModal.svelte";
  import RunLogConfirmModal from "$lib/components/RunLogConfirmModal.svelte";
  import SearchModal from "$lib/components/SearchModal.svelte";
  import ChatPane from "$lib/components/ChatPane.svelte";
  import HelpPane from "$lib/components/HelpPane.svelte";
  import PaneDivider from "$lib/components/PaneDivider.svelte";

  let showSettings = $state(false);
  let showSearch = $state(false);
  /// License list opened from the Third-Party Licenses menu item
  let showLicenses = $state(false);
  /// Settings editor. null = closed
  let configEditorMode = $state<"config" | "source" | null>(null);
  /// Whether the settings editor has unsaved changes (so a mode switch does not discard them)
  let configEditorDirty = $state(false);

  /// Open the settings editor from the menu. Switching to a different mode while the visible editor has unsaved changes
  /// would lose the edits because the #key rebuilds it, so refuse that.
  function openConfigEditor(mode: "config" | "source") {
    if (configEditorMode !== null && configEditorMode !== mode && configEditorDirty) {
      // source mode has no Save, so the wording is split to avoid suggesting an action that cannot be done
      toast.warning(
        configEditorMode === "config"
          ? "Save or discard your changes first"
          : "Discard your edits first (they cannot be saved)",
      );
      return;
    }
    configEditorMode = mode;
  }

  /// Whether any modal is open (the keyboard is considered to belong to the modal).
  /// aiAnalysis (the AI explanation of EXPLAIN) is rendered by ResultsPane rather than this file,
  /// but it covers the screen the same way, so it is checked here.
  const isModalOpen = () =>
    showSearch ||
    showSettings ||
    showLicenses ||
    configEditorMode !== null ||
    appStore.aiAnalysis !== null ||
    appStore.aiExplanation !== null ||
    appStore.dangerousConfirmReason !== null ||
    runLogConfirm !== null;

  /// Global shortcuts.
  /// - Cmd+K (mac) / Ctrl+K toggles the search modal
  /// - Ctrl+Tab / Ctrl+Shift+Tab switches editor tabs in history order
  function handleGlobalKeydown(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      showSearch = !showSearch;
      return;
    }
    // Ctrl+Tab is handled only when Ctrl is the sole modifier (Cmd+Tab is the OS app switcher and
    // Alt+Tab is the Windows window switcher, so do not interfere once more modifiers are held).
    // preventDefault is needed: the default is to move focus, so each press would take the focus
    // away from the editor.
    // Ignore it while a modal is open (so we never end up with the tab having moved unseen behind
    // it and a different file showing once it closes).
    if (e.key === "Tab" && e.ctrlKey && !e.metaKey && !e.altKey && !isModalOpen()) {
      e.preventDefault();
      void appStore.cycleEditorTab(e.shiftKey ? -1 : 1);
    }
  }

  /// End the cycling when Ctrl is released (only then does the chosen tab become the head of the history).
  function handleGlobalKeyup(e: KeyboardEvent) {
    if (e.key === "Control") {
      void appStore.endEditorTabCycle();
    }
  }

  /// When the window loses focus, the Ctrl keyup never arrives (e.g. when switching apps with
  /// Cmd+Tab). Carrying the cycling state over would make the next Ctrl+Tab continue from the old
  /// cycle, so end it here as well.
  function handleWindowBlur() {
    void appStore.endEditorTabCycle();
  }
  let editor: SqlEditor | undefined = $state();

  /// Confirmation dialog before writing a large number of rows back into the editor. null = not shown
  let runLogConfirm = $state<{
    rows: number;
    resolve: (choice: RunLogChoice) => void;
  } | null>(null);

  /// Ask whether to write back and wait for the choice.
  /// If an unanswered one remains, reject it before replacing it (same as the dangerous-statement confirmation)
  const confirmRunLog = (rows: number): Promise<RunLogChoice> =>
    new Promise((resolve) => {
      runLogConfirm?.resolve("cancel");
      runLogConfirm = { rows, resolve };
    });

  function resolveRunLogConfirm(choice: RunLogChoice) {
    const pending = runLogConfirm;
    runLogConfirm = null;
    pending?.resolve(choice);
  }

  /// Run from the editor. For a statement carrying `-- 📝 <label>`, after execution the result is
  /// written back below it as a TSV block comment (Run and Log).
  /// Display in the results table happens as usual regardless of whether anything is written back.
  async function runStatement(target: RunTarget) {
    // Remember the editor tab where the run started. SqlEditor is rebuilt on every tab switch by
    // {#key appStore.activeEditorTabId}, so the editor reference always points at "the file open now".
    // Matching only the target's range could write into a different file that happens to have the
    // same SQL at the same position
    const tabId = appStore.activeEditorTabId;
    // Also remember the active schema (database). If the Database field is switched during
    // execution, the result for the pre-switch schema would stay in the file while neither the tab
    // nor the SQL changed, and a later reader would take it for a result of the current schema
    const schema = appStore.activeSchema;
    // Also remember the connection used for the run (when writing to a tab that has become
    // inactive, check that the tab has not moved to a different connection)
    const connection = appStore.selectedConnection;
    const result = await appStore.runQuery(target.sql);
    if (!result || target.logLabel === null) {
      return;
    }
    // The heading uses the time when the execution finished. If the confirmation dialog below were
    // left open, it would become the approval time, so take it before waiting
    const executedAt = formatRunLogTimestamp(new Date());
    // A large number of rows would fill the editor, so before writing back let the user choose
    // "write all / write only the head / do not write". A result exactly at the limit is the same
    // amount even if written in full, so do not ask
    let maxRows: number | undefined;
    if (result.rows.length > RUN_LOG_CONFIRM_ROWS) {
      const choice = await confirmRunLog(result.rows.length);
      if (choice === "cancel") {
        return;
      }
      maxRows = choice === "limited" ? RUN_LOG_CONFIRM_ROWS : undefined;
    }
    // Use a label re-read from the body right before writing back (passed by SqlEditor)
    const buildBlock = (label: string) =>
      formatRunLogBlock(label, executedAt, runLogBody(result, maxRows));
    // `\c` / `USE` is itself a switch by execution, so the schema that statement switched to
    // is considered "unchanged" (this result belongs to the post-switch schema)
    const expectedSchema = result.switched_schema ?? schema;
    // If the user moved to another tab during execution, write directly into that tab's body
    // (CYBERNEURA-DEV-858). The matching conditions are the same as the editor path, and if the
    // user returns to the tab while waiting, "active" is returned and it is written via the editor path
    let outcome: RunLogOutcome | "active" = "active";
    if (tabId !== null && connection !== null && appStore.activeEditorTabId !== tabId) {
      outcome = await appStore.writeRunLogToInactiveTab(
        tabId,
        connection,
        expectedSchema,
        target,
        buildBlock,
      );
    }
    if (outcome === "active") {
      outcome =
        appStore.activeEditorTabId === tabId &&
        appStore.activeSchema === expectedSchema
          ? (editor?.writeRunLog(target, buildBlock) ?? "stale")
          : "stale";
    }
    switch (outcome) {
      case "stale":
        // The target drifted because of an edit, a tab close, or a schema switch during execution.
        // Notifying without writing is safer than writing to an unrelated position
        toast.warning(
          "The editor changed while the query was running — the log was not written.",
        );
        break;
      case "unmarked":
        // Removing the marker during execution = cancelling the write-back, so silently comply
        break;
      case "broken":
        toast.warning(
          "The existing log block is missing its closing */ — the log was not written.",
        );
        break;
      case "conflicted":
        toast.warning(
          "The file has unresolved external changes — the log was not written.",
        );
        break;
    }
  }

  // Replace Multiline: the editor's multi-line selection state and the display of the replace pane on the right.
  // Snapshot the selection when the pane is opened, and on insertion check that the range has not drifted
  // before replacing (prevents wrong insertion after a file switch or edit)
  let hasMultilineSelection = $state(false);
  let showReplacePane = $state(false);
  let replaceInitialLines = $state("");
  let replaceSnapshot: { from: number; to: number; text: string } | null = null;
  // Incremented every time the pane is reopened; remounts via #key to rebuild Lines
  let replaceOpenToken = $state(0);

  function openReplacePane() {
    const snap = editor?.getMainSelection();
    if (!snap) {
      return;
    }
    replaceSnapshot = snap;
    replaceInitialLines = snap.text;
    replaceOpenToken += 1;
    showReplacePane = true;
  }

  function applyReplace(result: string) {
    const snap = replaceSnapshot;
    const ok =
      snap != null &&
      (editor?.replaceRangeIfMatches(
        snap.from,
        snap.to,
        snap.text,
        result,
      ) ??
        false);
    if (ok) {
      showReplacePane = false;
    } else {
      // If the selection range has drifted (file switch or edit), notify without destroying anything
      toast.error("The editor selection changed — nothing was replaced.", {
        description: "Use Copy to grab the result instead.",
      });
    }
  }

  // Reset the selection tracking state and the replace pane on tab switch / close.
  // Depend on the active tab ID: when a same-named file is opened under a different connection,
  // only the tab can switch while selectedFile (the file name) stays the same, so depending on
  // selectedFile would leave a stale snapshot that gets wrongly applied to the new tab
  $effect(() => {
    void appStore.activeEditorTabId;
    hasMultilineSelection = false;
    showReplacePane = false;
    replaceSnapshot = null;
  });
  /// Tabs of the 2nd column of the left pane (query file list / query history / table list)
  let leftPaneTab = $state<"files" | "history" | "tables">("files");

  // Pane layout. Changed by dragging the dividers and saved to localStorage
  const LAYOUT_PREFIX = "queryfolio.layout.";
  const SIDEBAR_MIN = 140;
  const SIDEBAR_MAX = 500;
  /// The AI chat pane has long content, so allow a wider range than the sidebar
  const CHAT_MIN = 240;
  const CHAT_MAX = 720;
  const HELP_MIN = 260;
  const HELP_MAX = 720;
  const EDITOR_FRAC_MIN = 0.15;
  const EDITOR_FRAC_MAX = 0.85;

  function loadLayoutValue(key: string, fallback: number): number {
    try {
      const raw = localStorage.getItem(LAYOUT_PREFIX + key);
      if (raw === null) return fallback;
      const n = Number(raw);
      return Number.isFinite(n) ? n : fallback;
    } catch {
      return fallback;
    }
  }

  function saveLayoutValue(key: string, value: number) {
    try {
      localStorage.setItem(LAYOUT_PREFIX + key, String(value));
    } catch {
      // Keep the layout change itself working even when localStorage is unavailable
    }
  }

  function clamp(value: number, min: number, max: number): number {
    return Math.min(max, Math.max(min, value));
  }

  /// Width of the connection list pane (px). The default is the previous w-56 = 224px
  let connectionsWidth = $state(
    clamp(loadLayoutValue("connectionsWidth", 224), SIDEBAR_MIN, SIDEBAR_MAX),
  );
  /// Width of the 2nd column (Files / History / Tables) pane (px)
  let sidebarWidth = $state(
    clamp(loadLayoutValue("sidebarWidth", 224), SIDEBAR_MIN, SIDEBAR_MAX),
  );
  /// Vertical fraction occupied by the editor. The default is the previous flex 3:2 = 0.6
  let editorFrac = $state(
    clamp(loadLayoutValue("editorFrac", 0.6), EDITOR_FRAC_MIN, EDITOR_FRAC_MAX),
  );
  // For converting editorFrac to px. Using the sum of the actual heights of the 2 panes being split,
  // not the whole column (including the toolbar), makes the drag follow the cursor exactly
  let editorPaneEl: HTMLDivElement | undefined = $state();
  let resultsPaneEl: HTMLDivElement | undefined = $state();

  /// Width of the AI chat pane (right) (px)
  let chatWidth = $state(
    clamp(loadLayoutValue("chatWidth", 360), CHAT_MIN, CHAT_MAX),
  );
  /// Whether the AI chat pane is open (the visibility is also carried over to the next launch)
  let showChat = $state(loadLayoutValue("chatOpen", 0) === 1);
  let helpWidth = $state(
    clamp(loadLayoutValue("helpWidth", 380), HELP_MIN, HELP_MAX),
  );
  let showHelp = $state(loadLayoutValue("helpOpen", 0) === 1);

  // Base sizes at drag start. PaneDivider passes the cumulative delta from the start position, so
  // computing base + delta keeps in sync with the pointer even after clamp saturation
  let dragBaseConnections = 0;
  let dragBaseSidebar = 0;
  let dragBaseEditorFrac = 0;
  let dragBaseChat = 0;
  let dragBaseHelp = 0;

  const selectedConnectionInfo = $derived(
    appStore.connections.find((c) => c.name === appStore.selectedConnection) ??
      null,
  );
  const selectedEngine = $derived(selectedConnectionInfo?.engine ?? null);
  const selectedCapabilities = $derived(
    selectedConnectionInfo?.capabilities ?? null,
  );

  // If the connection is switched to an engine without table support (redis, etc.) while the TABLES pane
  // is open, go back to FILES (so TablesPane does not call listTables)
  $effect(() => {
    if (
      leftPaneTab === "tables" &&
      selectedCapabilities &&
      !selectedCapabilities.supports_tables
    ) {
      leftPaneTab = "files";
    }
  });

  onMount(() => {
    // Auto-reload / merge when an open query file is changed externally
    appStore.startFileWatcher();

    // Reload on notification from the Reload config file menu item
    const unlistenPromise = listen("menu-reload-config", async () => {
      if (await appStore.reloadConnections()) {
        toast.success("Config reloaded");
      } else {
        toast.error("Failed to reload the config", {
          description: appStore.errorMessage ?? undefined,
        });
      }
    });

    // Notifications from the Edit config.yml / View override config yaml menu items
    const unlistenEditPromise = listen("menu-edit-config", () => {
      openConfigEditor("config");
    });
    const unlistenEditSourcePromise = listen("menu-view-override-config", () => {
      openConfigEditor("source");
    });
    // Third-Party Licenses menu item (app menu on macOS, directly under About in Help elsewhere)
    const unlistenLicensesPromise = listen("menu-show-licenses", () => {
      showLicenses = true;
    });

    // Close Tab menu item (Cmd+W / Ctrl+W). Does not close the window; closes only the active
    // editor tab. Does nothing if there is no tab. Also does nothing while a modal is open (so a tab
    // never closes unseen behind it; same treatment as Ctrl+Tab).
    // isModalOpen only knows modals whose state this page holds, so modals that individual components
    // open themselves (e.g. the cell edit preview of ResultsPane) are detected by the data-modal
    // attribute on the modal's root element
    const unlistenCloseTabPromise = listen("menu-close-editor-tab", () => {
      const id = appStore.activeEditorTabId;
      if (id === null || isModalOpen() || document.querySelector("[data-modal]")) {
        return;
      }
      appStore.closeEditorTab(id);
    });

    // A queue that processes open requests serially. openFileByTarget calls selectConnection, and the
    // store's generation guard cancels the earlier one when a later connection switch arrives, so
    // running several concurrently could silently drop a file of another connection. Open them one at
    // a time in order via a Promise chain (catch so one failure does not stop the chain; individual
    // failures are shown by openFileByTarget through errorMessage).
    let openQueue: Promise<void> = Promise.resolve();
    const enqueueOpen = (connection: string, fileName: string) => {
      openQueue = openQueue
        .then(() => appStore.openFileByTarget(connection, fileName))
        .catch(() => {});
    };

    // Notification when a request to open arrives via a queryfolio:// deep link / CLI while running.
    // Delivers the connection / file name whose location under the storage area the backend has
    // already verified. Even with multiple URLs in one event or several launches in quick succession, open them serially.
    const unlistenOpenFilePromise = listen<OpenTarget>(
      "open-query-file",
      (event) => {
        enqueueOpen(event.payload.connection, event.payload.fileName);
      },
    );
    const unlistenOpenFileErrPromise = listen<string>(
      "open-query-file-error",
      (event) => {
        toast.error("Failed to open the file", {
          description: event.payload,
        });
      },
    );

    void (async () => {
      // Calling frontend_ready makes the backend set ready=true and switch to sending events directly.
      // Before that, wait until the open-query-file / -error listeners are actually installed (the
      // listen Promise resolves); otherwise a request arriving in between would be dropped.
      await unlistenOpenFilePromise;
      await unlistenOpenFileErrPromise;
      try {
        const createdPath = await ensureConfigFile();
        if (createdPath) {
          toast.info("Created a config file", {
            description: `Edit ${createdPath} to add your connections`,
            action: {
              label: "Edit config.yml",
              onClick: () => openConfigEditor("config"),
            },
          });
        }
      } catch (e) {
        toast.error("Failed to create the config file", {
          description: String(e),
        });
      }
      await appStore.loadConnections();
      // Now that the listeners are installed, call frontend_ready to signal "ready", and receive and open
      // the launch-time targets plus those accumulated during startup all at once.
      // Later requests arrive directly via the open-query-file event (nothing is dropped).
      try {
        const { targets, errors } = await frontendReady();
        // Put them on the same queue as live events and open them serially (avoids running concurrently
        // with live events that arrive right after ready).
        for (const target of targets) {
          enqueueOpen(target.connection, target.fileName);
        }
        // Notify failures to resolve launch-time targets with a toast (in a GUI launch stderr is
        // invisible, and swallowing them would leave the user's explicit request with no response).
        for (const message of errors) {
          toast.error("Failed to open the requested file", {
            description: message,
          });
        }
      } catch (e) {
        toast.error("Failed to open the requested file", {
          description: String(e),
        });
      }
    })();

    return () => {
      appStore.stopFileWatcher();
      void unlistenPromise.then((unlisten) => unlisten());
      void unlistenEditPromise.then((unlisten) => unlisten());
      void unlistenEditSourcePromise.then((unlisten) => unlisten());
      void unlistenLicensesPromise.then((unlisten) => unlisten());
      void unlistenCloseTabPromise.then((unlisten) => unlisten());
      void unlistenOpenFilePromise.then((unlisten) => unlisten());
      void unlistenOpenFileErrPromise.then((unlisten) => unlisten());
    };
  });
</script>

<svelte:window
  onkeydown={handleGlobalKeydown}
  onkeyup={handleGlobalKeyup}
  onblur={handleWindowBlur}
/>

<!-- overflow-hidden: even if an inner pane overflows, do not let it extend beyond the app frame
     (works as a pair with overflow: hidden on html/body. CYBERNEURA-DEV-421) -->
<div class="flex h-screen flex-col overflow-hidden bg-zinc-950 text-zinc-200">
  <Toolbar
    onRunCurrent={() => editor?.runCurrentStatement()}
    onOpenSearch={() => {
      showSearch = true;
    }}
    onOpenSettings={() => {
      showSettings = true;
    }}
    helpOpen={showHelp}
    onToggleHelp={() => {
      showHelp = !showHelp;
      saveLayoutValue("helpOpen", showHelp ? 1 : 0);
    }}
    chatOpen={showChat}
    onToggleChat={() => {
      showChat = !showChat;
      saveLayoutValue("chatOpen", showChat ? 1 : 0);
    }}
  />

  <!-- overflow-x-auto: the connection list / sidebar / chat are fixed-width with shrink-0, so
       when the window is narrowed, the right edge overflows after the central editor has been
       squashed to 0 width. Since the document is not made to scroll, this row must scroll
       horizontally by itself, otherwise the right-hand panes become unreachable
       (CYBERNEURA-DEV-421).
       Vertical overflow is suppressed because each pane has its own inner scroll area (specifying
       only one axis makes the other compute to auto, so hidden is set explicitly) -->
  <div class="flex min-h-0 flex-1 overflow-x-auto overflow-y-hidden">
    <div class="shrink-0" style="width: {connectionsWidth}px">
      <ConnectionsPane onEditConfig={() => openConfigEditor("config")} />
    </div>
    <PaneDivider
      direction="vertical"
      annotate="pane-divider-connections"
      onDragStart={() => {
        dragBaseConnections = connectionsWidth;
      }}
      onDrag={(delta) => {
        connectionsWidth = clamp(
          dragBaseConnections + delta,
          SIDEBAR_MIN,
          SIDEBAR_MAX,
        );
      }}
      onDragEnd={() => saveLayoutValue("connectionsWidth", connectionsWidth)}
    />
    <div class="shrink-0" style="width: {sidebarWidth}px">
      {#if leftPaneTab === "files"}
        <FilesPane
          onShowHistory={() => {
            leftPaneTab = "history";
          }}
          onShowTables={() => {
            leftPaneTab = "tables";
          }}
        />
      {:else if leftPaneTab === "history"}
        <HistoryPane
          onShowFiles={() => {
            leftPaneTab = "files";
          }}
          onShowTables={() => {
            leftPaneTab = "tables";
          }}
        />
      {:else}
        <TablesPane
          onShowFiles={() => {
            leftPaneTab = "files";
          }}
          onShowHistory={() => {
            leftPaneTab = "history";
          }}
        />
      {/if}
    </div>
    <PaneDivider
      direction="vertical"
      annotate="pane-divider-sidebar"
      onDragStart={() => {
        dragBaseSidebar = sidebarWidth;
      }}
      onDrag={(delta) => {
        sidebarWidth = clamp(dragBaseSidebar + delta, SIDEBAR_MIN, SIDEBAR_MAX);
      }}
      onDragEnd={() => saveLayoutValue("sidebarWidth", sidebarWidth)}
    />

    <div class="flex min-w-0 flex-1 flex-col">
      {#if appStore.selectedConnection}
        <EditorToolbar
          engine={selectedEngine}
          capabilities={selectedCapabilities}
          readonly={selectedConnectionInfo?.readonly ?? false}
          onExplain={() =>
            appStore.explainQuery(editor?.getCurrentStatement() ?? "")}
          onExplainSql={() =>
            appStore.explainSql(editor?.getCurrentStatement() ?? "")}
          onFormat={() => editor?.formatCurrentStatement()}
          showReplaceMultiline={hasMultilineSelection &&
            appStore.selectedFile !== null}
          onReplaceMultiline={openReplacePane}
        />
      {/if}
      <div
        class="flex min-h-0 basis-0 flex-col border-b border-zinc-700"
        style="flex-grow: {editorFrac}"
        bind:this={editorPaneEl}
      >
        {#if appStore.editorTabs.length > 0}
          <EditorTabs />
        {/if}
        <div class="min-h-0 flex-1">
          {#if appStore.selectedFile}
            <!-- Lay out the editor and the Replace Multiline pane side by side -->
            <div class="flex h-full min-h-0">
              <div class="min-w-0 flex-1">
                <!-- Rebuild the editor on tab switch so the undo history and
                     cursor are not mixed between tabs -->
                {#key appStore.activeEditorTabId}
                  <SqlEditor
                    bind:this={editor}
                    content={appStore.editorContent}
                    engine={selectedEngine}
                    editorLanguage={selectedCapabilities?.editor_language ?? null}
                    schemaMap={appStore.schemaMap}
                    onChange={(content) => appStore.updateEditorContent(content)}
                    onRun={(target) => void runStatement(target)}
                    onSelectionChange={(info) => {
                      hasMultilineSelection = info.hasMultilineSelection;
                    }}
                  />
                {/key}
              </div>
              {#if showReplacePane}
                <div class="w-96 shrink-0 border-l border-zinc-700">
                  {#key replaceOpenToken}
                    <ReplaceMultilinePane
                      initialLines={replaceInitialLines}
                      onReplace={applyReplace}
                      onClose={() => {
                        showReplacePane = false;
                      }}
                    />
                  {/key}
                </div>
              {/if}
            </div>
          {:else}
            <div class="flex h-full items-center justify-center">
              <p class="text-sm text-zinc-600">
                Select or create a query file
              </p>
            </div>
          {/if}
        </div>
      </div>
      <PaneDivider
        direction="horizontal"
        annotate="pane-divider-results"
        onDragStart={() => {
          dragBaseEditorFrac = editorFrac;
        }}
        onDrag={(delta) => {
          const height =
            (editorPaneEl?.clientHeight ?? 0) +
            (resultsPaneEl?.clientHeight ?? 0);
          if (height <= 0) return;
          editorFrac = clamp(
            dragBaseEditorFrac + delta / height,
            EDITOR_FRAC_MIN,
            EDITOR_FRAC_MAX,
          );
        }}
        onDragEnd={() => saveLayoutValue("editorFrac", editorFrac)}
      />
      <div
        class="min-h-0 basis-0"
        style="flex-grow: {1 - editorFrac}"
        bind:this={resultsPaneEl}
      >
        <ResultsPane />
      </div>
    </div>

    <!-- AI chat pane. Sits to the right of the editor / results, spanning the full height -->
    {#if showChat}
      <PaneDivider
        direction="vertical"
        annotate="pane-divider-chat"
        onDragStart={() => {
          dragBaseChat = chatWidth;
        }}
        onDrag={(delta) => {
          // The pane is at the right edge, so the drag direction and the width change are inverted
          chatWidth = clamp(dragBaseChat - delta, CHAT_MIN, CHAT_MAX);
        }}
        onDragEnd={() => saveLayoutValue("chatWidth", chatWidth)}
      />
      <div class="shrink-0" style="width: {chatWidth}px">
        <ChatPane
          supportsAi={selectedCapabilities?.supports_ai ?? false}
          onClose={() => {
            showChat = false;
            saveLayoutValue("chatOpen", 0);
          }}
          onInsert={(sql) => appStore.insertSqlSnippet(sql)}
        />
      </div>
    {/if}

    <!-- Help pane. Sits further right of the chat pane (rightmost) -->
    {#if showHelp}
      <PaneDivider
        direction="vertical"
        annotate="pane-divider-help"
        onDragStart={() => {
          dragBaseHelp = helpWidth;
        }}
        onDrag={(delta) => {
          // The pane is at the right edge, so the drag direction and the width change are inverted
          helpWidth = clamp(dragBaseHelp - delta, HELP_MIN, HELP_MAX);
        }}
        onDragEnd={() => saveLayoutValue("helpWidth", helpWidth)}
      />
      <div class="shrink-0" style="width: {helpWidth}px">
        <HelpPane
          engine={selectedEngine}
          onClose={() => {
            showHelp = false;
            saveLayoutValue("helpOpen", 0);
          }}
          onInsert={(text) => appStore.insertSqlSnippet(text)}
        />
      </div>
    {/if}
  </div>
</div>

{#if showSearch}
  <SearchModal
    onClose={() => {
      showSearch = false;
    }}
  />
{/if}

{#if showSettings}
  <ConfigInfoModal
    onClose={() => {
      showSettings = false;
    }}
  />
{/if}

<!-- Editor for the config file (opened from the menu). mode switches between config, which can
     be saved, and source, which can be edited but not saved.
     The native menu can be used even while a modal is shown, so rebuild with #key when mode
     changes (because the reload is settled at mount time) -->
{#if configEditorMode !== null}
  {#key configEditorMode}
    <ConfigEditorModal
      mode={configEditorMode}
      onDirtyChange={(dirty) => {
        configEditorDirty = dirty;
      }}
      onClose={() => {
        configEditorMode = null;
        configEditorDirty = false;
      }}
    />
  {/key}
{/if}

<!-- Modal for the AI explanation of the selected SQL (reuses the EXPLAIN explanation modal with a different heading) -->
{#if appStore.aiExplanation !== null}
  <AiAnalysisModal
    title="AI SQL Explanation"
    text={appStore.aiExplanation}
    onClose={() => appStore.closeAiExplanation()}
  />
{/if}

<!-- Pre-execution confirmation modal for dangerous statements (connections with allow_dangerous_statements enabled) -->
{#if appStore.dangerousConfirmReason !== null}
  <DangerousConfirmModal
    reason={appStore.dangerousConfirmReason}
    onConfirm={() => appStore.confirmDangerous()}
    onCancel={() => appStore.cancelDangerous()}
  />
{/if}

<!-- Confirmation, for a statement with the 📝 marker, before writing a result with many rows back into the editor -->
{#if runLogConfirm !== null}
  <RunLogConfirmModal
    rows={runLogConfirm.rows}
    limit={RUN_LOG_CONFIRM_ROWS}
    onChoose={resolveRunLogConfirm}
  />
{/if}

<!-- Third-Party Licenses (opened from the menu). The native menu can be chosen even while a modal
     is shown, so place it after the other modals to put it on top (see also LicensesModal's z-index) -->
{#if showLicenses}
  <LicensesModal
    onClose={() => {
      showLicenses = false;
    }}
  />
{/if}
