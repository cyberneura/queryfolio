mod ai;
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
mod tunnel;

use std::path::PathBuf;
use std::sync::Arc;

use config::{AppConfig, ConfigInfo, ConnectionInfo, ServerConfig};
use db::{CancelRegistry, DbManager, DbPool, QueryResult, DEFAULT_MAX_ROWS};
use error::AppError;

/// 実行中に届いた「開く対象」のフロントへの受け渡し状態。
/// フロントの listener は onMount (webview 準備後) に登録されるため、それより前に
/// deep link / CLI が届くと `open-query-file` イベントを取りこぼす。ready になるまでは
/// キューに積み、frontend_ready でまとめて渡す (単一 Mutex で ready 判定と push/drain を
/// 直列化し、取りこぼし・二重配送を防ぐ)。
#[derive(Default)]
struct LiveDelivery {
    /// フロントの listener が用意できたか (frontend_ready で true になる)。
    ready: bool,
    /// ready 前に届いた開く対象 (frontend_ready で drain して渡す)。
    pending: Vec<router::OpenTarget>,
    /// ready 前に届いた解決失敗のメッセージ (frontend_ready で drain して渡す)。
    /// 成功対象と同様、listener 準備前は emit しても取りこぼすためキューする。
    pending_errors: Vec<String>,
}

/// アプリ全体の共有状態。
#[derive(Default)]
struct AppState {
    /// マージ済み設定 (config_override_command 適用後) のセッションキャッシュ。
    /// 取得コマンドは外部プロセス実行を伴うため、毎回走らせない
    /// (reset_connections でクリアして再取得する)。
    config: tokio::sync::Mutex<Option<Arc<AppConfig>>>,
    /// 接続設定のキャッシュ。get_connections で更新される。
    /// パスワード等の機密を含むためフロントエンドには渡さない。
    servers: tokio::sync::Mutex<Option<Vec<ServerConfig>>>,
    db: DbManager,
    /// 実行中クエリのキャンセルレジストリ (接続名単位)。
    query_cancels: CancelRegistry,
    /// クエリ実行履歴の記録 (接続ごとの行数キャッシュを保持)。
    history: history::HistoryManager,
    /// スキーマ情報 (テーブル・カラム) のキャッシュ。
    /// スキーマブラウザと SQL 補完 (get_schema_map) で共有する。
    schema_cache: schema_info::SchemaCache,
    /// AI 設定のセッションキャッシュ (reset_connections でクリア)。
    /// 外側の None は未解決を表す。api_key を含むためフロントには渡さず、
    /// get_ai_info で configured / model のみを返す。
    ai: tokio::sync::Mutex<Option<Option<ai::AiConfig>>>,
    /// 起動時に `queryfolio://` deep link / CLI サブコマンドで指定された
    /// 開くべきルート (無ければ None)。フロントが起動後に frontend_ready で
    /// 1 度だけ取り出す (取り出すと消える)。実行中に開かれたルートは live 経由
    /// (イベント / キュー) でフロントへ届けるため、ここには積まない。
    launch_route: std::sync::Mutex<Option<router::Route>>,
    /// 実行中に届いた開く対象の受け渡し (listener 準備前の取りこぼし対策)。
    live: std::sync::Mutex<LiveDelivery>,
}

impl AppState {
    /// マージ済み設定を解決する (セッションキャッシュあり)。
    /// config_override_command は 1Password 等の外部コマンドで数秒かかり
    /// Touch ID を要求することもあるため、クエリ実行のたびに走らせない。
    /// reset_connections でクリアされる。
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

    /// クエリファイル保存ディレクトリを解決する。
    /// config.yml は手編集されるため、開いているファイルの保存中に
    /// sqlfiles_dir が変わると未保存内容が新ディレクトリへ書かれてしまう。
    /// マージ済み設定のキャッシュが再読込 (reset_connections) まで固定される
    /// ため、dirty ファイルの保存先も読み込み時のディレクトリに固定される。
    async fn resolve_sqlfiles_dir(&self) -> Result<PathBuf, AppError> {
        self.resolve_config().await?.resolve_sqlfiles_dir()
    }

    /// クエリファイルの保存フォルダ名 → 接続名の対応表を作る (設定順)。
    /// `queryfolio://open/<path>` / CLI で指定されたパスから、そのファイルが
    /// どの接続のものかを解決するために使う (router::resolve_open_target)。
    async fn folder_connection_map(&self) -> Result<Vec<(String, String)>, AppError> {
        let servers = self.resolve_config().await?.resolve_servers()?;
        Ok(servers
            .iter()
            .map(|s| (s.sqlfiles_folder_name(), s.name.clone()))
            .collect())
    }

    /// ルート (deep link / CLI) を、開く対象のクエリファイル (接続 + ファイル名) へ
    /// 解決する。保存ディレクトリ配下の接続フォルダにある、接続エンジンの拡張子の
    /// クエリファイルでなければエラー。
    /// 既知の限界: ここでの検証と実際の読み込み (read_query_file) は別呼び出しで、
    /// その間に設定リロードが挟まると folder / 拡張子の解決結果がズレ得る
    /// (検証済み設定と読込時設定の狭い TOCTOU)。リロードはユーザーの明示操作で、
    /// どちらの解決も設定由来の保存領域内に閉じるため許容する。
    /// `cwd` は相対パスを解決する基準ディレクトリ。deep link / CLI を実行中
    /// インスタンスが受け取った時は「起動元ディレクトリ」を渡す (single-instance の
    /// callback cwd)。None の時はこのプロセスのカレントディレクトリを使う。
    async fn resolve_route_target(
        &self,
        route: &router::Route,
        cwd: Option<PathBuf>,
    ) -> Result<router::OpenTarget, AppError> {
        match route {
            router::Route::OpenFile { path } => {
                // sqlfiles_dir は「アプリプロセスのカレントディレクトリ」で絶対化する。
                // 設定が相対 sqlfiles_dir の場合、実際のファイル I/O (query_files) と
                // verify_within_dir はアプリプロセスの cwd 基準で解決されるため、
                // パス検証の base もそこに揃える (実行中インスタンスへ渡る起動元 cwd と
                // 混同しない)。std::path::absolute は FS に触れない字句的絶対化。
                let sqlfiles_dir = self.resolve_sqlfiles_dir().await?;
                let sqlfiles_dir =
                    std::path::absolute(&sqlfiles_dir).unwrap_or(sqlfiles_dir);
                let folders = self.folder_connection_map().await?;
                let home = dirs::home_dir();
                // 生の入力パスの相対解決だけは cwd (実行中インスタンスなら起動元) 基準。
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
                // 拡張子が接続エンジンのものと一致することを検証する。
                // 一致しないと「router / verify_within_dir が検証したパス」と
                // 「query_files が拡張子を付け直して実際に開くパス」がズレて、
                // symlink 防御が実 I/O 対象に効かなくなる (例: SQL 接続に
                // foo.redis を渡すと検証は foo.redis、実 I/O は foo.redis.sql)。
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
                // 多重防御: 字句検証 (router) を通っても、接続フォルダやファイルが
                // シンボリックリンクで保存領域外の実体を指していることがある。
                // 実際に開くパス (sqlfiles_dir/<folder>/<file>) を canonicalize して、
                // リンク解決後も保存ディレクトリ配下に留まることを確かめる
                // (「queryfolio のデータ保存パスのみ対象」の要件を実体レベルで担保)。
                let folder = server.sqlfiles_folder_name();
                let concrete = sqlfiles_dir.join(&folder).join(&target.file_name);
                verify_within_dir(&sqlfiles_dir, &concrete)?;
                Ok(target)
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

    /// クエリファイル操作に必要なコンテキストを解決する:
    /// 保存ディレクトリ・接続フォルダ名・エンジン別のファイル拡張子。
    /// フォルダ名は folder_name → <host>_<engine>_<schema>_<user> の順で決まる
    /// (接続 name はフォルダ名には使わない)。
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

    /// 接続のクエリファイルフォルダに、接続を説明するメタファイルを書き出す。
    /// フォルダが未作成なら何もしない (メタだけのために空フォルダを作らない)。
    /// クエリファイルの作成・保存・一覧時のリフレッシュに使う。
    async fn refresh_folder_meta(&self, server: &ServerConfig) -> Result<(), AppError> {
        let dir = query_files::connection_dir(
            &self.resolve_sqlfiles_dir().await?,
            &server.sqlfiles_folder_name(),
        )?;
        folder_meta::write_folder_meta(&dir, server)
    }

    /// スキーマキャッシュのキーになるアクティブスキーマ名を返す
    /// (オーバーライド > 設定のデフォルト > 空文字)。
    async fn active_schema_key(&self, server: &ServerConfig) -> String {
        match self.db.schema_override(&server.name).await {
            Some(schema) => schema,
            None => server.schema.clone().unwrap_or_default(),
        }
    }

    /// AI 設定を解決する (キャッシュあり)。未設定なら Ok(None)。
    /// マージ済み設定のトップレベル `ai:` を見る (config_override_command で
    /// 取得した YAML 側の ai がローカルより優先されるのはマージの結果)。
    /// 解決エラー (不明 provider 等) はキャッシュせず毎回返す
    /// (設定修正 + リロードで直せるように)。
    async fn resolve_ai_config(&self) -> Result<Option<ai::AiConfig>, AppError> {
        let mut cached = self.ai.lock().await;
        if let Some(ai_config) = cached.as_ref() {
            return Ok(ai_config.clone());
        }
        let ai_config = ai::resolve_ai_config(self.resolve_config().await?.ai().as_ref())?;
        *cached = Some(ai_config.clone());
        Ok(ai_config)
    }

    /// テーブル → カラム名リストのマップを解決する (キャッシュあり)。
    /// SQL 補完 (get_schema_map) と AI の SQL 生成コンテキストで共有する。
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

    /// AI コマンド (SQL 生成 / エラー修正) 共通のコンテキストを解決する:
    /// AI 設定・接続設定・プロンプト用アクティブスキーマ名・スキーママップ。
    /// AI 未設定時は案内メッセージのエラーを返す。
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
        // sqlite の schema はローカル DB ファイルパスなので、プロンプトには含めない
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
    // 同じマージ済み設定から AI 設定も解決してキャッシュする。解決エラーは
    // ここでは接続一覧を壊さず、get_ai_info / ai_generate_sql 側の再解決で返す。
    match ai::resolve_ai_config(config.ai().as_ref()) {
        Ok(ai_config) => *state.ai.lock().await = Some(ai_config),
        Err(_) => *state.ai.lock().await = None,
    }
    *state.servers.lock().await = Some(servers);
    Ok(infos)
}

/// 接続設定のキャッシュ・プール・SSH トンネルを破棄する。
/// 設定を変更した後のリロード時に呼ぶ。
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
    // 設定を編集して config_override_command の有無が変わることがあるため、
    // コピー用ビュー (保存不可) のメニュー項目の要否を再判定する
    rebuild_menu(&app);
    Ok(())
}

#[tauri::command]
async fn run_query(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
    max_rows: Option<usize>,
    // ツールバーの Writable スイッチの状態。省略・false は読み取り専用
    // (安全側の既定)。config の readonly: true はこれより優先される。
    writable: Option<bool>,
) -> Result<QueryResult, AppError> {
    let server = state.find_server(&connection).await?;
    // config の readonly が最優先のハードロック。次にスイッチ。
    let readonly_guard = if server.readonly {
        db::ReadonlyGuard::Config
    } else if writable.unwrap_or(false) {
        db::ReadonlyGuard::Off
    } else {
        db::ReadonlyGuard::Switch
    };
    // 履歴記録用に実行時点のアクティブスキーマを控えておく
    let schema = match state.db.schema_override(&connection).await {
        Some(schema) => Some(schema),
        None => server.schema.clone(),
    };
    let auto_limit = match state.resolve_default_limit().await? {
        0 => None,
        limit => Some(limit),
    };
    let started = std::time::Instant::now();

    let result = async {
        // \c <database> は SQL の実行ではなく接続状態の変更なので、
        // プールを取得する前にここで処理する。
        // (メタコマンドの解釈エラーもここで出すことで、失敗として履歴に残る)
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
            max_rows.unwrap_or(DEFAULT_MAX_ROWS),
            auto_limit,
            readonly_guard,
            server.allow_dangerous_statements,
        )
        .await
    }
    .await;

    // 成功・失敗にかかわらず実行履歴を記録する。
    // 記録の失敗でクエリ結果を損なわないよう、エラーはログに留める。
    // (追記は小さな同期 I/O なので async コンテキストのまま行う。
    //  ローテーション時のみ全読み・書き直しが走るが、上限 1 万行 =
    //  高々数 MB のため許容する)
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

/// `\c <database>` の実処理。アクティブスキーマを切り替え、切替後の接続で
/// 確認用のクエリを実行して結果として返す (空の結果だと成功が分かりにくいため)。
///
/// 切替に失敗した場合 (存在しない database 等) は元のスキーマへ戻す。
/// 戻さないと、以降すべてのクエリが接続できない状態で残ってしまう。
async fn switch_active_schema(
    state: &tauri::State<'_, AppState>,
    server: &ServerConfig,
    schema: String,
    started: std::time::Instant,
) -> Result<QueryResult, AppError> {
    let previous = state.db.schema_override(&server.name).await;
    state.db.set_schema_override(&server.name, schema.clone()).await;

    // 切替後の接続で実際に繋がることを確かめる。ここで失敗したら巻き戻す
    let confirm = async {
        let pool: DbPool = state.db.get_pool(server).await?;
        let sql = match db::parse_engine(&server.engine)? {
            db::Engine::MySql => "SELECT DATABASE() AS `database`",
            db::Engine::Postgres => "SELECT current_database() AS database",
            // sqlite / redis は meta_commands 側で弾いているのでここには来ない
            db::Engine::Sqlite => {
                return Err(AppError::Config(
                    "\\c is not supported for SQLite".into(),
                ));
            }
            db::Engine::Redis | db::Engine::Elasticsearch => {
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
            // 確認用の SELECT なので readonly 接続でも通る
            db::ReadonlyGuard::Config,
            false,
        )
        .await
    }
    .await;

    match confirm {
        Ok(mut result) => {
            // 確認クエリの実行中にユーザーがスキーマ選択で別の database へ
            // 変えていた場合、こちらの切替先をフロントへ報告すると
            // 実際の接続先と表示が食い違うため報告しない
            // (そちらの切替が自前でキャッシュ破棄と表示更新を済ませている)
            let still_ours =
                state.db.schema_override(&server.name).await.as_deref() == Some(schema.as_str());
            if still_ours {
                // 切替後は古いスキーマのテーブル一覧・カラムを返さないようにする
                state.schema_cache.invalidate_connection(&server.name).await;
                result.switched_schema = Some(schema);
            }
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        }
        Err(e) => {
            // 切替中にユーザーがスキーマ選択で別の database へ変えていた場合は
            // 巻き戻さない (そちらの選択を尊重する)
            state
                .db
                .rollback_schema_override(&server.name, &schema, previous)
                .await;
            // キャンセルはフロントが「Query cancelled」の完全一致で判定して
            // 専用表示にするため、理由を包まずそのまま返す
            if matches!(e, AppError::Cancelled) {
                return Err(e);
            }
            Err(AppError::Config(format!(
                "Failed to switch to {schema}: {e}"
            )))
        }
    }
}

/// 接続で実行中のクエリにキャンセルを要求する。
/// 実行中のクエリが無ければ何もせず false を返す。
/// キャンセルされた実行は run_query 側が AppError::Cancelled
/// ("Query cancelled") で返る。
#[tauri::command]
async fn cancel_query(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<bool, AppError> {
    state.query_cancels.cancel(&connection).await
}

/// 接続のクエリ実行履歴を新しい順に返す。
/// search を指定すると SQL の部分一致 (大文字小文字を区別しない) で絞り込む。
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
) -> Result<Vec<String>, AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    let files = query_files::list_query_files(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        ext,
    )?;
    // フォルダを開いた時に接続の説明メタファイルを最新化する (ベストエフォート:
    // メタ書き込みの失敗で一覧取得を壊さない)。フォルダ未作成時は何もしない。
    let _ = state.refresh_folder_meta(&server).await;
    Ok(files)
}

/// 接続のクエリファイルをファイル名・中身で検索する (大文字小文字を区別しない部分一致)。
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

/// クエリファイルの絶対パスを返す (FilesPane の「Copy full path」用)。
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
    // 保存でフォルダが確実に存在するタイミングで説明メタファイルを最新化する
    // (ベストエフォート: メタ書き込みの失敗で保存を壊さない)。
    let _ = state.refresh_folder_meta(&server).await;
    Ok(())
}

/// 楽観的排他つきの保存。expected_base とディスクの現在内容が一致する時だけ書き込む。
/// 書けたら true、アプリ外で変更されていて書かなかったら false を返す
/// (フロントはマージ/衝突処理へ回す)。暗黙の保存 (自動保存・閉じる前保存) で使い、
/// 外部変更を黙って上書きしないための atomic 寄りの CAS。
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
    // フォルダ新規作成のタイミングで接続の説明メタファイルを書き出す
    // (ベストエフォート: メタ書き込みの失敗で作成を壊さない)。
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

/// 接続先サーバー上の database (スキーマ) 一覧を返す。
#[tauri::command]
async fn list_schemas(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<Vec<String>, AppError> {
    let server = state.find_server(&connection).await?;
    let pool = state.db.get_pool(&server).await?;
    db::list_schemas(&pool, &server).await
}

/// 接続のアクティブスキーマ (database) を切り替える。
/// プールが再構築され、次のクエリから新しい database に接続される。
#[tauri::command]
async fn set_active_schema(
    state: tauri::State<'_, AppState>,
    connection: String,
    schema: String,
) -> Result<(), AppError> {
    if schema.trim().is_empty() {
        return Err(AppError::Config("The schema name is empty".into()));
    }
    // 接続名の実在確認 (存在しない接続へのオーバーライド蓄積を防ぐ)
    state.find_server(&connection).await?;
    state.db.set_schema_override(&connection, schema).await;
    // 切替後に古いスキーマ情報を返さないよう、接続単位でキャッシュを破棄する
    state.schema_cache.invalidate_connection(&connection).await;
    Ok(())
}

/// 接続のアクティブスキーマを返す (オーバーライトが無ければ設定のデフォルト)。
#[tauri::command]
async fn get_active_schema(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<Option<String>, AppError> {
    if let Some(schema) = state.db.schema_override(&connection).await {
        return Ok(Some(schema));
    }
    let server = state.find_server(&connection).await?;
    Ok(server.schema)
}

/// 指定接続のプールと SSH トンネルを破棄する。
/// この接続のエディタタブが全て閉じられた時にフロントから呼ぶ。
/// 接続設定・アクティブスキーマの選択は残るため、次に必要になった時
/// (ファイルを開く / スキーマブラウザを開く / クエリ実行) に自動で張り直される。
#[tauri::command]
async fn disconnect(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<(), AppError> {
    state.db.disconnect(&connection).await;
    Ok(())
}

/// 接続先のテーブル / ビューの一覧を返す (キャッシュあり)。
/// refresh = true でキャッシュを破棄して再取得する (リロードボタン用)。
#[tauri::command]
async fn list_tables(
    state: tauri::State<'_, AppState>,
    connection: String,
    refresh: Option<bool>,
) -> Result<Vec<schema_info::TableInfo>, AppError> {
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    if refresh.unwrap_or(false) {
        // カラムのキャッシュも古い可能性があるため、スキーマ単位で丸ごと破棄する
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

/// テーブルのカラム一覧を返す (キャッシュあり。ツリー展開時の遅延ロード用)。
/// table は list_tables が返す qualified_name を渡す。
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

/// テーブル名 → カラム名リストのマップを返す (SQL 補完の強化用)。
/// キャッシュに全テーブル分のカラムが無ければ一括取得してキャッシュする。
#[tauri::command]
async fn get_schema_map(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<std::collections::BTreeMap<String, Vec<String>>, AppError> {
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    state.resolve_schema_map(&server, &schema_key).await
}

/// テーブルの主キーを構成するカラム名を返す (結果グリッドのセル編集用)。
/// 主キーが無いテーブルでは空を返す。
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

/// 結果グリッドのセル編集を UPDATE 群として 1 トランザクションで適用する。
/// writable の解決は run_query と同じ (config readonly が最優先、次にスイッチ)。
/// 合計の影響行数を返す。
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

/// AI 設定の情報 (configured / model) を返す。api_key は含めない。
/// `ai:` セクションが無い場合はエラーではなく configured: false。
/// セクションはあるが不正 (不明 provider 等) な場合はエラーを返す。
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

/// 自然言語の指示から SQL を生成して返す。実行はせず、エディタへの
/// 挿入もフロント側に任せる (ユーザーが確認してから実行する)。
/// LLM に送るのはスキーマ情報 (テーブル・カラム名)・エンジン方言・
/// アクティブスキーマ名・ユーザーの指示のみ。クエリの結果データや
/// 接続情報 (ホスト・認証情報) は送らない。
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

/// 失敗した SQL と DB のエラーメッセージから修正案の SQL を生成して返す。
/// 実行はせず、エディタへの反映もユーザーの確認 (Apply) に任せる。
/// LLM に送るのは失敗した SQL・エラーメッセージ・スキーマ情報
/// (テーブル・カラム名)・エンジン方言・アクティブスキーマ名のみ。
/// クエリの結果データや接続情報 (ホスト・認証情報) は送らない。
/// 注意: DB のエラーメッセージ自体が値を含むことがある (例: 一意制約違反の
/// DETAIL に衝突したキー値が載る)。修正に必要な情報のため加工せず送る
/// 設計とし、フロントのボタン tooltip で送信内容を明示している。
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

/// エンジン別の EXPLAIN プレフィックスを付けた SQL を組み立てて返す。
/// 実行はしない (フロントが通常の run_query 経路で実行する)。
/// 対象は SELECT / WITH のみ (Postgres の EXPLAIN ANALYZE は対象文を
/// 実際に実行するため、DML への付与はエラーで拒否する)。
#[tauri::command]
async fn build_explain_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<String, AppError> {
    let server = state.find_server(&connection).await?;
    db::build_explain_sql(&server.engine, &sql)
}

/// 危険な文 (WHERE 無し UPDATE/DELETE、DROP/TRUNCATE) なら理由を返す。
/// 実行はしない。allow_dangerous_statements が有効な接続で、フロントが
/// 実行前に確認ダイアログを出すかどうかを判断するために使う
/// (無効な接続では run_query 側が拒否するため、フロントは呼ぶ必要がない)。
#[tauri::command]
async fn check_dangerous_statement(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<Option<String>, AppError> {
    let server = state.find_server(&connection).await?;
    db::dangerous_statement_reason(&server.engine, &sql)
}

/// EXPLAIN の実行計画を AI に解説させ、ボトルネックの特定・インデックス
/// 提案・書き直し案の Markdown を返す。LLM に送るのはスキーマ情報
/// (テーブル・カラム名)・エンジン方言・アクティブスキーマ名・SQL・
/// 実行計画テキストのみ (実行計画はクエリの結果データではなくプランナー
/// 出力なので許容する)。接続情報 (ホスト・認証情報) は送らない。
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
    // sqlite の schema はローカル DB ファイルパスなので、プロンプトには含めない
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

/// カーソル位置 (選択中) の SQL 文を AI に平易に解説させ、Markdown を返す。
/// 実行はしない。LLM に送るのは SQL・スキーマ情報 (テーブル・カラム名)・
/// エンジン方言・アクティブスキーマ名のみ。クエリの結果データや接続情報
/// (ホスト・認証情報) は送らない。
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

/// 設定の解決結果を返す (情報表示用。機密を含まない)。
/// マージ済み設定 (キャッシュ) から作るので、config_override_command で
/// 上書きされた sqlfiles_dir 等も実際に使われている値が表示される。
/// キャッシュ経由なので取得コマンドがモーダルを開くたびに走ることはない。
#[tauri::command]
async fn get_config_info(state: tauri::State<'_, AppState>) -> Result<ConfigInfo, AppError> {
    Ok(match state.resolve_config().await {
        Ok(config) => config
            .info()
            .unwrap_or_else(|e| config::config_info_error(&e)),
        Err(e) => config::config_info_error(&e),
    })
}

/// config.yml が無ければテンプレートを作成する。作成した場合はそのパスを返す。
#[tauri::command]
fn ensure_config_file() -> Result<Option<String>, AppError> {
    config::ensure_config_file()
}

/// 設定エディタ用に config.yml の中身を返す (無ければテンプレートを作成してから読む)。
#[tauri::command]
fn read_config_file() -> Result<String, AppError> {
    config::read_config_file()
}

/// 設定エディタからの保存。書き込んだファイルのパスを返す。
#[tauri::command]
fn write_config_file(content: String) -> Result<String, AppError> {
    config::write_config_file(&content)
}

/// config_override_command を実行して取得した生の YAML を返す
/// (コピー用ビュー用。表示先では編集できるが保存はしない)。
#[tauri::command]
async fn read_override_config_yaml() -> Result<String, AppError> {
    config::fetch_override_config_yaml().await
}

/// 結果テーブルの Export で、ネイティブ保存ダイアログ (フロントの
/// plugin-dialog save) でユーザーが選んだパスへテキストを書き出す。
/// パスは実行時にダイアログでユーザーが選んだものが渡ってくる。
///
/// バックエンドではパスを検証しない (任意パスへ書ける) 点に注意。これは
/// このアプリの信頼モデルに沿う: フロントエンドは自前の同梱コードのみで
/// リモートコンテンツを読み込まず、`run_query` で任意 SQL 実行・
/// `write_config_file` で設定書き込みが既に可能なため、フロントが侵害された
/// 場合の被害範囲は元々広い。ここで新たにファイル書き込みが増えることの
/// 追加リスクは限定的と判断している。
#[tauri::command]
async fn write_export_file(path: String, contents: String) -> Result<(), AppError> {
    std::fs::write(&path, contents)?;
    Ok(())
}

/// frontend_ready の戻り値。開く対象と、起動時指定の解決に失敗したエラーメッセージ。
/// GUI 起動では stderr が見えないため、失敗はフロントへ返してトーストで知らせる。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchResult {
    /// 開く対象 (起動時指定 + 起動中にキューされた分)。
    targets: Vec<router::OpenTarget>,
    /// 起動時指定 (launch route) の解決に失敗した理由 (無ければ空)。
    errors: Vec<String>,
}

/// フロントの listener 登録が済んだことを知らせ、それまでに溜まった「開く対象」を
/// まとめて受け取る。フロントは onMount で listener を登録した直後にこれを呼び、
/// 返った各対象について接続を選択してファイルを開き、errors はトーストで知らせる。
/// targets の内訳は次の 2 つ:
/// (1) 起動時に deep link / CLI で指定された launch route (解決してから返す)、
/// (2) 起動中 (ready 前) に届いてキューされた実行中ルートの解決済み対象。
/// 呼び出し後は ready = true になり、以降の実行中ルートは `open-query-file` イベントで
/// 直接届く (listener が既にあるため取りこぼさない)。
#[tauri::command]
async fn frontend_ready(
    state: tauri::State<'_, AppState>,
) -> Result<LaunchResult, AppError> {
    let mut targets = Vec::new();
    let mut errors = Vec::new();
    // (1) 起動時指定 (launch route)。このプロセスの cwd 基準で解決する (None)。
    //     std Mutex は await をまたいで保持しない (take だけして即解放)。
    let launch = state.launch_route.lock().unwrap().take();
    if let Some(route) = launch {
        match state.resolve_route_target(&route, None).await {
            Ok(target) => targets.push(target),
            // 起動時指定が失敗したら、GUI 起動では stderr が見えないためフロントへ
            // 返してトーストで知らせる (握り潰さない)。他の対象・起動は止めない。
            Err(e) => errors.push(e.to_string()),
        }
    }
    // (2) ready にして、それまでにキューされた対象を drain する。
    //     ready 設定と drain を 1 つのロックで行い、dispatch_route の
    //     「ready 判定 → push/emit」と直列化する (取りこぼし・二重配送を防ぐ)。
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

/// `file` をシンボリックリンク解決込みで canonicalize し、`base` (これも
/// canonicalize したもの) の配下に留まることを確かめる。保存領域外の実体を指す
/// リンクを弾く多重防御。開く対象は既存ファイルのはずなので、canonicalize
/// できない (存在しない等) 場合は拒否する。
fn verify_within_dir(base: &std::path::Path, file: &std::path::Path) -> Result<(), AppError> {
    let canonical_base = base.canonicalize().map_err(|e| {
        AppError::QueryFile(format!("Cannot resolve the query files directory: {e}"))
    })?;
    let canonical_file = file
        .canonicalize()
        .map_err(|e| AppError::QueryFile(format!("Cannot open the file: {e}")))?;
    if !canonical_file.starts_with(&canonical_base) {
        return Err(AppError::QueryFile(
            "The file resolves outside the query files directory".into(),
        ));
    }
    Ok(())
}

/// 実行中に受け取ったルート (deep link / CLI サブコマンド) を解決し、フロントへ
/// イベントで届ける。解決は設定の読み取りを伴い async なので、別タスクで行う。
/// 成功なら `open-query-file` に OpenTarget を、失敗なら `open-query-file-error` に
/// エラーメッセージを載せる。
fn dispatch_route(app: &tauri::AppHandle, route: router::Route, cwd: Option<PathBuf>) {
    use tauri::{Emitter, Manager};
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        match state.resolve_route_target(&route, cwd).await {
            Ok(target) => {
                // フロントの listener が未登録 (起動中) なら取りこぼすため、ready で
                // なければキューに積む (frontend_ready が drain する)。ready 判定と
                // push を 1 ロックで行い、frontend_ready の ready 設定 + drain と直列化。
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
                // 成功対象と同様、listener 未登録 (起動中) なら emit しても取りこぼす。
                // ready でなければエラーもキューに積み、frontend_ready で drain させる。
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

/// アプリのメニューバーを組み立てる。
///
/// macOS のアプリメニュー (QueryFolio) は NSApplication がメインメニュー設置時の
/// 内容で確定させるため、後から項目を insert しても反映されない。そのため
/// tauri のデフォルトメニューを流用せず、アプリメニューを含めて丸ごと自前で組み、
/// Builder::menu で最初の設置時から渡す。設定変更時はこの関数で組み直す。
///
/// 「View override config yaml (Copy only)」は config_override_command が
/// 設定されている時だけ出す。
///
/// 構成は tauri の `Menu::default` を踏襲する (アプリメニュー / View は macOS のみ、
/// File の quit は macOS 以外のみ)。アプリメニューを持たないプラットフォームでは
/// 設定編集の項目を Config サブメニューの先頭に置く。
/// About ダイアログに出すメタ情報 (tauri の Menu::default と同じ内容)。
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

    #[cfg(target_os = "macos")]
    let app_menu = {
        use tauri::menu::PredefinedMenuItem;

        let package_info = app.package_info();
        let mut builder = SubmenuBuilder::new(app, package_info.name.clone())
            .item(&PredefinedMenuItem::about(
                app,
                None,
                Some(about_metadata(app)),
            )?)
            .separator()
            .item(&edit_config_item);
        if show_source_item {
            builder = builder.item(&edit_source_item);
        }
        builder
            .separator()
            .services()
            .separator()
            .hide()
            .hide_others()
            .separator()
            .quit()
            .build()?
    };

    let file_menu = {
        let builder = SubmenuBuilder::new(app, "File").close_window();
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
    // Window / Help は tauri と同じ固定 ID で作る。macOS の init_app_menu は
    // この ID でメニューを探して NSApp の windowsMenu / helpMenu に登録するため、
    // ID が無いとウインドウ一覧やヘルプ検索が付かなくなる
    let window_menu = {
        let builder = SubmenuBuilder::with_id(app, tauri::menu::WINDOW_SUBMENU_ID, "Window")
            .minimize()
            .maximize();
        #[cfg(target_os = "macos")]
        let builder = builder.separator();
        builder.close_window().build()?
    };
    // tauri のデフォルトメニュー同様、macOS では中身を持たない
    // (About はアプリメニュー側にあり、システムがヘルプ検索を足す)
    let help_menu = {
        let builder = SubmenuBuilder::with_id(app, tauri::menu::HELP_SUBMENU_ID, "Help");
        #[cfg(not(target_os = "macos"))]
        let builder = builder.about(Some(about_metadata(app)));
        builder.build()?
    };

    let reload_item = MenuItemBuilder::with_id("reload_config_file", "Reload config file")
        .accelerator("CmdOrCtrl+R")
        .build(app)?;
    let reveal_item =
        MenuItemBuilder::with_id("reveal_config_folder", "Reveal config folder").build(app)?;
    let config_menu = {
        #[allow(unused_mut)]
        let mut builder = SubmenuBuilder::new(app, "Config");
        // アプリメニューが無いプラットフォームでは設定編集もここに置く
        #[cfg(not(target_os = "macos"))]
        {
            builder = builder.item(&edit_config_item);
            if show_source_item {
                builder = builder.item(&edit_source_item);
            }
            builder = builder.separator();
        }
        builder.item(&reload_item).item(&reveal_item).build()?
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

/// 設定を読み直した後にメニューを組み直す。
/// config_override_command の有無が変わるとコピー用ビューの項目の要否も変わるため。
fn rebuild_menu(app: &tauri::AppHandle) {
    match build_menu(app).and_then(|menu| app.set_menu(menu)) {
        Ok(_) => {}
        Err(e) => eprintln!("[menu] failed to rebuild the menu: {e}"),
    }
}

/// config.yml (無ければ設定フォルダ) を Finder 等のファイルマネージャで表示する。
fn reveal_config_folder() -> Result<(), AppError> {
    let target = match config::existing_config_path()? {
        Some(path) => path,
        None => config::app_config_dir()?,
    };
    tauri_plugin_opener::reveal_item_in_dir(&target)
        .map_err(|e| AppError::Config(format!("Failed to reveal {}: {e}", target.display())))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri::Emitter;

    tauri::Builder::default()
        // single-instance は最初に登録する (プラグインは登録順に走る)。
        // deep-link feature 有効: 2 個目の起動の argv に含まれる queryfolio:// URL は
        // 実行中インスタンスの deep-link プラグインへ転送され on_open_url が発火する。
        // ここでは追加で (1) ウインドウを前面化し (2) CLI サブコマンド
        // (queryfolio open <path>) を処理する (URL 引数は上記で処理済みなので無視)。
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            use tauri::Manager;
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
            // cwd は 2 個目の起動元ディレクトリ。CLI の相対パスをこの基準で解決する。
            if let Some(route) = router::route_from_cli_args(&argv) {
                dispatch_route(app, route, Some(PathBuf::from(cwd)));
            }
        }))
        // queryfolio:// スキームの deep link。macOS はネイティブに URL を受け取り、
        // Linux/Windows は上の single-instance (deep-link feature) 経由で受け取る。
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        // 結果テーブルの Export でネイティブ保存ダイアログを開くのに使う
        .plugin(tauri_plugin_dialog::init())
        // 終了時のウインドウサイズ・位置を保存し、起動時に復元する
        .plugin(tauri_plugin_window_state::Builder::default().build())
        // setup で set_menu すると、それより前に設置される tauri のデフォルト
        // メニューで macOS のアプリメニューが確定してしまうため、ここで渡す
        .menu(build_menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "reload_config_file" => {
                // 再読込はフロントの状態 (選択・未保存編集) と連動するため、
                // イベントで通知してフロント側の reloadConnections に任せる
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
            "view_override_config" => {
                if let Err(e) = app.emit("menu-view-override-config", ()) {
                    eprintln!("[menu] failed to emit view override config event: {e}");
                }
            }
            _ => {}
        })
        .manage(AppState::default())
        .setup(|app| {
            use tauri::Manager;
            use tauri_plugin_deep_link::DeepLinkExt;
            // dev / Linux 実行向けにスキームを実行時登録する (macOS は bundle 時に
            // Info.plist へ登録される)。ベストエフォート: 失敗しても起動は続ける。
            if let Err(e) = app.deep_link().register_all() {
                eprintln!("[router] failed to register deep link schemes: {e}");
            }
            // 実行中に URL を開かれた時のハンドラ (macOS ネイティブ / Linux 転送)。
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    match router::parse_uri(url.as_str()) {
                        // deep link の URL は絶対パス想定なので cwd は不要 (None)
                        Ok(route) => dispatch_route(&handle, route, None),
                        Err(e) => eprintln!("[router] ignoring URL {url}: {e}"),
                    }
                }
            });
            // 起動時に指定されたルートを控える (フロントが frontend_ready で取り出す)。
            // 優先度: deep link 起動 (macOS: get_current が URL を返す) → CLI サブコマンド。
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
            ai_explain_plan,
            ai_explain_sql,
            ai_fix_sql,
            get_config_info,
            ensure_config_file,
            read_config_file,
            write_config_file,
            read_override_config_yaml,
            write_export_file,
            frontend_ready,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
