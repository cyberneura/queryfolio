mod ai;
mod cli;
mod config;
mod db;
mod engines;
mod history;
mod meta_commands;
mod error;
mod folder_meta;
mod query_files;
mod router;
mod schema_info;
mod third_party_notices;
mod tunnel;

use std::path::PathBuf;
use std::sync::Arc;

use config::{AppConfig, ConfigInfo, ConnectionInfo, ServerConfig};
use db::{CancelRegistry, DbManager, DbPool, QueryResult, DEFAULT_MAX_ROWS};
use error::AppError;

/// State for handing "targets to open" that arrive while running over to the frontend.
/// The frontend listener is registered in onMount (after the webview is ready), so a
/// deep link / CLI that arrives earlier would miss the `open-query-file` event. Until
/// ready, targets are queued and handed over together by frontend_ready (a single Mutex
/// serializes the ready check and push/drain, preventing misses and double delivery).
#[derive(Default)]
struct LiveDelivery {
    /// Whether the frontend listener is ready (becomes true in frontend_ready).
    ready: bool,
    /// Targets to open that arrived before ready (drained and handed over by frontend_ready).
    pending: Vec<router::OpenTarget>,
    /// Resolution-failure messages that arrived before ready (drained and handed over by frontend_ready).
    /// Like successful targets, emitting before the listener is ready would be missed, so they are queued.
    pending_errors: Vec<String>,
}

/// Shared state of the whole app.
#[derive(Default)]
struct AppState {
    /// Session cache of the merged config (after config_override_command is applied).
    /// The fetch command runs an external process, so it is not run every time
    /// (cleared by reset_connections and fetched again).
    config: tokio::sync::Mutex<Option<Arc<AppConfig>>>,
    /// Cache of connection configs. Updated by get_connections.
    /// It contains secrets such as passwords, so it is not passed to the frontend.
    servers: tokio::sync::Mutex<Option<Vec<ServerConfig>>>,
    db: DbManager,
    /// Cancel registry for running queries (per connection name).
    query_cancels: CancelRegistry,
    /// Query execution history recording (holds a per-connection line count cache).
    history: history::HistoryManager,
    /// Cache of schema information (tables and columns).
    /// Shared by the schema browser and SQL completion (get_schema_map).
    schema_cache: schema_info::SchemaCache,
    /// Session cache of the AI config (cleared by reset_connections).
    /// The outer None means unresolved. It contains the api_key, so it is not passed to the
    /// frontend; get_ai_info returns only configured / model.
    ai: tokio::sync::Mutex<Option<Option<ai::AiConfig>>>,
    /// The route to open that was specified at startup via a `queryfolio://` deep link / CLI
    /// subcommand (None if none). The frontend takes it out exactly once after startup via
    /// frontend_ready (it disappears once taken). Routes opened while running are delivered
    /// to the frontend via live (event / queue), so they are not stored here.
    launch_route: std::sync::Mutex<Option<router::Route>>,
    /// Handing over targets to open that arrive while running (guards against misses before the listener is ready).
    live: std::sync::Mutex<LiveDelivery>,
    /// AI chat abort requests (per connection name).
    chat_cancels: ChatCancels,
}

/// Upper limit of IDs for which abort requests are remembered (they are kept to guard against
/// missed requests that never started, so old ones are dropped to keep it from growing without bound).
const CHAT_CANCEL_HISTORY_MAX: usize = 256;

/// Interval (ms) at which an agent's query execution checks for an abort request.
/// An abort that arrives before run_query_cancellable registers with the cancel registry
/// does not take effect via the registry, so it is watched for separately.
const CHAT_CANCEL_POLL_INTERVAL_MS: u64 = 200;

/// Holds AI chat (agent) abort requests **per request**.
///
/// Query cancellation (CancelRegistry) only stops "the one that is running"; it cannot stop
/// waiting for the model's response or the next tool round trip, so a mechanism to stop
/// the round trips themselves is needed. With a per-connection counter, (1) when two
/// requests run at once on the same connection, you cannot tell which to stop, and (2) an
/// abort that arrives right after starting is absorbed into the "baseline value at start",
/// so the request ID numbered by the frontend is used as is. If the ID is remembered, an
/// abort that arrived before the command started running is also caught by the entry check.
#[derive(Default)]
struct ChatCancels {
    inner: tokio::sync::Mutex<ChatCancelState>,
}

#[derive(Default)]
struct ChatCancelState {
    cancelled: std::collections::HashSet<String>,
    /// Insertion order (old ones are dropped first when the limit is exceeded)
    order: std::collections::VecDeque<String>,
}

impl ChatCancels {
    /// Requests an abort of the request (records it even if it has not started yet).
    async fn request(&self, request_id: &str) {
        let mut state = self.inner.lock().await;
        if state.cancelled.insert(request_id.to_string()) {
            state.order.push_back(request_id.to_string());
            while state.order.len() > CHAT_CANCEL_HISTORY_MAX {
                if let Some(old) = state.order.pop_front() {
                    state.cancelled.remove(&old);
                }
            }
        }
    }

    /// Whether an abort has been requested for this request.
    async fn is_cancelled(&self, request_id: &str) -> bool {
        self.inner.lock().await.cancelled.contains(request_id)
    }

    /// Discards the record of finished requests.
    async fn finish(&self, request_id: &str) {
        let mut state = self.inner.lock().await;
        if state.cancelled.remove(request_id) {
            state.order.retain(|id| id != request_id);
        }
    }
}

/// Key for the cancel registry in which AI chat tool executions are registered.
/// It includes the request ID so that it does not collide with user queries (keyed by
/// connection name) and so that, even when multiple round trips run on the same
/// connection, they do not overwrite each other's entries (CancelRegistry replaces a registration with the same key).
fn chat_cancel_key(connection: &str, request_id: &str) -> String {
    format!("{connection}\u{1}ai-chat\u{1}{request_id}")
}

impl AppState {
    /// Resolves the merged config (with a session cache).
    /// config_override_command is an external command such as 1Password that takes several
    /// seconds and may demand Touch ID, so it is not run on every query execution.
    /// Cleared by reset_connections.
    /// Builds the state with the merged config cached, when the pre-startup CLI write
    /// (`apply_cli_write_route`) has already resolved it.
    ///
    /// `config_override_command` spawns an external process such as 1Password, so it must not
    /// run twice in one CLI launch: besides being slow / showing Touch ID twice, if the fetched
    /// result changes, **the write and the read would use different snapshots**, and the file
    /// that was written could not be opened (sqlfiles_dir and the connection list would disagree).
    fn with_config(config: Option<Arc<AppConfig>>) -> Self {
        Self {
            config: tokio::sync::Mutex::new(config),
            ..Default::default()
        }
    }

    async fn resolve_config(&self) -> Result<Arc<AppConfig>, AppError> {
        let mut cached = self.config.lock().await;
        if let Some(config) = cached.as_ref() {
            return Ok(config.clone());
        }
        let config = Arc::new(AppConfig::load_merged().await?);
        *cached = Some(config.clone());
        Ok(config)
    }

    async fn resolve_default_limit(&self) -> Result<u64, AppError> {
        Ok(self.resolve_config().await?.default_limit())
    }

    /// Resolves the query file storage directory.
    /// config.yml is edited by hand, so if sqlfiles_dir changes while saving an open file,
    /// unsaved content would be written to the new directory. The merged config cache is fixed
    /// until reload (reset_connections), so the save destination of dirty files is also
    /// fixed to the directory at load time.
    async fn resolve_sqlfiles_dir(&self) -> Result<PathBuf, AppError> {
        self.resolve_config().await?.resolve_sqlfiles_dir()
    }

    /// Builds the table of query file storage folder name -> connection name (in config order).
    /// Used to resolve which connection a file belongs to from a path specified via
    /// `queryfolio://open/<path>` / CLI (router::resolve_open_target).
    async fn folder_connection_map(&self) -> Result<Vec<(String, String)>, AppError> {
        let servers = self.resolve_config().await?.resolve_servers()?;
        Ok(servers
            .iter()
            .map(|s| (s.sqlfiles_folder_name(), s.name.clone()))
            .collect())
    }

    /// Resolves a route (deep link / CLI) to the query file to open (connection + file name).
    /// It is an error unless it is a query file with the connection engine's extension in a
    /// connection folder under the storage directory.
    /// Known limitation: the validation here and the actual load (read_query_file) are
    /// separate calls, and if a config reload happens in between, the folder / extension
    /// resolution results can diverge (a narrow TOCTOU between the validated config and the
    /// config at load time). Reload is an explicit user action, and both resolutions stay
    /// inside the storage area derived from the config, so this is accepted.
    /// `cwd` is the base directory for resolving relative paths. When a running instance
    /// receives a deep link / CLI, pass the "launch origin directory" (the callback cwd of
    /// single-instance). When None, the current directory of this process is used.
    async fn resolve_route_target(
        &self,
        route: &router::Route,
        cwd: Option<PathBuf>,
    ) -> Result<router::OpenTarget, AppError> {
        match route {
            router::Route::OpenFile { path } => {
                // resolve_sqlfiles_dir returns a path made absolute against the config directory for
                // relative settings, so making it absolute here is essentially a no-op. It is kept
                // because if the base for path validation stayed relative, the strip_prefix comparison
                // would disagree with the real I/O, so the premise "base is always absolute" is
                // guaranteed right here (std::path::absolute is lexical absolutization that does not touch the FS).
                let sqlfiles_dir = self.resolve_sqlfiles_dir().await?;
                let sqlfiles_dir =
                    std::path::absolute(&sqlfiles_dir).unwrap_or(sqlfiles_dir);
                let folders = self.folder_connection_map().await?;
                let home = dirs::home_dir();
                // Only the relative resolution of the raw input path is against cwd (the launch origin for a running instance).
                let raw_cwd = cwd.or_else(|| std::env::current_dir().ok());
                let target = router::resolve_open_target(
                    &sqlfiles_dir,
                    &folders,
                    path,
                    home.as_deref(),
                    raw_cwd.as_deref(),
                )
                .map_err(|e| AppError::QueryFile(e.to_string()))?;
                let server = self.find_server(&target.connection).await?;
                // Verify that the extension matches the connection engine's. If it does not, "the path
                // validated by router / verify_within_dir" and "the path query_files actually opens after
                // re-attaching the extension" diverge, and the symlink defense no longer applies to the
                // real I/O target (e.g. passing foo.redis to a SQL connection is validated as foo.redis,
                // but the real I/O is foo.redis.sql).
                let ext = engines::capabilities_for_name(&server.engine).file_extension;
                if !target
                    .file_name
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{ext}"))
                {
                    return Err(AppError::QueryFile(format!(
                        "The file extension does not match the connection's engine \
                         (expected .{ext}): {}",
                        target.file_name
                    )));
                }
                // Defense in depth: even after passing the lexical validation (router), the connection
                // folder or file may be a symbolic link pointing to something outside the storage area.
                // Canonicalize the path that is actually opened (sqlfiles_dir/<folder>/<file>) and confirm
                // it still stays under the storage directory after resolving links (guaranteeing the
                // requirement "only queryfolio's data storage path is a target" at the entity level).
                let folder = server.sqlfiles_folder_name();
                let concrete = sqlfiles_dir.join(&folder).join(&target.file_name);
                query_files::verify_within_dir(&sqlfiles_dir, &concrete)?;
                Ok(target)
            }
            // Specified by connection name + file name (CLI `write`). **Nothing is written here** —
            // the launching process finished the write before starting Tauri
            // (see the documentation of router::Route::WriteFile). This only performs the
            // resolution of "open that file of that connection".
            router::Route::WriteFile {
                connection,
                file_name,
                ..
            } => {
                let server = self.find_server(connection).await?;
                let ext = engines::capabilities_for_name(&server.engine).file_extension;
                // Go through the same normalization as the launching side's write (extension completion /
                // name validation). Using the same function guarantees that "the validated name" and
                // "the name actually written" always match.
                let file_name = query_files::normalize_file_name(file_name, ext)?;
                let sqlfiles_dir = self.resolve_sqlfiles_dir().await?;
                let sqlfiles_dir =
                    std::path::absolute(&sqlfiles_dir).unwrap_or(sqlfiles_dir);
                let concrete = sqlfiles_dir
                    .join(server.sqlfiles_folder_name())
                    .join(&file_name);
                // If the write failed (the file does not exist), return an easy-to-understand message
                // instead of the generic I/O error from canonicalize.
                if !concrete.exists() {
                    return Err(AppError::QueryFile(format!(
                        "File not found: {}",
                        concrete.display()
                    )));
                }
                // The same defense in depth as OpenFile (whether a symlink points
                // outside the storage area).
                query_files::verify_within_dir(&sqlfiles_dir, &concrete)?;
                Ok(router::OpenTarget {
                    connection: server.name.clone(),
                    file_name,
                })
            }
        }
    }

    async fn find_server(&self, connection: &str) -> Result<ServerConfig, AppError> {
        let mut servers = self.servers.lock().await;
        if servers.is_none() {
            *servers = Some(self.resolve_config().await?.resolve_servers()?);
        }
        servers
            .as_ref()
            .unwrap()
            .iter()
            .find(|s| s.name == connection)
            .cloned()
            .ok_or_else(|| {
                AppError::Config(format!("Connection '{connection}' is not defined in the config"))
            })
    }

    /// Resolves the context required for query file operations:
    /// the storage directory, the connection folder name, and the per-engine file extension.
    /// The folder name is decided in the order folder_name -> <host>_<engine>_<schema>_<user>
    /// (the connection name is not used for the folder name).
    async fn resolve_files_ctx(
        &self,
        connection: &str,
    ) -> Result<(PathBuf, String, &'static str), AppError> {
        let server = self.find_server(connection).await?;
        Ok((
            self.resolve_sqlfiles_dir().await?,
            server.sqlfiles_folder_name(),
            engines::capabilities_for_name(&server.engine).file_extension,
        ))
    }

    /// Writes a meta file describing the connection into the connection's query file folder.
    /// Does nothing if the folder has not been created (does not create an empty folder just for the meta).
    /// Used when creating / saving query files and when refreshing on listing.
    async fn refresh_folder_meta(&self, server: &ServerConfig) -> Result<(), AppError> {
        let dir = query_files::connection_dir(
            &self.resolve_sqlfiles_dir().await?,
            &server.sqlfiles_folder_name(),
        )?;
        folder_meta::write_folder_meta(&dir, server)
    }

    /// Returns the active schema name that becomes the schema cache key
    /// (override > config default > empty string).
    async fn active_schema_key(&self, server: &ServerConfig) -> String {
        match self.db.schema_override(&server.name).await {
            Some(schema) => schema,
            None => server.schema.clone().unwrap_or_default(),
        }
    }

    /// Resolves the AI config (with cache). Ok(None) if not configured.
    /// Looks at the top-level `ai:` of the merged config (the `ai` on the YAML side fetched
    /// by config_override_command takes precedence over the local one as a result of the merge).
    /// Resolution errors (unknown provider etc.) are not cached and are returned every time
    /// (so they can be fixed by editing the config + reloading).
    async fn resolve_ai_config(&self) -> Result<Option<ai::AiConfig>, AppError> {
        let mut cached = self.ai.lock().await;
        if let Some(ai_config) = cached.as_ref() {
            return Ok(ai_config.clone());
        }
        let ai_config = ai::resolve_ai_config(self.resolve_config().await?.ai().as_ref())?;
        *cached = Some(ai_config.clone());
        Ok(ai_config)
    }

    /// Resolves the table -> column name list map (with cache).
    /// Shared by SQL completion (get_schema_map) and the AI SQL generation context.
    async fn resolve_schema_map(
        &self,
        server: &ServerConfig,
        schema_key: &str,
    ) -> Result<std::collections::BTreeMap<String, Vec<String>>, AppError> {
        if let Some(map) = self
            .schema_cache
            .get_schema_map(&server.name, schema_key)
            .await
        {
            return Ok(map);
        }
        let pool = self.db.get_pool(server).await?;
        let all = schema_info::fetch_all_columns(&pool).await?;
        let map = all
            .iter()
            .map(|(table, columns)| {
                (
                    table.clone(),
                    columns.iter().map(|c| c.name.clone()).collect(),
                )
            })
            .collect();
        self.schema_cache
            .put_all_columns(&server.name, schema_key, all)
            .await;
        Ok(map)
    }

    /// Resolves the context common to AI commands (SQL generation / error fixing):
    /// AI config, connection config, active schema name for the prompt, and the schema map.
    /// Returns an error with a guidance message when AI is not configured.
    async fn resolve_ai_context(
        &self,
        connection: &str,
    ) -> Result<
        (
            ai::AiConfig,
            ServerConfig,
            Option<String>,
            std::collections::BTreeMap<String, Vec<String>>,
        ),
        AppError,
    > {
        let ai_config = self.resolve_ai_config().await?.ok_or_else(|| {
            AppError::Ai(
                "AI is not configured. Add an 'ai:' section (provider / api_key) \
                 to config.yml or the YAML fetched by config_override_command"
                    .into(),
            )
        })?;
        let server = self.find_server(connection).await?;
        let schema_key = self.active_schema_key(&server).await;
        let schema_map = self.resolve_schema_map(&server, &schema_key).await?;
        // The schema of sqlite is a local DB file path, so it is not included in the prompt
        let is_sqlite = matches!(
            server.engine.to_ascii_lowercase().as_str(),
            "sqlite" | "sqlite3"
        );
        let active_schema =
            (!is_sqlite && !schema_key.trim().is_empty()).then_some(schema_key);
        Ok((ai_config, server, active_schema, schema_map))
    }
}

#[tauri::command]
async fn get_connections(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ConnectionInfo>, AppError> {
    let config = state.resolve_config().await?;
    let servers = config.resolve_servers()?;
    let infos = servers.iter().map(ConnectionInfo::from).collect();
    // Also resolve the AI config from the same merged config and cache it. A resolution error
    // does not break the connection list here; it is returned by the re-resolution in get_ai_info / ai_generate_sql.
    match ai::resolve_ai_config(config.ai().as_ref()) {
        Ok(ai_config) => *state.ai.lock().await = Some(ai_config),
        Err(_) => *state.ai.lock().await = None,
    }
    *state.servers.lock().await = Some(servers);
    Ok(infos)
}

/// Discards the connection config cache, pools and SSH tunnels.
/// Called on reload after the config has been changed.
#[tauri::command]
async fn reset_connections(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), AppError> {
    *state.config.lock().await = None;
    *state.servers.lock().await = None;
    *state.ai.lock().await = None;
    state.db.reset().await;
    state.schema_cache.clear().await;
    // Editing the config can change whether config_override_command exists, so
    // re-determine whether the menu item for the copy view (not saveable) is needed
    rebuild_menu(&app);
    Ok(())
}

#[tauri::command]
async fn run_query(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
    max_rows: Option<usize>,
    // State of the toolbar's Writable switch. Omitted / false means read-only
    // (the safe default). The config's readonly: true takes precedence over this.
    writable: Option<bool>,
    // Whether to auto-apply the config's default_limit. Applied when omitted (as before).
    // Copy / Export want all rows rather than the display for the result table, so they call with false.
    apply_default_limit: Option<bool>,
) -> Result<QueryResult, AppError> {
    let server = state.find_server(&connection).await?;
    // The config's readonly is the highest-priority hard lock. Next is the switch.
    let readonly_guard = if server.readonly {
        db::ReadonlyGuard::Config
    } else if writable.unwrap_or(false) {
        db::ReadonlyGuard::Off
    } else {
        db::ReadonlyGuard::Switch
    };
    // Note the active schema at execution time for history recording
    let schema = match state.db.schema_override(&connection).await {
        Some(schema) => Some(schema),
        None => server.schema.clone(),
    };
    // Row handling is separated between the display for the result table (apply_default_limit)
    // and the fetch of all rows for Copy / Export (apply_default_limit = false)
    let apply_default_limit = apply_default_limit.unwrap_or(true);
    let default_limit = if apply_default_limit {
        state.resolve_default_limit().await?
    } else {
        0
    };
    let auto_limit = match default_limit {
        0 => None,
        limit => Some(limit),
    };
    // For display execution, even when the SQL itself has a LIMIT and auto_limit cannot be
    // applied (LIMIT 10000 etc.), the result table shows only up to default_limit rows
    // (truncation is shown in the UI via truncated).
    //
    // Statements that get an auto_limit (a SELECT without LIMIT) are not trimmed here. If
    // trimmed, truncated would always be set when "500 rows came back with LIMIT 500", and a
    // truncation indicator would appear for every ordinary query.
    let max_rows = max_rows.unwrap_or(DEFAULT_MAX_ROWS);
    // For statements that auto_limit narrows on the SQL side, the client-side limit is not touched.
    // If the engine name is invalid, this falls back to false instead of failing here (the
    // async block below returns the same error at execution time and it is recorded in the history as a failure).
    let sql_gets_auto_limit = auto_limit.is_some()
        && db::parse_engine(&server.engine)
            .map(|engine| db::should_auto_limit(&sql, engine))
            .unwrap_or(false);
    let max_rows = if default_limit > 0 && !sql_gets_auto_limit {
        max_rows.min(default_limit as usize)
    } else {
        max_rows
    };
    let started = std::time::Instant::now();

    let result = async {
        // `\c <database>` and `USE <database>` are not SQL execution but changes to the
        // connection state, so they are handled here before acquiring the pool.
        // (Reporting meta command parse errors here as well records them in the history as failures)
        let engine = db::parse_engine(&server.engine)?;
        if let Some(meta_commands::MetaCommand::Connect(schema)) =
            meta_commands::translate(engine, &sql)?
        {
            return switch_active_schema(&state, &server, schema, started).await;
        }
        let pool: DbPool = state.db.get_pool(&server).await?;
        db::run_query_cancellable(
            &pool,
            &state.query_cancels,
            &connection,
            &sql,
            max_rows,
            auto_limit,
            readonly_guard,
            server.allow_dangerous_statements,
        )
        .await
    }
    .await;

    // Record the execution history regardless of success or failure.
    // Recording failures only go to the log so that they do not spoil the query result.
    // (The append is small synchronous I/O, so it is done as is in the async context.
    //  Only on rotation does a full read and rewrite run, but with the 10,000-line limit =
    //  at most a few MB, so this is accepted)
    let entry = history::HistoryEntry {
        time: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        sql,
        schema,
        row_count: result.as_ref().ok().map(|r| match r.affected_rows {
            Some(affected) => affected,
            None => r.row_count as u64,
        }),
        elapsed_ms: started.elapsed().as_millis() as u64,
        success: result.is_ok(),
    };
    match history::default_history_dir() {
        Ok(dir) => {
            if let Err(e) = state.history.append(&dir, &connection, &entry) {
                eprintln!("[history] failed to record the query history: {e}");
            }
        }
        Err(e) => eprintln!("[history] {e}"),
    }

    result
}

/// Actual processing of `\c <database>` / `USE <database>`. Switches the active schema and
/// runs a confirmation query on the switched connection and returns it as the result (an empty result would make success hard to see).
///
/// If the switch fails (a nonexistent database, etc.), return to the original schema.
/// If not restored, all subsequent queries would be left in a state where they cannot connect.
async fn switch_active_schema(
    state: &tauri::State<'_, AppState>,
    server: &ServerConfig,
    schema: String,
    started: std::time::Instant,
) -> Result<QueryResult, AppError> {
    let previous = state.db.schema_override(&server.name).await;
    state.db.set_schema_override(&server.name, schema.clone()).await;

    // Confirm that it can actually connect with the switched connection. If it fails here, roll back
    let confirm = async {
        let pool: DbPool = state.db.get_pool(server).await?;
        let sql = match db::parse_engine(&server.engine)? {
            db::Engine::MySql => "SELECT DATABASE() AS `database`",
            db::Engine::Postgres => "SELECT current_database() AS database",
            db::Engine::MsSql => "SELECT DB_NAME() AS [database]",
            // sqlite / duckdb / redis are rejected on the meta_commands side, so they never reach here
            db::Engine::Sqlite => {
                return Err(AppError::Config(
                    "\\c is not supported for SQLite".into(),
                ));
            }
            db::Engine::DuckDb => {
                return Err(AppError::Config(
                    "\\c is not supported for DuckDB".into(),
                ));
            }
            db::Engine::Redis | db::Engine::Elasticsearch | db::Engine::DynamoDb => {
                return Err(AppError::Config(
                    "\\c is not supported for this engine".into(),
                ));
            }
        };
        db::run_query_cancellable(
            &pool,
            &state.query_cancels,
            &server.name,
            sql,
            DEFAULT_MAX_ROWS,
            None,
            // This is a confirmation SELECT, so it passes even on a readonly connection
            db::ReadonlyGuard::Config,
            false,
        )
        .await
    }
    .await;

    match confirm {
        Ok(mut result) => {
            // If, while the confirmation query was running, the user changed to another database via
            // schema selection, reporting our switch destination to the frontend would make the
            // display disagree with the actual connection target, so it is not reported
            // (that switch has already discarded caches and updated the display by itself)
            let still_ours =
                state.db.schema_override(&server.name).await.as_deref() == Some(schema.as_str());
            if still_ours {
                // After switching, do not return the table list / columns of the old schema
                state.schema_cache.invalidate_connection(&server.name).await;
                result.switched_schema = Some(schema);
            }
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        }
        Err(e) => {
            // If, during the switch, the user changed to another database via schema selection,
            // do not roll back (respect that selection)
            state
                .db
                .rollback_schema_override(&server.name, &schema, previous)
                .await;
            // The frontend determines cancellation by an exact match on "Query cancelled" and
            // shows a dedicated display, so the reason is returned as is without wrapping
            if matches!(e, AppError::Cancelled) {
                return Err(e);
            }
            Err(AppError::Config(format!(
                "Failed to switch to {schema}: {e}"
            )))
        }
    }
}

/// Requests cancellation of the query running on a connection.
/// Does nothing and returns false if no query is running.
/// The cancelled execution is returned by run_query as AppError::Cancelled
/// ("Query cancelled").
#[tauri::command]
async fn cancel_query(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<bool, AppError> {
    state.query_cancels.cancel(&connection).await
}

/// Aborts the agent round trip of the AI chat.
/// request_ids are the IDs of running requests numbered by the frontend
/// (multiple round trips can run on the same connection, so they are passed together).
///
/// Besides stopping the running query (CancelRegistry), it remembers the IDs so that the
/// next model call and tool execution are not performed either. IDs of requests for which
/// ai_chat has not started running yet are also remembered, so aborting right after sending works.
/// The return value is "whether a running query was actually stopped".
#[tauri::command]
async fn cancel_ai_chat(
    state: tauri::State<'_, AppState>,
    connection: String,
    request_ids: Vec<String>,
) -> Result<bool, AppError> {
    // First **record all the IDs**. Cancelling a query asks the DB and can fail, but if we
    // stopped there, the remaining round trips would not be aborted and would keep running
    // against the backend after the switch
    for request_id in &request_ids {
        state.chat_cancels.request(request_id).await;
    }
    let mut cancelled_query = false;
    let mut first_error: Option<AppError> = None;
    for request_id in &request_ids {
        match state
            .query_cancels
            .cancel(&chat_cancel_key(&connection, request_id))
            .await
        {
            Ok(true) => cancelled_query = true,
            Ok(false) => {}
            // A failure of one does not stop the cancellation of the rest (the record is already made, so
            // the round trip itself stops at the next check). Only the first error is returned
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(cancelled_query),
    }
}

/// Returns the connection's query execution history newest first.
/// If search is given, filters by substring match on the SQL (case-insensitive).
#[tauri::command]
fn list_query_history(
    connection: String,
    search: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<history::HistoryEntry>, AppError> {
    history::list_history(
        &history::default_history_dir()?,
        &connection,
        search.as_deref(),
        limit.unwrap_or(history::DEFAULT_LIST_LIMIT),
    )
}

#[tauri::command]
async fn list_query_files(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<Vec<query_files::QueryFileEntry>, AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    let files = query_files::list_query_file_entries(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        ext,
    )?;
    // When a folder is opened, refresh the connection's description meta file (best effort:
    // a failure to write the meta does not break listing). Does nothing if the folder has not been created.
    let _ = state.refresh_folder_meta(&server).await;
    Ok(files)
}

/// Searches a connection's query files by file name and content (case-insensitive substring match).
#[tauri::command]
async fn search_query_files(
    state: tauri::State<'_, AppState>,
    connection: String,
    query: String,
) -> Result<Vec<query_files::FileSearchHit>, AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::search_query_files(&dir, &folder, &query, ext)
}

#[tauri::command]
async fn read_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
) -> Result<String, AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::read_query_file(&dir, &folder, &file_name, ext)
}

/// Returns the absolute path of a query file (for "Copy full path" in FilesPane).
#[tauri::command]
async fn query_file_path(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
) -> Result<String, AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::query_file_path(&dir, &folder, &file_name, ext)
}

#[tauri::command]
async fn write_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
    content: String,
) -> Result<(), AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    query_files::write_query_file(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        &file_name,
        &content,
        ext,
    )?;
    // At the timing when saving guarantees the folder exists, refresh the description meta file
    // (best effort: a failure to write the meta does not break saving).
    let _ = state.refresh_folder_meta(&server).await;
    Ok(())
}

/// Save with optimistic locking. Writes only when expected_base matches the current content on disk.
/// Returns true if written, false if not written because it was changed outside the app
/// (the frontend routes to merge / conflict handling). Used for implicit saves (autosave /
/// save before closing), as an atomic-leaning CAS so that external changes are not silently overwritten.
#[tauri::command]
async fn write_query_file_if_unchanged(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
    content: String,
    expected_base: String,
) -> Result<bool, AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    let wrote = query_files::write_query_file_if_unchanged(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        &file_name,
        &content,
        &expected_base,
        ext,
    )?;
    if wrote {
        let _ = state.refresh_folder_meta(&server).await;
    }
    Ok(wrote)
}

#[tauri::command]
async fn create_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
) -> Result<String, AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    let normalized = query_files::create_query_file(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        &file_name,
        ext,
    )?;
    // At the timing when a folder is newly created, write the connection's description meta file
    // (best effort: a failure to write the meta does not break creation).
    let _ = state.refresh_folder_meta(&server).await;
    Ok(normalized)
}

#[tauri::command]
async fn delete_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
) -> Result<(), AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::delete_query_file(&dir, &folder, &file_name, ext)
}

#[tauri::command]
async fn rename_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    old_name: String,
    new_name: String,
) -> Result<String, AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::rename_query_file(&dir, &folder, &old_name, &new_name, ext)
}

/// Moves a query file to another connection's folder (drag & drop from FILES to
/// CONNECTIONS). Returns the normalized file name after the move.
#[tauri::command]
async fn move_query_file(
    state: tauri::State<'_, AppState>,
    from_connection: String,
    to_connection: String,
    file_name: String,
) -> Result<String, AppError> {
    let from = state.find_server(&from_connection).await?;
    let to = state.find_server(&to_connection).await?;
    let from_ext = engines::capabilities_for_name(&from.engine).file_extension;
    let to_ext = engines::capabilities_for_name(&to.engine).file_extension;
    // The extension of query files differs per engine (.sql / .redis / .es).
    // A move that changes the extension only creates a file that does not appear in the
    // destination's list, so it is not accepted (changing the extension on our own would disagree with the content).
    if from_ext != to_ext {
        return Err(AppError::QueryFile(format!(
            "Cannot move a .{from_ext} file to \"{to_connection}\": it uses .{to_ext} files"
        )));
    }
    // Even different connections can have the same query file storage folder
    // (an explicit folder_name, or the same host/engine/schema/user).
    // The move would land in the same place, so we report that here instead of returning
    // success. If it returned success, the frontend would close the tab and show "Moved",
    // but the file would remain in the source's list.
    if from.sqlfiles_folder_name() == to.sqlfiles_folder_name() {
        return Err(AppError::QueryFile(format!(
            "\"{to_connection}\" shares the same query file folder as \"{from_connection}\": the file is already there"
        )));
    }
    let moved = query_files::move_query_file(
        &state.resolve_sqlfiles_dir().await?,
        &from.sqlfiles_folder_name(),
        &to.sqlfiles_folder_name(),
        &file_name,
        from_ext,
    )?;
    // The destination folder may have been newly created, so write the connection's
    // description meta file (best effort: a failure to write the meta does not break the move).
    let _ = state.refresh_folder_meta(&to).await;
    Ok(moved)
}

/// Returns the list of databases (schemas) on the connection's server.
#[tauri::command]
async fn list_schemas(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<Vec<String>, AppError> {
    let server = state.find_server(&connection).await?;
    let pool = state.db.get_pool(&server).await?;
    db::list_schemas(&pool, &server).await
}

/// Switches the connection's active schema (database).
/// The pool is rebuilt, and subsequent queries connect to the new database.
#[tauri::command]
async fn set_active_schema(
    state: tauri::State<'_, AppState>,
    connection: String,
    schema: String,
) -> Result<(), AppError> {
    if schema.trim().is_empty() {
        return Err(AppError::Config("The schema name is empty".into()));
    }
    // Confirm the connection name exists (prevents accumulating overrides for nonexistent connections)
    state.find_server(&connection).await?;
    state.db.set_schema_override(&connection, schema).await;
    // Discard the cache per connection so that old schema information is not returned after the switch
    state.schema_cache.invalidate_connection(&connection).await;
    Ok(())
}

/// Returns the connection's active schema (the config default if there is no override).
#[tauri::command]
async fn get_active_schema(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<Option<String>, AppError> {
    if let Some(schema) = state.db.schema_override(&connection).await {
        return Ok(Some(schema));
    }
    let server = state.find_server(&connection).await?;
    Ok(db::resolve_active_schema(
        &server.engine,
        server.schema.as_deref(),
    ))
}

/// Discards the pool and SSH tunnel of the specified connection.
/// Called from the frontend when all editor tabs of this connection have been closed.
/// The connection config and the active schema selection remain, so it is re-established
/// automatically the next time it is needed (opening a file / opening the schema browser / running a query).
#[tauri::command]
async fn disconnect(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<(), AppError> {
    state.db.disconnect(&connection).await;
    Ok(())
}

/// Returns the list of tables / views on the connection (with cache).
/// With refresh = true, discards the cache and fetches again (for the reload button).
#[tauri::command]
async fn list_tables(
    state: tauri::State<'_, AppState>,
    connection: String,
    refresh: Option<bool>,
) -> Result<Vec<schema_info::TableInfo>, AppError> {
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    if refresh.unwrap_or(false) {
        // The column cache may be stale too, so discard it wholesale per schema
        state
            .schema_cache
            .invalidate_schema(&connection, &schema_key)
            .await;
    } else if let Some(tables) = state.schema_cache.get_tables(&connection, &schema_key).await {
        return Ok(tables);
    }
    let pool = state.db.get_pool(&server).await?;
    let tables = schema_info::fetch_tables(&pool).await?;
    state
        .schema_cache
        .put_tables(&connection, &schema_key, &tables)
        .await;
    Ok(tables)
}

/// Returns the list of columns of a table (with cache. For lazy loading when expanding the tree).
/// For table, pass the qualified_name returned by list_tables.
#[tauri::command]
async fn list_columns(
    state: tauri::State<'_, AppState>,
    connection: String,
    table: String,
) -> Result<Vec<schema_info::ColumnInfo>, AppError> {
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    if let Some(columns) = state
        .schema_cache
        .get_columns(&connection, &schema_key, &table)
        .await
    {
        return Ok(columns);
    }
    let pool = state.db.get_pool(&server).await?;
    let columns = schema_info::fetch_columns(&pool, &table).await?;
    state
        .schema_cache
        .put_columns(&connection, &schema_key, &table, &columns)
        .await;
    Ok(columns)
}

/// Returns a map of table name -> column name list (to enhance SQL completion).
/// If the cache lacks columns for all tables, fetches them in bulk and caches them.
#[tauri::command]
async fn get_schema_map(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<std::collections::BTreeMap<String, Vec<String>>, AppError> {
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    state.resolve_schema_map(&server, &schema_key).await
}

/// Returns the column names that make up the table's primary key (for cell editing in the result grid).
/// Returns empty for a table with no primary key.
#[tauri::command]
async fn get_primary_keys(
    state: tauri::State<'_, AppState>,
    connection: String,
    table: String,
) -> Result<Vec<String>, AppError> {
    let server = state.find_server(&connection).await?;
    let pool = state.db.get_pool(&server).await?;
    schema_info::fetch_primary_keys(&pool, &table).await
}

/// Applies cell edits in the result grid as a group of UPDATEs in one transaction.
/// The resolution of writable is the same as run_query (config readonly takes precedence, then the switch).
/// Returns the total number of affected rows.
#[tauri::command]
async fn run_statements(
    state: tauri::State<'_, AppState>,
    connection: String,
    statements: Vec<String>,
    writable: Option<bool>,
) -> Result<u64, AppError> {
    let server = state.find_server(&connection).await?;
    let readonly_guard = if server.readonly {
        db::ReadonlyGuard::Config
    } else if writable.unwrap_or(false) {
        db::ReadonlyGuard::Off
    } else {
        db::ReadonlyGuard::Switch
    };
    let pool = state.db.get_pool(&server).await?;
    db::run_statements(
        &pool,
        &statements,
        readonly_guard,
        server.allow_dangerous_statements,
    )
    .await
}

/// Returns information on the AI config (configured / model). Does not include the api_key.
/// If there is no `ai:` section, it is configured: false rather than an error.
/// If the section exists but is invalid (unknown provider etc.), an error is returned.
#[tauri::command]
async fn get_ai_info(state: tauri::State<'_, AppState>) -> Result<ai::AiInfo, AppError> {
    Ok(match state.resolve_ai_config().await? {
        Some(config) => ai::AiInfo {
            configured: true,
            model: config.model().to_string(),
        },
        None => ai::AiInfo {
            configured: false,
            model: String::new(),
        },
    })
}

/// Generates SQL from a natural language instruction and returns it. It does not execute
/// it, and leaves inserting it into the editor to the frontend (the user confirms before running).
/// What is sent to the LLM is only the schema information (table and column names), the
/// engine dialect, the active schema name and the user's instruction. Query result data
/// and connection information (host, credentials) are not sent.
#[tauri::command]
async fn ai_generate_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    instruction: String,
) -> Result<String, AppError> {
    if instruction.trim().is_empty() {
        return Err(AppError::Ai("The instruction is empty".into()));
    }
    let (ai_config, server, active_schema, schema_map) =
        state.resolve_ai_context(&connection).await?;
    let system_prompt =
        ai::build_sql_system_prompt(&server.engine, active_schema.as_deref(), &schema_map);
    let response = ai::chat_complete(&ai_config, &system_prompt, &instruction).await?;
    Ok(ai::strip_sql_fences(&response))
}

/// Generates a suggested fix SQL from the failed SQL and the DB error message and returns it.
/// It does not execute it, and leaves reflecting it in the editor to the user's confirmation (Apply).
/// What is sent to the LLM is only the failed SQL, the error message, the schema information
/// (table and column names), the engine dialect and the active schema name.
/// Query result data and connection information (host, credentials) are not sent.
/// Note: the DB error message itself may contain values (e.g. the conflicting key value is
/// in the DETAIL of a unique constraint violation). Since it is information needed for the
/// fix, it is designed to be sent without processing, and the frontend button tooltip states what is sent.
#[tauri::command]
async fn ai_fix_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
    error_message: String,
) -> Result<String, AppError> {
    if sql.trim().is_empty() {
        return Err(AppError::Ai("The SQL statement is empty".into()));
    }
    if error_message.trim().is_empty() {
        return Err(AppError::Ai("The error message is empty".into()));
    }
    let (ai_config, server, active_schema, schema_map) =
        state.resolve_ai_context(&connection).await?;
    let system_prompt =
        ai::build_fix_sql_system_prompt(&server.engine, active_schema.as_deref(), &schema_map);
    let user_prompt = ai::build_fix_sql_user_prompt(&sql, &error_message);
    let response = ai::chat_complete(&ai_config, &system_prompt, &user_prompt).await?;
    Ok(ai::strip_sql_fences(&response))
}

/// Builds and returns SQL with the engine-specific EXPLAIN prefix.
/// It does not execute it (the frontend runs it through the normal run_query path).
/// Only SELECT / WITH are targeted (Postgres EXPLAIN ANALYZE actually executes the
/// target statement, so adding it to DML is rejected with an error).
#[tauri::command]
async fn build_explain_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<String, AppError> {
    let server = state.find_server(&connection).await?;
    db::build_explain_sql(&server.engine, &sql)
}

/// Returns the reason if the statement is dangerous (UPDATE/DELETE without WHERE, DROP/TRUNCATE).
/// It does not execute it. Used by the frontend, on connections where allow_dangerous_statements
/// is enabled, to decide whether to show a confirmation dialog before execution
/// (on connections where it is disabled run_query rejects it, so the frontend need not call it).
#[tauri::command]
async fn check_dangerous_statement(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<Option<String>, AppError> {
    let server = state.find_server(&connection).await?;
    db::dangerous_statement_reason(&server.engine, &sql)
}

/// Returns whether that SQL may be executed again to re-fetch all rows for Copy / Export.
///
/// The result table is truncated by default_limit, so showing all rows requires running
/// the same SQL again. But running a statement that writes twice would be an accident,
/// so only statements that pass the same strict read-only check as the AI agent path
/// (which also rejects multiple statements, EXPLAIN ANALYZE and CALL / PRAGMA) are allowed.
#[tauri::command]
async fn can_rerun_for_output(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<bool, AppError> {
    let server = state.find_server(&connection).await?;
    let engine = db::parse_engine(&server.engine)?;
    Ok(db::is_safe_to_rerun(&sql, engine))
}

/// Has the AI explain an EXPLAIN execution plan, and returns Markdown identifying
/// bottlenecks, suggesting indexes and proposing rewrites. What is sent to the LLM is only
/// the schema information (table and column names), the engine dialect, the active schema
/// name, the SQL and the plan text (the plan is planner output, not query result data, so
/// it is accepted). Connection information (host, credentials) is not sent.
#[tauri::command]
async fn ai_explain_plan(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
    plan_text: String,
) -> Result<String, AppError> {
    if sql.trim().is_empty() {
        return Err(AppError::Ai("The SQL statement is empty".into()));
    }
    if plan_text.trim().is_empty() {
        return Err(AppError::Ai("The execution plan is empty".into()));
    }
    let ai_config = state.resolve_ai_config().await?.ok_or_else(|| {
        AppError::Ai(
            "AI is not configured. Add an 'ai:' section (provider / api_key) \
             to config.yml or the YAML fetched by config_override_command"
                .into(),
        )
    })?;
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    let schema_map = state.resolve_schema_map(&server, &schema_key).await?;
    // The schema of sqlite is a local DB file path, so it is not included in the prompt
    let is_sqlite = matches!(
        server.engine.to_ascii_lowercase().as_str(),
        "sqlite" | "sqlite3"
    );
    let active_schema =
        (!is_sqlite && !schema_key.trim().is_empty()).then_some(schema_key.as_str());
    let system_prompt =
        ai::build_explain_system_prompt(&server.engine, active_schema, &schema_map);
    let user_message = ai::build_explain_user_message(&sql, &plan_text);
    let response = ai::chat_complete(&ai_config, &system_prompt, &user_message).await?;
    Ok(response.trim().to_string())
}

/// Has the AI explain the SQL statement at the cursor (selected) in plain terms, and returns Markdown.
/// It does not execute it. What is sent to the LLM is only the SQL, the schema information
/// (table and column names), the engine dialect and the active schema name. Query result
/// data and connection information (host, credentials) are not sent.
#[tauri::command]
async fn ai_explain_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<String, AppError> {
    if sql.trim().is_empty() {
        return Err(AppError::Ai("The SQL statement is empty".into()));
    }
    let (ai_config, server, active_schema, schema_map) =
        state.resolve_ai_context(&connection).await?;
    let system_prompt = ai::build_explain_sql_system_prompt(
        &server.engine,
        active_schema.as_deref(),
        &schema_map,
    );
    let user_message = ai::build_explain_sql_user_message(&sql);
    let response = ai::chat_complete(&ai_config, &system_prompt, &user_message).await?;
    Ok(response.trim().to_string())
}

/// Formats the result of the agent's run_sql into text for the LLM.
/// States the row count explicitly and passes the column names + each row as one line of
/// JSON (balancing token efficiency and ease of parsing).
fn format_chat_tool_result(result: &QueryResult) -> String {
    let mut text = format!(
        "{} row(s){}",
        result.row_count,
        if result.truncated {
            " (truncated)"
        } else {
            ""
        }
    );
    if let Some(affected) = result.affected_rows {
        text.push_str(&format!(", {affected} affected"));
    }
    text.push_str(&format!("\ncolumns: {}\n", result.columns.join(", ")));
    for row in &result.rows {
        let line = serde_json::to_string(row).unwrap_or_else(|_| "[unserializable row]".into());
        text.push_str(&line);
        text.push('\n');
    }
    ai::truncate_tool_result(&text)
}

/// Runs one round trip of the AI chat (agent).
/// The frontend passes the conversation history as is every time, and the backend is
/// responsible for building the system prompt and the tool execution loop.
///
/// The only tool is the read-only `run_sql`, and execution is **always read-only**
/// (writes are not permitted even if the toolbar's Writable switch is ON).
/// This is to structurally prevent accidents where the agent writes on its own judgment.
/// What is sent to the LLM is only the schema information, dialect, active schema name,
/// conversation history, and the results of read queries the agent itself executed.
/// Connection information (host, credentials) is not sent.
#[tauri::command]
async fn ai_chat(
    state: tauri::State<'_, AppState>,
    connection: String,
    history: Vec<ai::ChatTurn>,
    request_id: String,
) -> Result<ai::ChatReply, AppError> {
    // Tool calls that were executed are returned on failure as well (do not hide queries that
    // were executed partway; aborts and timeouts in particular tend to happen after tool execution)
    let mut tool_calls: Vec<ai::ChatToolCall> = Vec::new();
    let result =
        run_ai_chat(&state, &connection, &history, &request_id, &mut tool_calls).await;
    // Clean up the abort record at the end of the round trip (even if left it would be dropped
    // by the limit, but the same ID is never reused, so there is no point in keeping it)
    state.chat_cancels.finish(&request_id).await;
    Ok(match result {
        Ok(content) => ai::ChatReply {
            content,
            tool_calls,
            error: None,
        },
        Err(e) => ai::ChatReply {
            content: String::new(),
            tool_calls,
            error: Some(e.to_string()),
        },
    })
}

/// The body of ai_chat. Returns the assistant's final message, and pushes the tool calls
/// that were executed into the argument Vec (so the caller can pick them up even on failure).
async fn run_ai_chat(
    state: &AppState,
    connection: &str,
    history: &[ai::ChatTurn],
    request_id: &str,
    tool_calls: &mut Vec<ai::ChatToolCall>,
) -> Result<String, AppError> {
    let connection = connection.to_string();
    let mut messages = ai::chat_history_messages(history);
    if messages.is_empty() {
        return Err(AppError::Ai("The chat history is empty".into()));
    }
    // Abort is determined by request ID. With a per-connection counter, two requests running
    // on the same connection cannot be distinguished, and an abort that arrives right after
    // starting would be absorbed into the "baseline value at start". With an ID, an abort that
    // arrived before this command started running is also caught here
    let cancelled = || async { state.chat_cancels.is_cancelled(&request_id).await };
    if cancelled().await {
        return Err(AppError::Cancelled);
    }

    let (ai_config, server, active_schema, schema_map) =
        state.resolve_ai_context(&connection).await?;
    // If aborted during context resolution, cut off here
    // (the schema_map / prompt at this point may already be stale)
    if cancelled().await {
        return Err(AppError::Cancelled);
    }
    // Engines without AI support (redis / elasticsearch / dynamodb) have their input blocked
    // in the frontend too, but the command rejects them as well (the prompt assumes SQL)
    if !engines::capabilities_for_name(&server.engine).supports_ai {
        return Err(AppError::Ai(format!(
            "The AI features are not available for the '{}' engine",
            server.engine
        )));
    }
    let system_prompt =
        ai::build_chat_system_prompt(&server.engine, active_schema.as_deref(), &schema_map);
    messages.insert(
        0,
        serde_json::json!({ "role": "system", "content": system_prompt }),
    );

    // Agent execution is fixed to Agent regardless of the Writable switch or config.
    // In addition to the statement-level guard, DB-level read-only (read-only
    // transaction / PRAGMA query_only) is also enforced.
    let readonly_guard = db::ReadonlyGuard::Agent;
    // Use a key that collides neither with the user's query cancellation (keyed by connection
    // name) nor with another round trip on the same connection
    let cancel_key = chat_cancel_key(&connection, &request_id);

    let engine = db::parse_engine(&server.engine)?;
    // Cumulative count of tool executions. One response can list multiple tool_calls, so a
    // cumulative limit is imposed separately from the number of round trips (rounds)
    let mut executed_calls = 0usize;
    for _ in 0..ai::CHAT_MAX_TOOL_ROUNDS {
        // When the cumulative limit is reached, have it write the final answer without passing tools
        // (do not make exceeding the limit an error; have it answer with what it could read so far)
        let allow_tools = executed_calls < ai::CHAT_MAX_TOOL_CALLS;
        // If the conversation was discarded (connection / schema switch, Clear, Stop), cut off
        // without making the next model call or running tools
        if cancelled().await {
            return Err(AppError::Cancelled);
        }
        let message = ai::chat_step(&ai_config, &messages, allow_tools, &cancelled).await?;
        // If aborted while waiting for the model's response, that response is not adopted
        // (a round trip ending with a response that has no tools is the most common, so if this
        // is overlooked, an ordinary answer still comes back after pressing Stop)
        if cancelled().await {
            return Err(AppError::Cancelled);
        }
        let requested = ai::parse_tool_calls(&message);
        if requested.is_empty() {
            let content = ai::message_content(&message);
            if content.is_empty() {
                return Err(AppError::Ai(
                    "The AI returned an empty message".into(),
                ));
            }
            return Ok(content);
        }
        // An assistant message that contains tool calls is pushed to the history as is
        // (tool messages must correspond to the preceding tool_calls)
        messages.push(message);
        for (id, name, arguments) in requested {
            // One response can list multiple tool_calls, so also cut off by the cumulative limit.
            // A tool message is also returned for the cut-off ones (if the tool message corresponding
            // to a tool_call is missing, the API returns an error)
            if executed_calls >= ai::CHAT_MAX_TOOL_CALLS {
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": format!(
                        "Error: the tool call budget ({}) for this reply is exhausted. \
                         Answer with what you have.",
                        ai::CHAT_MAX_TOOL_CALLS
                    ),
                }));
                continue;
            }
            if cancelled().await {
                return Err(AppError::Cancelled);
            }
            executed_calls += 1;
            let (ok, argument, result_text) = if name == "run_sql" {
                match ai::parse_run_sql_argument(&arguments) {
                    Ok(sql) => {
                        // Whether it was actually sent to the DB (a marker so that an abort
                        // that did not send it is not recorded as "executed")
                        let mut started = false;
                        let outcome = async {
                            // The agent path imposes a narrower whitelist than the usual
                            // readonly guard (rejecting CALL / PRAGMA / multiple statements)
                            if let Some(reason) = db::agent_rejection_reason(&sql, engine) {
                                return Err(AppError::Readonly(reason));
                            }
                            // Acquiring the pool (including establishing the SSH tunnel) can wait a long time. An
                            // abort that arrives in the meantime does not reach the CancelRegistry
                            // (run_query_cancellable has not registered yet), so check once more right before execution
                            let pool = state.db.get_pool(&server).await?;
                            if cancelled().await {
                                return Err(AppError::Cancelled);
                            }
                            started = true;
                            // run_query_cancellable does not appear in the cancel registry until it registers
                            // internally (connection acquisition, session ID lookup), so an abort that arrives then
                            // has no effect. Watch for it ourselves by polling, and when aborted, drop the query's
                            // future and stop waiting (the server side stops if it is registered, and if not it is
                            // a client-side cutoff)
                            let query = db::run_query_cancellable(
                                &pool,
                                &state.query_cancels,
                                &cancel_key,
                                &sql,
                                ai::CHAT_TOOL_MAX_ROWS,
                                None,
                                readonly_guard,
                                // Do not permit dangerous statements for the agent either
                                false,
                            );
                            tokio::pin!(query);
                            loop {
                                tokio::select! {
                                    biased;
                                    result = &mut query => break result,
                                    _ = tokio::time::sleep(
                                        std::time::Duration::from_millis(
                                            CHAT_CANCEL_POLL_INTERVAL_MS,
                                        ),
                                    ) => {
                                        if cancelled().await {
                                            // Merely dropping the future does not stop engines that run via spawn_blocking
                                            // (DuckDB). At this point registration should be done, so request the
                                            // engine-specific cancellation (DuckDB's InterruptHandle etc.)
                                            // again and then stop waiting
                                            let _ = state
                                                .query_cancels
                                                .cancel(&cancel_key)
                                                .await;
                                            break Err(AppError::Cancelled);
                                        }
                                    }
                                }
                            }
                        }
                        .await;
                        // Even when ending with an abort, SQL that was sent to the DB is kept in the record
                        // (the result is not shown to the model or the user, but "what was executed" is not
                        // hidden). The aim of the abort is "not to send data to the AI provider after the
                        // conversation has been discarded or the connection / schema has been switched", so only
                        // the result is discarded and the whole round trip ends
                        let cancelled_now = cancelled().await;
                        if cancelled_now || matches!(outcome, Err(AppError::Cancelled)) {
                            // Do not record one aborted before being sent to the DB as "executed"
                            // (listing a query that was not executed would be rather misleading as an audit)
                            if started {
                                tool_calls.push(ai::ChatToolCall {
                                    name: name.clone(),
                                    argument: sql,
                                    ok: false,
                                    // It is known that execution was entered, but it may have stopped partway through
                                    // connection acquisition, so do not assert that it was "executed"
                                    summary: "Cancelled (may not have run)".to_string(),
                                });
                            }
                            return Err(AppError::Cancelled);
                        }
                        match outcome {
                            Ok(result) => (true, sql, format_chat_tool_result(&result)),
                            Err(e) => (false, sql, format!("Error: {e}")),
                        }
                    }
                    Err(e) => (false, arguments.clone(), format!("Error: {e}")),
                }
            } else {
                (
                    false,
                    arguments.clone(),
                    format!("Error: unknown tool '{name}'"),
                )
            };
            tool_calls.push(ai::ChatToolCall {
                name: name.clone(),
                argument,
                ok,
                // Keep the summary to one line (for the frontend's tooltip display)
                summary: result_text.lines().next().unwrap_or("").to_string(),
            });
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result_text,
            }));
        }
    }
    // Also when the round trip limit is reached, have it write the answer in one last call
    // without passing tools (the tool results so far are in the history, so what was investigated is not wasted)
    if cancelled().await {
        return Err(AppError::Cancelled);
    }
    let message = ai::chat_step(&ai_config, &messages, false, &cancelled).await?;
    if cancelled().await {
        return Err(AppError::Cancelled);
    }
    let content = ai::message_content(&message);
    if content.is_empty() {
        return Err(AppError::Ai(format!(
            "The AI kept calling tools without answering (stopped after {} rounds)",
            ai::CHAT_MAX_TOOL_ROUNDS
        )));
    }
    Ok(content)
}

/// Returns the resolved config (for information display; contains no secrets).
/// It is built from the merged config (cache), so values actually in use, such as
/// sqlfiles_dir overridden by config_override_command, are displayed.
/// Because it goes through the cache, the fetch command does not run every time the modal is opened.
#[tauri::command]
async fn get_config_info(state: tauri::State<'_, AppState>) -> Result<ConfigInfo, AppError> {
    Ok(match state.resolve_config().await {
        Ok(config) => config
            .info()
            .unwrap_or_else(|e| config::config_info_error(&e)),
        Err(e) => config::config_info_error(&e),
    })
}

/// Creates a template if config.yml does not exist. If created, returns its path.
#[tauri::command]
fn ensure_config_file() -> Result<Option<String>, AppError> {
    config::ensure_config_file()
}

/// Returns the contents of config.yml for the config editor (creates the template first if it does not exist, then reads).
#[tauri::command]
fn read_config_file() -> Result<String, AppError> {
    config::read_config_file()
}

/// Save from the config editor. Returns the path of the file written.
#[tauri::command]
fn write_config_file(content: String) -> Result<String, AppError> {
    config::write_config_file(&content)
}

/// From the screen with 0 connections, appends the connection for the SQLite / DuckDB file
/// chosen in the file selection dialog to the servers of config.yml (text append that preserves comments; config.rs).
/// The frontend performs the reload after the append and the selection of the connection.
#[tauri::command]
fn add_file_connection(path: String) -> Result<config::FileConnection, AppError> {
    config::add_file_connection(&path)
}

/// Returns the raw YAML fetched by running config_override_command
/// (for the copy view. It can be edited at the display destination but is not saved).
#[tauri::command]
async fn read_override_config_yaml() -> Result<String, AppError> {
    config::fetch_override_config_yaml().await
}

/// In the result table's Export, writes text to the path the user chose in the native save
/// dialog (the frontend's plugin-dialog save).
/// The path passed in is the one the user chose in the dialog at runtime.
///
/// Note that the backend does not validate the path (it can write to any path). This
/// follows this app's trust model: the frontend loads only its own bundled code and no
/// remote content, and arbitrary SQL execution via `run_query` and config writes via
/// `write_config_file` are already possible, so the damage if the frontend were
/// compromised is already broad. The additional risk of adding a new file write here is judged to be limited.
/// Character encoding used on export.
///
/// The default is UTF-8. CP932 / EUC-JP can be chosen for tools such as Excel that do not assume UTF-8.
/// The encoding name is passed from the frontend as a string.
fn encode_export_contents(contents: &str, encoding: &str) -> Result<Vec<u8>, AppError> {
    let encoder = match encoding {
        // Empty or unspecified is treated as the default UTF-8
        "" | "utf-8" | "utf8" => return Ok(contents.as_bytes().to_vec()),
        // By the Encoding Standard's definition, encoding_rs's SHIFT_JIS is Windows-31J (CP932)
        "cp932" | "shift_jis" | "sjis" => encoding_rs::SHIFT_JIS,
        "euc-jp" | "eucjp" => encoding_rs::EUC_JP,
        other => {
            return Err(AppError::Export(format!(
                "Unsupported export encoding: {other}"
            )));
        }
    };

    let (encoded, _, had_unmappable) = encoder.encode(contents);
    if had_unmappable {
        // Characters that cannot be converted are replaced by encoding_rs with numeric character references (&#12345;).
        // Silently writing broken output would lead to data mix-ups, so it is returned as a failure.
        return Err(AppError::Export(format!(
            "The result contains characters that cannot be represented in {encoding}. \
             Export as UTF-8 instead, or remove those characters."
        )));
    }
    Ok(encoded.into_owned())
}

#[tauri::command]
async fn write_export_file(
    path: String,
    contents: String,
    encoding: Option<String>,
) -> Result<(), AppError> {
    let encoding = encoding.unwrap_or_default();
    let bytes = encode_export_contents(&contents, &encoding)?;
    std::fs::write(&path, bytes)?;
    Ok(())
}

/// Return value of frontend_ready. The targets to open, and the error messages for failures to resolve the startup specification.
/// Stderr is not visible when launched as a GUI, so failures are returned to the frontend and shown in a toast.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchResult {
    /// Targets to open (the startup specification + those queued while starting).
    targets: Vec<router::OpenTarget>,
    /// The reason the startup specification (launch route) failed to resolve (empty if none).
    errors: Vec<String>,
}

/// Notifies that the frontend's listener registration is done, and receives together the
/// "targets to open" accumulated until then. The frontend calls this right after
/// registering the listener in onMount, selects the connection and opens the file for each
/// returned target, and shows errors in a toast.
/// The breakdown of targets is the following two:
/// (1) the launch route specified by deep link / CLI at startup (resolved and then returned),
/// (2) the resolved targets of running routes that arrived and were queued during startup (before ready).
/// After the call, ready = true, and subsequent running routes arrive directly via the
/// `open-query-file` event (the listener already exists, so nothing is missed).
#[tauri::command]
async fn frontend_ready(
    state: tauri::State<'_, AppState>,
) -> Result<LaunchResult, AppError> {
    let mut targets = Vec::new();
    let mut errors = Vec::new();
    // (1) The startup specification (launch route). Resolved against this process's cwd (None).
    //     The std Mutex is not held across an await (just take and release immediately).
    let launch = state.launch_route.lock().unwrap().take();
    if let Some(route) = launch {
        match state.resolve_route_target(&route, None).await {
            Ok(target) => targets.push(target),
            // If the startup specification fails, stderr is not visible when launched as a GUI, so
            // return it to the frontend and show it in a toast (do not swallow it). Other targets and startup are not stopped.
            Err(e) => errors.push(e.to_string()),
        }
    }
    // (2) Set ready and drain the targets queued until then.
    //     Doing the ready setting and drain under one lock serializes with dispatch_route's
    //     "ready check -> push/emit" (preventing misses and double delivery).
    let (mut queued, mut queued_errors) = {
        let mut live = state.live.lock().unwrap();
        live.ready = true;
        (
            std::mem::take(&mut live.pending),
            std::mem::take(&mut live.pending_errors),
        )
    };
    targets.append(&mut queued);
    errors.append(&mut queued_errors);
    Ok(LaunchResult { targets, errors })
}

/// Resolves a route received while running (deep link / CLI subcommand) and delivers it to
/// the frontend by event. Resolution involves reading the config and is async, so it is done in a separate task.
/// On success, puts the OpenTarget on `open-query-file`; on failure, puts the
/// error message on `open-query-file-error`.
fn dispatch_route(app: &tauri::AppHandle, route: router::Route, cwd: Option<PathBuf>) {
    use tauri::{Emitter, Manager};
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        match state.resolve_route_target(&route, cwd).await {
            Ok(target) => {
                // If the frontend listener is not registered yet (during startup), it would be missed, so
                // if not ready, push to the queue (frontend_ready drains it). The ready check and the
                // push are done under one lock, serialized with frontend_ready's ready setting + drain.
                let emit_now = {
                    let mut live = state.live.lock().unwrap();
                    if live.ready {
                        true
                    } else {
                        live.pending.push(target.clone());
                        false
                    }
                };
                if emit_now {
                    if let Err(e) = app.emit("open-query-file", target) {
                        eprintln!("[router] failed to emit open-query-file: {e}");
                    }
                }
            }
            Err(e) => {
                // Like successful targets, if the listener is not registered (during startup), emitting would miss it.
                // If not ready, errors are also queued and drained by frontend_ready.
                let message = e.to_string();
                let emit_now = {
                    let mut live = state.live.lock().unwrap();
                    if live.ready {
                        true
                    } else {
                        live.pending_errors.push(message.clone());
                        false
                    }
                };
                if emit_now {
                    if let Err(emit_err) = app.emit("open-query-file-error", message) {
                        eprintln!("[router] failed to emit open-query-file-error: {emit_err}");
                    }
                }
            }
        }
    });
}

/// List of licenses of dependency libraries bundled with the distribution (Third-Party Licenses modal).
#[tauri::command]
fn third_party_notices() -> &'static str {
    third_party_notices::NOTICES
}

/// Meta information shown in the About dialog (same content as tauri's Menu::default).
fn about_metadata(app: &tauri::AppHandle) -> tauri::menu::AboutMetadata<'_> {
    let package_info = app.package_info();
    let config = app.config();
    tauri::menu::AboutMetadata {
        name: Some(package_info.name.clone()),
        version: Some(package_info.version.to_string()),
        copyright: config.bundle.copyright.clone(),
        authors: config.bundle.publisher.clone().map(|p| vec![p]),
        ..Default::default()
    }
}

/// Builds the app's menu bar.
///
/// The macOS app menu (Queryfolio) is fixed by NSApplication with the content at the time
/// the main menu is installed, so inserting items later has no effect. Therefore, tauri's
/// default menu is not reused; the whole menu including the app menu is built ourselves and
/// passed to Builder::menu from the first installation. On config change, it is rebuilt by this function.
///
/// "View override config yaml (Copy only)" is shown only when config_override_command is set.
///
/// The structure follows tauri's `Menu::default` (the app menu / View are macOS only, and
/// File's quit is non-macOS only). However, Close Window is not placed; Close Tab
/// (CmdOrCtrl+W) is placed in File instead (CYBERNEURA-DEV-773). Config-related items are
/// grouped in the Config submenu regardless of platform
/// (they are hard to find when scattered between the app menu and Config).
fn build_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};

    let edit_config_item =
        MenuItemBuilder::with_id("edit_config_file", "Edit config.yml").build(app)?;
    let edit_source_item = MenuItemBuilder::with_id(
        "view_override_config",
        "View override config yaml (Copy only)",
    )
    .build(app)?;
    let show_source_item = config::has_config_override_command();
    // Place it right under About (the app menu on macOS, the Help menu elsewhere)
    let licenses_item =
        MenuItemBuilder::with_id("show_licenses", "Third-Party Licenses").build(app)?;

    #[cfg(target_os = "macos")]
    let app_menu = {
        use tauri::menu::PredefinedMenuItem;

        let package_info = app.package_info();
        SubmenuBuilder::new(app, package_info.name.clone())
            .item(&PredefinedMenuItem::about(
                app,
                None,
                Some(about_metadata(app)),
            )?)
            .item(&licenses_item)
            .separator()
            .services()
            .separator()
            .hide()
            .hide_others()
            .separator()
            .quit()
            .build()?
    };

    // CmdOrCtrl+W closes the active editor tab, not the window.
    // The predefined Close Window has Cmd+W on macOS, and in this app with a single window,
    // pressing it would close the whole app. So it is placed in neither File nor Window
    // (menu key bindings are processed by NSApp before the WebView, so they cannot be stopped
    // by preventDefault in the frontend's keydown). The decision of what to close is left to
    // the frontend, which knows the editor tabs and open modals
    let close_tab_item = MenuItemBuilder::with_id("close_editor_tab", "Close Tab")
        .accelerator("CmdOrCtrl+W")
        .build(app)?;
    let file_menu = {
        let builder = SubmenuBuilder::new(app, "File").item(&close_tab_item);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.quit();
        builder.build()?
    };
    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    #[cfg(target_os = "macos")]
    let view_menu = SubmenuBuilder::new(app, "View").fullscreen().build()?;
    // Window / Help are created with the same fixed IDs as tauri. macOS's init_app_menu looks
    // up menus by these IDs and registers them to NSApp's windowsMenu / helpMenu, so without
    // the IDs the window list and help search would not be attached
    let window_menu = SubmenuBuilder::with_id(app, tauri::menu::WINDOW_SUBMENU_ID, "Window")
        .minimize()
        .maximize()
        .build()?;
    // Like tauri's default menu, it has no contents on macOS
    // (About is in the app menu, and the system adds help search)
    let help_menu = {
        let builder = SubmenuBuilder::with_id(app, tauri::menu::HELP_SUBMENU_ID, "Help");
        #[cfg(not(target_os = "macos"))]
        let builder = builder
            .about(Some(about_metadata(app)))
            .item(&licenses_item);
        builder.build()?
    };

    // No accelerator is attached. It used to be assigned CmdOrCtrl+R, but although it is the
    // same key as the browser's reload, what actually happens is reload_config_file = discarding
    // all editor tabs, re-establishing connections and aborting chats, so pressing it intending
    // a page reload would return the whole app to its initial state (CYBERNEURA-DEV-648).
    // It is destructive and cannot be undone, so only an explicit selection from the Config menu is allowed
    let reload_item =
        MenuItemBuilder::with_id("reload_config_file", "Reload config file").build(app)?;
    let reveal_item =
        MenuItemBuilder::with_id("reveal_config_folder", "Reveal config folder").build(app)?;
    let config_menu = {
        let mut builder = SubmenuBuilder::new(app, "Config").item(&edit_config_item);
        if show_source_item {
            builder = builder.item(&edit_source_item);
        }
        builder
            .separator()
            .item(&reload_item)
            .item(&reveal_item)
            .build()?
    };

    #[allow(unused_mut)]
    let mut menu = MenuBuilder::new(app);
    #[cfg(target_os = "macos")]
    {
        menu = menu.item(&app_menu);
    }
    menu = menu.item(&file_menu).item(&edit_menu);
    #[cfg(target_os = "macos")]
    {
        menu = menu.item(&view_menu);
    }
    menu.item(&window_menu)
        .item(&help_menu)
        .item(&config_menu)
        .build()
}

/// Rebuilds the menu after the config is re-read.
/// This is because the need for the copy view item changes when the presence of config_override_command changes.
fn rebuild_menu(app: &tauri::AppHandle) {
    match build_menu(app).and_then(|menu| app.set_menu(menu)) {
        Ok(_) => {}
        Err(e) => eprintln!("[menu] failed to rebuild the menu: {e}"),
    }
}

/// Shows config.yml (or the config folder if absent) in a file manager such as Finder.
fn reveal_config_folder() -> Result<(), AppError> {
    let target = match config::existing_config_path()? {
        Some(path) => path,
        None => config::app_config_dir()?,
    };
    tauri_plugin_opener::reveal_item_in_dir(&target)
        .map_err(|e| AppError::Config(format!("Failed to reveal {}: {e}", target.display())))
}

/// Upper limit (bytes) of the content received from stdin by the CLI `write` subcommand.
/// Large enough for a query file, yet a size that prevents loading a huge file into memory
/// whole through a wrongly connected pipe. Exceeding it is an error rather than being
/// truncated (writing partway would save a broken query).
const MAX_STDIN_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// Decides the content of the CLI `write` subcommand.
///
/// If given as an argument, use it; otherwise read from stdin. In the following cases it is
/// treated as "no content specified" = `Ok(None)` (= the existing file is not clobbered):
///
/// - stdin is a terminal (content was not passed in an interactive shell; reading would hang waiting for input)
/// - stdin is empty (for a GUI launch `open -a Queryfolio --args write ...`, stdin is
///   /dev/null and immediately hits EOF. Interpreting this as "write it empty" would
///   silently empty an existing query file)
///
/// A read failure and exceeding the limit are `Err` (do not overwrite with unknown content /
/// a partial one. The caller aborts here and exits non-zero).
fn resolve_write_content(arg_content: Option<String>) -> Result<Option<String>, AppError> {
    use std::io::{IsTerminal, Read};

    if let Some(content) = arg_content {
        return Ok(Some(content));
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    // Read up to the limit + 1 byte, and judge "limit exceeded" if it goes over
    // (cutting exactly at the limit would not tell whether it was truncated or was exactly the limit to begin with).
    let mut buf = String::new();
    stdin
        .take(MAX_STDIN_CONTENT_BYTES as u64 + 1)
        .read_to_string(&mut buf)
        .map_err(|e| {
            AppError::QueryFile(format!("Failed to read the content from stdin: {e}"))
        })?;
    if buf.len() > MAX_STDIN_CONTENT_BYTES {
        return Err(AppError::QueryFile(format!(
            "The content from stdin is larger than the {MAX_STDIN_CONTENT_BYTES} byte limit"
        )));
    }
    Ok(if buf.is_empty() { None } else { Some(buf) })
}

/// Handles the CLI `write <connection> <file-name> [content]` and writes the query file
/// (if no content is specified, it only creates an empty file. An existing one is not changed).
///
/// **The launching process itself does this, before starting Tauri.** When a running
/// instance exists, the single-instance plugin forwards only argv and cwd, and stdin is not
/// passed, so leaving the write to the running instance would lose the content passed via a
/// pipe. The write is completed on the launching side, and the running instance
/// (or itself) is left only with "open".
///
/// On failure, returns `Err`. The caller (run) does not continue startup and **exits non-zero**:
/// if it continued and just did "open", the old content would be opened as is even though
/// the write failed, and it would look like a success to the agent that requested it
/// (the failure could not be told from the exit status either).
///
/// Returns the resolved merged config (`None` if no write was performed).
/// The caller puts this into the `AppState` cache so that `config_override_command` is not
/// run twice in one launch (see `AppState::with_config`).
fn apply_cli_write_route(
    route: &router::Route,
) -> Result<Option<Arc<AppConfig>>, AppError> {
    let router::Route::WriteFile {
        connection,
        file_name,
        content,
    } = route
    else {
        return Ok(None);
    };
    let content = resolve_write_content(content.clone())?;
    tauri::async_runtime::block_on(async {
        let config = Arc::new(AppConfig::load_merged().await?);
        let servers = config.resolve_servers()?;
        let server = servers
            .iter()
            .find(|s| &s.name == connection)
            .ok_or_else(|| {
                AppError::Config(format!(
                    "Connection '{connection}' is not defined in the config"
                ))
            })?;
        let sqlfiles_dir = config.resolve_sqlfiles_dir()?;
        let folder = server.sqlfiles_folder_name();
        let ext = engines::capabilities_for_name(&server.engine).file_extension;
        match &content {
            // Overwrite only when content is specified.
            Some(text) => {
                let name = query_files::normalize_file_name(file_name, ext)?;
                query_files::write_query_file(
                    &sqlfiles_dir,
                    &folder,
                    &name,
                    text,
                    ext,
                )?;
            }
            // If not specified, only "create if absent" (leave existing content).
            None => {
                query_files::ensure_query_file(
                    &sqlfiles_dir,
                    &folder,
                    file_name,
                    ext,
                )?;
            }
        }
        // When a folder is newly created, place the connection's description meta file
        // (same handling as create / save within the app. Best effort).
        let dir = query_files::connection_dir(&sqlfiles_dir, &folder)?;
        let _ = folder_meta::write_folder_meta(&dir, server);
        Ok::<Option<Arc<AppConfig>>, AppError>(Some(config))
    })
}

/// Handles CLI options that do not launch the GUI and returns the process exit code.
///
/// Building the display is left to the pure functions of [`crate::cli`]; this only does
/// reading the config and output. `--list-servers` reads the config, so it can fail. In that
/// case it writes to stderr and ends with 1 (so the calling script can tell the failure).
fn run_info_command(command: cli::InfoCommand) -> i32 {
    // Windows release builds have no console, so reattach before writing
    // (see cli::attach_parent_console for details). Does nothing on other OSes
    cli::attach_parent_console();

    match command {
        cli::InfoCommand::Help => {
            print!("{}", cli::help_text());
            0
        }
        cli::InfoCommand::Version => {
            println!("{}", cli::version_text());
            0
        }
        cli::InfoCommand::License => {
            print!("{}", third_party_notices::NOTICES);
            0
        }
        cli::InfoCommand::ListServers => {
            let result = tauri::async_runtime::block_on(async {
                let config = AppConfig::load_merged().await?;
                let servers = config.resolve_servers()?;
                let sqlfiles_dir = config.resolve_sqlfiles_dir()?;
                // Environment variables that override endpoints are resolved here
                // (keep the table building in cli.rs a pure function that does not depend on the process environment)
                let aws_endpoint = cli::aws_endpoint_override_from_env();
                Ok::<String, AppError>(cli::format_server_list(
                    &servers,
                    &sqlfiles_dir,
                    aws_endpoint.as_deref(),
                ))
            });
            match result {
                Ok(text) => {
                    print!("{text}");
                    0
                }
                Err(e) => {
                    eprintln!("[cli] failed to read the config: {e}");
                    1
                }
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri::Emitter;

    // The CLI `write` is written out by this process before starting Tauri
    // (the single-instance plugin) (see the documentation of apply_cli_write_route).
    // Only the startup arguments are looked at: on the path where a running instance
    // processes the argv forwarded to it (the single-instance callback), that argv belongs to
    // another process, so the write has already been done.
    //
    // The merged config resolved here is carried over to the AppState cache
    // (do not run config_override_command twice per launch).
    let preflight_config = {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        // --help / --version / --license / --list-servers only display and exit
        // (neither the GUI nor a window is launched). Look at them before the write.
        if let Some(command) = cli::info_command_from_args(&argv) {
            std::process::exit(run_info_command(command));
        }
        match router::route_from_cli_args(&argv) {
            Some(route) => match apply_cli_write_route(&route) {
                Ok(config) => config,
                Err(e) => {
                    // Do not go to open something that could not be written. Make it possible for the
                    // caller of the CLI to tell the failure from the exit status.
                    eprintln!("[cli] failed to write the query file: {e}");
                    std::process::exit(1);
                }
            },
            None => None,
        }
    };

    tauri::Builder::default()
        // single-instance is registered first (plugins run in registration order).
        // With the deep-link feature enabled: a queryfolio:// URL in the argv of a second launch
        // is forwarded to the running instance's deep-link plugin and on_open_url fires.
        // Here, additionally (1) bring the window to the front and (2) process the CLI subcommand
        // (queryfolio open <path>) (URL arguments were already processed above, so they are ignored).
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            use tauri::Manager;
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
            // cwd is the directory the second launch came from. Relative CLI paths are resolved against it.
            if let Some(route) = router::route_from_cli_args(&argv) {
                dispatch_route(app, route, Some(PathBuf::from(cwd)));
            }
        }))
        // Deep link for the queryfolio:// scheme. macOS receives the URL natively;
        // Linux/Windows receive it via the single-instance above (deep-link feature).
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        // Used to open the native save dialog in the result table's Export
        .plugin(tauri_plugin_dialog::init())
        // Save the window size and position on exit and restore them at startup
        .plugin(tauri_plugin_window_state::Builder::default().build())
        // If set_menu is done in setup, the macOS app menu gets fixed by tauri's default menu
        // installed before that, so it is passed here
        .menu(build_menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "reload_config_file" => {
                // Reload is tied to the frontend's state (selection / unsaved edits), so
                // notify by event and leave it to the frontend's reloadConnections
                if let Err(e) = app.emit("menu-reload-config", ()) {
                    eprintln!("[menu] failed to emit reload event: {e}");
                }
            }
            "reveal_config_folder" => {
                if let Err(e) = reveal_config_folder() {
                    eprintln!("[menu] {e}");
                }
            }
            "edit_config_file" => {
                if let Err(e) = app.emit("menu-edit-config", ()) {
                    eprintln!("[menu] failed to emit edit config event: {e}");
                }
            }
            "close_editor_tab" => {
                if let Err(e) = app.emit("menu-close-editor-tab", ()) {
                    eprintln!("[menu] failed to emit close editor tab event: {e}");
                }
            }
            "show_licenses" => {
                if let Err(e) = app.emit("menu-show-licenses", ()) {
                    eprintln!("[menu] failed to emit show licenses event: {e}");
                }
            }
            "view_override_config" => {
                if let Err(e) = app.emit("menu-view-override-config", ()) {
                    eprintln!("[menu] failed to emit view override config event: {e}");
                }
            }
            _ => {}
        })
        .manage(AppState::with_config(preflight_config))
        .setup(|app| {
            use tauri::Manager;
            use tauri_plugin_deep_link::DeepLinkExt;
            // Register the scheme at runtime for dev / Linux runs (on macOS it is registered in
            // Info.plist at bundle time). Best effort: startup continues even if it fails.
            if let Err(e) = app.deep_link().register_all() {
                eprintln!("[router] failed to register deep link schemes: {e}");
            }
            // Handler for when a URL is opened while running (macOS native / Linux forwarding).
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    match router::parse_uri(url.as_str()) {
                        // A deep link URL is assumed to be an absolute path, so cwd is not needed (None)
                        Ok(route) => dispatch_route(&handle, route, None),
                        Err(e) => eprintln!("[router] ignoring URL {url}: {e}"),
                    }
                }
            });
            // Note the route specified at startup (the frontend takes it out in frontend_ready).
            // Priority: deep link launch (macOS: get_current returns the URL) -> CLI subcommand.
            let mut launch: Option<router::Route> = None;
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for url in urls {
                    if let Ok(route) = router::parse_uri(url.as_str()) {
                        launch = Some(route);
                        break;
                    }
                }
            }
            if launch.is_none() {
                let argv: Vec<String> = std::env::args().skip(1).collect();
                launch = router::route_from_cli_args(&argv);
            }
            if launch.is_some() {
                *app.state::<AppState>().launch_route.lock().unwrap() = launch;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_connections,
            reset_connections,
            run_query,
            cancel_query,
            list_query_history,
            list_query_files,
            search_query_files,
            read_query_file,
            query_file_path,
            write_query_file,
            write_query_file_if_unchanged,
            create_query_file,
            delete_query_file,
            rename_query_file,
            move_query_file,
            list_schemas,
            set_active_schema,
            get_active_schema,
            disconnect,
            list_tables,
            list_columns,
            get_schema_map,
            get_primary_keys,
            run_statements,
            get_ai_info,
            ai_generate_sql,
            build_explain_sql,
            check_dangerous_statement,
            can_rerun_for_output,
            ai_explain_plan,
            ai_explain_sql,
            ai_fix_sql,
            ai_chat,
            cancel_ai_chat,
            get_config_info,
            third_party_notices,
            ensure_config_file,
            read_config_file,
            write_config_file,
            add_file_connection,
            read_override_config_yaml,
            write_export_file,
            frontend_ready,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod export_encoding_tests {
    use super::*;

    #[test]
    fn utf8_is_the_default() {
        assert_eq!(
            encode_export_contents("あa", "").unwrap(),
            "あa".as_bytes()
        );
        assert_eq!(
            encode_export_contents("あa", "utf-8").unwrap(),
            "あa".as_bytes()
        );
    }

    #[test]
    fn encodes_cp932_and_euc_jp() {
        // "あ" (hiragana "a") is 0x82 0xA0 in CP932 and 0xA4 0xA2 in EUC-JP
        assert_eq!(
            encode_export_contents("あa", "cp932").unwrap(),
            vec![0x82, 0xA0, 0x61]
        );
        assert_eq!(
            encode_export_contents("あa", "euc-jp").unwrap(),
            vec![0xA4, 0xA2, 0x61]
        );
    }

    #[test]
    fn rejects_unknown_encoding() {
        assert!(encode_export_contents("a", "utf-16").is_err());
    }

    #[test]
    fn rejects_unmappable_characters() {
        // Characters that cannot be converted turn into numeric character references, so return an error instead of silently writing
        assert!(encode_export_contents("a🍣b", "cp932").is_err());
        assert!(encode_export_contents("🍣", "euc-jp").is_err());
    }
}
