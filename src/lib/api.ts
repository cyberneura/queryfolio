import { invoke } from "@tauri-apps/api/core";

/// SSH tunnel info (secrets excluded. Corresponds to config::SshTunnelInfo in the backend)
export interface SshTunnelInfo {
  host: string;
  port: number;
  user: string;
  /// Host alias in ~/.ssh/config (in system ssh delegation mode). null in libssh2 mode
  ssh_config: string | null;
}

/// Engine capability declaration (corresponds to engines::EngineCapabilities in the backend).
/// The UI decides what to show by these flags, not by engine name.
export interface EngineCapabilities {
  /// Syntax highlighting language of the editor
  editor_language: "sql" | "redis" | "es";
  /// Extension of query files (without the dot)
  file_extension: string;
  /// Whether listing / switching schemas (databases) is supported
  supports_schemas: boolean;
  /// Whether the schema browser (TABLES pane) is supported
  supports_tables: boolean;
  supports_explain: boolean;
  supports_format: boolean;
  /// Whether cell editing in the result grid (UPDATE generation) is supported
  supports_editable_cells: boolean;
  /// Whether AI features (SQL generation / explanation) are supported
  supports_ai: boolean;
}

export interface ConnectionInfo {
  name: string;
  description: string | null;
  engine: string;
  has_ssh_tunnel: boolean;
  /// Host to connect to (null if not set)
  host: string | null;
  /// Port to connect to (null if not set)
  port: number | null;
  /// User to connect as (null if not set)
  user: string | null;
  schema: string | null;
  /// SSH tunnel info (secrets excluded). null if no tunnel is used
  ssh_tunnel: SshTunnelInfo | null;
  readonly: boolean;
  /// A connection that is allowed to run dangerous statements (UPDATE/DELETE without WHERE, DROP/TRUNCATE).
  /// Even when true, confirmation is requested before running
  allow_dangerous_statements: boolean;
  /// Display group name in the connection list (null if not in a group)
  group_name: string | null;
  /// Effective TLS mode of SQL engines (mysql / postgres).
  /// disable / prefer may downgrade to plaintext (prefer is sqlx's default).
  /// null for other engines
  sql_ssl_mode: string | null;
  /// Engine capability declaration (used to decide what the UI shows)
  capabilities: EngineCapabilities;
}

export interface QueryResult {
  columns: string[];
  rows: unknown[][];
  row_count: number;
  affected_rows: number | null;
  truncated: boolean;
  applied_limit: number | null;
  elapsed_ms: number;
  /// The switch target when the active schema was switched with `\c` (null otherwise)
  switched_schema: string | null;
}

/// One entry of the query execution history (corresponds to history::HistoryEntry in the backend)
export interface QueryHistoryEntry {
  /// Execution time (ISO 8601)
  time: string;
  sql: string;
  /// Active schema (database) at execution time
  schema: string | null;
  /// Number of rows fetched or affected (null on failure)
  row_count: number | null;
  elapsed_ms: number;
  success: boolean;
}

/// Table / view info (corresponds to schema_info::TableInfo in the backend)
export interface TableInfo {
  /// Table name (without schema qualification)
  name: string;
  /// Owning schema name (PostgreSQL only. null for MySQL / SQLite)
  schema: string | null;
  /// "table" or "view"
  kind: string;
  /// Qualified name that can be embedded in SQL. Use this value for the table argument of
  /// listColumns and for insertion into the editor
  qualified_name: string;
}

/// Column info (corresponds to schema_info::ColumnInfo in the backend)
export interface ColumnInfo {
  name: string;
  data_type: string;
  nullable: boolean;
}

/// AI settings info (corresponds to ai::AiInfo in the backend). api_key is not included
export interface AiInfo {
  configured: boolean;
  /// Model name in use (empty string if not set)
  model: string;
}

export interface ConfigInfo {
  config_path: string;
  config_exists: boolean;
  source: string;
  sqlfiles_dir: string;
}

export const getConnections = () =>
  invoke<ConnectionInfo[]>("get_connections");

export const resetConnections = () => invoke<void>("reset_connections");

/// writable is the state of the Writable switch (unspecified / false means read-only, the safe side).
/// For a config connection with readonly: true, writes are rejected regardless of writable.
/// If applyDefaultLimit is false, the default_limit from the config is not added automatically
/// (to get all rows for Copy / Export). When omitted, it is added as before.
export const runQuery = (
  connection: string,
  sql: string,
  maxRows?: number,
  writable?: boolean,
  applyDefaultLimit?: boolean,
) =>
  invoke<QueryResult>("run_query", {
    connection,
    sql,
    maxRows,
    writable,
    applyDefaultLimit,
  });

/// Request cancellation of the query running on a connection. false if nothing is running.
/// The cancelled run_query returns with an error of CANCELLED_ERROR_MESSAGE.
export const cancelQuery = (connection: string) =>
  invoke<boolean>("cancel_query", { connection });

/// The string returned by the backend's AppError::Cancelled (for detecting cancellation)
export const CANCELLED_ERROR_MESSAGE = "Query cancelled";

/// Returns the reason if it is a dangerous statement (UPDATE/DELETE without WHERE, DROP/TRUNCATE),
/// otherwise null. Used on a connection with allow_dangerous_statements enabled to decide whether
/// confirmation is needed before running (on a connection without it, runQuery rejects it).
export const checkDangerousStatement = (connection: string, sql: string) =>
  invoke<string | null>("check_dangerous_statement", { connection, sql });

/// Returns whether it is OK to run the SQL again to re-fetch all rows for Copy / Export.
/// Only statements judged read-only return true, so a statement with writes is never run twice.
export const canRerunForOutput = (connection: string, sql: string) =>
  invoke<boolean>("can_rerun_for_output", { connection, sql });

/// Returns the query execution history, newest first. search is a substring match on SQL (case-insensitive).
export const listQueryHistory = (
  connection: string,
  search?: string,
  limit?: number,
) => invoke<QueryHistoryEntry[]>("list_query_history", { connection, search, limit });

/// One row of the FILES pane (corresponds to query_files::QueryFileEntry in the backend)
export interface QueryFileEntry {
  /// File name (with extension)
  file_name: string;
  /// Last modified time (milliseconds since the UNIX epoch). null if unavailable
  modified_ms: number | null;
  /// File size (bytes)
  size: number;
}

/// List of a connection's query files (descending by modified time = most recently edited first)
export const listQueryFiles = (connection: string) =>
  invoke<QueryFileEntry[]>("list_query_files", { connection });

/// One hit of the query file search (corresponds to query_files::FileSearchHit in the backend)
export interface FileSearchHit {
  /// File name of the hit (with extension. .sql / .redis)
  file_name: string;
  /// Whether the file name matched the query
  name_match: boolean;
  /// The first line whose contents matched (for preview. null if only the name matched)
  content_preview: string | null;
}

/// Search a connection's query files by file name and contents (case-insensitive substring match).
export const searchQueryFiles = (connection: string, query: string) =>
  invoke<FileSearchHit[]>("search_query_files", { connection, query });

export const readQueryFile = (connection: string, fileName: string) =>
  invoke<string>("read_query_file", { connection, fileName });

/// Returns the absolute path of a query file (for "Copy full path" in FilesPane).
export const queryFilePath = (connection: string, fileName: string) =>
  invoke<string>("query_file_path", { connection, fileName });

export const writeQueryFile = (
  connection: string,
  fileName: string,
  content: string,
) => invoke<void>("write_query_file", { connection, fileName, content });

/// Write with optimistic locking. Writes only when expectedBase matches the current contents on disk.
/// Returns true if written, false if nothing was written because it was changed outside the app.
export const writeQueryFileIfUnchanged = (
  connection: string,
  fileName: string,
  content: string,
  expectedBase: string,
) =>
  invoke<boolean>("write_query_file_if_unchanged", {
    connection,
    fileName,
    content,
    expectedBase,
  });

export const createQueryFile = (connection: string, fileName: string) =>
  invoke<string>("create_query_file", { connection, fileName });

export const deleteQueryFile = (connection: string, fileName: string) =>
  invoke<void>("delete_query_file", { connection, fileName });

export const renameQueryFile = (
  connection: string,
  oldName: string,
  newName: string,
) => invoke<string>("rename_query_file", { connection, oldName, newName });

/// Move a query file to another connection's folder (drag & drop from FILES to
/// CONNECTIONS). Returns the file name after the move.
/// The backend rejects a move between engines whose query file extensions differ.
export const moveQueryFile = (
  fromConnection: string,
  toConnection: string,
  fileName: string,
) =>
  invoke<string>("move_query_file", { fromConnection, toConnection, fileName });

export const listSchemas = (connection: string) =>
  invoke<string[]>("list_schemas", { connection });

export const setActiveSchema = (connection: string, schema: string) =>
  invoke<void>("set_active_schema", { connection, schema });

export const getActiveSchema = (connection: string) =>
  invoke<string | null>("get_active_schema", { connection });

/// Discard the pool / SSH tunnel of the given connection (called when all editor tabs are closed).
/// The connection settings remain, and it is re-established automatically the next time it is needed
/// (opening a file / schema browser / query).
export const disconnect = (connection: string) =>
  invoke<void>("disconnect", { connection });

/// Returns the list of tables / views. refresh = true discards the cache and re-fetches.
export const listTables = (connection: string, refresh?: boolean) =>
  invoke<TableInfo[]>("list_tables", { connection, refresh });

/// Returns the column list of a table. Pass TableInfo.qualified_name as table.
export const listColumns = (connection: string, table: string) =>
  invoke<ColumnInfo[]>("list_columns", { connection, table });

/// Returns a map of table name -> column name list (for SQL completion).
export const getSchemaMap = (connection: string) =>
  invoke<Record<string, string[]>>("get_schema_map", { connection });

/// Returns the column names that make up the table's primary key (for cell editing in the result grid).
/// An empty array for a table with no primary key.
export const getPrimaryKeys = (connection: string, table: string) =>
  invoke<string[]>("get_primary_keys", { connection, table });

/// Apply the result-grid cell edits as a group of UPDATEs in a single transaction.
/// writable means the same as in runQuery (unspecified / false is read-only).
/// Returns the total number of affected rows.
export const runStatements = (
  connection: string,
  statements: string[],
  writable?: boolean,
) => invoke<number>("run_statements", { connection, statements, writable });

/// Returns the AI settings info. configured: false if there is no `ai:` section.
/// Rejects if the section exists but is invalid (unknown provider, etc.).
export const getAiInfo = () => invoke<AiInfo>("get_ai_info");

/// Generate SQL from a natural-language instruction and return it (does not run it).
export const aiGenerateSql = (connection: string, instruction: string) =>
  invoke<string>("ai_generate_sql", { connection, instruction });

/// Return a suggested fixed SQL from the failed SQL and the error message (does not run it).
export const aiFixSql = (
  connection: string,
  sql: string,
  errorMessage: string,
) => invoke<string>("ai_fix_sql", { connection, sql, errorMessage });

/// Build and return SQL with the engine-specific EXPLAIN prefix
/// (does not run it). Statements other than SELECT / WITH are rejected.
export const buildExplainSql = (connection: string, sql: string) =>
  invoke<string>("build_explain_sql", { connection, sql });

/// Have the AI explain the EXPLAIN execution plan and return Markdown text.
export const aiExplainPlan = (
  connection: string,
  sql: string,
  planText: string,
) => invoke<string>("ai_explain_plan", { connection, sql, planText });

/// Have the AI explain the SQL statement at the cursor in plain terms and return Markdown text
/// (does not run it).
export const aiExplainSql = (connection: string, sql: string) =>
  invoke<string>("ai_explain_sql", { connection, sql });

/// One turn of the AI chat (corresponds to ai::ChatTurn in the backend).
/// role is only "user" / "assistant" (the backend builds system).
export interface ChatTurn {
  role: "user" | "assistant";
  content: string;
}

/// Record of a tool call executed by the AI agent
/// (corresponds to ai::ChatToolCall in the backend).
export interface ChatToolCall {
  name: string;
  /// The SQL that was run (the raw arguments if parsing the arguments failed)
  argument: string;
  ok: boolean;
  /// Summary of the result (row count / first line of the error message)
  summary: string;
}

/// Response of one AI chat round trip (corresponds to ai::ChatReply in the backend).
/// A failed round trip also returns in this shape instead of rejecting (so that queries
/// executed partway are not hidden). If error is non-null it failed, and content is empty.
export interface ChatReply {
  content: string;
  tool_calls: ChatToolCall[];
  error: string | null;
}

/// Run one round trip of the AI chat (agent). The conversation history is sent as is every time,
/// and the backend builds the system prompt and runs the tool-execution loop.
/// The agent can run only read-only SQL.
/// requestId is the round-trip identifier numbered by the frontend (abort specifies this ID;
/// several round trips can run on the same connection, so the connection name alone cannot distinguish them).
export const aiChat = (
  connection: string,
  history: ChatTurn[],
  requestId: string,
) => invoke<ChatReply>("ai_chat", { connection, history, requestId });

/// Abort an AI chat agent round trip (stops the running query and does not let it
/// make the next model call or tool round trip). An ID can also be specified before ai_chat has
/// started running, in which case it is aborted at start.
/// Returns true if a running query was actually stopped.
export const cancelAiChat = (connection: string, requestIds: string[]) =>
  invoke<boolean>("cancel_ai_chat", { connection, requestIds });

/// The "open target" specified by a `queryfolio://open/<path>` deep link / CLI
/// (corresponds to router::OpenTarget in the backend).
export interface OpenTarget {
  /// Name of the connection the target file belongs to
  connection: string;
  /// File name to open (with extension. .sql / .redis)
  fileName: string;
}

/// Return value of frontend_ready (corresponds to LaunchResult in the backend).
export interface LaunchResult {
  /// Targets to open (those specified at launch + those that arrived while running)
  targets: OpenTarget[];
  /// Reason why resolving the launch-time target failed (shown in a toast)
  errors: string[];
}

/// Notifies that the frontend's listener registration is complete, and receives together the open
/// targets accumulated until then (launch-time deep link / CLI specification + those that arrived
/// while running) and the reason resolving the launch-time target failed. After the call, later
/// specifications arrive directly via the open-query-file event. Call it only once, right after registering the listener in onMount.
export const frontendReady = () => invoke<LaunchResult>("frontend_ready");

export const getConfigInfo = () => invoke<ConfigInfo>("get_config_info");

/// List of licenses of the dependency libraries bundled with the distribution (the body of THIRD-PARTY-NOTICES.txt).
export const getThirdPartyNotices = () => invoke<string>("third_party_notices");

/// Create a template if config.yml does not exist. If created, returns its path.
export const ensureConfigFile = () =>
  invoke<string | null>("ensure_config_file");

/// Read the contents of config.yml for the config editor (creating the template first if it does not exist).
export const readConfigFile = () => invoke<string>("read_config_file");

/// Save from the config editor. Returns the path of the written file.
export const writeConfigFile = (content: string) =>
  invoke<string>("write_config_file", { content });

/// Result of add_file_connection. added = false means "a connection for the same file already existed"
/// (config.yml was not rewritten).
export type FileConnection = { name: string; added: boolean };

/// Append a connection for the chosen SQLite / DuckDB file to the servers of config.yml
/// (existing comments and order are preserved). Reloading is done by the caller.
export const addFileConnection = (path: string) =>
  invoke<FileConnection>("add_file_connection", { path });

/// Run config_override_command and return the raw YAML obtained
/// (for the copy view. It can be edited at the destination but is not saved).
export const readOverrideConfigYaml = () =>
  invoke<string>("read_override_config_yaml");

/// Character encoding on export. Default is UTF-8.
/// CP932 / EUC-JP can be chosen for tools such as Excel that do not assume UTF-8.
export type ExportEncoding = "utf-8" | "cp932" | "euc-jp";

/// In the result table's Export, write text (CSV/TSV/JSON) to the path chosen in the
/// native save dialog. The path is one the user explicitly selected.
export const writeExportFile = (
  path: string,
  contents: string,
  encoding: ExportEncoding = "utf-8",
) => invoke<void>("write_export_file", { path, contents, encoding });
