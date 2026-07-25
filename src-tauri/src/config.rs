use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// config_override_command の実行タイムアウト (秒)。
/// 1Password 等の認証待ちで無限ハングするとコマンド呼び出しが固まるため必須。
const SOURCE_COMMAND_TIMEOUT_SECS: u64 = 60;

/// ~/.config/queryfolio ディレクトリを返す。
pub fn app_config_dir() -> Result<PathBuf, AppError> {
    let home = dirs::home_dir()
        .ok_or_else(|| AppError::Config("Could not determine the home directory".into()))?;
    Ok(home.join(".config").join("queryfolio"))
}

/// パス文字列の先頭の ~ をホームディレクトリに展開する。
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// 初回起動時に自動作成する config.yml のテンプレート。
/// そのままで有効な設定 (接続 0 件) としてパースできる内容にする。
const CONFIG_TEMPLATE: &str = r#"# QueryFolio config file
# See config.example.yaml in the repository for the full format.
# https://github.com/ytyng/queryfolio

# Connection definitions.
#
# sql_servers:
#   - name: local-sqlite
#     description: "Local SQLite file"
#     engine: sqlite
#     schema: ~/data/example.sqlite3
#   - name: dev-postgres
#     engine: postgres
#     host: localhost
#     port: 5432
#     schema: development_db
#     user: dev_user
#     password: your_password

sql_servers: []

# Keep secrets out of this file by fetching them from elsewhere.
#
# config_override_command runs a command whose stdout must be YAML, and merges
# that YAML over this file. The merge is recursive for mappings; scalars and
# lists (including sql_servers) are replaced wholesale. Any key can be
# overridden this way, not just sql_servers.
#
# config_override_command: op read "op://development/queryfolio/config-yaml"

# Where query files are stored (default: ~/.config/queryfolio/sqlfiles)
# sqlfiles_dir: ~/queries
#
# Query files live under <sqlfiles_dir>/<folder>/<name>.sql. The per-connection
# folder is <host>_<engine>_<schema>_<user> by default (the connection name is
# not used). Set `folder_name:` on a server to pin the folder explicitly.
"#;

/// 実在する設定ファイルのパスを返す。config.yml / config.yaml のどちらも
/// 無ければ None。
pub fn existing_config_path() -> Result<Option<PathBuf>, AppError> {
    let dir = app_config_dir()?;
    for name in ["config.yml", "config.yaml"] {
        let path = dir.join(name);
        if path.exists() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// config.yml / config.yaml が無ければテンプレートを作成する。
/// 作成した場合は Some(作成パス) を返す。既に存在する場合と、
/// QUERYFOLIO_CONFIG_YAML 環境変数で上書き中の場合は None。
pub fn ensure_config_file() -> Result<Option<String>, AppError> {
    let env_override = std::env::var("QUERYFOLIO_CONFIG_YAML")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if env_override {
        return Ok(None);
    }
    ensure_config_file_in(&app_config_dir()?)
}

/// dir 内の設定ファイルのパス。config.yml を優先し、無ければ config.yaml、
/// どちらも無ければ config.yml のパスを返す。
fn config_path_in(dir: &std::path::Path) -> PathBuf {
    let yml = dir.join("config.yml");
    if yml.exists() {
        return yml;
    }
    let yaml = dir.join("config.yaml");
    if yaml.exists() {
        return yaml;
    }
    yml
}

fn ensure_config_file_in(dir: &std::path::Path) -> Result<Option<String>, AppError> {
    let yml = dir.join("config.yml");
    let yaml = dir.join("config.yaml");
    if yml.exists() || yaml.exists() {
        // 既存ファイルが緩い権限 (umask 依存の 644 等) で作られていた場合に
        // 所有者のみ (600) へ是正する。config には接続パスワードや SSH 鍵の
        // パスフレーズが平文で入り得るため。是正の主経路は AppConfig::load
        // (起動時の build_menu から走り、フロントの ensure_config_file より
        // 早い) だが、ここでも実施して設定エディタ (read_config_file_in) の
        // 経路や旧バージョン・手動作成のファイルも確実に救済する。
        #[cfg(unix)]
        {
            tighten_config_permissions(&yml)?;
            tighten_config_permissions(&yaml)?;
        }
        return Ok(None);
    }
    std::fs::create_dir_all(dir)?;
    // 作成時からパーミッションを 600 で固定する。std::fs::write だと umask
    // 依存 (通常 644) で作られ、書き込み直後に同一マシンの他ユーザーへ中身を
    // 読まれる隙ができる。create_new (O_EXCL) にすることで、上の exists 判定
    // 後に別プロセスが作った config.yml を truncate してしまう競合も防ぐ。
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&yml)
        {
            Ok(mut file) => {
                // mode() は umask で更に絞られるだけだが、異常な umask で所有者
                // ビットが落ちる事態に備え、開いた fd に対して明示的にも設定する。
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                file.write_all(CONFIG_TEMPLATE.as_bytes())?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // exists 判定後に別プロセスが作成した。上書きせず権限だけ是正する。
                tighten_config_permissions(&yml)?;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    }
    #[cfg(not(unix))]
    std::fs::write(&yml, CONFIG_TEMPLATE)?;
    Ok(Some(yml.display().to_string()))
}

/// 既存の設定ファイルに group / other の許可ビットが立っていたら、
/// 所有者のみ (600) へ絞る。存在しなければ何もしない。macOS では staff
/// グループが全ローカルユーザーで共有されるため、640 でも他ユーザーへ
/// 漏れる。owner-only まで絞るのが安全。
#[cfg(unix)]
fn tighten_config_permissions(path: &std::path::Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        // 無ければ何もしない (別拡張子や、判定と stat の間に消えた場合)。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        // それ以外の I/O エラーは握り潰さず伝播する。黙って return すると
        // 是正できていないのに成功したように見えてしまうため。
        Err(e) => return Err(e.into()),
    };
    // 通常ファイルにのみ適用する。config.yml がディレクトリ (やその symlink)
    // だと 600 にした瞬間に owner の検索ビット (x) が落ちてアクセス不能になり、
    // その後の設定読み込みが失敗する。ファイル以外は触らない。
    if !meta.is_file() {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// SSH トンネル設定。sql-agent-mcp-server の config.yaml と互換。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTunnelConfig {
    /// SSH host. Not required when `ssh_config` is set (the host is then taken
    /// from the ~/.ssh/config alias by the system ssh client).
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// SSH user. Not required when `ssh_config` is set (resolved from
    /// ~/.ssh/config).
    #[serde(default)]
    pub user: String,
    /// queryfolio extension: when set, delegate the tunnel to the system `ssh`
    /// client using this ~/.ssh/config Host alias (`ssh -N -L`). This enables
    /// ProxyJump / multi-hop tunnels and full ssh_config resolution
    /// (HostName / User / Port / ProxyJump). When set, the libssh2 fields
    /// (host / user / password / private_key_* / identity_agent) are ignored;
    /// authentication and host-key checking are handled entirely by OpenSSH.
    #[serde(default)]
    pub ssh_config: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub private_key_path: Option<String>,
    #[serde(default)]
    pub private_key_passphrase: Option<String>,
    /// queryfolio extension: the ssh-agent socket to use for agent
    /// authentication (equivalent to OpenSSH's IdentityAgent). Use "none" to
    /// disable the agent. When omitted, the agent socket is resolved from
    /// ~/.ssh/config (IdentityAgent) and then SSH_AUTH_SOCK. This lets a GUI
    /// launch reach an agent it did not inherit in its environment (e.g. the
    /// 1Password SSH agent).
    #[serde(default)]
    pub identity_agent: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}

/// 接続先サーバー設定。sql-agent-mcp-server の config.yaml と互換。
/// queryfolio では engine: sqlite を拡張し、schema を DB ファイルパスとして扱う。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// queryfolio 独自拡張: クエリファイルの保存フォルダ名を明示する。
    /// 省略時は <host>_<engine>_<schema>_<user> から組み立てる
    /// (name はフォルダ名には使わない)。sqlfiles_folder_name を参照。
    #[serde(default)]
    pub folder_name: Option<String>,
    pub engine: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub ssh_tunnel: Option<SshTunnelConfig>,
    /// queryfolio 独自拡張: true の場合、HTTP 系エンジン (elasticsearch) の
    /// 接続に https を使う。省略時 false。SQL 系エンジンでは無視される。
    /// dynamodb ではエンドポイント上書き (host 指定) 時のスキームに使う。
    #[serde(default)]
    pub tls: bool,
    /// queryfolio 独自拡張 (dynamodb 用): aws-config に渡す AWS プロファイル名
    /// (~/.aws/config / credentials)。省略時は既定の credentials chain
    /// (環境変数 → default プロファイル → IMDS)。他のエンジンでは無視される。
    #[serde(default)]
    pub aws_profile: Option<String>,
    /// queryfolio 独自拡張: true の場合、行を返さない文 (INSERT / UPDATE /
    /// DELETE / DDL 等) の実行を拒否する。省略時 false。
    /// SELECT に副作用のある関数 (nextval 等) までは防げない事故防止ガード。
    #[serde(default)]
    pub readonly: bool,
    /// queryfolio 独自拡張: true の場合、危険な文 (WHERE 無しの UPDATE /
    /// DELETE、DROP / TRUNCATE 等) の実行を許可する。省略時 false で、
    /// これらの文は誤操作による全行破壊・テーブル消失を防ぐため拒否される。
    /// true にしても、フロントエンドは実行前に確認を求める。
    #[serde(default)]
    pub allow_dangerous_statements: bool,
    /// queryfolio 独自拡張: 接続一覧での表示グループ名。
    /// sql_servers のグループエントリ (group_name + sql_servers) に
    /// 属するサーバーへ parse_server_entries が設定する。
    /// サーバーエントリ直下の group_name: はグループエントリの検証
    /// (空チェック・未知キー拒否) を迂回するため受け付けない (無視される)。
    #[serde(default, skip_deserializing)]
    pub group_name: Option<String>,
}

/// 文字列の安定した短いハッシュ (FNV-1a 64bit の先頭 8 hex)。
/// AWS アクセスキー ID のような「そのまま出したくないが接続の区別には使いたい」
/// 識別子をフォルダ名に落とすために使う (非可逆・依存クレート不要)。
fn stable_hash_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", (hash >> 32) as u32)
}

/// フォルダ名としてファイルシステム上安全になるようサニタイズする。
/// パス区切り (/ \) や NUL を _ に置換し、先頭ドット (不可視/相対) を避ける。
/// query_files::validate_component が拒否する文字を事前に潰しておく。
fn sanitize_folder_component(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            _ => c,
        })
        .collect();
    s = s.trim().to_string();
    if s.is_empty() {
        return "_".to_string();
    }
    if s.starts_with('.') {
        s.insert(0, '_');
    }
    s
}

impl ServerConfig {
    /// クエリファイルの保存フォルダ名を返す。
    /// folder_name が設定されていればそれを使い、無ければ
    /// <host>_<engine>_<schema>_<user> を組み立てる (name は使わない)。
    /// パス要素として安全になるよう区切り文字等はサニタイズする。
    pub fn sqlfiles_folder_name(&self) -> String {
        if let Some(folder) = self.folder_name.as_deref() {
            let folder = folder.trim();
            if !folder.is_empty() {
                return sanitize_folder_component(folder);
            }
        }
        // dynamodb の user は AWS アクセスキー ID (資格情報の識別子) なので
        // フォルダ名にそのまま出さない。代わりに非機密の識別子
        // (aws_profile 名、静的キーなら短いハッシュ) で接続を区別する —
        // 同一リージョンでプロファイル/キーだけ違う 2 接続が同じフォルダに
        // 落ちてクエリファイルが混ざるのを防ぐ
        let dynamodb_discriminator;
        let user = if self.engine.eq_ignore_ascii_case("dynamodb") {
            if let Some(profile) =
                self.aws_profile.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                profile
            } else if let Some(user) =
                self.user.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                dynamodb_discriminator = format!("key-{}", stable_hash_hex(user));
                &dynamodb_discriminator
            } else {
                ""
            }
        } else {
            self.user.as_deref().unwrap_or("")
        };
        let joined = [
            self.host.as_deref().unwrap_or(""),
            self.engine.as_str(),
            self.schema.as_deref().unwrap_or(""),
            user,
        ]
        .join("_");
        sanitize_folder_component(&joined)
    }
}

/// フロントエンドに渡す SSH トンネル情報。パスワードや鍵等の機密は含めない。
#[derive(Debug, Clone, Serialize)]
pub struct SshTunnelInfo {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// queryfolio extension: the ~/.ssh/config Host alias when the tunnel is
    /// delegated to the system ssh client. null in the libssh2 mode (host /
    /// port / user are used instead).
    pub ssh_config: Option<String>,
}

impl From<&SshTunnelConfig> for SshTunnelInfo {
    fn from(tunnel: &SshTunnelConfig) -> Self {
        Self {
            host: tunnel.host.clone(),
            port: tunnel.port,
            user: tunnel.user.clone(),
            ssh_config: tunnel
                .ssh_config
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }
}

/// フロントエンドに渡す接続先情報。パスワード等の機密は含めない。
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionInfo {
    pub name: String,
    pub description: Option<String>,
    pub engine: String,
    pub has_ssh_tunnel: bool,
    /// 接続先ホスト (未設定なら null)
    pub host: Option<String>,
    /// 接続先ポート (未設定なら null)
    pub port: Option<u16>,
    /// 接続ユーザー (未設定なら null)
    pub user: Option<String>,
    /// 設定上のデフォルト database (スキーマ)
    pub schema: Option<String>,
    /// SSH トンネル情報 (機密を除く)。トンネル未使用なら null
    pub ssh_tunnel: Option<SshTunnelInfo>,
    /// 読み取り専用接続 (書き込み系の文の実行を拒否する)
    pub readonly: bool,
    /// 危険な文 (WHERE 無し UPDATE/DELETE、DROP/TRUNCATE 等) の実行を許可する。
    /// フロントエンドは true の接続でも実行前に確認を求める
    pub allow_dangerous_statements: bool,
    /// 接続一覧での表示グループ名 (グループ未所属なら null)
    pub group_name: Option<String>,
    /// エンジンの能力宣言 (エディタ言語・ファイル拡張子・UI の出し分け)。
    /// フロントはエンジン名ではなくこれで UI を出し分ける。
    pub capabilities: crate::engines::EngineCapabilities,
}

impl From<&ServerConfig> for ConnectionInfo {
    fn from(server: &ServerConfig) -> Self {
        Self {
            name: server.name.clone(),
            description: server.description.clone(),
            engine: server.engine.clone(),
            has_ssh_tunnel: server.ssh_tunnel.is_some(),
            host: server.host.clone(),
            port: server.port,
            user: server.user.clone(),
            schema: server.schema.clone(),
            ssh_tunnel: server.ssh_tunnel.as_ref().map(SshTunnelInfo::from),
            readonly: server.readonly,
            allow_dangerous_statements: server.allow_dangerous_statements,
            group_name: server.group_name.clone(),
            capabilities: crate::engines::capabilities_for_name(&server.engine),
        }
    }
}

/// 設定を外部コマンドの YAML で上書きするためのトップレベルキー。
/// 値はコマンド文字列で、その stdout (YAML) を設定全体へ再帰マージする。
pub const CONFIG_OVERRIDE_COMMAND_KEY: &str = "config_override_command";

/// フロントエンドの情報表示用。設定の解決結果 (機密を含まない)。
#[derive(Debug, Serialize)]
pub struct ConfigInfo {
    pub config_path: String,
    pub config_exists: bool,
    pub source: String,
    pub sqlfiles_dir: String,
}

/// ~/.config/queryfolio/config.yml (無ければ config.yaml) のパース結果。
///
/// トップレベルキー:
/// - sql_servers: サーバー定義リスト
/// - sql_server_templates: 接続情報の雛形
/// - sqlfiles_dir: クエリファイル保存ディレクトリ (任意)
/// - config_override_command: 設定を上書きする YAML を取得するコマンド (任意)
///
/// `load` はローカルのファイルだけを読む (同期)。`load_merged` は加えて
/// config_override_command を実行し、取得 YAML を再帰マージした設定を返す。
/// コマンド実行は 1Password 等で数秒かかり Touch ID を要求することもあるため、
/// 呼び出し側 (AppState) でセッションキャッシュすること。
pub struct AppConfig {
    doc: serde_yaml::Mapping,
    /// 読み込んだファイルのパス。QUERYFOLIO_CONFIG_YAML 環境変数由来なら None
    source_path: Option<PathBuf>,
    /// load_merged で実際に適用した config_override_command。
    /// マージ後の doc からはキーを落とすため、表示用にここへ退避する。
    applied_override: Option<String>,
}

impl AppConfig {
    /// 設定をロードする。
    /// QUERYFOLIO_CONFIG_YAML 環境変数があればそれを設定ファイルの内容として
    /// 扱う (開発・テスト用オーバーライド)。無ければ config.yml / config.yaml を読む。
    pub fn load() -> Result<Self, AppError> {
        if let Ok(yaml) = std::env::var("QUERYFOLIO_CONFIG_YAML") {
            if !yaml.trim().is_empty() {
                let doc = parse_mapping(&yaml, "env QUERYFOLIO_CONFIG_YAML")?;
                return Ok(Self {
                    doc,
                    source_path: None,
                    applied_override: None,
                });
            }
        }

        let path = Self::find_config_path()?;
        if !path.exists() {
            return Err(AppError::Config(format!(
                "Config file not found. Create {} (see config.example.yaml)",
                path.display()
            )));
        }
        // 読み込む前に、緩い権限で置かれた設定ファイルを所有者のみへ是正する。
        // build_menu からの load はフロントの ensure_config_file より先に走る
        // ため、ここを是正の主経路にする (config は平文の接続パスワードや SSH
        // 鍵パスフレーズを含み得る)。
        #[cfg(unix)]
        tighten_config_permissions(&path)?;
        let text = std::fs::read_to_string(&path)?;
        let doc = parse_mapping(&text, &path.display().to_string())?;
        Ok(Self {
            doc,
            source_path: Some(path),
            applied_override: None,
        })
    }

    /// ローカル設定を読み、`config_override_command` があればそれを実行して
    /// 取得 YAML を再帰マージした設定を返す。
    ///
    /// マージは取得 YAML 側が優先。マッピング同士は再帰的に混ぜ、
    /// スカラー・シーケンス (sql_servers を含む) は丸ごと置き換える
    /// (リストの要素単位マージは、どれが「同じ項目」かを決められないため行わない)。
    pub async fn load_merged() -> Result<Self, AppError> {
        let mut config = Self::load()?;
        let Some(command) = config.override_command()? else {
            return Ok(config);
        };
        let yaml = run_source_command(&command).await?;
        let overrides = parse_mapping(&yaml, &format!("{CONFIG_OVERRIDE_COMMAND_KEY}: {command}"))?;
        merge_mapping(&mut config.doc, &overrides);
        // 取得 YAML 側が config_override_command を持っていても再帰取得はしない。
        // 適用済みであることを表すためキー自体を落とす (info の表示はローカル
        // 側の値を使うため、ここで消しても表示には影響しない)。
        config.doc.remove(CONFIG_OVERRIDE_COMMAND_KEY);
        config.applied_override = Some(command);
        Ok(config)
    }

    /// config.yml を優先し、無ければ config.yaml、どちらも無ければ
    /// デフォルトの config.yml のパスを返す。
    fn find_config_path() -> Result<PathBuf, AppError> {
        Ok(config_path_in(&app_config_dir()?))
    }

    /// LIMIT 未指定の SELECT に自動付与する行数上限。
    /// 省略時は 500。0 を指定すると無効。
    pub fn default_limit(&self) -> u64 {
        self.doc
            .get("default_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(500)
    }

    /// クエリファイルの保存ディレクトリを解決する。
    pub fn resolve_sqlfiles_dir(&self) -> Result<PathBuf, AppError> {
        match self.doc.get("sqlfiles_dir").and_then(|v| v.as_str()) {
            Some(dir) if !dir.trim().is_empty() => Ok(expand_tilde(dir)),
            _ => Ok(app_config_dir()?.join("sqlfiles")),
        }
    }

    /// 設定を上書きする YAML を取得するコマンド (未設定なら None)。
    ///
    /// キーが存在するのに文字列でない・空文字の場合はエラーにする。
    /// 黙って「未設定」に倒すと、オーバーライド側の接続情報や readonly が
    /// 適用されないままローカル設定で動いてしまい、事故に気付けないため。
    pub fn override_command(&self) -> Result<Option<String>, AppError> {
        let Some(value) = self.doc.get(CONFIG_OVERRIDE_COMMAND_KEY) else {
            return Ok(None);
        };
        let command = value.as_str().map(str::trim).ok_or_else(|| {
            AppError::Config(format!("{CONFIG_OVERRIDE_COMMAND_KEY} must be a string"))
        })?;
        if command.is_empty() {
            return Err(AppError::Config(format!(
                "{CONFIG_OVERRIDE_COMMAND_KEY} is empty"
            )));
        }
        Ok(Some(command.to_string()))
    }

    /// トップレベルの `ai:` セクション (未検証の生値)。
    /// load_merged 済みなら取得 YAML 側の ai が反映されている。
    pub fn ai(&self) -> Option<serde_yaml::Value> {
        self.doc.get("ai").cloned()
    }

    /// 接続サーバー一覧を解決する。
    /// 取得を伴わない (config_override_command の適用は load_merged で済んでいる)。
    pub fn resolve_servers(&self) -> Result<Vec<ServerConfig>, AppError> {
        let servers = self
            .doc
            .get("sql_servers")
            .ok_or_else(|| AppError::Config("The config has no sql_servers key".into()))?
            .as_sequence()
            .cloned()
            .ok_or_else(|| {
                // 旧方式 (sql_servers: {command|env|file: ...}) からの移行案内。
                // 単に「リストであるべき」とだけ言われても、どう直すか分からない
                AppError::Config(format!(
                    "sql_servers must be a list of server definitions. \
                     Source declarations (command / env / file) under sql_servers were \
                     removed; use the top-level {CONFIG_OVERRIDE_COMMAND_KEY} instead \
                     (note: it runs without a shell, so use an absolute path, e.g. \
                     `{CONFIG_OVERRIDE_COMMAND_KEY}: /bin/cat /Users/you/secrets/servers.yaml`)"
                ))
            })?;
        let templates = self
            .doc
            .get("sql_server_templates")
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default();
        parse_server_entries(&servers, &templates, "config")
    }

    /// 情報表示用のサマリを返す (機密を含まない)。
    pub fn info(&self) -> Result<ConfigInfo, AppError> {
        let config_path = match &self.source_path {
            Some(path) => path.display().to_string(),
            None => "(env QUERYFOLIO_CONFIG_YAML)".to_string(),
        };
        // マージ後は doc からキーを落としているため applied_override を先に見る。
        // load (マージ前) の設定でも表示できるよう doc 側もフォールバックで見る。
        let source = match self
            .applied_override
            .clone()
            .or_else(|| self.override_command().ok().flatten())
        {
            Some(command) => format!("{CONFIG_OVERRIDE_COMMAND_KEY}: {command}"),
            None => "inline".to_string(),
        };
        Ok(ConfigInfo {
            config_path,
            config_exists: true,
            source,
            sqlfiles_dir: self.resolve_sqlfiles_dir()?.display().to_string(),
        })
    }
}

/// 設定の解決に失敗した時の情報表示用サマリ (ファイルが無い / YAML が壊れて
/// いる / 取得コマンドが失敗した場合)。フロントが常に何かを表示できるよう、
/// エラー文言を source に載せて返す。
pub fn config_info_error(error: &AppError) -> ConfigInfo {
    // 失敗には「ファイルが無い」以外に「存在するが YAML が壊れている」場合が
    // あるため、存在判定はパースの成否と独立に行う
    let (config_path, config_exists) = match AppConfig::find_config_path() {
        Ok(path) => (path.display().to_string(), path.exists()),
        Err(_) => (String::new(), false),
    };
    ConfigInfo {
        config_path,
        config_exists,
        source: format!("(error: {error})"),
        sqlfiles_dir: String::new(),
    }
}

/// QUERYFOLIO_CONFIG_YAML 環境変数で設定が上書きされているか。
/// 上書き中は編集対象のファイルが存在しないため、エディタから編集できない。
fn config_env_override() -> bool {
    std::env::var("QUERYFOLIO_CONFIG_YAML")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// 設定エディタ用に config.yml の中身を読む。
/// ファイルがまだ無い場合はテンプレートを作成してから読む。
pub fn read_config_file() -> Result<String, AppError> {
    if config_env_override() {
        return Err(AppError::Config(
            "The config is overridden by QUERYFOLIO_CONFIG_YAML, so there is no file to edit"
                .into(),
        ));
    }
    read_config_file_in(&app_config_dir()?)
}

fn read_config_file_in(dir: &std::path::Path) -> Result<String, AppError> {
    ensure_config_file_in(dir)?;
    Ok(std::fs::read_to_string(config_path_in(dir))?)
}

/// 設定エディタからの保存。YAML として妥当なことを確認してから書き込む。
///
/// 書き込みは一時ファイル + rename で行い、途中で失敗しても既存の設定を
/// 半端な内容で壊さないようにする。
pub fn write_config_file(content: &str) -> Result<String, AppError> {
    if config_env_override() {
        return Err(AppError::Config(
            "The config is overridden by QUERYFOLIO_CONFIG_YAML, so it cannot be saved".into(),
        ));
    }
    write_config_file_in(&app_config_dir()?, content)
}

fn write_config_file_in(dir: &std::path::Path, content: &str) -> Result<String, AppError> {
    // 壊れた YAML をそのまま保存すると次回起動で接続一覧を失うため、
    // 保存前にマッピングとしてパースできることを確認する
    parse_mapping(content, "the edited config")?;

    std::fs::create_dir_all(dir)?;
    let path = config_path_in(dir);

    // config は接続パスワードや SSH 鍵パスフレーズを平文で含み得るため、
    // 常に所有者のみ (600) で書く。既存が 644/640 等でも 600 へ絞り、
    // ensure_config_file_in / AppConfig::load の是正方針と揃える (既存権限を
    // 引き継ぐと macOS の共有 staff グループ経由で他ユーザーへ漏れ得る)。
    #[cfg(unix)]
    let mode = 0o600;

    let temp = path.with_extension("yml.tmp");
    // 作成時からパーミッションを指定する。書いてから set_permissions すると、
    // その間だけ umask 依存 (通常 644) の権限で中身が置かれ、パスワードを
    // 含む設定を同一マシンの他ユーザーに読まれる隙ができる
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&temp)?;
        // mode は新規作成時にしか効かないため、前回の中断等で temp が
        // 残っていた場合に備えて明示的にも設定する
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&temp, content)?;
    std::fs::rename(&temp, &path)?;
    Ok(path.display().to_string())
}

/// config_override_command が設定されているか。
/// メニュー項目の出し分けに使う。設定が読めない場合は false。
pub fn has_config_override_command() -> bool {
    AppConfig::load()
        .map(|c| c.override_command().unwrap_or(None).is_some())
        .unwrap_or(false)
}

/// config_override_command を実行して、取得した生の YAML を返す。
/// コピー用ビュー用 (表示先では編集できるが保存はしない)。未設定ならエラーにする。
///
/// AppState のマージ済み設定キャッシュは**意図的に経由しない**。このビューは
/// 保管場所 (1Password 等) の現在値を確認・整形してコピーする用途なので、
/// 起動時に取得したキャッシュではなく毎回最新を取りに行く。開くたびに
/// コマンドが 1 回走る (1Password なら都度認証が要る場合がある)。
pub async fn fetch_override_config_yaml() -> Result<String, AppError> {
    let config = AppConfig::load()?;
    match config.override_command()? {
        Some(command) => run_source_command(&command).await,
        None => Err(AppError::Config(format!(
            "The config has no {CONFIG_OVERRIDE_COMMAND_KEY}"
        ))),
    }
}

/// 取得 YAML (over) をローカル設定 (base) へ再帰的にマージする。over 側が優先。
/// 値がどちらもマッピングの時だけ中へ入って混ぜ、それ以外 (スカラー・
/// シーケンス) は over で丸ごと置き換える。sql_servers のようなリストを
/// 要素単位でマージしないのは、どの要素が「同じ項目」かを決める安定した
/// 同一性が無いため (name 一致で混ぜると意図しない部分適用が起きる)。
fn merge_mapping(base: &mut serde_yaml::Mapping, over: &serde_yaml::Mapping) {
    for (key, over_value) in over {
        match (base.get_mut(key), over_value) {
            (Some(serde_yaml::Value::Mapping(base_map)), serde_yaml::Value::Mapping(over_map)) => {
                merge_mapping(base_map, over_map);
            }
            _ => {
                base.insert(key.clone(), over_value.clone());
            }
        }
    }
}

fn parse_mapping(yaml_text: &str, source: &str) -> Result<serde_yaml::Mapping, AppError> {
    let doc: serde_yaml::Value = serde_yaml::from_str(yaml_text)
        .map_err(|e| AppError::Config(format!("Failed to parse YAML from {source}: {e}")))?;
    doc.as_mapping().cloned().ok_or_else(|| {
        AppError::Config(format!("{source} is not a YAML mapping"))
    })
}

/// sql_servers のリスト項目をパースする。項目は次のどちらか:
/// - サーバー定義そのもの
/// - グループエントリ (group_name + ネストした sql_servers リスト)。
///   ネストしたサーバーへフラット化し、各サーバーの group_name に記録する。
///   グループの中にさらにグループを書く再帰は禁止 (深さ 1 まで)。
fn parse_server_entries(
    servers: &[serde_yaml::Value],
    templates: &[serde_yaml::Value],
    source: &str,
) -> Result<Vec<ServerConfig>, AppError> {
    let mut result = Vec::new();
    for entry_value in servers {
        let entry = entry_value.as_mapping().ok_or_else(|| {
            AppError::Config(format!("A sql_servers entry in {source} is not a mapping"))
        })?;
        if !entry.contains_key("sql_servers") {
            result.push(parse_server_entry(entry_value, templates, source)?);
            continue;
        }

        // グループエントリ。typo をサイレントに飲み込まないよう未知キーは拒否する
        for (key, _) in entry {
            let key = key.as_str().unwrap_or_default();
            if key != "group_name" && key != "sql_servers" {
                return Err(AppError::Config(format!(
                    "Unknown key '{key}' in a sql_servers group entry in {source} \
                     (only group_name / sql_servers are allowed)"
                )));
            }
        }
        let group_name = entry
            .get("group_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AppError::Config(format!(
                    "A sql_servers group entry in {source} requires a non-empty group_name"
                ))
            })?;
        let grouped = entry
            .get("sql_servers")
            .and_then(|v| v.as_sequence())
            .ok_or_else(|| {
                AppError::Config(format!(
                    "sql_servers in group '{group_name}' in {source} must be a list"
                ))
            })?;
        for server_value in grouped {
            let is_nested_group = server_value
                .as_mapping()
                .is_some_and(|m| m.contains_key("sql_servers"));
            if is_nested_group {
                return Err(AppError::Config(format!(
                    "Nested groups are not allowed in group '{group_name}' in {source}"
                )));
            }
            let mut server = parse_server_entry(server_value, templates, source)?;
            server.group_name = Some(group_name.to_string());
            result.push(server);
        }
    }
    Ok(result)
}

fn parse_server_entry(
    server_value: &serde_yaml::Value,
    templates: &[serde_yaml::Value],
    source: &str,
) -> Result<ServerConfig, AppError> {
    let expanded = expand_template(server_value, templates)?;
    serde_yaml::from_value(expanded).map_err(|e| {
        AppError::Config(format!(
            "Failed to parse a sql_servers entry in {source}: {e}"
        ))
    })
}

/// config_override_command を実行して stdout を返す。
///
/// shlex で argv に分解し、シェルを介さず実行する。シェルメタ文字が混入しても
/// 解釈されないためコマンドインジェクションの余地が無い。その代わり
/// パイプ・リダイレクト・変数展開は使えない (単一コマンド前提)。
async fn run_source_command(command: &str) -> Result<String, AppError> {
    let argv = shlex::split(command).ok_or_else(|| {
        AppError::Config(format!(
            "Failed to parse config_override_command (unbalanced quotes?): {command}"
        ))
    })?;
    if argv.is_empty() {
        return Err(AppError::Config("config_override_command is empty".into()));
    }

    let output = tokio::time::timeout(
        Duration::from_secs(SOURCE_COMMAND_TIMEOUT_SECS),
        tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            // Finder / Dock から起動した GUI の PATH は最小構成 (/usr/bin:/bin 等) で、
            // Homebrew の op 等が見つからないため定番パスを補う
            .env("PATH", supplemented_path())
            // タイムアウトで future が drop された時に子プロセスを残さない
            // (認証待ちでハングした op が遺児化し、リトライで多重起動するのを防ぐ)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        AppError::Config(format!(
            "config_override_command timed out ({SOURCE_COMMAND_TIMEOUT_SECS}s): {command} \
             (it may be hanging on 1Password or another auth prompt)"
        ))
    })?
    .map_err(|e| {
        AppError::Config(format!("Failed to run config_override_command: {command}: {e}"))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Config(format!(
            "config_override_command exited with an error (code={:?}): {command}\nstderr: {}",
            output.status.code(),
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if stdout.trim().is_empty() {
        return Err(AppError::Config(format!(
            "config_override_command produced no output: {command}"
        )));
    }
    Ok(stdout)
}

/// PATH に Homebrew 等の定番ディレクトリを補ったものを返す。
fn supplemented_path() -> String {
    supplement_path(&std::env::var("PATH").unwrap_or_default())
}

pub(crate) fn supplement_path(base: &str) -> String {
    let mut path = base.to_string();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let already = base.split(':').any(|p| p == extra);
        if !already {
            if !path.is_empty() {
                path.push(':');
            }
            path.push_str(extra);
        }
    }
    path
}

/// `template: <名前>` を持つサーバーエントリに、sql_server_templates の
/// 同名テンプレートをシャローマージで継承させる。
/// サーバー側で指定したキーはテンプレートの同名キーを上書きする。
fn expand_template(
    server_value: &serde_yaml::Value,
    templates: &[serde_yaml::Value],
) -> Result<serde_yaml::Value, AppError> {
    let server_map = server_value
        .as_mapping()
        .ok_or_else(|| AppError::Config("A sql_servers entry is not a mapping".into()))?;

    let template_name = match server_map.get("template").and_then(|v| v.as_str()) {
        Some(name) => name.to_string(),
        None => return Ok(server_value.clone()),
    };

    let template = templates
        .iter()
        .filter_map(|t| t.as_mapping())
        .find(|t| {
            t.get("name").and_then(|v| v.as_str()) == Some(template_name.as_str())
        })
        .ok_or_else(|| {
            AppError::Config(format!(
                "Template '{template_name}' not found in sql_server_templates"
            ))
        })?;

    let mut merged = template.clone();
    // テンプレート自身の name はサーバー名ではないので除去する
    merged.remove("name");
    for (key, value) in server_map {
        if key.as_str() == Some("template") {
            continue;
        }
        merged.insert(key.clone(), value.clone());
    }
    Ok(serde_yaml::Value::Mapping(merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from_yaml(yaml: &str) -> AppConfig {
        AppConfig {
            doc: parse_mapping(yaml, "test").unwrap(),
            source_path: None,
            applied_override: None,
        }
    }

    #[tokio::test]
    async fn test_inline_servers() {
        let config = config_from_yaml(
            r#"
sql_servers:
  - name: dev-postgres
    description: "dev"
    engine: postgres
    host: localhost
    port: 5432
    schema: dev_db
    user: dev_user
    password: secret
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "dev-postgres");
        assert_eq!(servers[0].port, Some(5432));
        assert!(servers[0].ssh_tunnel.is_none());
        assert!(servers[0].group_name.is_none());
    }

    #[tokio::test]
    async fn test_grouped_servers() {
        // グループエントリはフラット化され、各サーバーに group_name が付く。
        // グループと直書きサーバーの混在も設定順のまま解決される
        let config = config_from_yaml(
            r#"
sql_servers:
  - group_name: production
    sql_servers:
      - name: prod-main
        engine: mysql
        host: prod.example.com
      - name: prod-replica
        engine: mysql
        host: replica.example.com
  - name: standalone
    engine: sqlite
    schema: /tmp/x.db
  - group_name: development
    sql_servers:
      - name: dev-db
        engine: postgres
        host: localhost
"#,
        );
        let servers = config.resolve_servers().unwrap();
        let summary: Vec<(&str, Option<&str>)> = servers
            .iter()
            .map(|s| (s.name.as_str(), s.group_name.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("prod-main", Some("production")),
                ("prod-replica", Some("production")),
                ("standalone", None),
                ("dev-db", Some("development")),
            ]
        );
        // ConnectionInfo にも伝わる
        let info = ConnectionInfo::from(&servers[0]);
        assert_eq!(info.group_name.as_deref(), Some("production"));
    }

    #[tokio::test]
    async fn test_flat_entry_group_name_is_ignored() {
        // サーバーエントリ直下の group_name: はグループエントリの検証を
        // 迂回できてしまうため、デシリアライズしない (無視される)
        let config = config_from_yaml(
            r#"
sql_servers:
  - name: sneaky
    engine: sqlite
    schema: /tmp/x.db
    group_name: bypassed
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert!(servers[0].group_name.is_none());
    }

    #[tokio::test]
    async fn test_group_requires_non_empty_group_name() {
        let config = config_from_yaml(
            r#"
sql_servers:
  - group_name: ""
    sql_servers:
      - name: a
        engine: sqlite
        schema: /tmp/a.db
"#,
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("group_name"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_group_rejects_nested_group() {
        let config = config_from_yaml(
            r#"
sql_servers:
  - group_name: outer
    sql_servers:
      - group_name: inner
        sql_servers:
          - name: a
            engine: sqlite
            schema: /tmp/a.db
"#,
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("Nested groups"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_group_rejects_unknown_key() {
        // グループエントリの typo (servers: 等) をサイレントに無視しない
        let config = config_from_yaml(
            r#"
sql_servers:
  - group_name: g
    sql_servers: []
    description: typo-extra-key
"#,
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("Unknown key 'description'"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_group_with_template() {
        // グループ内のサーバーでも sql_server_templates を継承できる
        let config = config_from_yaml(
            r#"
sql_servers:
  - group_name: shared
    sql_servers:
      - name: db-a
        template: base
        schema: a_db
sql_server_templates:
  - name: base
    engine: mysql
    host: db.example.com
    port: 3306
    user: shared_user
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert_eq!(servers[0].name, "db-a");
        assert_eq!(servers[0].engine, "mysql");
        assert_eq!(servers[0].host.as_deref(), Some("db.example.com"));
        assert_eq!(servers[0].schema.as_deref(), Some("a_db"));
        assert_eq!(servers[0].group_name.as_deref(), Some("shared"));
    }

    #[tokio::test]
    async fn test_readonly_flag() {
        // readonly は省略可能 (デフォルト false)。true 指定は ConnectionInfo に伝わる
        let config = config_from_yaml(
            r#"
sql_servers:
  - name: writable-db
    engine: sqlite
    schema: /tmp/x.db
  - name: readonly-db
    engine: sqlite
    schema: /tmp/x.db
    readonly: true
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert!(!servers[0].readonly);
        assert!(servers[1].readonly);
        assert!(!ConnectionInfo::from(&servers[0]).readonly);
        assert!(ConnectionInfo::from(&servers[1]).readonly);
    }

    #[tokio::test]
    async fn test_connection_info_exposes_host_port_user_and_ssh() {
        // ConnectionInfo は host/port/user と SSH トンネル情報 (機密を除く) を
        // フロントへ渡す。パスワードや鍵は含めない。
        let config = config_from_yaml(
            r#"
sql_servers:
  - name: tunneled-db
    engine: postgres
    host: 10.0.0.5
    port: 5432
    user: app_user
    password: db-secret
    schema: app_db
    ssh_tunnel:
      host: bastion.example.com
      port: 2222
      user: jump
      password: ssh-secret
      private_key_path: /home/me/.ssh/id_ed25519
"#,
        );
        let servers = config.resolve_servers().unwrap();
        let info = ConnectionInfo::from(&servers[0]);
        assert_eq!(info.host.as_deref(), Some("10.0.0.5"));
        assert_eq!(info.port, Some(5432));
        assert_eq!(info.user.as_deref(), Some("app_user"));
        assert!(info.has_ssh_tunnel);
        // 機密がシリアライズに漏れないことを確認する
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("db-secret"));
        assert!(!json.contains("ssh-secret"));
        assert!(!json.contains("id_ed25519"));
        let ssh = info.ssh_tunnel.expect("ssh tunnel info");
        assert_eq!(ssh.host, "bastion.example.com");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.user, "jump");
    }

    #[tokio::test]
    async fn test_inline_with_template() {
        let config = config_from_yaml(
            r#"
sql_servers:
  - template: shared-host
    name: app-db
    schema: app_db
  - template: shared-host
    name: log-db
    schema: log_db
    port: 3307
sql_server_templates:
  - name: shared-host
    engine: mysql
    host: db.example.com
    port: 3306
    user: shared_user
    password: shared_password
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].engine, "mysql");
        assert_eq!(servers[0].host.as_deref(), Some("db.example.com"));
        assert_eq!(servers[0].port, Some(3306));
        // サーバー側の指定がテンプレートを上書きする
        assert_eq!(servers[1].port, Some(3307));
    }

    /// 上書き YAML をコマンドで取得して設定へ再帰マージする経路のテスト用に、
    /// ローカル設定 + 取得 YAML から load_merged 相当の処理を組み立てる。
    /// (load_merged 自体は実ファイルを読むため、ここではマージ部分を検証する)
    fn merged_from(local_yaml: &str, fetched_yaml: &str) -> AppConfig {
        let mut config = config_from_yaml(local_yaml);
        let overrides = parse_mapping(fetched_yaml, "test override").unwrap();
        merge_mapping(&mut config.doc, &overrides);
        config.doc.remove(CONFIG_OVERRIDE_COMMAND_KEY);
        config.applied_override = Some("test-command".to_string());
        config
    }

    /// load_merged の実経路 (設定読み込み → コマンド実行 → マージ) を通す。
    /// QUERYFOLIO_CONFIG_YAML を使うのでこのプロセスで env を触る唯一のテスト
    /// (他のテストは config_from_yaml を使い env を読まない)。
    #[tokio::test]
    async fn test_load_merged_runs_command_and_merges_result() {
        std::env::set_var(
            "QUERYFOLIO_CONFIG_YAML",
            "sql_servers: []\ndefault_limit: 500\n\
             config_override_command: /bin/echo default_limit:\\ 7\n",
        );
        let config = AppConfig::load_merged().await.unwrap();
        std::env::remove_var("QUERYFOLIO_CONFIG_YAML");

        // 取得 YAML の値が適用され、キー自体は落ちている
        assert_eq!(config.default_limit(), 7);
        assert!(config.override_command().unwrap().is_none());
        assert!(config.info().unwrap().source.contains("/bin/echo"));
    }

    #[tokio::test]
    async fn test_override_command_is_executed_and_merged() {
        // /bin/echo で上書き YAML を出力させ、load_merged と同じ経路を通す
        let yaml = run_source_command(
            "/bin/echo -n sql_servers:\\n  - name: fetched\\n    engine: sqlite\\n    schema: /tmp/x.db\\n",
        )
        .await
        .unwrap();
        assert!(yaml.contains("fetched"));
    }

    #[test]
    fn test_override_replaces_sql_servers_wholesale() {
        // sql_servers はリストなので要素マージではなく丸ごと置き換わる
        let config = merged_from(
            r#"
sql_servers:
  - name: local-a
    engine: sqlite
    schema: /tmp/a.db
  - name: local-b
    engine: sqlite
    schema: /tmp/b.db
"#,
            r#"
sql_servers:
  - name: fetched-only
    engine: sqlite
    schema: /tmp/c.db
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "fetched-only");
    }

    #[test]
    fn test_override_can_set_any_top_level_key() {
        // sql_servers 以外のキーも上書きできる (旧方式との最大の違い)
        let config = merged_from(
            "sql_servers: []\ndefault_limit: 500\nsqlfiles_dir: ~/local\n",
            "default_limit: 42\n",
        );
        assert_eq!(config.default_limit(), 42);
        // 上書き YAML に無いキーはローカルの値が残る
        assert!(config
            .resolve_sqlfiles_dir()
            .unwrap()
            .to_string_lossy()
            .ends_with("local"));
    }

    #[test]
    fn test_override_merges_mappings_recursively() {
        // マッピング同士は再帰的に混ざる (ローカルの model は残り api_key だけ上書き)
        let config = merged_from(
            "sql_servers: []\nai:\n  provider: openai\n  model: local-model\n  api_key: sk-local\n",
            "ai:\n  api_key: sk-fetched\n",
        );
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-fetched"));
        assert_eq!(ai.get("model").and_then(serde_yaml::Value::as_str), Some("local-model"));
        assert_eq!(ai.get("provider").and_then(serde_yaml::Value::as_str), Some("openai"));
    }

    #[test]
    fn test_override_ai_wins_over_local_ai() {
        // API キーを 1Password 側に置く運用: 取得 YAML の ai が優先される
        let config = merged_from(
            "sql_servers: []\nai:\n  api_key: sk-local\n",
            "ai:\n  api_key: sk-fetched\n",
        );
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-fetched"));
    }

    #[test]
    fn test_local_ai_survives_without_override_ai() {
        let config = merged_from("sql_servers: []\nai:\n  api_key: sk-local\n", "default_limit: 10\n");
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-local"));
    }

    #[test]
    fn test_override_key_is_dropped_after_merge() {
        // 取得 YAML 側が config_override_command を持っていても再帰取得はしない
        let config = merged_from(
            "sql_servers: []\nconfig_override_command: local-cmd\n",
            "config_override_command: fetched-cmd\nsql_servers: []\n",
        );
        assert!(config.override_command().unwrap().is_none());
        // 適用済みコマンドは info の表示用に残る
        assert!(config.info().unwrap().source.contains("test-command"));
    }

    #[test]
    fn test_no_override_command_reports_inline() {
        let config = config_from_yaml("sql_servers: []\n");
        assert!(config.override_command().unwrap().is_none());
        assert_eq!(config.info().unwrap().source, "inline");
    }

    #[test]
    fn test_override_command_is_read_from_config() {
        let config = config_from_yaml("sql_servers: []\nconfig_override_command: op read x\n");
        assert_eq!(config.override_command().unwrap().as_deref(), Some("op read x"));
        assert!(config.info().unwrap().source.contains("op read x"));
    }

    #[test]
    fn test_blank_override_command_is_error() {
        // 空文字を黙って「未設定」に倒すと、オーバーライドが効かないまま
        // ローカル設定で動いていることに気付けない
        let config = config_from_yaml("sql_servers: []\nconfig_override_command: \"   \"\n");
        let err = config.override_command().unwrap_err().to_string();
        assert!(err.contains("is empty"));
    }

    #[test]
    fn test_non_string_override_command_is_error() {
        // 旧方式のマッピング形式を書いてしまった場合も含め、型誤りは黙認しない
        for yaml in [
            "sql_servers: []\nconfig_override_command: 123\n",
            "sql_servers: []\nconfig_override_command:\n  command: op read x\n",
        ] {
            let config = config_from_yaml(yaml);
            let err = config.override_command().unwrap_err().to_string();
            assert!(err.contains("must be a string"), "unexpected error: {err}");
        }
    }

    #[test]
    fn test_old_sql_servers_source_declaration_explains_migration() {
        // 旧方式の設定のまま上げたユーザーに移行先を伝える
        let config = config_from_yaml("sql_servers:\n  file: ~/secrets/servers.yaml\n");
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("config_override_command"), "unexpected error: {err}");
    }

    #[test]
    fn test_sql_servers_mapping_is_rejected() {
        // 旧方式 (sql_servers: {command: ...}) はサポートしない
        let config = config_from_yaml("sql_servers:\n  command: op read x\n");
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("must be a list"));
    }

    #[test]
    fn test_default_limit() {
        let config = config_from_yaml("sql_servers: []\n");
        assert_eq!(config.default_limit(), 500);
        let config = config_from_yaml("sql_servers: []\ndefault_limit: 100\n");
        assert_eq!(config.default_limit(), 100);
        let config = config_from_yaml("sql_servers: []\ndefault_limit: 0\n");
        assert_eq!(config.default_limit(), 0);
    }

    #[test]
    fn test_sqlfiles_dir_default_and_custom() {
        let config = config_from_yaml("sql_servers: []\n");
        let default_dir = config.resolve_sqlfiles_dir().unwrap();
        assert!(default_dir.ends_with(".config/queryfolio/sqlfiles"));

        let config = config_from_yaml("sql_servers: []\nsqlfiles_dir: ~/my-queries\n");
        let custom = config.resolve_sqlfiles_dir().unwrap();
        assert_eq!(custom, dirs::home_dir().unwrap().join("my-queries"));
    }

    #[test]
    fn test_ensure_config_file_in() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-ensure-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // 無ければ作成して Some(パス) を返す
        let created = ensure_config_file_in(&dir).unwrap();
        assert!(created.is_some());
        assert!(dir.join("config.yml").exists());

        // 新規作成は 600 (umask 依存の 644 で作らない)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("config.yml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        // 既に存在すれば None (上書きしない)
        assert!(ensure_config_file_in(&dir).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 既存の config.yml が緩い権限 (644) で置かれていたら、起動時の
    /// ensure_config_file_in が 600 へ是正する (中身は変えない)。
    #[cfg(unix)]
    #[test]
    fn test_ensure_config_file_in_tightens_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "queryfolio-ensure-perm-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 他ユーザーから読める 644 で手動作成された既存ファイルを模す
        let path = dir.join("config.yml");
        std::fs::write(&path, "sql_servers: []\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // 既存なので None を返しつつ、権限は 600 へ是正される
        assert!(ensure_config_file_in(&dir).unwrap().is_none());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // 中身は書き換えない
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sql_servers: []\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// config.yml がディレクトリの場合、tighten はパーミッションを変えない
    /// (600 にすると検索ビットが落ちてアクセス不能になるため触らない)。
    #[cfg(unix)]
    #[test]
    fn test_tighten_config_permissions_skips_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "queryfolio-tighten-dir-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // config.yml という名前のディレクトリ (異常状態) を作る
        let as_dir = dir.join("config.yml");
        std::fs::create_dir_all(&as_dir).unwrap();
        std::fs::set_permissions(&as_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        tighten_config_permissions(&as_dir).unwrap();

        // ディレクトリの権限は変えない (600 にしない)
        let mode = std::fs::metadata(&as_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 設定エディタの読み書き。無ければテンプレートを作ってから読み、
    /// 保存した内容がそのまま読み戻せる。
    #[test]
    fn test_read_write_config_file_in() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // ファイルが無い状態でもテンプレートが作られて読める
        let initial = read_config_file_in(&dir).unwrap();
        assert!(initial.contains("sql_servers"));

        let edited = "sql_servers:\n  - name: edited\n    engine: sqlite\n    schema: /tmp/a.db\n";
        let saved_path = write_config_file_in(&dir, edited).unwrap();
        assert_eq!(saved_path, dir.join("config.yml").display().to_string());
        assert_eq!(read_config_file_in(&dir).unwrap(), edited);
        // 一時ファイルを残さない
        assert!(!dir.join("config.yml.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 壊れた YAML は保存を拒否し、既存の設定を残す。
    #[test]
    fn test_write_config_file_in_rejects_invalid_yaml() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-invalid-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let valid = "sql_servers: []\n";
        write_config_file_in(&dir, valid).unwrap();

        // マッピングとしてパースできない内容
        assert!(write_config_file_in(&dir, "sql_servers: [\n").is_err());
        // YAML ではあるがマッピングではない
        assert!(write_config_file_in(&dir, "- just\n- a list\n").is_err());
        // 既存の内容は壊れていない
        assert_eq!(read_config_file_in(&dir).unwrap(), valid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 保存時は常に 600 で書く (新規も、緩い既存権限の是正も)。
    #[cfg(unix)]
    #[test]
    fn test_write_config_file_in_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-perm-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // 新規作成は 600
        write_config_file_in(&dir, "sql_servers: []\n").unwrap();
        let path = dir.join("config.yml");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // 既存が緩い権限 (640) でも、保存時に 600 へ絞る
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_config_file_in(&dir, "sql_servers: []\n# edited\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// config.yaml (拡張子 yaml) を使っている場合も、そのファイルへ保存する。
    #[test]
    fn test_write_config_file_in_keeps_yaml_extension() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-yaml-ext-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "sql_servers: []\n").unwrap();

        let edited = "sql_servers: []\n# edited\n";
        let saved_path = write_config_file_in(&dir, edited).unwrap();
        assert_eq!(saved_path, dir.join("config.yaml").display().to_string());
        assert!(!dir.join("config.yml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yaml")).unwrap(),
            edited
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_config_template_is_valid() {
        // テンプレートはそのままで有効な設定 (接続 0 件) としてパースできること
        let config = config_from_yaml(CONFIG_TEMPLATE);
        let servers = config.resolve_servers().unwrap();
        assert!(servers.is_empty());
        config.resolve_sqlfiles_dir().unwrap();
    }

    #[test]
    fn test_expand_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn test_supplement_path() {
        // 無ければ追加される
        let path = supplement_path("/usr/bin:/bin");
        assert!(path.split(':').any(|p| p == "/opt/homebrew/bin"));
        assert!(path.split(':').any(|p| p == "/usr/local/bin"));
        // 既にあれば重複追加しない
        let path = supplement_path("/opt/homebrew/bin:/usr/bin");
        let count = path.split(':').filter(|p| *p == "/opt/homebrew/bin").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_connection_info_hides_password() {
        let server = ServerConfig {
            name: "s".into(),
            description: None,
            folder_name: None,
            engine: "mysql".into(),
            host: Some("h".into()),
            port: Some(3306),
            schema: Some("db".into()),
            user: Some("u".into()),
            password: Some("secret".into()),
            ssh_tunnel: None,
            tls: false,
            aws_profile: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
        };
        let info = ConnectionInfo::from(&server);
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("secret"));
    }

    fn server_with(
        folder_name: Option<&str>,
        host: Option<&str>,
        engine: &str,
        schema: Option<&str>,
        user: Option<&str>,
    ) -> ServerConfig {
        ServerConfig {
            name: "conn-name".into(),
            description: None,
            folder_name: folder_name.map(|s| s.to_string()),
            engine: engine.into(),
            host: host.map(|s| s.to_string()),
            port: None,
            schema: schema.map(|s| s.to_string()),
            user: user.map(|s| s.to_string()),
            password: None,
            ssh_tunnel: None,
            tls: false,
            aws_profile: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
        }
    }

    #[test]
    fn test_sqlfiles_folder_name() {
        // folder_name があればそれを使う (name は使わない)
        let s = server_with(Some("my-folder"), Some("h"), "mysql", Some("db"), Some("u"));
        assert_eq!(s.sqlfiles_folder_name(), "my-folder");

        // folder_name が空文字列ならフォールバック
        let s = server_with(Some("   "), Some("h"), "mysql", Some("db"), Some("u"));
        assert_eq!(s.sqlfiles_folder_name(), "h_mysql_db_u");

        // folder_name 無し → <host>_<engine>_<schema>_<user>
        let s = server_with(
            None,
            Some("db.example.com"),
            "postgres",
            Some("prod"),
            Some("app"),
        );
        assert_eq!(s.sqlfiles_folder_name(), "db.example.com_postgres_prod_app");

        // sqlite: host/user 無し、schema はファイルパス → 区切りをサニタイズ
        let s = server_with(None, None, "sqlite", Some("/Users/me/data.db"), None);
        assert_eq!(s.sqlfiles_folder_name(), "_sqlite__Users_me_data.db_");

        // 先頭ドットは避ける (不可視/相対パス化を防ぐ)
        let s = server_with(Some(".hidden"), None, "sqlite", None, None);
        assert_eq!(s.sqlfiles_folder_name(), "_.hidden");
    }

    #[test]
    fn test_sqlfiles_folder_name_dynamodb_discriminator() {
        let mut server = ServerConfig {
            name: "ddb".into(),
            description: None,
            folder_name: None,
            engine: "dynamodb".into(),
            host: None,
            port: None,
            schema: Some("ap-northeast-1".into()),
            user: Some("AKIAEXAMPLEKEYID".into()),
            password: Some("secret".into()),
            ssh_tunnel: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
            tls: false,
            aws_profile: None,
        };
        // アクセスキー ID はフォルダ名に出さず、短いハッシュで区別する
        let folder = server.sqlfiles_folder_name();
        assert!(!folder.contains("AKIAEXAMPLEKEYID"), "{folder}");
        assert!(folder.contains("key-"), "{folder}");
        // 別のキーなら別のフォルダになる
        let mut other = server.clone();
        other.user = Some("AKIAOTHERKEYID".into());
        assert_ne!(folder, other.sqlfiles_folder_name());
        // aws_profile があればプロファイル名 (非機密) を使う
        server.user = None;
        server.password = None;
        server.aws_profile = Some("myprofile".into());
        let folder = server.sqlfiles_folder_name();
        assert!(folder.contains("myprofile"), "{folder}");
        // 同一ハッシュの安定性
        assert_eq!(stable_hash_hex("abc"), stable_hash_hex("abc"));
        assert_ne!(stable_hash_hex("abc"), stable_hash_hex("abd"));
    }
}
