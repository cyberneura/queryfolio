import { toast } from "svelte-sonner";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import * as api from "$lib/api";
import type {
  AiInfo,
  ChatToolCall,
  ChatTurn,
  ConnectionInfo,
  QueryResult,
} from "$lib/api";
import { merge3 } from "$lib/mergeText";
import { planRunLogWrite } from "$lib/runLog";
import type { RunLogOutcome, RunTarget } from "$lib/runLog";
import {
  buildCycleOrder,
  forgetMru,
  stepCycleIndex,
  touchMru,
} from "$lib/editor/tabCycle";
import { buildEngineHelpContext } from "$lib/help";

const AUTO_SAVE_DELAY_MS = 1000;

/// Interval (ms) for checking whether the open query file was changed outside the app.
const FILE_WATCH_INTERVAL_MS = 2500;
/// Interval for re-fetching the FILES pane list (modified time / size). External changes to files
/// that are not open, and rewrites with identical content (only mtime changes), cannot be detected
/// by comparing tab contents, so alongside the watcher ticks we also periodically re-fetch the list itself (CYBERNEURA-DEV-774)
const FILE_LIST_REFRESH_INTERVAL_MS = 10_000;

/// Maximum number of result tabs. When exceeded, the oldest non-pinned tab is discarded.
const MAX_RESULT_TABS = 10;

/// State of one tab in the results pane. In addition to the result set, it keeps
/// "what was run, where, and when" so the tab can be re-executed.
export interface ResultTab {
  id: number;
  pinned: boolean;
  sql: string;
  connection: string;
  schema: string | null;
  /// Execution start time (epoch ms)
  executedAt: number;
  result: QueryResult | null;
  error: string | null;
  /// Execution was aborted by a cancel request (shown differently from an error)
  cancelled: boolean;
  running: boolean;
  /// Asking the AI for an error fix suggestion (spinner shown, prevents duplicate runs)
  fixing: boolean;
  /// Fix suggestion SQL returned by the AI (null if none). It is never run automatically;
  /// the user inserts it into the editor with Apply.
  fixSuggestion: string | null;
}

/// One message shown in the AI chat pane.
/// Only role / content are sent to the LLM; everything else is auxiliary information for display.
export interface ChatMessage {
  id: number;
  role: "user" | "assistant";
  content: string;
  /// Read queries the agent ran while producing this response (assistant only)
  toolCalls?: ChatToolCall[];
  /// Fetching the response failed (the error text goes in content and is shown in red)
  failed?: boolean;
}

/// Determines whether the tab's SQL comes from EXPLAIN (used to show the Analyze with AI button).
/// SQL built by the Explain button always starts with EXPLAIN, so we decide by matching
/// the leading keyword (a hand-typed EXPLAIN also qualifies).
export const isExplainSql = (sql: string): boolean =>
  sql.trimStart().toLowerCase().startsWith("explain");

/// One open editor tab. Tabs are global (across all connections) and lined up in a single row,
/// and each tab holds its own connection. Activating a tab switches to that connection
/// (engine / schema / completion / execution are all driven by the active tab's connection).
export interface EditorTab {
  id: number;
  connection: string;
  file: string;
  content: string;
  /// Whether there are unsaved edits (goes back to false on autosave)
  dirty: boolean;
  /// Content known to be on disk (on load = what was read, on save = what was written).
  /// Used to detect external changes (compare this value with the actual file) and as the base for the 3-way merge.
  diskContent: string;
  /// Whether an external change and the local unsaved edits are in a conflict that cannot be auto-merged.
  /// While true, autosave on "close", "quit the app", etc. is suppressed so that local edits
  /// do not silently overwrite the external change (matching the warning text "reopen the file to discard"
  /// to the actual behavior).
  /// If the user keeps editing or saves explicitly, it goes back to false and normal overwrite-save applies.
  conflicted?: boolean;
}

let connections = $state<ConnectionInfo[]>([]);
let selectedConnection = $state<string | null>(null);
/// Writable switch. While false (the default), only side-effect-free statements such as
/// SELECT/SHOW can be run (enforced by the backend). To prevent accidents it starts OFF
/// in every session and is not persisted (a restart never silently makes it writable).
let writable = $state(false);
/// List for the FILES pane (descending by modified time, with timestamps and sizes)
let fileEntries = $state<api.QueryFileEntry[]>([]);
/// Advanced every time fileEntries is rewritten. Prevents a stale fetch result that resolves later
/// from overwriting a newer list (refreshFileEntries is fire-and-forget and calls can overlap).
let fileEntriesVersion = 0;
const setFileEntries = (entries: api.QueryFileEntry[]) => {
  fileEntriesVersion++;
  fileEntries = entries;
};
/// File names only from the list (for existence checks / numbering; same order as fileEntries)
const files = $derived(fileEntries.map((e) => e.file_name));
let editorTabs = $state<EditorTab[]>([]);
let activeEditorTabId = $state<number | null>(null);
let resultTabs = $state<ResultTab[]>([]);
let activeTabId = $state<number | null>(null);
let errorMessage = $state<string | null>(null);
let loadingConnections = $state(false);
let schemas = $state<string[]>([]);
let activeSchema = $state<string | null>(null);
/// AI settings info (null if not yet fetched or the fetch failed)
let aiInfo = $state<AiInfo | null>(null);
/// AI settings resolution error (unknown provider etc.; shown via the button's title)
let aiError = $state<string | null>(null);
/// Generating SQL with AI (button spinner, prevents double submit)
let aiGenerating = $state(false);
/// Explaining the execution plan with AI (button spinner, prevents double submit)
let aiAnalyzing = $state(false);
/// Markdown of the AI execution plan explanation (non-null only while the modal is shown)
let aiAnalysis = $state<string | null>(null);
/// Explaining the selected SQL with AI (button spinner, prevents double submit)
let aiExplaining = $state(false);
/// Markdown of the AI explanation of the selected SQL (non-null only while the modal is shown)
let aiExplanation = $state<string | null>(null);
/// Messages currently shown in the AI chat (right pane).
/// The schema differs per connection, so discard them when the connection changes.
let chatMessages = $state<ChatMessage[]>([]);
/// Chat generation that the in-flight round trip belongs to (null when not waiting).
/// So that the spinner of an old round trip does not linger in a new conversation after the
/// conversation is discarded, "sending" in the UI is shown only when it matches the current generation (chatSending).
let chatSendingGen = $state<number | null>(null);
/// Connection running an in-flight round trip -> set of that round trip's request IDs.
/// Treated as a "running connection" just like query execution, so that the tunnel / pool is not
/// torn down (maybeDisconnectIfIdle) merely because there is no editor tab.
/// Discarding the conversation lets the next send happen while an old round trip is still running,
/// so several can run on the same connection at once. To be able to specify which one to abort,
/// we keep a set of IDs rather than a count (the Map is rebuilt each time to keep reactivity).
let chatRunningConnections = $state<Map<string, Set<string>>>(new Map());

/// Numbering of IDs attached to chat round trips (unique within the process is enough).
let nextChatRequestSeq = 1;

/// Number of in-progress "discard conversation -> wait for abort to arrive -> switch backend" transitions.
/// New sends are not accepted during this time: an abort request targets "the IDs running at that
/// moment", so a round trip sent while we are waiting would not be aborted and would use the
/// post-switch pool with the old prompt.
/// A boolean would let the one that finishes first also release the later ones when transitions overlap
/// (e.g. schemas switched in succession), so we use a counter.
let chatTransitions = $state(0);
/// Map of table name -> column name list for SQL completion (null if not fetched or the fetch failed)
let schemaMap = $state<Record<string, string[]> | null>(null);
/// Confirmation dialog before running a dangerous statement (connection with allow_dangerous_statements enabled).
/// The modal is shown while non-null, and the user's response is passed to resolve.
let dangerousConfirm = $state<{
  reason: string;
  resolve: (ok: boolean) => void;
} | null>(null);

// Sequence number for result tab IDs (unique within the session is enough, so not persisted)
let nextTabId = 1;
// Sequence number for editor tab IDs
let nextEditorTabId = 1;
/// Remember the editor tab ID last active for each connection and restore it when returning to the connection
const lastActiveTabByConnection = new Map<string, number>();

/// Order in which editor tabs were last activated (head = most recent). Used for the Ctrl+Tab cycling order.
/// Kept independently of the tab layout because it follows history order, not display order.
/// Not $state: the UI does not render it, so it does not need to be reactive.
let tabMruOrder: number[] = [];

/// Cycling state while Tab is pressed repeatedly with Ctrl held down (null = not cycling).
/// order is a snapshot of the MRU order at the start of the cycle.
///
/// The key point is that **the MRU is not rewritten during cycling**. Promoting on every press would
/// just bounce between the two most recent tabs instead of "advancing as many times as pressed".
/// When Ctrl is released (endEditorTabCycle), the selected tab is promoted to the head exactly once. This
/// way, pressing Ctrl+Tab twice as single presses alternates between two tabs (same as ordinary MRU switching).
let tabCycle: { order: number[]; index: number } | null = null;

/// Serialization queue for cycling. Each step awaits a connection switch, so pressing Ctrl+Tab
/// rapidly runs several steps concurrently, and depending on completion order the "last visible tab"
/// and tabCycle.index can drift apart. Feeding steps through in order removes that drift structurally.
/// The commit (endEditorTabCycle) is queued in the same queue as well, so the MRU is settled in the
/// state after the key input has been fully processed.
let tabCycleChain: Promise<void> = Promise.resolve();
const queueTabCycleStep = (step: () => void | Promise<void>): Promise<void> => {
  tabCycleChain = tabCycleChain.then(step).catch(() => {});
  return tabCycleChain;
};

/// Cycle generation. Advanced each time a cycle is aborted. **Remember the generation when queuing and
/// check it just before running**: aborting only sets `tabCycle` to null, so on its own a step that was
/// already queued would run later and recreate the cycle
/// (moving away from the tab the user explicitly chose).
let tabCycleGeneration = 0;

/// Abort the cycle in progress (when a move other than cycling happens: tab click, opening a file,
/// closing a tab, etc.).
const cancelTabCycle = () => {
  tabCycle = null;
  tabCycleGeneration++;
};

/// Promote a tab to the head of the MRU. Not called during cycling (for the reason above).
const touchTabMru = (id: number) => {
  tabMruOrder = touchMru(tabMruOrder, id);
};

const getActiveEditorTab = (): EditorTab | null =>
  editorTabs.find((t) => t.id === activeEditorTabId) ?? null;

/// Generation number of the running loadSchemaMap. Used so that, on successive connection / schema
/// switches, only the result of the latest request is applied even if an old response resolves later.
let schemaMapGeneration = 0;

/// Generation number of the running applyConnectionContext. On successive connection switches, checks
/// before commit whether this is the latest generation, so that the response of a slow connection resolving later
/// does not overwrite the new connection's files / schemas / activeSchema (same intent as schemaMapGeneration)
let connectionContextGeneration = 0;

/// File / tab navigation generation. Advanced each time a file is opened (selectFile) or a tab is activated
/// (activateEditorTab). Used by processing that awaits to check whether the user navigated to another
/// file / tab in the meantime (connectionContextGeneration only catches connection switches, so
/// moves between files within the same connection are covered by this one).
let navigationGeneration = 0;

let autoSaveTimer: ReturnType<typeof setTimeout> | null = null;
/// ID of the editor tab with an autosave scheduled (the target while debouncing)
let autoSavePendingTabId: number | null = null;

const toErrorMessage = (e: unknown): string =>
  typeof e === "string" ? e : e instanceof Error ? e.message : String(e);

/// Effective Writable for the given connection. The Writable switch is a single one in the toolbar and
/// represents the state of the currently selected connection, so for another connection (e.g. re-running
/// from another connection's tab) it is always false (read-only). Both the execution guard and the
/// dangerous statement confirmation use this value, keeping "writes are allowed only on the connection the toggle shows" consistent.
const effectiveWritable = (connection: string): boolean =>
  connection === selectedConnection && writable;

const loadConnections = async () => {
  loadingConnections = true;
  errorMessage = null;
  try {
    connections = await api.getConnections();
    // The resolved connection config is already cached in the backend, so
    // fetching the AI settings is cheap here (the fetch command is not re-run)
    await loadAiInfo();
  } catch (e) {
    errorMessage = toErrorMessage(e);
    connections = [];
  } finally {
    loadingConnections = false;
  }
};

/// Fetch the AI settings info (configured / model).
/// Unconfigured returns configured: false, and a config resolution error
/// (unknown provider etc.) goes into aiError and is explained in the AI button's title.
const loadAiInfo = async () => {
  try {
    aiInfo = await api.getAiInfo();
    aiError = null;
  } catch (e) {
    aiInfo = null;
    aiError = toErrorMessage(e);
  }
};

/// Reload the connection settings (pools and SSH tunnels are discarded too).
/// Clear the selection state of the old settings first, and reselect only if a connection with
/// the same name still exists (the file list is also re-fetched with the new settings).
/// Returns false on failure (errorMessage has been set).
const reloadConnections = async (): Promise<boolean> => {
  // A reload discards all editor tabs. Conflicted tabs are not saved by saveAllDirtyTabs
  // (so external changes are not silently overwritten), so proceeding as is would lose local edits
  // without saving or an explicit discard. While a conflict remains, abort the reload and
  // ask the user to resolve it (overwrite by saving / discard by reopening).
  const conflicted = editorTabs.find((t) => t.conflicted);
  if (conflicted) {
    errorMessage =
      `Cannot reload config while "${conflicted.file}" has an unresolved ` +
      `external-change conflict. Resolve it with Overwrite or Discard in the ` +
      `editor toolbar, then reload.`;
    return false;
  }
  // Tabs are discarded, so save all unsaved tabs first, not just the pending one
  if (!(await saveAllDirtyTabs())) {
    return false;
  }
  // Abort a chat awaiting a response **before** resetConnections and wait until the request
  // arrives. If deferred / fire-and-forget, an agent still holding the pre-reload connection config
  // could reopen the discarded pool and keep running tools with stale credentials / schema.
  await clearChatAndWait();
  try {
    await api.resetConnections();
  } catch (e) {
    endChatTransition();
    errorMessage = toErrorMessage(e);
    return false;
  }
  // The backend swap is done, so the chat transition is over
  // (round trips sent from now on run with the new settings)
  endChatTransition();
  const previousConnection = selectedConnection;
  selectedConnection = null;
  // The connections are replaced by the settings reload, so also return Writable to the safe side (OFF)
  writable = false;
  setFileEntries([]);
  // The settings are replaced wholesale, so discard all open editor tabs
  if (autoSaveTimer) {
    clearTimeout(autoSaveTimer);
    autoSaveTimer = null;
  }
  autoSavePendingTabId = null;
  // Even if a connection switch (applyConnectionContext) is in flight before/after the reset, advance the
  // generation so that its old response cannot commit later and revive a stale connection
  connectionContextGeneration++;
  editorTabs = [];
  activeEditorTabId = null;
  lastActiveTabByConnection.clear();
  tabMruOrder = [];
  cancelTabCycle();
  // A settings reload makes the backend discard all pools / tunnels (resetConnections).
  // Also clear the established flags so they can be re-established at the next opportunity.
  resourcesLoaded.clear();
  // A reload discards the pools of all connections, so advance the global reset generation so that an
  // in-flight schema import (of any connection) does not re-register a discarded pool as "established".
  // The per-connection generation (connectionLifecycleGen) would miss in-flight work of a connection
  // that is not selected, so resets are detected with this generation shared by all connections.
  connectionsResetGen++;
  // The settings are replaced wholesale, so discard all tabs including pinned ones
  resultTabs = [];
  activeTabId = null;
  schemas = [];
  activeSchema = null;
  aiInfo = null;
  aiError = null;
  aiAnalysis = null;
  aiExplanation = null;
  // Chat abort and discard were done before resetConnections (clearChat)
  // Advance the generation and discard so that an in-flight fetch cannot later write an old map
  schemaMapGeneration++;
  schemaMap = null;
  await loadConnections();
  if (errorMessage) {
    return false;
  }
  if (
    previousConnection &&
    connections.some((c) => c.name === previousConnection)
  ) {
    await selectConnection(previousConnection);
  }
  return true;
};

/// Re-fetch the schema map for SQL completion in the background.
/// Completion is only an aid, so on failure continue without completion and do not notify.
const loadSchemaMap = async () => {
  const connection = selectedConnection;
  const generation = ++schemaMapGeneration;
  if (!connection) {
    schemaMap = null;
    return;
  }
  // Clear first so that candidates from the old schema are not offered while fetching
  schemaMap = null;
  try {
    const map = await api.getSchemaMap(connection);
    // If a newer request has started, discard the old response
    if (generation === schemaMapGeneration) {
      schemaMap = map;
    }
  } catch {
    // Continue silently without completion (neither toast nor errorMessage)
  }
};

// Commit the pending autosave. Returns false if saving fails.
// The caller should abort screen navigation on false, to protect unsaved edits.
const flushPendingSave = async (): Promise<boolean> => {
  if (autoSaveTimer) {
    clearTimeout(autoSaveTimer);
    autoSaveTimer = null;
  }
  const pendingId = autoSavePendingTabId;
  autoSavePendingTabId = null;
  // Commit only the debounce-scheduled tab (= the tab edited until just now).
  // So that another tab's save failure does not drag the navigation down, the target is limited to one tab
  // (editor tabs persist across connections, so switching does not lose their content).
  if (pendingId == null) {
    return true;
  }
  const tab = editorTabs.find((t) => t.id === pendingId);
  if (tab && tab.dirty) {
    return saveEditorTab(tab);
  }
  return true;
};

/// Save all dirty editor tabs (best-effort). Returns true if all succeed.
/// Called before discarding tabs (reloadConnections) so unsaved edits are not lost.
/// A dirty tab whose pending flag was cleared by an autosave failure is reliably included here too.
const saveAllDirtyTabs = async (): Promise<boolean> => {
  if (autoSaveTimer) {
    clearTimeout(autoSaveTimer);
    autoSaveTimer = null;
  }
  autoSavePendingTabId = null;
  let ok = true;
  for (const tab of editorTabs) {
    // Do not save conflicted tabs (so external changes are not silently overwritten by local edits).
    // Only when the user resolves the conflict by continuing to edit / saving explicitly is it saved as a normal dirty tab.
    if (tab.dirty && !tab.conflicted) {
      if (!(await saveEditorTab(tab))) {
        ok = false;
      }
    }
  }
  return ok;
};

/// Load the file list, schema and completion map of the given connection and switch the connection
/// context (does not touch editor tabs). Used by both connection selection and tab activation.
///
/// Important: reflect `selectedConnection` **all at once at the end**, after loading finishes.
/// If `selectedConnection = name` is set first, during the await there is a mismatch of "the connection is
/// new (name) but the editor is still showing the old tab's SQL", and pressing Run in that window
/// would run the old SQL on the new connection (fatal for a DB client). The caller's update of
/// `activeEditorTabId` happens in the same microtask right after this function resolves, so no user
/// action (macrotask) can interrupt between the commit and the tab update, and the connection and tab always stay consistent.
///
/// Returns true if committed. If overtaken by a newer request during successive switches, nothing is
/// reflected and false is returned (the caller aborts without touching activeEditorTabId).
const applyConnectionContext = async (name: string): Promise<boolean> => {
  const generation = ++connectionContextGeneration;
  const defaultSchema = connections.find((c) => c.name === name)?.schema ?? null;
  let loadedFiles: api.QueryFileEntry[] = [];
  let filesError: string | null = null;
  try {
    loadedFiles = await api.listQueryFiles(name);
  } catch (e) {
    filesError = toErrorMessage(e);
  }
  // If a newer switch started during the await, discard this response (prevents overwriting)
  if (generation !== connectionContextGeneration) {
    return false;
  }
  // Fetching the active schema does not open a connection (it only returns schema_override or the config
  // value), so it is fine to do on selection. The schema list (listSchemas) opens a connection,
  // so it is not fetched here (to avoid "the tunnel opens the moment it is selected").
  let schema = defaultSchema;
  try {
    schema = (await api.getActiveSchema(name)) ?? schema;
  } catch {
    // getActiveSchema does not open a connection, but just in case keep the default schema on failure
  }
  if (generation !== connectionContextGeneration) {
    return false;
  }
  // From here to resolve, reflect the connection context in one go without awaiting.
  // When switching to another connection, return Writable to the safe side (OFF). This prevents the accident
  // of moving to another connection (e.g. production) with writes still allowed on one and writing by mistake.
  // Keep it on re-selecting the same connection (e.g. switching editor tabs of the same connection).
  if (selectedConnection !== name) {
    writable = false;
    // Also discard the AI chat conversation and abort the running agent.
    // The schema in the system prompt differs per connection so the conversation cannot carry over, and
    // merely discarding it would let the backend keep running tools on the pre-switch connection
    clearChat();
  }
  selectedConnection = name;
  errorMessage = filesError;
  setFileEntries(filesError ? [] : loadedFiles);
  activeSchema = schema;
  // The schema list / completion map open a connection, so they are not fetched at selection time
  // (to avoid "the tunnel opens the moment it is selected"). Leave only the current schema in the dropdown, and
  // import the full list and completion map via ensureConnectionResources when a file is loaded into the
  // editor / the schema browser is opened.
  schemas = schema ? [schema] : [];
  // Clear the completion candidates for the new connection (also advance the generation to prevent a late write from an old fetch)
  schemaMapGeneration++;
  schemaMap = null;
  if (resourcesLoaded.has(name)) {
    // When an already established connection is re-selected, the tunnel is still open, so re-fetch the schema
    // list / completion map reset above and restore the UI (dropdown / completion). This does not newly open
    // a tunnel (it does not violate the "do not open at selection" policy).
    // Do not clear resourcesLoaded — clearing it and re-registering asynchronously would race with the disconnect
    // check (maybeDisconnectIfIdle) and re-registration if another connection is switched to in that gap, and a
    // tunnel with no editor tab could be left open. Keep the established state and re-fetch only the UI.
    void loadConnectionSchemaResources(name);
  }
  return true;
};

/// Names of connections that are established (tunnel/pool opened, schema list and completion map imported).
/// Registered in ensureConnectionResources and removed on disconnect / reset.
/// Use this as the source of truth for the connection state and do not substitute the presence of a cache (schemas etc.)
/// (the cache remains after disconnect, so substituting would miss the trigger to reopen).
const resourcesLoaded = new Set<string>();

/// Per connection, "the number of times a disconnect / reset happened". Used to detect whether the connection was
/// disconnected while waiting for the schema import (listSchemas is async and the tunnel is opened in the meantime).
/// Remember the value at the start of the import, and if it changed after completion we know "it was disconnected while waiting"
/// (state (selectedConnection / editorTabs) alone cannot distinguish a selected connection whose last tab was closed
/// and disconnected from a selected connection kept alive only by the schema browser, so we count events).
const connectionLifecycleGen = new Map<string, number>();
const bumpConnectionLifecycle = (connection: string) => {
  connectionLifecycleGen.set(
    connection,
    (connectionLifecycleGen.get(connection) ?? 0) + 1,
  );
};

/// A settings reload (reloadConnections -> resetConnections) discards the pools/tunnels of all connections at once.
/// The per-connection generation would miss "in-flight imports of non-selected connections", so
/// resets are detected with this generation shared by all connections. Remember it at the start of the import,
/// and if it advanced after completion we know "a global reset happened while waiting".
let connectionsResetGen = 0;

/// Import the connection's schema list / completion map and reflect them in the UI (dropdown / completion).
/// This is the point where listSchemas acquires the pool (= opens the tunnel). Returns true if listSchemas
/// succeeded and no disconnect intervened during the import (used by the caller to manage resourcesLoaded).
/// If another connection was switched to during the import, skip reflecting (selectedConnection guard).
/// resourcesLoaded itself is not touched here — to separate the "is it established" decision from
/// "is it importing", so that a switch during a re-import does not conflict with lifecycle management
/// (maybeDisconnectIfIdle).
const loadConnectionSchemaResources = async (
  connection: string,
): Promise<boolean> => {
  const gen = connectionLifecycleGen.get(connection) ?? 0;
  const resetGen = connectionsResetGen;
  let loaded: string[];
  try {
    loaded = await api.listSchemas(connection);
  } catch {
    // A connection failure etc. is not fatal. Keep only the current value in the dropdown and retry at the next opportunity.
    return false;
  }
  // If a disconnect (closing the last editor tab / switching to another connection) or a settings reload
  // intervened while waiting for the import, this connection is no longer alive. In case listSchemas
  // reopened the tunnel, close it again (if idle), and do neither the UI reflection nor the registration as established.
  // Without this, an idle connection would remain "established" and the lazy lifecycle would break. Disconnects are detected by
  // the per-connection generation, and a settings reload (discarding all connections) by the global reset generation.
  if (
    (connectionLifecycleGen.get(connection) ?? 0) !== gen ||
    connectionsResetGen !== resetGen
  ) {
    maybeDisconnectIfIdle(connection);
    return false;
  }
  if (selectedConnection === connection) {
    schemas = loaded;
    // Also import the completion map (it uses the same pool, so no additional tunnel is opened.
    // On failure continue without completion).
    void loadSchemaMap();
  }
  return true;
};

/// Call at the moment the tunnel / connection should actually be opened (when a file is loaded into the editor,
/// when the schema browser is opened). Establish the connection and import the schema list and completion map.
/// Not called by connection selection alone (to avoid opening the tunnel the moment it is selected).
/// Does nothing if already established (resourcesLoaded). It is cleared on disconnect, so after closing
/// all editors and disconnecting, opening again re-establishes it here reliably.
const ensureConnectionResources = async (connection: string) => {
  if (connection !== selectedConnection || resourcesLoaded.has(connection)) {
    return;
  }
  // Registering before success would mistake a failed connection for "established" and miss retries, so
  // register after listSchemas (which opens the connection) succeeds. If a disconnect intervened during the import,
  // loadConnectionSchemaResources returns false (and closes the reopened tunnel again),
  // so do not register in that case — do not mistake an idle connection for "established".
  if (await loadConnectionSchemaResources(connection)) {
    resourcesLoaded.add(connection);
  }
};

/// Destroy the SSH tunnel / pool if this connection is no longer needed (no editor tab, and no running
/// query / cell edit). Called when an editor tab is closed and when a query completes. It is not
/// destroyed while a query is running (cutting the tunnel midway would break the running query's connection).
/// schema_override remains in the backend, so after reopening it connects with the same active schema.
/// It is reopened automatically the next time a file is opened / the schema browser is opened /
/// a query is run.
const maybeDisconnectIfIdle = (connection: string) => {
  if (editorTabs.some((t) => t.connection === connection)) {
    return;
  }
  if (isConnectionRunning(connection)) {
    return;
  }
  resourcesLoaded.delete(connection);
  // Record the disconnect event. If this connection's schema import is in flight, detect on completion that
  // "it was disconnected while waiting" and prevent registration as established (avoids defeating the lazy lifecycle).
  bumpConnectionLifecycle(connection);
  // fire-and-forget. A failure is not fatal (it is overwritten the next time the connection is reopened), so
  // swallow it to avoid an unhandled rejection.
  void api.disconnect(connection).catch(() => {});
};

/// Pick, among the editor tabs tied to the connection, the one to restore as active.
/// Prefer the most recently active tab, else the last opened tab, else null.
const pickTabForConnection = (name: string): number | null => {
  const remembered = lastActiveTabByConnection.get(name);
  if (
    remembered != null &&
    editorTabs.some((t) => t.id === remembered && t.connection === name)
  ) {
    return remembered;
  }
  for (let i = editorTabs.length - 1; i >= 0; i--) {
    if (editorTabs[i].connection === name) {
      return editorTabs[i].id;
    }
  }
  return null;
};

const selectConnection = async (name: string) => {
  if (name === selectedConnection) {
    // Re-selecting the current connection cancels an in-progress switch to another connection (since
    // applyConnectionContext does not change selectedConnection until commit, the current connection appears
    // selected during it). Advancing the generation keeps the in-flight switch from committing,
    // matching the intent of the operation, "stay on the current connection".
    connectionContextGeneration++;
    return;
  }
  const previous = selectedConnection;
  // Unsaved tabs are saved best-effort, but a save failure does not stop the switch.
  // Editor tabs persist across connections, so switching does not lose their content
  // (even if saving fails, e.g. not writable, they stay dirty in the tab).
  await flushPendingSave();
  // Result tabs and editor tabs persist across connections (not discarded on connection switch).
  // If overtaken by a newer switch, abort without touching the tab selection.
  if (!(await applyConnectionContext(name))) {
    return;
  }
  // Explicitly choosing a connection is a separate operation from cycling. If cycling is in progress
  // (e.g. the connection was clicked with Ctrl held), abort the cycle here and treat it as a normal move
  // (otherwise later steps would run on the old order snapshot).
  cancelTabCycle();
  // Restore the tab last open on this connection (the editor shows empty if none)
  activeEditorTabId = pickTabForConnection(name);
  if (activeEditorTabId != null) {
    // This path does not go through activateEditorTab, so update the MRU here ourselves
    touchTabMru(activeEditorTabId);
  }
  // If the connection switched from has "no editor tab at all", judge it no longer needed and close the
  // tunnel / pool. A connection where only the schema browser (TABLES) was opened has no editor tab and
  // does not go through removeEditorTab / executeTab, so if not closed here it stays open.
  // A connection with editor tabs makes maybeDisconnectIfIdle a no-op and stays open after the switch
  // (per the policy "a tunnel once made stays open").
  if (previous) {
    maybeDisconnectIfIdle(previous);
  }
};

/// Activate an editor tab. If the tab's connection differs from the current one, switch to
/// that connection (files / schema / completion are aligned to the tab's connection too).
///
/// @param viaCycle - Whether it was called from Ctrl+Tab cycling. For paths other than cycling (tab
///   click etc.), abort the cycle in progress and treat it as a normal move. This prevents, when a
///   tab is clicked with Ctrl held, continuing from the old cycle position without putting that tab
///   on the MRU.
const activateEditorTab = async (id: number, viaCycle = false) => {
  if (!viaCycle) {
    cancelTabCycle();
  }
  if (id === activeEditorTabId) {
    return;
  }
  const tab = editorTabs.find((t) => t.id === id);
  if (!tab) {
    return;
  }
  // Activating a tab is navigation too. Advance the generation so that processing that awaits can
  // detect that "the user then moved to another tab".
  const navGen = ++navigationGeneration;
  // Unsaved tabs are saved best-effort, but a save failure does not stop activation.
  // (so that unsaved SQL can still be viewed / copied even if saving fails, e.g. not writable.
  //  The tab remains, so the content is not lost)
  await flushPendingSave();
  const previous = selectedConnection;
  if (tab.connection !== selectedConnection) {
    // If overtaken by a newer switch, do not activate this tab
    if (!(await applyConnectionContext(tab.connection))) {
      return;
    }
    // Activating a tab is also a path that switches the connection. If the connection switched from was opened only via the schema browser
    // (no editor tab), close it here too, as selectConnection does.
    // Without closing this path, the tunnel of a connection where only TABLES was opened would be left open.
    if (previous) {
      maybeDisconnectIfIdle(previous);
    }
  }
  // If the user moved to another tab / file during the await, writing
  // activeEditorTabId here would overwrite that move. A cycling step that involves a connection switch
  // is especially long, and a click during it hits this.
  if (navGen !== navigationGeneration) {
    return;
  }
  activeEditorTabId = id;
  lastActiveTabByConnection.set(tab.connection, id);
  if (!tabCycle) {
    touchTabMru(id);
  }
};

/// Move one step forward / back in history (MRU) order with Ctrl+Tab / Ctrl+Shift+Tab.
/// Pressing repeatedly with Ctrl held advances that many steps deeper.
/// Steps are queued so they are processed in order even when pressed rapidly.
const cycleEditorTab = (direction: 1 | -1): Promise<void> => {
  // Generation at the time the key was pressed. If the cycle was aborted before execution, this step
  // is treated as if it never happened.
  const generation = tabCycleGeneration;
  return queueTabCycleStep(() => {
    if (generation !== tabCycleGeneration) {
      return;
    }
    return cycleEditorTabStep(direction);
  });
};

const cycleEditorTabStep = async (direction: 1 | -1) => {
  if (editorTabs.length === 0) {
    return;
  }
  if (!tabCycle) {
    const order = buildCycleOrder(
      tabMruOrder,
      editorTabs.map((t) => t.id),
    );
    const current = activeEditorTabId == null ? -1 : order.indexOf(activeEditorTabId);
    // When there is no active tab (e.g. a connection with no tabs is selected), start from
    // "one before the head" so that the first Ctrl+Tab lands on the head of the history.
    // In the reverse direction, start from "one after the tail" = 0 so it lands on the tail. Starting at 0
    // would skip the most recent tab and move to the second one.
    tabCycle = {
      order,
      index: current >= 0 ? current : direction === 1 ? -1 : 0,
    };
  }
  const cycle = tabCycle;
  if (cycle.order.length === 0) {
    return;
  }
  cycle.index = stepCycleIndex(cycle.index, direction, cycle.order.length);
  await activateEditorTab(cycle.order[cycle.index], true);
};

/// End the cycle when Ctrl is released (or the window loses focus).
/// Only now is the tab currently being viewed promoted to the head of the MRU.
/// It is queued after pending steps, so the commit always comes after the last move.
const endEditorTabCycle = (): Promise<void> =>
  queueTabCycleStep(() => {
    if (!tabCycle) {
      return;
    }
    tabCycle = null;
    if (activeEditorTabId != null) {
      touchTabMru(activeEditorTabId);
    }
  });

// Switch the active schema (database). Returns true on success.
const changeActiveSchema = async (schema: string): Promise<boolean> => {
  const connection = selectedConnection;
  if (!connection || schema === activeSchema) {
    return true;
  }
  // Save unsaved tabs best-effort (a failure does not stop the schema switch)
  await flushPendingSave();
  // Discard the AI chat conversation **before** the switch and wait until the abort arrives.
  // The schema in the system prompt changes so the conversation cannot carry over, and if the switch
  // came first, a round trip awaiting a response would run queries on the new schema's pool
  // with the old prompt
  await clearChatAndWait();
  try {
    await api.setActiveSchema(connection, schema);
    // If we moved to another connection during the switch, do not apply that schema display to the new connection
    if (selectedConnection !== connection) {
      return false;
    }
    activeSchema = schema;
    errorMessage = null;
    // Fetch the completion candidates of the target schema in the background (do not wait)
    void loadSchemaMap();
    return true;
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return false;
  } finally {
    // End the transition regardless of success (so that input does not stay blocked on failure)
    endChatTransition();
  }
};

/// Open a file. If a tab is already open, activate it; otherwise load the
/// content, create a new tab and activate it (called from FilesPane).
const selectFile = async (fileName: string) => {
  // Record this file open as a navigation generation (to later check whether we moved to another file / tab
  // during the await).
  const navGen = ++navigationGeneration;
  // Fix the connection to load from before the await. Even if the connection switches while loading,
  // the tab is always tied to "the connection the content was read from" (prevents running / saving on the wrong connection).
  const connection = selectedConnection;
  if (!connection) {
    return;
  }
  const existing = editorTabs.find(
    (t) => t.connection === connection && t.file === fileName,
  );
  if (existing) {
    // When reopening a conflicted tab, follow the warning text "reopen the file to discard them":
    // discard local edits and re-read the disk content (best-effort. On failure keep the current state).
    if (existing.conflicted) {
      let disk: string | undefined;
      try {
        disk = await api.readQueryFile(connection, fileName);
      } catch (e) {
        errorMessage = toErrorMessage(e);
      }
      // If the user moved to another connection during the load await, or to another file / tab within the same
      // connection, do not rewrite the tab with this result and do not steal focus (so a slow load does
      // not roll back newer navigation; same policy as the stale-read guard on the path that creates a not-yet-arrived tab).
      // A file move within the same connection is not caught by comparing the connection, so it is checked
      // with the navigation generation.
      if (selectedConnection !== connection || navigationGeneration !== navGen) {
        return;
      }
      if (disk !== undefined) {
        existing.content = disk;
        existing.diskContent = disk;
        existing.dirty = false;
        existing.conflicted = false;
        cancelPendingSaveFor(existing.id);
        conflictNotified.delete(existing.id);
        toast.info(`Reloaded "${fileName}" (discarded unsaved edits)`);
      }
    }
    await activateEditorTab(existing.id);
    // Opening a file in the editor = an opportunity to use the connection. Re-establish the tunnel / connection
    // (this also recovers when reopened after closing all editors and disconnecting).
    void ensureConnectionResources(connection);
    return;
  }
  // Save unsaved tabs best-effort (a failure does not stop the file open)
  await flushPendingSave();
  try {
    const content = await api.readQueryFile(connection, fileName);
    // If the connection switched during the load, discard this load result
    // (the user is no longer looking at that connection, so do not open it)
    if (selectedConnection !== connection) {
      return;
    }
    const tab: EditorTab = {
      id: nextEditorTabId++,
      connection,
      file: fileName,
      content,
      dirty: false,
      diskContent: content,
    };
    editorTabs = [...editorTabs, tab];
    // Opening a new tab is an explicit move. This path does not go through activateEditorTab, so abort
    // the cycle in progress here (otherwise the next Ctrl+Tab would continue from an old snapshot
    // that does not include this tab).
    cancelTabCycle();
    activeEditorTabId = tab.id;
    lastActiveTabByConnection.set(connection, tab.id);
    touchTabMru(tab.id);
    errorMessage = null;
    // Loading a file into the editor = an opportunity to use the connection. Open the tunnel / connection here
    // and import the schema list and completion map (they were not opened at connection selection).
    void ensureConnectionResources(connection);
  } catch (e) {
    errorMessage = toErrorMessage(e);
  }
};

/// Open the file specified by a deep link (`queryfolio://open/<path>`) / the CLI.
/// Switch to the target connection and then open the file. If the connection is not in the settings, show an error.
/// (receives the connection / fileName already verified by the backend to be under the storage area)
const openFileByTarget = async (connection: string, fileName: string) => {
  // A deep link to a running instance can, rarely, arrive before the initial load of the connection list
  // completes. So as not to discard it as "no such connection" while still empty, load first if not loaded.
  if (connections.length === 0) {
    await loadConnections();
  }
  if (!connections.some((c) => c.name === connection)) {
    errorMessage = `Connection '${connection}' is not defined in the config`;
    return;
  }
  // selectConnection is a no-op for the same connection (files are already loaded), and for a different
  // connection it switches and loads the file list. If overtaken, selectedConnection changes,
  // so the guard below does not open.
  await selectConnection(connection);
  if (selectedConnection !== connection) {
    return;
  }
  // If the file is not in the list, re-fetch the FILES pane. The CLI's
  // `queryfolio write <connection> <file-name>` creates files outside the running instance, so
  // if that connection is already selected (= selectConnection is a no-op), the file can be opened
  // in the editor yet never appears in the list.
  if (!files.includes(fileName)) {
    try {
      const latest = await api.listQueryFiles(connection);
      if (selectedConnection === connection) {
        setFileEntries(latest);
      }
    } catch {
      // The file can be opened even if refreshing the list fails (only a display issue).
      // It will be re-fetched the next time this connection is selected.
    }
    // If we moved to another connection during the fetch wait, touch neither that connection's list nor
    // the open target (selectFile opens on "the currently selected connection", so it would open
    // a same-named file on the switched-to connection).
    if (selectedConnection !== connection) {
      return;
    }
  } else {
    // Even for a file in the list, if the CLI's write rewrote its content, the modified time and size
    // have changed. Do not make opening wait for this (only a display issue)
    refreshFileEntries(connection);
  }
  // If the tab is already open, compare with the disk content before activating it.
  // The CLI's `queryfolio write` rewrites files outside this process, so waiting for the external change
  // polling (FILE_WATCH_INTERVAL_MS) would, even though you opened a query that was rewritten, **show the old SQL
  // for up to 2.5 seconds and let it be run as is**.
  // The check uses the same path as normal polling (local unsaved edits are treated as a conflict and
  // are not silently discarded).
  const opened = editorTabs.find(
    (t) => t.connection === connection && t.file === fileName,
  );
  if (opened) {
    await checkTabForExternalChange(opened.id);
    // If we moved to another connection while waiting for the comparison, do not touch the open target
    // (same guard as the list re-fetch above).
    if (selectedConnection !== connection) {
      return;
    }
  }
  await selectFile(fileName);
};

/// Close an editor tab. If unsaved, save before closing (best-effort).
/// When the active tab is closed, activate the right neighbor (or the left one if none).
const removeEditorTab = async (id: number, save: boolean) => {
  const tab = editorTabs.find((t) => t.id === id);
  if (!tab) {
    return;
  }
  // When closing a conflicted tab (external change vs local unsaved edits), do not silently lose either.
  // A normal dirty tab is saved before closing, but saving a conflicted tab would silently overwrite the external
  // change, and conversely discarding would silently lose the local edits. Which one to throw away should be left to
  // the user's explicit action (overwrite by saving / discard by reopening), so block the close and
  // prompt for resolution with a toast. save=false (closing because the file was deleted etc.) neither saves nor overwrites, so it is excluded.
  if (save && tab.conflicted) {
    toast.warning(
      `"${tab.file}" has unsaved edits conflicting with an external change`,
      {
        description:
          "Resolve it first with Overwrite or Discard in the editor toolbar, then close.",
      },
    );
    return;
  }
  // If an autosave is still scheduled for this tab, cancel it (so it does not run after closing)
  if (autoSavePendingTabId === id) {
    if (autoSaveTimer) {
      clearTimeout(autoSaveTimer);
      autoSaveTimer = null;
    }
    autoSavePendingTabId = null;
  }
  // A normal dirty tab is saved before closing (a conflicted tab was already blocked above, so it
  // does not reach here).
  if (save && tab.dirty) {
    // If saving fails, do not close (so unsaved content is not lost along with memory)
    if (!(await saveEditorTab(tab))) {
      return;
    }
    // If edited further during the save await, saveEditorTab leaves dirty set.
    // Abort the close so that edit is not lost (the autosave timer will commit it later)
    if (tab.dirty) {
      return;
    }
  }
  // The array may have changed during the save await, so re-obtain the position
  const index = editorTabs.findIndex((t) => t.id === id);
  if (index < 0) {
    return;
  }
  editorTabs = editorTabs.filter((t) => t.id !== id);
  tabMruOrder = forgetMru(tabMruOrder, id);
  // If a tab is closed during cycling (with Ctrl held), the order snapshot is left
  // with a vanished ID. Proceeding as is would select a nonexistent tab and waste one step,
  // so abort the cycle itself (the next Ctrl+Tab is rebuilt from the current tab layout)
  if (tabCycle) {
    cancelTabCycle();
  }
  if (lastActiveTabByConnection.get(tab.connection) === id) {
    lastActiveTabByConnection.delete(tab.connection);
  }
  if (activeEditorTabId === id) {
    // After filter, the right-hand neighbor has moved up into the original index position
    const neighbor = editorTabs[index] ?? editorTabs[index - 1] ?? null;
    activeEditorTabId = null;
    if (neighbor) {
      await activateEditorTab(neighbor.id);
    }
  }
  // When all of this connection's editor tabs are closed, close the SSH tunnel / pool
  maybeDisconnectIfIdle(tab.connection);
};

const closeEditorTab = (id: number) => {
  void removeEditorTab(id, true);
};

const createFile = async (fileName: string) => {
  const connection = selectedConnection;
  if (!connection) {
    return;
  }
  try {
    const normalized = await api.createQueryFile(connection, fileName);
    // If the connection switched to another during creation, do not pollute the new connection's list and do not open
    if (selectedConnection !== connection) {
      return;
    }
    setFileEntries(await api.listQueryFiles(connection));
    await selectFile(normalized);
  } catch (e) {
    errorMessage = toErrorMessage(e);
  }
};

const deleteFile = async (fileName: string) => {
  const connection = selectedConnection;
  if (!connection) {
    return;
  }
  try {
    await api.deleteQueryFile(connection, fileName);
    // Close the open tab of the deleted file (the file is gone, so do not save)
    const victims = editorTabs.filter(
      (t) => t.connection === connection && t.file === fileName,
    );
    for (const v of victims) {
      await removeEditorTab(v.id, false);
    }
    // Update the list unless the connection switched in the course of closing the tab
    if (selectedConnection === connection) {
      setFileEntries(await api.listQueryFiles(connection));
    }
  } catch (e) {
    errorMessage = toErrorMessage(e);
  }
};

// Rename a file. Returns the normalized new file name on success, null on failure.
// If a tab has the target file open, save before renaming and follow it after success.
const renameFile = async (
  oldName: string,
  newName: string,
): Promise<string | null> => {
  // Even if the connection switches across awaits, do the rename against the connection at the start
  // (prevents mix-ups from a connection switch during flushPendingSave)
  const connection = selectedConnection;
  if (!connection) {
    return null;
  }
  // If a tab has the target file open, commit its unsaved content first
  const opened = editorTabs.some(
    (t) => t.connection === connection && t.file === oldName,
  );
  if (opened && !(await flushPendingSave())) {
    return null;
  }
  try {
    const normalized = await api.renameQueryFile(connection, oldName, newName);
    // If the connection switched during the rename, do not overwrite with the old connection's list
    if (selectedConnection === connection) {
      setFileEntries(await api.listQueryFiles(connection));
    }
    // Make the open tab's file name follow
    for (const t of editorTabs) {
      if (t.connection === connection && t.file === oldName) {
        t.file = normalized;
      }
    }
    errorMessage = null;
    return normalized;
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return null;
  }
};

// Move a query file to another connection's folder (drag & drop from FILES to CONNECTIONS).
// Returns true on success; on failure sets errorMessage and returns false.
//
// The source (fromConnection) is received from the caller as **the connection at the time the drag started**.
// If we looked at selectedConnection, a connection switch during the drag would move a
// same-named file of another connection.
const moveFileToConnection = async (
  fileName: string,
  fromConnection: string,
  toConnection: string,
): Promise<boolean> => {
  if (fromConnection === toConnection) {
    return false;
  }
  // Save and close the tab that has the target file open **before moving**. The order is the key point:
  // - If we save after moving, the file is recreated at the source path.
  // - If we only save and leave it open, characters typed while waiting for the move I/O vanish in a state of
  //   "neither in the moved file nor in the UI". If we close it, that window does not exist.
  //
  // removeEditorTab(save=true) returns without closing the tab if saving fails or if it was edited further during
  // the save (it also cancels the autosave schedule inside). If it could not be fully closed, abort
  // the move itself. Reopening on the destination connection is left to the user —
  // swapping the tab's connection would stop matching the pool / SSH tunnel references (maybeDisconnectIfIdle).
  const openedTabs = editorTabs.filter(
    (t) => t.connection === fromConnection && t.file === fileName,
  );
  for (const tab of openedTabs) {
    await removeEditorTab(tab.id, true);
  }
  if (
    editorTabs.some(
      (t) => t.connection === fromConnection && t.file === fileName,
    )
  ) {
    return false;
  }
  try {
    await api.moveQueryFile(fromConnection, toConnection, fileName);
  } catch (e) {
    errorMessage = toErrorMessage(e);
    // There are paths where the move is rejected (a same-named file exists at the destination / an engine with a different extension /
    // they share a save folder). The file remains at the source, so reopen the closed
    // tab and return to the editing state (the content was saved before closing).
    // So as not to overwrite the original error even on failure, put errorMessage back last.
    const moveError = errorMessage;
    if (openedTabs.length > 0) {
      await openFileByTarget(fromConnection, fileName);
    }
    errorMessage = moveError;
    return false;
  }
  // Refresh the list currently shown whether it is the source or the destination
  // (it disappears from the source and appears at the destination). When a tab is closed, the neighboring tab can become
  // active and the selected connection can change to the destination, so looking only at the source
  // would leave the moved file out of the list.
  const shown = selectedConnection;
  if (shown === fromConnection || shown === toConnection) {
    setFileEntries(await api.listQueryFiles(shown));
  }
  errorMessage = null;
  return true;
};

// Copy the absolute path of a query file to the clipboard. Returns true on success.
// On failure, sets errorMessage and returns false.
const copyFilePath = async (fileName: string): Promise<boolean> => {
  const connection = selectedConnection;
  if (!connection) {
    return false;
  }
  try {
    const path = await api.queryFilePath(connection, fileName);
    await writeText(path);
    errorMessage = null;
    return true;
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return false;
  }
};

// Save the content of an editor tab. Returns true on success.
// On failure, sets errorMessage while keeping dirty.
//
// force=false (default): an implicit save (autosave, save before closing, etc.). With the backend's
//   write_query_file_if_unchanged, write only when the known base (diskContent) matches the current
//   disk content (optimistic locking). If they differ, do not write and leave it to external change handling
//   (auto-merge or conflict detection). Returns true if the merge goes through cleanly and the tab becomes saved, false
//   if a conflict remains. Putting the check and the write adjacent in a single backend call
//   eliminates the TOCTOU of a frontend round trip.
// force=true: the user's explicit overwrite (Overwrite button). The intent is to replace the disk content
//   with the local content whatever it is, so write without checking.
//   (The auto-merge write-back does not use force but the CAS with expectedBase=disk directly.)
const saveEditorTab = async (
  tab: EditorTab,
  opts: { force?: boolean } = {},
): Promise<boolean> => {
  // If edited further during the write, that stale save completion must not
  // clear the dirty of the new edit (prevents lost updates). Remember the saved content and clear dirty on completion
  // only if the content has not changed.
  const saved = tab.content;
  const expectedBase = tab.diskContent;
  try {
    if (!opts.force) {
      // CAS write in the backend. If it cannot write, there is an external change.
      let wrote: boolean;
      try {
        wrote = await api.writeQueryFileIfUnchanged(
          tab.connection,
          tab.file,
          saved,
          expectedBase,
        );
      } catch (e) {
        errorMessage = `Failed to save the file: ${toErrorMessage(e)}`;
        return false;
      }
      if (!wrote) {
        // The disk differs from the base = there is an external change. Do not silently overwrite; leave it to external change handling
        // (auto-merge / conflict detection). If the auto-merge goes through cleanly the tab becomes saved
        // (dirty=false), so return that actual result (so that close / reload / rename do not mistake
        // a successful merge for a "save failure" and abort).
        await checkTabForExternalChange(tab.id);
        const after = editorTabs.find((t) => t.id === tab.id);
        return !!after && !after.dirty && !after.conflicted;
      }
      // Wrote it -> state settled.
      tab.diskContent = saved;
      tab.conflicted = false;
      if (tab.content === saved) {
        tab.dirty = false;
      }
      refreshFileEntries(tab.connection);
      return true;
    }
    await api.writeQueryFile(tab.connection, tab.file, saved);
    // Remember the content we wrote as the known disk content. This keeps the external change
    // watcher from mistaking "our own save" for an external change.
    tab.diskContent = saved;
    // An explicit save / auto-merge save went through = the local content has been reflected on the disk.
    // The conflict state is resolved, so lift the autosave suppression.
    tab.conflicted = false;
    if (tab.content === saved) {
      tab.dirty = false;
    }
    refreshFileEntries(tab.connection);
    return true;
  } catch (e) {
    errorMessage = `Failed to save the file: ${toErrorMessage(e)}`;
    return false;
  }
};

// A save or external change alters the file's modified time and size, so if that connection's list is being shown,
// re-fetch it (the FILES pane shows time and size in descending order of modified time. CYBERNEURA-DEV-774).
// It is only a display issue, so ignore failures and do not make callers wait (separate from whether the save succeeded).
const refreshFileEntries = (connection: string) => {
  if (selectedConnection !== connection) {
    return;
  }
  // Version at the time the fetch started. If the list was rewritten before resolution (a later-started fetch,
  // file creation / deletion, etc.), do not overwrite with this stale result
  const version = ++fileEntriesVersion;
  void api
    .listQueryFiles(connection)
    .then((latest) => {
      // If we moved to another connection while waiting for the fetch, do not overwrite that list either
      if (selectedConnection !== connection || version !== fileEntriesVersion) {
        return;
      }
      // Most periodic fetches show no change, so do not rewrite if identical (avoids re-rendering)
      const same =
        latest.length === fileEntries.length &&
        latest.every(
          (e, i) =>
            e.file_name === fileEntries[i].file_name &&
            e.modified_ms === fileEntries[i].modified_ms &&
            e.size === fileEntries[i].size,
        );
      if (!same) {
        setFileEntries(latest);
      }
    })
    .catch(() => {});
};

// Save the active editor tab (for explicit saves from the Toolbar etc.)
const saveCurrentFile = async (): Promise<boolean> => {
  const tab = getActiveEditorTab();
  if (!tab) {
    return true;
  }
  return saveEditorTab(tab);
};

/// For the active conflicted tab, discard the local unsaved edits and re-read the disk content.
/// A means to explicitly invoke, from an always-reachable UI (the toolbar), the same effect as the warning text's
/// "reopen the file to discard" (an escape hatch for cases where the reopen path is unreachable, e.g. clicking the
/// selected file again becomes Rename).
const discardActiveFileConflict = async (): Promise<void> => {
  const tab = getActiveEditorTab();
  if (!tab || !tab.conflicted) {
    return;
  }
  const { id, connection, file } = tab;
  let disk: string;
  try {
    disk = await api.readQueryFile(connection, file);
  } catch (e) {
    errorMessage = toErrorMessage(e);
    return;
  }
  // If the tab was closed / moved to another connection / the file name changed during the load await
  // do not apply (an old load must not break the current state).
  const cur = editorTabs.find((t) => t.id === id);
  if (
    !cur ||
    selectedConnection !== connection ||
    cur.connection !== connection ||
    cur.file !== file
  ) {
    return;
  }
  cur.content = disk;
  cur.diskContent = disk;
  cur.dirty = false;
  cur.conflicted = false;
  cancelPendingSaveFor(id);
  conflictNotified.delete(id);
  toast.info(`Reloaded "${file}" (discarded unsaved edits)`);
};

/// For the active conflicted tab, overwrite the disk with the local edits (the user's
/// explicit intent to overwrite). It writes with force, bypassing the CAS, so it intentionally replaces the external change.
const overwriteActiveFileConflict = async (): Promise<boolean> => {
  const tab = getActiveEditorTab();
  if (!tab) {
    return true;
  }
  return saveEditorTab(tab, { force: true });
};

/// Schedule (re-arm) the debounced autosave for the given tab. Any existing schedule is cancelled
/// (matching the existing spec that autosave targets only one tab at a time).
const scheduleAutoSave = (tabId: number) => {
  autoSavePendingTabId = tabId;
  if (autoSaveTimer) {
    clearTimeout(autoSaveTimer);
  }
  autoSaveTimer = setTimeout(() => {
    autoSaveTimer = null;
    const id = autoSavePendingTabId;
    autoSavePendingTabId = null;
    const target = editorTabs.find((t) => t.id === id);
    // Do not implicitly save a conflicted tab (the CAS in saveEditorTab protects this too, but to avoid a useless read
    // reject it here as well). saveEditorTab has no force, so if an external write came in during
    // the debounce, the CAS before writing detects it and, without overwriting, leaves it to conflict detection.
    if (target && target.dirty && !target.conflicted) {
      void saveEditorTab(target);
    }
  }, AUTO_SAVE_DELAY_MS);
};

/// Change notification from the editor. Updates the active tab's content and schedules
/// a debounced autosave. The schedule targets the tab being edited.
const updateEditorContent = (content: string) => {
  const tab = getActiveEditorTab();
  if (!tab || content === tab.content) {
    return;
  }
  tab.content = content;
  tab.dirty = true;
  // Do not schedule an implicit autosave during a conflict (so external changes are not silently overwritten).
  // The edit is reflected, but resolution is left to the user's explicit action (toolbar Overwrite / Discard).
  // If the edit makes the content match the disk, the next watcher tick resolves the conflict automatically.
  if (!tab.conflicted) {
    scheduleAutoSave(tab.id);
  }
};

/// Write the `-- 📝` result log back to a tab that **became inactive** after execution
/// (CYBERNEURA-DEV-858). Writing back to the active tab is done by SqlEditor.writeRunLog
/// (as a CodeMirror change to keep the cursor while editing).
///
/// Rewrites the tab body (tab.content) directly without going through the editor. The decision uses the same
/// planRunLogWrite as the editor path (range matching, re-fetching the label), and in addition:
/// - The tab was closed / moved to another connection -> do not write (stale)
/// - The executed connection's active schema changed since execution started -> do not write
///   (while another connection is open, activeSchema belongs to that other connection, so ask the backend)
/// - A tab in conflict with an external change -> do not write (conflicted). Resolution is left to the user's explicit action
///
/// An unedited CRLF file keeps tab.content as CRLF, but CodeMirror builds the target at positions
/// normalized to LF, so align to LF before matching.
///
/// If the tab becomes active again while waiting for the schema query, return "active"
/// (the caller rewrites via the editor path; the displayed body is not replaced wholesale).
///
/// Save right after writing. Autosave can hold a schedule for only one tab, so scheduling here
/// would steal the schedule of another tab being edited.
const writeRunLogToInactiveTab = async (
  tabId: number,
  connection: string,
  expectedSchema: string | null,
  target: RunTarget,
  buildBlock: (label: string) => string,
): Promise<RunLogOutcome | "active"> => {
  const findTab = () =>
    editorTabs.find((t) => t.id === tabId && t.connection === connection);
  if (!findTab()) {
    return "stale";
  }
  let schema: string | null = activeSchema;
  if (selectedConnection !== connection) {
    // Same resolution as applyConnectionContext (the config default if there is no override)
    const defaultSchema =
      connections.find((c) => c.name === connection)?.schema ?? null;
    try {
      schema = (await api.getActiveSchema(connection)) ?? defaultSchema;
    } catch {
      return "stale";
    }
    // If we returned to that connection while waiting, the frontend's value is the latest
    if (selectedConnection === connection) {
      schema = activeSchema;
    }
  }
  const tab = findTab();
  if (!tab) {
    return "stale";
  }
  if (activeEditorTabId === tabId) {
    return "active";
  }
  if (schema !== expectedSchema) {
    return "stale";
  }
  if (tab.conflicted) {
    return "conflicted";
  }
  // As with CodeMirror, normalize line breaks to LF before matching (target positions are LF-based).
  // Even when written via the editor path, doc.toString() returns LF, so the result is the same
  const doc = tab.content.replace(/\r\n?/g, "\n");
  const write = planRunLogWrite(doc, target, buildBlock);
  if (typeof write === "string") {
    return write;
  }
  tab.content = doc.slice(0, write.from) + write.insert + doc.slice(write.to);
  tab.dirty = true;
  // If a stale schedule remains for this tab, remove it (we save the current content below, so it would be redundant)
  cancelPendingSaveFor(tabId);
  // CAS save. Even on failure it stays in the tab as dirty, and errorMessage reports it
  void saveEditorTab(tab);
  return "written";
};

/// Insert a SQL snippet from the history panel / schema browser.
/// Append to the end of the open file (append rather than replace, so as not to
/// overwrite existing edits). Does not execute. Returns true if inserted.
/// Reflection into the editor is done by the $effect on the SqlEditor side.
const insertSqlSnippet = (sql: string): boolean => {
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return false;
  }
  const tab = getActiveEditorTab();
  if (!tab) {
    toast.warning("Select or create a query file first");
    return false;
  }
  const trimmed = tab.content.replace(/\s+$/, "");
  updateEditorContent(trimmed ? `${trimmed}\n\n${sql}\n` : `${sql}\n`);
  return true;
};

/// Generate SQL with AI from a natural-language instruction and insert it into the editor
/// (not run automatically. The user runs it after reviewing the content).
/// Returns true on success (used to decide whether to close the input field).
const generateSql = async (instruction: string): Promise<boolean> => {
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return false;
  }
  if (!getActiveEditorTab()) {
    toast.warning("Select or create a query file first");
    return false;
  }
  if (!instruction.trim()) {
    toast.warning("Enter an instruction for the SQL to generate");
    return false;
  }
  if (aiGenerating) {
    return false;
  }
  aiGenerating = true;
  try {
    const sql = await api.aiGenerateSql(selectedConnection, instruction);
    if (!sql.trim()) {
      toast.warning("The AI returned an empty response");
      return false;
    }
    // If the connection / file selection is lost during generation, nothing is inserted
    // (insertSqlSnippet shows a warning), so notify only on success
    if (!insertSqlSnippet(sql)) {
      return false;
    }
    toast.success("Generated SQL inserted into the editor");
    return true;
  } catch (e) {
    toast.error("Failed to generate SQL", {
      description: toErrorMessage(e),
    });
    return false;
  } finally {
    aiGenerating = false;
  }
};

/// From the SQL and error message recorded in the tab, ask the AI for a fix suggestion and
/// write it to the tab (not run automatically. The user inserts it into the editor with Apply).
const fixSqlWithAi = async (tabId: number) => {
  const tab = resultTabs.find((t) => t.id === tabId);
  if (!tab || !tab.error || !tab.sql.trim()) {
    return;
  }
  // Prevent double execution (the button is disabled too, but guard defensively)
  if (tab.fixing) {
    return;
  }
  // If Re-run during the query produces a different execution result, keep the execution time
  // so we do not write a fix suggestion for the old error
  const requestedExecutedAt = tab.executedAt;
  tab.fixing = true;
  try {
    const fixed = await api.aiFixSql(tab.connection, tab.sql, tab.error);
    // If the tab was discarded during the query (settings reload etc.) or the result was
    // replaced by a re-run, discard the old fix suggestion
    const current = resultTabs.find((t) => t.id === tabId);
    if (!current || current.executedAt !== requestedExecutedAt) {
      return;
    }
    if (!fixed.trim()) {
      toast.warning("The AI returned an empty response");
      return;
    }
    tab.fixSuggestion = fixed;
  } catch (e) {
    toast.error("Failed to get a fix suggestion", {
      description: toErrorMessage(e),
    });
  } finally {
    tab.fixing = false;
  }
};

/// Insert the AI's fix suggestion into the editor and close the suggestion display (does not execute).
/// If it could not be inserted (no connection / file selected, connection switched), keep the suggestion.
const applyFixSuggestion = (tabId: number) => {
  const tab = resultTabs.find((t) => t.id === tabId);
  if (!tab?.fixSuggestion) {
    return;
  }
  // Result tabs persist across connections, so switching connections while a suggestion is displayed
  // would insert into a file of another connection (another dialect). Prevent the wrong insertion
  if (selectedConnection !== tab.connection) {
    toast.warning(
      `This suggestion is for '${tab.connection}'. Switch back to that connection to apply it.`,
    );
    return;
  }
  if (insertSqlSnippet(tab.fixSuggestion)) {
    toast.success("Fixed SQL inserted into the editor");
    tab.fixSuggestion = null;
  }
};

/// Discard the AI's fix suggestion and close the suggestion display.
const dismissFixSuggestion = (tabId: number) => {
  const tab = resultTabs.find((t) => t.id === tabId);
  if (tab) {
    tab.fixSuggestion = null;
  }
};

/// Connections applying cell edits (running run_statements). Included in isConnectionRunning
/// to suppress parallel execution on the same connection, as with query execution.
let applyingConnections = $state(new Set<string>());

/// Return whether any tab is running a query on the given connection.
/// The backend's cancel registry manages only the last execution per connection,
/// so parallel execution on the same connection is suppressed on the frontend side
/// (allowing it would mix up or miss cancel targets).
/// Cell-edit application (applyingConnections) is also treated as running.
const isConnectionRunning = (connection: string): boolean =>
  resultTabs.some((t) => t.running && t.connection === connection) ||
  applyingConnections.has(connection) ||
  // AI chat tool execution also uses this connection's pool. If it is cut during execution,
  // the in-flight connection breaks and the next tool round trip would reopen an
  // untracked tunnel
  chatRunningConnections.has(connection);

/// Decide the tab to write the execution result to.
/// If there is an active non-pinned tab, reuse it; otherwise create a new tab.
/// On reaching the limit, discard the oldest non-pinned tab.
/// Returns null when all tabs are pinned and no room can be made (already notified by toast).
const prepareTargetTab = (): ResultTab | null => {
  const current = resultTabs.find((t) => t.id === activeTabId);
  if (current && !current.pinned) {
    // Prevent double writes to the same tab (against rapid Cmd+Enter presses)
    // Tab management notifications use toast so they do not cover the results pane
    if (current.running) {
      toast.warning("A query is already running in this tab.");
      return null;
    }
    return current;
  }
  if (resultTabs.length >= MAX_RESULT_TABS) {
    const oldest = resultTabs
      .filter((t) => !t.pinned)
      .reduce<ResultTab | null>(
        (acc, t) => (acc === null || t.executedAt < acc.executedAt ? t : acc),
        null,
      );
    if (!oldest) {
      toast.warning(
        "All result tabs are pinned. Unpin or close a tab to run a new query.",
      );
      return null;
    }
    resultTabs = resultTabs.filter((t) => t.id !== oldest.id);
  }
  const tab: ResultTab = {
    id: nextTabId++,
    pinned: false,
    sql: "",
    connection: "",
    schema: null,
    executedAt: Date.now(),
    result: null,
    error: null,
    cancelled: false,
    running: false,
    fixing: false,
    fixSuggestion: null,
  };
  resultTabs = [...resultTabs, tab];
  // Return a reference via the $state proxy, not the raw object
  // (rewriting a raw reference is not reflected reactively)
  return resultTabs[resultTabs.length - 1];
};

/// Run a query with the connection / SQL recorded in the tab and write the result to the tab.
/// On success returns that result (null on error, cancel, or tab discard).
/// Used by the caller to post-process the result (writing back the 📝 marker log etc.)
const executeTab = async (tab: ResultTab): Promise<QueryResult | null> => {
  tab.running = true;
  // Clear before running so a failure does not lead to mistaking or wrongly exporting the previous result
  tab.result = null;
  tab.error = null;
  tab.cancelled = false;
  // A fix suggestion for the previous error becomes stale on re-run, so discard it
  tab.fixSuggestion = null;
  tab.executedAt = Date.now();
  activeTabId = tab.id;
  let result: QueryResult | null = null;
  let error: string | null = null;
  let cancelled = false;
  try {
    result = await api.runQuery(
      tab.connection,
      tab.sql,
      undefined,
      effectiveWritable(tab.connection),
    );
  } catch (e) {
    const message = toErrorMessage(e);
    // An abort by cancel is not an error; show it as "Query cancelled"
    if (message === api.CANCELLED_ERROR_MESSAGE) {
      cancelled = true;
    } else {
      error = message;
    }
  }
  tab.running = false;
  // If the tab was discarded during execution (settings reload etc.),
  // discard the result instead of writing to a nonexistent tab (a detached object)
  if (!resultTabs.some((t) => t.id === tab.id)) {
    return null;
  }
  tab.result = result;
  tab.error = error;
  tab.cancelled = cancelled;
  // If `\c` switched the active schema, make the display follow
  // (the switch itself is already done in the backend)
  if (result?.switched_schema) {
    // The check query ran on the post-switch database,
    // so align the schema recorded in the tab with it too
    tab.schema = result.switched_schema;
    applySwitchedSchema(tab.connection, result.switched_schema);
  }
  // The tunnel cannot be cut while a query is running, so if only the query kept running after
  // all editor tabs were closed, re-evaluate the disconnect at this point, when it completes.
  maybeDisconnectIfIdle(tab.connection);
  return result;
};

/// Reflect a schema switch via `\c` into the frontend state.
/// The schema browser subscribes to changes of activeSchema so it follows automatically, and
/// the SQL completion schema map is re-fetched here.
const applySwitchedSchema = (connection: string, schema: string) => {
  // If we moved to another connection during execution, do not apply that schema display to the new connection
  if (selectedConnection !== connection || activeSchema === schema) {
    return;
  }
  activeSchema = schema;
  // Discard the AI chat conversation for the same reason as changeActiveSchema.
  // However, with `\c` the query execution itself is the switch, so we cannot abort before the
  // switch (we learn of it as a result after execution). In the short time until the abort request arrives,
  // an agent awaiting a response may read the post-switch schema
  clearChat();
  // Re-fetch the completion candidates of the switch target (do not wait)
  void loadSchemaMap();
  toast.success(`Switched to ${schema}`);
};

/// Request cancellation of the query running in a tab.
/// The backend does the actual abort, and when the running runQuery returns with
/// "Query cancelled", executeTab reflects it in the tab.
const cancelQuery = async (id: number) => {
  const tab = resultTabs.find((t) => t.id === id);
  if (!tab || !tab.running) {
    return;
  }
  try {
    const requested = await api.cancelQuery(tab.connection);
    // Notification for when there was no cancel target, e.g. execution had just completed
    if (!requested) {
      toast.info("No running query to cancel. It may have just finished.");
    }
  } catch (e) {
    toast.error("Failed to cancel the query", {
      description: toErrorMessage(e),
    });
  }
};

/// Ask for confirmation to run a dangerous statement and wait for the user's response (true = run).
/// If an unanswered earlier confirmation remains, reject it and then replace it
const requestDangerousConfirm = (reason: string): Promise<boolean> =>
  new Promise((resolve) => {
    if (dangerousConfirm) {
      dangerousConfirm.resolve(false);
    }
    dangerousConfirm = { reason, resolve };
  });

/// Response of the confirmation dialog (called from the modal). ok=true continues the execution
const resolveDangerousConfirm = (ok: boolean) => {
  if (!dangerousConfirm) {
    return;
  }
  const { resolve } = dangerousConfirm;
  dangerousConfirm = null;
  resolve(ok);
};

/// On a connection with allow_dangerous_statements enabled, show a confirmation before execution for a dangerous statement.
/// Returns true if it may run, false if cancelled.
/// Always returns true on a connection where it is disabled (the backend's run_query rejects it),
/// and also returns true when the danger-check call fails, leaving it to execution (since allow is the intent).
const confirmIfDangerous = async (
  connection: string,
  sql: string,
): Promise<boolean> => {
  const info = connections.find((c) => c.name === connection);
  // While read-only is in effect (config readonly, or the effective Writable for this connection
  // is OFF), the backend rejects write statements as Read-only.
  // A confirmation for destructive operations only makes sense for statements that can actually run, so here we
  // show no confirmation and leave it to execution (the backend returns a clear Read-only error). Since the effective
  // Writable is used, a re-run of a tab on another connection (always treated as read-only) also shows no needless confirmation.
  if (info?.readonly || !effectiveWritable(connection)) {
    return true;
  }
  if (!info?.allow_dangerous_statements) {
    return true;
  }
  let reason: string | null = null;
  try {
    reason = await api.checkDangerousStatement(connection, sql);
  } catch (e) {
    // Even if the check fails, leave it to execution (allow is the intent). But so as not to
    // swallow a real bug, leave the failure itself in the console
    console.warn("checkDangerousStatement failed; running without confirm", e);
    return true;
  }
  if (!reason) {
    return true;
  }
  return await requestDangerousConfirm(reason);
};

/// Row limit when re-fetching everything for Copy / Export.
/// The config's default_limit is ignored, but reading without limit would exhaust memory, so
/// a client-side safety net remains. When cut off, truncated is set.
const EXPORT_MAX_ROWS = 1_000_000;

/// For Copy / Export, re-run the active tab's SQL without default_limit.
///
/// Re-fetch only when the displayed result is narrowed:
/// - `applied_limit` is present (default_limit was added to a SELECT without LIMIT)
/// - `truncated` is set (the SQL's own LIMIT is large and was cut by the display row limit)
///
/// **Statements involving writes are not re-run.** The decision is left to the backend's
/// `can_rerun_for_output` (the same strict read-only check as the AI agent path).
/// `truncated` can be set even for write statements such as INSERT ... RETURNING, so
/// the presence of applied_limit alone cannot guarantee safety.
///
/// Returns null when re-fetching is unnecessary or impossible, and the caller uses the displayed result
/// (in that case the caller warns about the cutoff with a toast).
const fetchResultWithoutDefaultLimit = async (
  tab: ResultTab,
): Promise<QueryResult | null> => {
  const result = tab.result;
  if (!result || (result.applied_limit == null && !result.truncated)) {
    return null;
  }
  if (!(await api.canRerunForOutput(tab.connection, tab.sql))) {
    return null;
  }
  return await api.runQuery(
    tab.connection,
    tab.sql,
    EXPORT_MAX_ROWS,
    // Run with the same privileges as the original execution (judged read-only, so no writes happen)
    effectiveWritable(tab.connection),
    false,
  );
};

/// Run SQL from the editor. Returns its result on success
/// (null if not run, failed, or cancelled).
/// The caller looks at the return value to decide on writing back the 📝 marker log
const runQuery = async (sql: string): Promise<QueryResult | null> => {
  // Fix the target connection before the await. Even if the connection switches during later awaits (save,
  // dangerous-statement confirmation modal), the confirmed connection and the executing connection must not diverge
  // (prevents the accident of running old SQL on the new DB).
  const connection = selectedConnection;
  // Notifications from pre-execution guards use toast so they do not cover existing result tabs
  if (!connection) {
    toast.warning("Select a connection first");
    return null;
  }
  if (!sql.trim()) {
    toast.warning("There is no SQL statement to run");
    return null;
  }
  // Suppress parallel execution on the same connection (rejected even if running in another tab)
  if (isConnectionRunning(connection)) {
    toast.warning(
      "A query is already running on this connection. Cancel it or wait for it to finish.",
    );
    return null;
  }
  // Save unsaved tabs best-effort (a failure does not stop execution. The SQL is the in-memory value)
  await flushPendingSave();
  // Dangerous statements (UPDATE/DELETE without WHERE, DROP/TRUNCATE) are confirmed before execution even on
  // a connection that allows execution. If cancelled, do nothing
  if (!(await confirmIfDangerous(connection, sql))) {
    return null;
  }
  // If the connection switched during the confirmation modal, abort so as not to run on another DB
  if (selectedConnection !== connection) {
    return null;
  }
  errorMessage = null;
  const tab = prepareTargetTab();
  if (!tab) {
    return null;
  }
  tab.sql = sql;
  tab.connection = connection;
  tab.schema = activeSchema;
  return await executeTab(tab);
};

/// Run the statement at the cursor with an engine-specific EXPLAIN prefix.
/// Building the prefix and the target decision (SELECT / WITH only) are done by the backend's
/// build_explain_sql, and statements out of scope are declined with a toast.
const explainQuery = async (sql: string) => {
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return;
  }
  if (!sql.trim()) {
    toast.warning("There is no SQL statement to explain");
    return;
  }
  // Use a different name so it does not clash with the module-level explainSql action
  let explainStatement: string;
  try {
    explainStatement = await api.buildExplainSql(selectedConnection, sql);
  } catch (e) {
    // A statement out of scope (DML etc.) or unknown engine. It is a refusal before execution, so make it a warning
    toast.warning(toErrorMessage(e));
    return;
  }
  await runQuery(explainStatement);
};

/// Format the EXPLAIN result into text to hand to the AI (header + tab-separated rows).
/// Only the execution plan text is passed (it is EXPLAIN output, not result data)
const formatPlanText = (result: QueryResult): string => {
  const cellText = (value: unknown): string =>
    value === null || value === undefined
      ? "NULL"
      : typeof value === "object"
        ? JSON.stringify(value)
        : String(value);
  const lines = result.rows.map((row) => row.map(cellText).join("\t"));
  return [result.columns.join("\t"), ...lines].join("\n");
};

/// Have the AI explain the EXPLAIN result tab and show the Markdown in a modal
const analyzeExplainTab = async (id: number) => {
  const tab = resultTabs.find((t) => t.id === id);
  if (!tab || !tab.result || aiAnalyzing) {
    return;
  }
  aiAnalyzing = true;
  try {
    const text = await api.aiExplainPlan(
      tab.connection,
      tab.sql,
      formatPlanText(tab.result),
    );
    if (!text.trim()) {
      toast.warning("The AI returned an empty response");
      return;
    }
    aiAnalysis = text;
  } catch (e) {
    toast.error("Failed to analyze the execution plan", {
      description: toErrorMessage(e),
    });
  } finally {
    aiAnalyzing = false;
  }
};

/// Close the AI explanation modal
const closeAiAnalysis = () => {
  aiAnalysis = null;
};

/// Have the AI explain the SQL statement at the cursor in plain terms and show the Markdown in a modal
/// (does not execute). Only the SQL and schema info are sent to the LLM;
/// query result data is not sent (see the backend's ai_explain_sql)
const explainSql = async (sql: string) => {
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return;
  }
  if (!sql.trim()) {
    toast.warning("There is no SQL statement to explain");
    return;
  }
  // Prevent double execution (the button is disabled too, but guard defensively)
  if (aiExplaining) {
    return;
  }
  aiExplaining = true;
  try {
    const text = await api.aiExplainSql(selectedConnection, sql);
    if (!text.trim()) {
      toast.warning("The AI returned an empty response");
      return;
    }
    aiExplanation = text;
  } catch (e) {
    toast.error("Failed to explain the SQL statement", {
      description: toErrorMessage(e),
    });
  } finally {
    aiExplaining = false;
  }
};

/// Close the modal of the AI explanation of the selected SQL
const closeAiExplanation = () => {
  aiExplanation = null;
};

/// Numbering of chat message IDs (a key for display. Not sent to the backend)
let nextChatMessageId = 1;

/// Chat generation. Advanced every time the conversation is discarded
/// (connection switch, schema switch, Clear, settings reload).
/// Whether the conversation was discarded while awaiting a response is checked with this generation, not just by comparing connection names:
/// a settings reload recreates a connection of the same name, so the name alone cannot reject "a response of an
/// old connection across a reload" (the backend returns the response still with the pre-reload connection config and schema).
/// A schema switch does not change the connection name either, so likewise.
let chatGeneration = $state(0);

/// Record the start of a chat round trip.
const retainChatRequest = (connection: string, requestId: string) => {
  const next = new Map(chatRunningConnections);
  next.set(connection, new Set(next.get(connection)).add(requestId));
  chatRunningConnections = next;
};

/// Record the end of a chat round trip.
/// Returns true only if it was the last one running on that connection
/// (the disconnect re-evaluation is done only when the last one completes).
const releaseChatRequest = (connection: string, requestId: string): boolean => {
  const next = new Map(chatRunningConnections);
  const ids = new Set(next.get(connection));
  ids.delete(requestId);
  if (ids.size > 0) {
    next.set(connection, ids);
  } else {
    next.delete(connection);
  }
  chatRunningConnections = next;
  return ids.size === 0;
};

/// Add the user's message to the AI chat (right pane) and get one round trip of response.
/// The entire conversation history is sent to the backend each time (the frontend holds the conversation state).
const sendChatMessage = async (text: string) => {
  const message = text.trim();
  if (!message) {
    return;
  }
  if (!selectedConnection) {
    toast.warning("Select a connection first");
    return;
  }
  // Prevent double sending (the send button is disabled too, but guard defensively).
  // Sending is allowed while discarded round trips remain (do not make the new conversation wait)
  if (chatSendingGen !== null && chatSendingGen === chatGeneration) {
    return;
  }
  // Not accepted during a connection / schema switch or settings reload transition
  if (chatTransitions > 0) {
    return;
  }
  const connection = selectedConnection;
  const generation = chatGeneration;
  chatMessages = [
    ...chatMessages,
    { id: nextChatMessageId++, role: "user", content: message },
  ];
  // Keep a failed response in the history but do not send it to the LLM (so the error text
  // is not mistaken for part of the conversation)
  const history: ChatTurn[] = chatMessages
    .filter((m) => !m.failed)
    .map((m) => ({ role: m.role, content: m.content }));
  // Pass engine-specific writing conventions to the model (CYBERNEURA-DEV-407). Same text as the help pane.
  // It is not shown on screen and goes only into the send payload, so it is not put in chatMessages.
  // Attach it to the **last message**. The backend truncates the history to the most recent N entries,
  // so placing it at the head would drop it once the conversation grows.
  const helpContext = buildEngineHelpContext(
    connections.find((c) => c.name === connection)?.engine,
  );
  if (helpContext && history.length > 0) {
    const last = history[history.length - 1];
    history[history.length - 1] = {
      ...last,
      content: `${helpContext}\n\n${last.content}`,
    };
  }
  chatSendingGen = generation;
  // Abort by specifying this ID (several round trips can run on the same connection)
  const requestId = `chat-${nextChatRequestSeq++}`;
  // While the agent is running tools, treat this connection as "running" so that the tunnel / pool
  // is not cut merely because there is no editor tab
  retainChatRequest(connection, requestId);
  try {
    const reply = await api.aiChat(connection, history, requestId);
    // If the conversation was discarded while awaiting the response, discard it (connection switch, Clear, settings
    // reload). A settings reload recreates a connection of the same name, so comparing connection
    // names alone cannot reject it
    if (selectedConnection !== connection || chatGeneration !== generation) {
      return;
    }
    // A failed round trip also returns (not as a reject) with an error. The queries that were run
    // are displayed just as on success
    chatMessages = [
      ...chatMessages,
      {
        id: nextChatMessageId++,
        role: "assistant",
        content: reply.error ?? reply.content,
        toolCalls: reply.tool_calls,
        failed: reply.error !== null,
      },
    ];
  } catch (e) {
    if (selectedConnection !== connection || chatGeneration !== generation) {
      return;
    }
    chatMessages = [
      ...chatMessages,
      {
        id: nextChatMessageId++,
        role: "assistant",
        content: toErrorMessage(e),
        failed: true,
      },
    ];
  } finally {
    // If a newer round trip has started, do not clear its waiting state
    if (chatSendingGen === generation) {
      chatSendingGen = null;
    }
    // As with query execution, re-evaluate the disconnect when the run finishes
    // (it cannot be cut while running, so the tunnel remains even after moving to another connection).
    // Do not evaluate if another round trip is still running on the same connection
    if (releaseChatRequest(connection, requestId)) {
      maybeDisconnectIfIdle(connection);
    }
  }
};

/// Request the running agents to abort, and wait until the request reaches the backend.
/// A failure of the abort request does not stop the caller's processing (the run will end eventually).
const requestChatCancel = async () => {
  await Promise.all(
    [...chatRunningConnections.entries()].map(([connection, ids]) =>
      api.cancelAiChat(connection, [...ids]).catch(() => {}),
    ),
  );
};

/// Stop the agent awaiting a response (Stop button). The conversation is kept, so
/// the abort appears in the conversation as a failure message.
const stopChat = () => {
  void requestChatCancel();
};

/// Discard the AI chat conversation
/// (Clear button / connection switch / schema switch / settings reload).
/// Advance the generation to prevent round trips awaiting a response from writing into the post-discard conversation.
/// The waiting display (chatSending) is also judged by the generation, so it disappears at the same time as the discard.
///
/// Merely discarding the response would let the backend agent keep running, and heavy queries or
/// the next tool round trip (which would read the post-switch schema) would be executed as is, so
/// an abort is also requested for the running connections.
const clearChat = () => {
  chatGeneration++;
  chatMessages = [];
  void requestChatCancel();
};

/// Discard the conversation and wait until the abort request reaches the backend.
/// Use **before** operations that rewrite backend state (schema switch / settings reload):
/// if fire-and-forget, the switch could precede the abort counter advancing, and an old agent
/// could keep running tools on the new schema / pool.
const clearChatAndWait = async () => {
  chatGeneration++;
  chatMessages = [];
  // Do not let a new round trip start while waiting for the abort to arrive (if one started,
  // it would use the post-switch pool without being included in the abort targets).
  // The caller calls endChatTransition() after the switch completes
  chatTransitions++;
  await requestChatCancel();
};

/// End of a transition started with clearChatAndWait (call exactly once regardless of success).
const endChatTransition = () => {
  chatTransitions = Math.max(0, chatTransitions - 1);
};

/// Re-run the SQL recorded in the tab on the same connection
const rerunTab = async (id: number) => {
  const tab = resultTabs.find((t) => t.id === id);
  if (!tab || tab.running || !tab.sql.trim()) {
    return;
  }
  // Suppress parallel execution on the same connection (rejected even if running in another tab)
  if (isConnectionRunning(tab.connection)) {
    toast.warning(
      "A query is already running on this connection. Cancel it or wait for it to finish.",
    );
    return;
  }
  // Confirm dangerous statements on re-run too (same guard as normal execution)
  if (!(await confirmIfDangerous(tab.connection, tab.sql))) {
    return;
  }
  errorMessage = null;
  // The connection's active schema may have changed since the time of execution, so
  // re-fetch it so the display does not diverge from the actual target (continue the run even on failure)
  try {
    tab.schema = (await api.getActiveSchema(tab.connection)) ?? tab.schema;
  } catch {
    // If the fetch fails, keep the recorded schema display
  }
  await executeTab(tab);
};

/// Apply the result grid's cell edits (a batch of UPDATEs) in one transaction.
/// Applied only when "the active connection and Writable ON" (also suppressed in the UI, but guarded here too to
/// prevent mistaken application on another connection's tab and wasted round trips). On success,
/// re-run to align the displayed values with the actual DB state, and return true.
const submitCellEdits = async (
  tabId: number,
  statements: string[],
): Promise<boolean> => {
  const tab = resultTabs.find((t) => t.id === tabId);
  if (!tab || statements.length === 0) {
    return false;
  }
  if (!effectiveWritable(tab.connection)) {
    toast.warning(
      "Turn on the Writable switch for this connection to apply edits.",
    );
    return false;
  }
  // The generated UPDATE is schema-unqualified and runs on "the connection's current active schema", so
  // if the schema was switched after editing, it could update a same-named table in another schema.
  // Do not apply if the tab's execution-time schema differs from the current active schema
  // (the UI's canEditActiveConnection suppresses only new edits, so guard on the Submit side too).
  if (tab.schema !== activeSchema) {
    toast.warning(
      "The active schema changed since these edits were made. Cancel them and re-run the query.",
    );
    return false;
  }
  // Do not apply if running on the same connection (a query or another apply)
  // (prevents mixing up cancel targets and repeated Submit presses. Same invariant as runQuery)
  if (isConnectionRunning(tab.connection)) {
    toast.warning(
      "A query is already running on this connection. Wait for it to finish.",
    );
    return false;
  }
  const connection = tab.connection;
  // While applying, register the connection as "running" to suppress parallel execution and double Submit
  applyingConnections = new Set(applyingConnections).add(connection);
  let affected: number;
  try {
    affected = await api.runStatements(
      connection,
      statements,
      effectiveWritable(connection),
    );
  } catch (e) {
    toast.error("Failed to apply the changes", {
      description: toErrorMessage(e),
    });
    return false;
  } finally {
    // rerunTab returns early by looking at isConnectionRunning, so remove it before re-fetching
    const next = new Set(applyingConnections);
    next.delete(connection);
    applyingConnections = next;
  }
  toast.success(`Applied ${affected} row change${affected === 1 ? "" : "s"}`);
  // Re-fetch to align the display with the DB's actual values (the apply itself has already succeeded)
  await rerunTab(tabId);
  return true;
};

const selectResultTab = (id: number) => {
  if (resultTabs.some((t) => t.id === id)) {
    activeTabId = id;
  }
};

const closeResultTab = (id: number) => {
  const index = resultTabs.findIndex((t) => t.id === id);
  if (index < 0) {
    return;
  }
  // Closing a running tab would make the in-flight query state vanish from the UI, so reject it
  // (the close button is disabled too, but guard defensively here as well)
  if (resultTabs[index].running) {
    toast.warning("Cannot close a tab while its query is running.");
    return;
  }
  resultTabs = resultTabs.filter((t) => t.id !== id);
  if (activeTabId === id) {
    // Activate the right neighbor of the closed tab (or the left one if none)
    const neighbor = resultTabs[index] ?? resultTabs[index - 1] ?? null;
    activeTabId = neighbor?.id ?? null;
  }
};

const toggleResultTabPin = (id: number) => {
  const tab = resultTabs.find((t) => t.id === id);
  if (tab) {
    tab.pinned = !tab.pinned;
  }
};

/// ===== Watcher for external file changes =====
/// When an open query file is changed outside the app (another editor, git, another process),
/// auto-reload it if unedited, try a 3-way merge if being edited, and warn on conflict.
/// Reading query files is local FS only (does not touch the DB / SSH tunnel), so
/// re-reading periodically in the background never opens a connection.

let fileWatchTimer: ReturnType<typeof setInterval> | null = null;
/// Prevent overlapping ticks (file reading is async).
let fileWatchTicking = false;
/// Remember per tab "the disk content for which a conflict was already warned", so the same state does not
/// repeat the warning on every tick. The entry is removed on resolution / tab close.
const conflictNotified = new Map<number, string>();

/// Cancel this tab's pending autosave schedule (to prevent re-saving over the disk
/// with the old editor content after taking in an external change).
const cancelPendingSaveFor = (tabId: number) => {
  if (autoSavePendingTabId === tabId) {
    if (autoSaveTimer) {
      clearTimeout(autoSaveTimer);
      autoSaveTimer = null;
    }
    autoSavePendingTabId = null;
  }
};

/// Check one editor tab for external changes and reflect them.
const checkTabForExternalChange = async (tabId: number) => {
  const before = editorTabs.find((t) => t.id === tabId);
  if (!before) {
    return;
  }
  const { connection, file } = before;
  // Remember the base before reading. If an autosave completes during the read and diskContent advances,
  // the old read result could be mistaken for an external change to the new base, so if
  // the base changed after reading, skip this tick (the next tick sees the latest state).
  const baseAtStart = before.diskContent;
  let disk: string;
  try {
    disk = await api.readQueryFile(connection, file);
  } catch {
    // If it cannot be read (deleted externally, etc.), do nothing. Leave it to the next opportunity.
    return;
  }
  // The tab may have been closed / the file name changed during the await, so re-obtain it.
  const tab = editorTabs.find((t) => t.id === tabId);
  if (!tab || tab.connection !== connection || tab.file !== file) {
    return;
  }
  // If a save slipped in during the read and the base (diskContent) changed, this read may be
  // stale, so make no decision.
  if (tab.diskContent !== baseAtStart) {
    return;
  }

  const base = tab.diskContent;
  if (disk === base) {
    // No external change. Clear any past conflict warning that remains.
    conflictNotified.delete(tabId);
    // If the external change that caused the conflict was reverted and the disk matches the base, the conflict is resolved.
    // Leaving conflicted set would keep a dirty tab treated as conflicted although there is no external change,
    // and local edits would be discarded on close / reload. Lower the flag, and if still dirty
    // re-arm the normal debounced autosave to persist the edits reliably
    // (cancelPendingSaveFor removed the schedule when the conflict was detected, so without re-arming
    //  they would not be autosaved until the next edit).
    if (tab.conflicted) {
      tab.conflicted = false;
      if (tab.dirty) {
        scheduleAutoSave(tabId);
      }
    }
    return;
  }
  // Reaching here = the disk differs from the last known content (there is an external change).
  if (tab.content === base) {
    // No unsaved local edits -> adopt the external content as is and reload.
    tab.content = disk;
    tab.diskContent = disk;
    tab.dirty = false;
    tab.conflicted = false;
    cancelPendingSaveFor(tabId);
    conflictNotified.delete(tabId);
    toast.info(`Reloaded "${file}" (changed on disk)`);
    refreshFileEntries(connection);
    return;
  }
  if (tab.content === disk) {
    // The local edits happen to match the disk (a save slipped in, etc.). Do not change the display; only update the base.
    tab.diskContent = disk;
    tab.dirty = false;
    tab.conflicted = false;
    conflictNotified.delete(tabId);
    // Even if the content is the same, the modified time has changed if it was rewritten
    refreshFileEntries(connection);
    return;
  }
  // base / local / remote are all different -> try a 3-way merge.
  // Remember the local content used for the merge (so that if the user types more during the write await,
  // that new edit is not silently overwritten by merged).
  const localSnapshot = tab.content;
  const { merged, conflict } = merge3(base, localSnapshot, disk);
  if (!conflict) {
    // The changes are to separate places, so they can be auto-merged. Write the result back to the disk.
    // But writing unconditionally with force would, if another process saved between this tick's read (disk) and the write
    // completing, silently overwrite that newer external save with merged based on the old disk.
    // So use a CAS that writes only when the disk snapshot the merge was based on still matches the disk
    // (expectedBase=disk). If they differ, do not write, and have the next tick
    // redo the merge / conflict decision against the latest content.
    let wrote = false;
    try {
      wrote = await api.writeQueryFileIfUnchanged(connection, file, merged, disk);
    } catch (e) {
      errorMessage = `Failed to save the file: ${toErrorMessage(e)}`;
    }
    // If the tab was closed / became another file / the base advanced during the write await,
    // do not reflect this result (the next tick sees the latest state).
    const t2 = editorTabs.find((t) => t.id === tabId);
    if (
      !t2 ||
      t2.connection !== connection ||
      t2.file !== file ||
      t2.diskContent !== base
    ) {
      return;
    }
    if (wrote) {
      // The merge result could be reflected to the disk. The disk is now merged.
      refreshFileEntries(connection);
      t2.diskContent = merged;
      t2.conflicted = false;
      conflictNotified.delete(tabId);
      if (t2.content === localSnapshot) {
        // No additional edits during the write -> settle the editor on the merge result too.
        t2.content = merged;
        t2.dirty = false;
        cancelPendingSaveFor(tabId);
        toast.info(`Merged external changes into "${file}"`);
      } else {
        // Typed more during the await -> do not discard that new edit; leave it dirty.
        // The base (diskContent) has advanced to merged, so the next tick / the scheduled autosave
        // saves / merges the new edit with merged as the base (the schedule is kept as is).
      }
    } else {
      // Another external write came in during the merge. Do not reflect this time (local edits stay as they are),
      // and clear the dedup key to have the next tick re-decide against the latest disk content.
      conflictNotified.delete(tabId);
    }
    return;
  }
  // Both sides changed the same place -> cannot auto-merge. Keep the unsaved edits and warn.
  // Cancel the pending autosave (leaving it could let a debounced save run on a conflicted tab).
  cancelPendingSaveFor(tabId);
  // During a conflict, suppress autosave (close, quitting the app, reloadConnections) to prevent local edits from
  // silently overwriting the external change. Resolution is left to the user's explicit action (toolbar
  // Overwrite to overwrite / Discard to discard). Implicit autosave itself is also doubly protected by
  // the CAS in saveEditorTab so it does not overwrite external changes.
  tab.conflicted = true;
  // Record that we have notified, so we do not warn on every tick for the same disk state.
  if (conflictNotified.get(tabId) !== disk) {
    conflictNotified.set(tabId, disk);
    toast.warning(`"${file}" was changed on disk, but you have unsaved edits`, {
      description:
        "Your edits are kept. Use Overwrite or Discard in the editor toolbar to resolve.",
    });
  }
};

let lastFileListRefreshAt = 0;

const fileWatchTick = async () => {
  if (fileWatchTicking) {
    return;
  }
  fileWatchTicking = true;
  try {
    // Iterate over a snapshot of ids at the start (safe even if the array changes during processing).
    const ids = editorTabs.map((t) => t.id);
    for (const id of ids) {
      await checkTabForExternalChange(id);
    }
    if (
      selectedConnection &&
      Date.now() - lastFileListRefreshAt >= FILE_LIST_REFRESH_INTERVAL_MS
    ) {
      lastFileListRefreshAt = Date.now();
      refreshFileEntries(selectedConnection);
    }
    // Clean up the notification records of closed tabs.
    const alive = new Set(editorTabs.map((t) => t.id));
    for (const id of [...conflictNotified.keys()]) {
      if (!alive.has(id)) {
        conflictNotified.delete(id);
      }
    }
  } finally {
    fileWatchTicking = false;
  }
};

/// Start polling for external file changes (called from +page's onMount).
const startFileWatcher = () => {
  if (fileWatchTimer) {
    return;
  }
  fileWatchTimer = setInterval(
    () => void fileWatchTick(),
    FILE_WATCH_INTERVAL_MS,
  );
};

/// Stop polling (called from +page's onDestroy).
const stopFileWatcher = () => {
  if (fileWatchTimer) {
    clearInterval(fileWatchTimer);
    fileWatchTimer = null;
  }
  conflictNotified.clear();
};

export default {
  get connections() {
    return connections;
  },
  get selectedConnection() {
    return selectedConnection;
  },
  /// State of the Writable switch. While false, write statements cannot be run
  get writable() {
    return writable;
  },
  /// Whether the selected connection is readonly: true in config (cannot be lifted by the switch).
  /// false if no connection is selected. Used for the toggle's lock display
  get selectedConnectionReadonly() {
    return (
      connections.find((c) => c.name === selectedConnection)?.readonly ?? false
    );
  },
  /// Toggle the Writable switch. For a config-readonly connection the backend rejects writes,
  /// but the state itself is kept as a session setting across connections
  toggleWritable() {
    writable = !writable;
  },
  /// The selected connection's engine capability declaration (null if none selected).
  /// Used to decide whether to show the TABLES pane and the query file extension
  get selectedCapabilities() {
    return (
      connections.find((c) => c.name === selectedConnection)?.capabilities ??
      null
    );
  },
  /// The selected connection's query file extension (without dot; "sql" if none selected)
  get selectedFileExtension() {
    return this.selectedCapabilities?.file_extension ?? "sql";
  },
  get files() {
    return files;
  },
  get fileEntries() {
    return fileEntries;
  },
  /// Open editor tabs (across all connections, multi-row display)
  get editorTabs() {
    return editorTabs;
  },
  get activeEditorTabId() {
    return activeEditorTabId;
  },
  /// File name of the active editor tab (null if none)
  get selectedFile() {
    return getActiveEditorTab()?.file ?? null;
  },
  /// Content of the active editor tab (empty string if none)
  get editorContent() {
    return getActiveEditorTab()?.content ?? "";
  },
  get resultTabs() {
    return resultTabs;
  },
  get activeTabId() {
    return activeTabId;
  },
  /// The active result tab (null if none)
  get activeResultTab() {
    return resultTabs.find((t) => t.id === activeTabId) ?? null;
  },
  get errorMessage() {
    return errorMessage;
  },
  /// True if a query is running in any tab (used to disable the Run button, etc.)
  get running() {
    return resultTabs.some((t) => t.running);
  },
  get loadingConnections() {
    return loadingConnections;
  },
  /// Whether the active editor tab has unsaved edits
  get dirty() {
    return getActiveEditorTab()?.dirty ?? false;
  },
  /// Whether the active editor tab is in conflict with an external change (used to show the toolbar's resolution UI)
  get activeFileConflicted() {
    return getActiveEditorTab()?.conflicted ?? false;
  },
  get schemas() {
    return schemas;
  },
  get activeSchema() {
    return activeSchema;
  },
  get aiInfo() {
    return aiInfo;
  },
  get aiError() {
    return aiError;
  },
  get aiGenerating() {
    return aiGenerating;
  },
  get aiAnalyzing() {
    return aiAnalyzing;
  },
  /// Markdown of the AI execution plan explanation (non-null only while the modal is shown)
  get aiAnalysis() {
    return aiAnalysis;
  },
  get aiExplaining() {
    return aiExplaining;
  },
  /// Markdown of the AI explanation of the selected SQL (non-null only while the modal is shown)
  get aiExplanation() {
    return aiExplanation;
  },
  /// Messages currently shown in the AI chat (right pane)
  get chatMessages() {
    return chatMessages;
  },
  /// Waiting for the AI chat's response (discarded round trips are not treated as waiting).
  /// Set to true during a switch transition too, to block input
  get chatSending() {
    return (
      chatTransitions > 0 ||
      (chatSendingGen !== null && chatSendingGen === chatGeneration)
    );
  },
  /// Map of table name -> column name list for SQL completion (null if not fetched)
  get schemaMap() {
    return schemaMap;
  },
  /// Reason shown in the confirmation dialog before running a dangerous statement (null when hidden)
  get dangerousConfirmReason() {
    return dangerousConfirm?.reason ?? null;
  },
  /// "Run" was chosen in the confirmation dialog
  confirmDangerous: () => resolveDangerousConfirm(true),
  /// "Cancel" was chosen in the confirmation dialog
  cancelDangerous: () => resolveDangerousConfirm(false),
  loadConnections,
  loadAiInfo,
  generateSql,
  loadSchemaMap,
  ensureConnectionResources,
  reloadConnections,
  selectConnection,
  changeActiveSchema,
  selectFile,
  openFileByTarget,
  activateEditorTab,
  cycleEditorTab,
  endEditorTabCycle,
  closeEditorTab,
  createFile,
  deleteFile,
  renameFile,
  moveFileToConnection,
  copyFilePath,
  saveCurrentFile,
  discardActiveFileConflict,
  overwriteActiveFileConflict,
  updateEditorContent,
  writeRunLogToInactiveTab,
  insertSqlSnippet,
  fixSqlWithAi,
  applyFixSuggestion,
  dismissFixSuggestion,
  isConnectionRunning,
  runQuery,
  fetchResultWithoutDefaultLimit,
  explainQuery,
  analyzeExplainTab,
  closeAiAnalysis,
  explainSql,
  closeAiExplanation,
  sendChatMessage,
  clearChat,
  stopChat,
  cancelQuery,
  rerunTab,
  submitCellEdits,
  selectResultTab,
  closeResultTab,
  toggleResultTabPin,
  startFileWatcher,
  stopFileWatcher,
};
