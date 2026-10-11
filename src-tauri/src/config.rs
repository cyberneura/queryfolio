use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// Timeout (seconds) for running config_override_command.
/// Required because a command that hangs forever waiting for authentication (1Password etc.) would
/// freeze the caller.
const SOURCE_COMMAND_TIMEOUT_SECS: u64 = 60;

/// Returns the ~/.config/queryfolio directory.
pub fn app_config_dir() -> Result<PathBuf, AppError> {
    let home = dirs::home_dir()
        .ok_or_else(|| AppError::Config("Could not determine the home directory".into()))?;
    Ok(home.join(".config").join("queryfolio"))
}

/// Expands a leading ~ in a path string to the home directory.
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

/// Template for the config.yml created automatically on first launch.
/// It must parse as a valid configuration as-is (zero connections).
const CONFIG_TEMPLATE: &str = r#"# Queryfolio config file
# See config.example.yaml in the repository for the full format.
# https://github.com/cyberneura/queryfolio

# Connection definitions.
#
# servers:
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

servers: []

# Keep secrets out of this file by fetching them from elsewhere.
#
# config_override_command runs a command whose stdout must be YAML, and merges
# that YAML over this file. The merge is recursive for mappings; scalars and
# lists (including servers) are replaced wholesale. Any key can be
# overridden this way, not just servers.
#
# config_override_command: op read "op://development/queryfolio/config-yaml"

# Where query files are stored (default: ~/.config/queryfolio/sqlfiles).
# A relative path is resolved against this config directory, not the current
# working directory, so the CLI and a running window always agree on it.
# sqlfiles_dir: ~/queries
#
# Query files live under <sqlfiles_dir>/<folder>/<name>.sql. The per-connection
# folder is <host>_<engine>_<schema>_<user> by default (the connection name is
# not used). Set `folder_name:` on a server to pin the folder explicitly.
"#;

/// Returns the path of the config file that actually exists. Returns None if neither
/// config.yml nor config.yaml exists.
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

/// Creates the template if neither config.yml nor config.yaml exists.
/// Returns Some(created path) if it was created. Returns None if the file already exists or
/// if the config is overridden via the QUERYFOLIO_CONFIG_YAML environment variable.
pub fn ensure_config_file() -> Result<Option<String>, AppError> {
    let env_override = std::env::var("QUERYFOLIO_CONFIG_YAML")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if env_override {
        return Ok(None);
    }
    ensure_config_file_in(&app_config_dir()?)
}

/// Path of the config file in dir. Prefers config.yml, then config.yaml;
/// if neither exists, returns the config.yml path.
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
        // If the existing file was created with loose permissions (644 depending on umask, etc.),
        // tighten it to owner-only (600). The config may contain connection passwords and SSH key
        // passphrases in plain text. The main path for this fix is AppConfig::load (it runs from
        // build_menu at startup, earlier than the frontend's ensure_config_file), but we also do it
        // here so that the config editor path (read_config_file_in) and files from older versions
        // or created by hand are reliably fixed too.
        #[cfg(unix)]
        {
            tighten_config_permissions(&yml)?;
            tighten_config_permissions(&yaml)?;
        }
        return Ok(None);
    }
    std::fs::create_dir_all(dir)?;
    // Fix the permissions to 600 from the moment of creation. std::fs::write would create the
    // file depending on umask (usually 644), leaving a window right after the write in which
    // other users on the same machine can read the contents. Using create_new (O_EXCL) also
    // prevents the race where another process creates config.yml after the exists check above
    // and we truncate it.
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
                // mode() is only narrowed further by umask, but to guard against an abnormal umask
                // that
                // drops the owner bits, also set it explicitly on the opened fd.
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                file.write_all(CONFIG_TEMPLATE.as_bytes())?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another process created it after the exists check. Do not overwrite; only fix the
                // permissions.
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

/// If the existing config file has group / other permission bits set, narrow it to
/// owner-only (600). Does nothing if it does not exist. On macOS the staff group is shared by
/// all local users, so even 640 leaks to other users. Narrowing to owner-only is the safe choice.
#[cfg(unix)]
fn tighten_config_permissions(path: &std::path::Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        // Do nothing if it does not exist (a different extension, or it vanished between the check
        // and stat).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        // Propagate other I/O errors instead of swallowing them. Silently returning would make it
        // look like the fix succeeded when it did not.
        Err(e) => return Err(e.into()),
    };
    // Apply only to regular files. If config.yml is a directory (or a symlink to one), setting
    // 600 would drop the owner's search bit (x), making it inaccessible and breaking the
    // subsequent config load. Leave non-files alone.
    if !meta.is_file() {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// SSH tunnel settings. Compatible with sql-agent-mcp-server's config.yaml.
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

/// Connection target server settings. Compatible with sql-agent-mcp-server's config.yaml.
/// queryfolio extends engine: sqlite and treats schema as the DB file path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// queryfolio extension: explicitly sets the folder name for saved query files.
    /// If omitted, it is built from <host>_<engine>_<schema>_<user>
    /// (name is not used for the folder name). See sqlfiles_folder_name.
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
    /// queryfolio extension: if true, use https for connections of HTTP-based engines
    /// (elasticsearch). Defaults to false.
    /// For dynamodb it is used as the scheme when the endpoint is overridden (host specified).
    /// For SQL engines (mysql / postgres / mssql) it is treated as a request to "require TLS and
    /// also verify the certificate" (the default when ssl_mode is omitted becomes verify-full).
    /// For redis it enables a TLS connection (equivalent to `rediss://`). The certificate is
    /// always verified (connection_addr in engines/redis.rs).
    #[serde(default)]
    pub tls: bool,
    /// queryfolio extension: TLS mode for SQL engines (mysql / postgres / mssql).
    /// disable / prefer / require / verify-ca / verify-full.
    /// mssql treats verify-ca the same as verify-full (SQL Server TLS has no setting that
    /// "verifies only the chain and not the host name"; see engines/mssql.rs).
    /// If omitted: verify-full when tls: true, otherwise prefer
    /// (sqlx's default: tries TLS, falls back to plaintext if it cannot be established, and does
    /// not verify the certificate).
    /// For connections through an SSH tunnel the target is 127.0.0.1, so verify-full fails the
    /// certificate host name check (the tunnel itself is encrypted, so stop at require or omit it).
    #[serde(default)]
    pub ssl_mode: Option<String>,
    /// queryfolio extension: path of the root CA certificate (PEM) used to verify the certificate.
    /// ~ is expanded. Specify it when using a self-signed CA with verify-ca / verify-full.
    #[serde(default)]
    pub ssl_root_cert: Option<String>,
    /// queryfolio extension (for dynamodb): AWS profile name passed to aws-config
    /// (~/.aws/config / credentials). If omitted, the default credentials chain
    /// (environment variables -> default profile -> IMDS). Ignored by other engines.
    #[serde(default)]
    pub aws_profile: Option<String>,
    /// queryfolio extension: if true, refuse to run statements that return no rows (INSERT /
    /// UPDATE / DELETE / DDL, etc.). Defaults to false.
    /// An accident-prevention guard that cannot stop SELECTs calling functions with side effects
    /// (nextval, etc.).
    #[serde(default)]
    pub readonly: bool,
    /// queryfolio extension: if true, allow running dangerous statements (UPDATE / DELETE
    /// without WHERE, DROP / TRUNCATE, etc.). Defaults to false, in which case these statements
    /// are rejected to prevent whole-table destruction or table loss from mistakes.
    /// Even when true, the frontend asks for confirmation before running.
    #[serde(default)]
    pub allow_dangerous_statements: bool,
    /// queryfolio extension: display group name in the connection list.
    /// parse_server_entries sets it on the servers belonging to a servers group entry
    /// (group_name + servers).
    /// A group_name: directly under a server entry is not accepted (it is ignored) because it
    /// would bypass the group entry validation (empty check / unknown key rejection).
    #[serde(default, skip_deserializing)]
    pub group_name: Option<String>,
}

/// A stable short hash of a string (the first 8 hex digits of FNV-1a 64-bit).
/// Used to turn identifiers like an AWS access key ID, which we do not want to show as-is but
/// need to tell connections apart, into a folder name (irreversible, no extra crate needed).
fn stable_hash_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", (hash >> 32) as u32)
}

/// Sanitizes a string so that it is safe as a folder name on the filesystem.
/// Replaces path separators (/ \) and NUL with _, and avoids a leading dot (hidden / relative).
/// Pre-empts the characters that query_files::validate_component rejects.
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

/// TLS mode for SQL engines (mysql / postgres).
/// Names and meanings follow the ssl-mode of libpq / MySQL clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlSslMode {
    /// Do not use TLS
    Disable,
    /// Try TLS and fall back to plaintext if it cannot be established. The certificate is not
    /// verified
    /// (sqlx's default)
    Prefer,
    /// Require TLS. The certificate is not verified
    /// (eavesdropping on the path is prevented, but a man-in-the-middle is not).
    /// libpq behaves like verify-ca if ssl_root_cert is given, but **sqlx 0.8 always handles
    /// Require with accept_invalid_certs** (connection/tls.rs in sqlx-postgres), so the certificate
    /// is not verified even if a root CA is passed.
    /// To get verification you must explicitly choose verify-ca / verify-full
    Require,
    /// Require TLS and verify that the server certificate comes from a trusted CA
    VerifyCa,
    /// In addition to VerifyCa, verify that the target host name matches the certificate
    VerifyFull,
}

impl SqlSslMode {
    /// Whether this is a mode that can fall back to plaintext (to warn in the UI / logs)
    pub fn allows_plaintext(self) -> bool {
        matches!(self, SqlSslMode::Disable | SqlSslMode::Prefer)
    }

    /// String representation written in the config (also the value passed to the frontend via
    /// ConnectionInfo)
    pub fn as_str(self) -> &'static str {
        match self {
            SqlSslMode::Disable => "disable",
            SqlSslMode::Prefer => "prefer",
            SqlSslMode::Require => "require",
            SqlSslMode::VerifyCa => "verify-ca",
            SqlSslMode::VerifyFull => "verify-full",
        }
    }
}

impl ServerConfig {
    /// Returns the effective TLS mode for SQL engines.
    ///
    /// Precedence: ssl_mode (explicit) -> verify-full if tls: true -> prefer.
    /// The default stays prefer for backward compatibility (suddenly switching to verify-full
    /// would break connections that worked with, e.g., an internal self-signed certificate).
    /// prefer "falls back to plaintext if TLS cannot be established, and does not verify the
    /// certificate even if it can", so for direct connections specifying tls: true or ssl_mode is
    /// recommended.
    pub fn sql_ssl_mode(&self) -> Result<SqlSslMode, AppError> {
        let Some(raw) = self.ssl_mode.as_deref() else {
            return Ok(if self.tls {
                SqlSslMode::VerifyFull
            } else {
                SqlSslMode::Prefer
            });
        };

        // Treating an empty string as "unset" would silently ignore a setting the user thought they
        // had written
        let normalized = raw.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "disable" => Ok(SqlSslMode::Disable),
            "prefer" => Ok(SqlSslMode::Prefer),
            "require" => Ok(SqlSslMode::Require),
            "verify-ca" => Ok(SqlSslMode::VerifyCa),
            "verify-full" => Ok(SqlSslMode::VerifyFull),
            other => Err(AppError::Config(format!(
                "Server '{}': unsupported ssl_mode '{}'. \
                 Use one of: disable / prefer / require / verify-ca / verify-full",
                self.name, other
            ))),
        }
    }

    /// Returns the ssl_root_cert setting value (None if unset).
    ///
    /// Returns an error if it is specified together with a mode that does not verify
    /// (disable / prefer / require). sqlx **silently ignores** the root CA in non-verifying modes,
    /// so leaving the misconception that "I specified a CA, so it must be verified" would make
    /// the user believe the connection is safe while accepting a man-in-the-middle.
    pub fn sql_ssl_root_cert(&self) -> Result<Option<&str>, AppError> {
        let Some(raw) = self.ssl_root_cert.as_deref().map(str::trim) else {
            return Ok(None);
        };
        if raw.is_empty() {
            return Err(AppError::Config(format!(
                "Server '{}': ssl_root_cert is empty",
                self.name
            )));
        }

        let mode = self.sql_ssl_mode()?;
        if !matches!(mode, SqlSslMode::VerifyCa | SqlSslMode::VerifyFull) {
            return Err(AppError::Config(format!(
                "Server '{}': ssl_root_cert is ignored with ssl_mode: {} \
                 (the certificate is not verified). Use ssl_mode: verify-ca or \
                 verify-full to verify it, or remove ssl_root_cert.",
                self.name,
                mode.as_str()
            )));
        }
        Ok(Some(raw))
    }

    /// Returns the folder name for saved query files.
    /// Uses folder_name if set; otherwise builds <host>_<engine>_<schema>_<user> (name is not
    /// used).
    /// Separators and the like are sanitized so the result is safe as a path component.
    pub fn sqlfiles_folder_name(&self) -> String {
        if let Some(folder) = self.folder_name.as_deref() {
            let folder = folder.trim();
            if !folder.is_empty() {
                return sanitize_folder_component(folder);
            }
        }
        // The dynamodb user is an AWS access key ID (a credential identifier), so do not put it in
        // the folder name as-is. Instead tell connections apart with a non-sensitive identifier
        // (the aws_profile name, or a short hash for static keys) — this prevents two connections
        // in
        // the same region that differ only in profile/key from landing in the same folder and
        // mixing
        // their query files
        let dynamodb_discriminator;
        let user = if self.engine.eq_ignore_ascii_case("dynamodb") {
            // Choose the identifier in the same priority order as the authentication resolution
            // (user/password -> aws_profile -> default chain). Reversing it would make two
            // connections
            // ("static key is effective, profile is ignored") land in the same profile-name folder
            // and mix
            if let Some(user) =
                self.user.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                dynamodb_discriminator = format!("key-{}", stable_hash_hex(user));
                &dynamodb_discriminator
            } else if let Some(profile) =
                self.aws_profile.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                profile
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

/// SSH tunnel info passed to the frontend. Does not include secrets such as passwords or keys.
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

/// Connection target info passed to the frontend. Does not include secrets such as passwords.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionInfo {
    pub name: String,
    pub description: Option<String>,
    pub engine: String,
    pub has_ssh_tunnel: bool,
    /// Target host (null if unset)
    pub host: Option<String>,
    /// Target port (null if unset)
    pub port: Option<u16>,
    /// Connection user (null if unset)
    pub user: Option<String>,
    /// Default database (schema) in the config
    pub schema: Option<String>,
    /// SSH tunnel info (secrets excluded). null if no tunnel is used
    pub ssh_tunnel: Option<SshTunnelInfo>,
    /// Read-only connection (refuses to run write statements)
    pub readonly: bool,
    /// Allow running dangerous statements (UPDATE/DELETE without WHERE, DROP/TRUNCATE, etc.).
    /// The frontend asks for confirmation before running even on a connection where this is true
    pub allow_dangerous_statements: bool,
    /// Display group name in the connection list (null if not in a group)
    pub group_name: Option<String>,
    /// Effective TLS mode (string representation of SqlSslMode).
    /// For mysql / postgres / mssql it is the value resolved from ssl_mode / tls; for redis it is
    /// verify-full when tls: true (certificate and host name are both verified) and disable when
    /// false.
    /// null for other engines and when the ssl_mode value is invalid.
    /// The frontend uses it to flag "direct connections that may not be encrypted".
    /// (The sql_ prefix of the field name is a leftover from when it was SQL-only)
    pub sql_ssl_mode: Option<String>,
    /// Declaration of the engine's capabilities (editor language, file extension, UI toggles).
    /// The frontend toggles the UI based on this rather than on the engine name.
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
            // Go through parse_engine so engine name aliases (mariadb / postgresql) are also picked
            // up.
            // A config with an invalid engine name or ssl_mode value errors at connect time, so
            // here we give up on displaying it and use null
            sql_ssl_mode: match crate::db::parse_engine(&server.engine) {
                Ok(crate::db::Engine::MySql)
                | Ok(crate::db::Engine::Postgres)
                | Ok(crate::db::Engine::MsSql) => server
                    .sql_ssl_mode()
                    .ok()
                    .map(|mode| mode.as_str().to_string()),
                // For redis, whether tls is set directly decides TLS vs plaintext (there is no
                // intermediate mode). We still report plaintext as disable because it is the only
                // way to
                // notice that a connection where the user thought they had enabled TLS is actually
                // connecting in plaintext
                // (CYBERNEURA-DEV-420)
                Ok(crate::db::Engine::Redis) => Some(
                    if server.tls {
                        SqlSslMode::VerifyFull
                    } else {
                        SqlSslMode::Disable
                    }
                    .as_str()
                    .to_string(),
                ),
                _ => None,
            },
            capabilities: crate::engines::capabilities_for_name(&server.engine),
        }
    }
}

/// Top-level key for overriding the config with the YAML from an external command.
/// The value is a command string; its stdout (YAML) is recursively merged into the whole config.
pub const CONFIG_OVERRIDE_COMMAND_KEY: &str = "config_override_command";

/// Info for display in the frontend. The result of config resolution (contains no secrets).
#[derive(Debug, Serialize)]
pub struct ConfigInfo {
    pub config_path: String,
    pub config_exists: bool,
    pub source: String,
    pub sqlfiles_dir: String,
}

/// Parse result of ~/.config/queryfolio/config.yml (or config.yaml if absent).
///
/// Top-level keys:
/// - servers: list of server definitions
/// - server_templates: templates for connection info
/// - sqlfiles_dir: directory for saved query files (optional)
/// - config_override_command: command that fetches a YAML to override the config (optional)
///
/// `load` reads only the local file (synchronous). `load_merged` additionally runs
/// config_override_command and returns the config with the fetched YAML recursively merged.
/// Command execution can take several seconds with 1Password etc. and may even ask for
/// Touch ID, so the caller (AppState) must cache it for the session.
pub struct AppConfig {
    doc: serde_yaml::Mapping,
    /// Path of the file that was read. None if it came from the QUERYFOLIO_CONFIG_YAML environment
    /// variable
    source_path: Option<PathBuf>,
    /// The config_override_command actually applied in load_merged.
    /// The key is dropped from the merged doc, so it is saved here for display.
    applied_override: Option<String>,
}

impl AppConfig {
    /// Loads the config.
    /// If the QUERYFOLIO_CONFIG_YAML environment variable is set, it is treated as the config
    /// file contents (override for development / tests). Otherwise reads config.yml / config.yaml.
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
        // Before reading, tighten a config file that was left with loose permissions to owner-only.
        // The load from build_menu runs earlier than the frontend's ensure_config_file, so this is
        // the main path for the fix (the config may contain plaintext connection passwords and SSH
        // key passphrases).
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

    /// Reads the local config and, if `config_override_command` is present, runs it and returns
    /// the config with the fetched YAML recursively merged.
    ///
    /// The fetched YAML takes precedence in the merge. Mappings are mixed recursively, while
    /// scalars and sequences (including servers) are replaced wholesale
    /// (element-wise merging of lists is not done because there is no way to decide which items are
    /// "the same").
    pub async fn load_merged() -> Result<Self, AppError> {
        let mut config = Self::load()?;
        let Some(command) = config.override_command()? else {
            return Ok(config);
        };
        let yaml = run_source_command(&command).await?;
        let overrides = parse_mapping(&yaml, &format!("{CONFIG_OVERRIDE_COMMAND_KEY}: {command}"))?;
        merge_mapping(&mut config.doc, &overrides);
        // Do not fetch recursively even if the fetched YAML has config_override_command.
        // Drop the key itself to indicate it has already been applied (the info display uses the
        // local value, so removing it here does not affect the display).
        config.doc.remove(CONFIG_OVERRIDE_COMMAND_KEY);
        config.applied_override = Some(command);
        Ok(config)
    }

    /// Prefers config.yml, then config.yaml; if neither exists,
    /// returns the default config.yml path.
    fn find_config_path() -> Result<PathBuf, AppError> {
        Ok(config_path_in(&app_config_dir()?))
    }

    /// Row limit automatically applied to a SELECT without LIMIT.
    /// 500 if omitted. Specifying 0 disables it.
    pub fn default_limit(&self) -> u64 {
        self.doc
            .get("default_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(500)
    }

    /// Resolves the directory for saved query files.
    ///
    /// If a relative path is written, it is **resolved against the config directory
    /// (`~/.config/queryfolio`), not the current directory**. The CLI `write` is written out by the
    /// launched process itself, while the file is opened by the running instance (a different
    /// process with a different cwd), so with cwd as the base the two processes would point to
    /// different places (the written file could not be opened and would be left in an unintended
    /// directory). The cwd (`/`) when the GUI is launched from Finder is also meaningless as a
    /// base, so we anchor to something independent of the process.
    pub fn resolve_sqlfiles_dir(&self) -> Result<PathBuf, AppError> {
        match self.doc.get("sqlfiles_dir").and_then(|v| v.as_str()) {
            Some(dir) if !dir.trim().is_empty() => {
                let path = expand_tilde(dir);
                if path.is_absolute() {
                    Ok(path)
                } else {
                    Ok(app_config_dir()?.join(path))
                }
            }
            _ => Ok(app_config_dir()?.join("sqlfiles")),
        }
    }

    /// Command that fetches the YAML overriding the config (None if unset).
    ///
    /// If the key exists but is not a string or is an empty string, it is an error.
    /// Silently treating it as "unset" would let the app run on the local config without the
    /// override side's connection info or readonly applied, so the problem would go unnoticed.
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

    /// The top-level `ai:` section (raw value, unvalidated).
    /// If load_merged has been applied, the ai from the fetched YAML is reflected.
    pub fn ai(&self) -> Option<serde_yaml::Value> {
        self.doc.get("ai").cloned()
    }

    /// Resolves the list of connection servers.
    /// Does no fetching (applying config_override_command is already done in load_merged).
    pub fn resolve_servers(&self) -> Result<Vec<ServerConfig>, AppError> {
        let servers = self
            .doc
            .get("servers")
            .ok_or_else(|| AppError::Config("The config has no servers key".into()))?
            .as_sequence()
            .cloned()
            .ok_or_else(|| {
                // Migration guidance from the old format (sql_servers: {command|env|file: ...}).
                // Just saying "it should be a list" does not tell the user how to fix it
                // (a config still using the old key names gets the same guidance from
                // reject_renamed_keys)
                AppError::Config(format!(
                    "servers must be a list of server definitions. \
                     Source declarations (command / env / file) were \
                     removed; use the top-level {CONFIG_OVERRIDE_COMMAND_KEY} instead \
                     (note: it runs without a shell, so use an absolute path, e.g. \
                     `{CONFIG_OVERRIDE_COMMAND_KEY}: /bin/cat /Users/you/secrets/servers.yaml`)"
                ))
            })?;
        let templates = self
            .doc
            .get("server_templates")
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default();
        parse_server_entries(&servers, &templates, "config")
    }

    /// Returns a summary for info display (contains no secrets).
    pub fn info(&self) -> Result<ConfigInfo, AppError> {
        let config_path = match &self.source_path {
            Some(path) => path.display().to_string(),
            None => "(env QUERYFOLIO_CONFIG_YAML)".to_string(),
        };
        // After merging the key is dropped from the doc, so look at applied_override first.
        // Also fall back to the doc side so that a pre-merge (load) config can be displayed.
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

/// Summary for info display when config resolution fails (file missing / YAML broken /
/// the fetch command failed). The error message is put in source so that the frontend can
/// always show something.
pub fn config_info_error(error: &AppError) -> ConfigInfo {
    // Failure can mean not only "the file is missing" but also "it exists but the YAML is
    // broken", so existence is determined independently of whether parsing succeeded
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

/// Whether the config is overridden by the QUERYFOLIO_CONFIG_YAML environment variable.
/// While overridden there is no file to edit, so it cannot be edited from the editor.
fn config_env_override() -> bool {
    std::env::var("QUERYFOLIO_CONFIG_YAML")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Reads the contents of config.yml for the config editor.
/// If the file does not exist yet, creates the template first and then reads it.
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

/// Save from the config editor. Verifies the content is valid YAML before writing.
///
/// The write is done via a temp file + rename so that a failure midway does not leave the
/// existing config half-written and broken.
pub fn write_config_file(content: &str) -> Result<String, AppError> {
    if config_env_override() {
        return Err(AppError::Config(
            "The config is overridden by QUERYFOLIO_CONFIG_YAML, so it cannot be saved".into(),
        ));
    }
    write_config_file_in(&app_config_dir()?, content)
}

fn write_config_file_in(dir: &std::path::Path, content: &str) -> Result<String, AppError> {
    // Saving broken YAML as-is would lose the connection list on the next launch, so
    // verify before saving that it parses as a mapping
    parse_mapping(content, "the edited config")?;

    std::fs::create_dir_all(dir)?;
    let path = config_path_in(dir);

    // The config may contain connection passwords and SSH key passphrases in plain text, so
    // always write with owner-only (600). Even if the existing file is 644/640 etc., narrow it to
    // 600, matching the fix policy of ensure_config_file_in / AppConfig::load (inheriting the
    // existing permissions could leak to other users via the shared staff group on macOS)
    #[cfg(unix)]
    let mode = 0o600;

    let temp = path.with_extension("yml.tmp");
    // Specify the permissions at creation time. Calling set_permissions after writing would
    // leave the contents, which include passwords, with umask-dependent permissions (usually
    // 644) for that interval, letting other users on the same machine read them
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
        // mode only takes effect on creation, so also set it explicitly in case a temp
        // file was left behind by a previous interruption
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&temp, content)?;
    std::fs::rename(&temp, &path)?;
    Ok(path.display().to_string())
}

/// Determines the engine (sqlite / duckdb) that connects just by choosing a file, from the
/// extension. Returns None for unsupported extensions.
fn file_connection_engine(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "sqlite" | "sqlite3" | "db" => Some("sqlite"),
        "duckdb" => Some("duckdb"),
        _ => None,
    }
}

/// Result of `add_file_connection`.
#[derive(Debug, Serialize, PartialEq)]
pub struct FileConnection {
    /// Name of the connection that was added (or was already registered)
    pub name: String,
    /// Whether it was appended to config.yml. false (nothing written) if a connection for the same
    /// file already exists
    pub added: bool,
}

/// From "Open SQLite / DuckDB file…" on the zero-connection screen, appends one connection
/// for the chosen DB file to the servers in config.yml.
///
/// Re-parsing and re-writing the YAML would lose comments and key order, so it is appended as
/// text (`append_server_entry`). The write goes through the same `write_config_file_in` as the
/// config editor (YAML validation + 600 + rename from a temp file).
pub fn add_file_connection(path: &str) -> Result<FileConnection, AppError> {
    if config_env_override() {
        return Err(AppError::Config(
            "The config is overridden by QUERYFOLIO_CONFIG_YAML, so it cannot be saved".into(),
        ));
    }
    add_file_connection_in(&app_config_dir()?, path)
}

fn add_file_connection_in(dir: &std::path::Path, path: &str) -> Result<FileConnection, AppError> {
    let file = std::path::Path::new(path);
    let engine = file_connection_engine(file).ok_or_else(|| {
        AppError::Config(format!(
            "{path} is not a SQLite / DuckDB file (expected .sqlite, .sqlite3, .db or .duckdb)"
        ))
    })?;
    // The dialog only returns existing files, but writing a relative path or a vanished file
    // to the config would just leave a "connection that cannot connect", so reject it here
    if !file.is_absolute() || !file.is_file() {
        return Err(AppError::Config(format!("{path} is not an existing file")));
    }

    let text = read_config_file_in(dir)?;
    let doc = parse_mapping(&text, "the config")?;
    // Duplicate detection is done on the effective config, with templates expanded and groups
    // flattened (so that connections inheriting engine / schema from a template, or with a
    // path written in host instead of schema, are also picked up as the same file).
    // A config whose servers is not a list is treated as empty, and the append side
    // (append_server_entry) turns it into an error
    let servers = doc
        .get("servers")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();
    let templates = doc
        .get("server_templates")
        .and_then(|v| v.as_sequence())
        .cloned()
        .unwrap_or_default();
    let existing = parse_server_entries(&servers, &templates, "config")?;

    // If the same file is already registered with the same engine, return that connection
    // instead of adding it twice. An engine alias (sqlite3) is regarded as sqlite, as in
    // parse_engine in db.rs. Paths are compared by their real location with `..` and symlinks
    // resolved (so choosing the same file written differently does not create a duplicate).
    // Ones that cannot be resolved (the file an existing config points to does not exist,
    // etc.) are compared lexically
    let same_engine = |e: &str| {
        let e = e.to_ascii_lowercase();
        e == engine || (engine == "sqlite" && e == "sqlite3")
    };
    let canonical =
        |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = canonical(file);
    if let Some(server) = existing.iter().find(|s| {
        same_engine(&s.engine)
            && s.schema
                .as_deref()
                .or(s.host.as_deref())
                .map(|p| canonical(&expand_tilde(p)))
                .as_ref()
                == Some(&target)
    }) {
        return Ok(FileConnection {
            name: server.name.clone(),
            added: false,
        });
    }

    let base_name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let names: std::collections::HashSet<&str> = existing.iter().map(|s| s.name.as_str()).collect();
    let name = unique_connection_name(&base_name, &names);

    let updated = append_server_entry(&text, &name, engine, path)?;
    write_config_file_in(dir, &updated)?;
    Ok(FileConnection { name, added: true })
}

/// If base collides with an existing name, returns the first free one of `base (2)`, `base (3)`,
/// ...
fn unique_connection_name(base: &str, existing: &std::collections::HashSet<&str>) -> String {
    if !existing.contains(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} ({n})"))
        .find(|candidate| !existing.contains(candidate.as_str()))
        .expect("an unused name always exists")
}

/// Returns the text of config.yml with one entry appended to the end of the servers list.
///
/// To preserve comments, key order and how other items are written, it adds lines without
/// rebuilding the YAML. The three shapes handled are:
/// - `servers: []` (the first-launch template) — replace that line with a block-style `servers:`
/// - `servers:` + a block-style list — add right after the last item of the list. The
///   indentation (position of `- `) matches the first existing item
/// - no servers key — add `servers:` itself at the end of the file
///
/// Other shapes (flow style `[...]` with content, anchors, etc.) are not rewritten by guessing;
/// it errors out and asks for manual editing. Finally, the result is re-parsed to confirm that
/// "exactly one entry was added to the end of servers and nothing else changed" (so a
/// misjudgment in the line-based logic does not save a broken config or an append at an
/// unintended position).
fn append_server_entry(
    text: &str,
    name: &str,
    engine: &str,
    schema: &str,
) -> Result<String, AppError> {
    let unsupported = || {
        AppError::Config(
            "Could not add the connection automatically because of how servers is written \
             in config.yml. Add it with Edit config.yml instead."
                .into(),
        )
    };
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    // Always write values in double quotes. A JSON string is also a valid YAML double-quoted
    // scalar, so it does not break even if the file name contains `: ` or `#`, newlines, or
    // Windows `\`
    let quote = |s: &str| serde_json::to_string(s).expect("a string always serializes");
    let entry = |indent: &str| {
        format!(
            "{indent}- name: {name}{newline}{indent}  engine: {engine}{newline}{indent}  schema: {schema}{newline}",
            name = quote(name),
            schema = quote(schema),
        )
    };

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let content = |line: &str| line.trim_end_matches(['\r', '\n']).to_string();
    let servers_line = lines.iter().position(|line| {
        content(line)
            .strip_prefix("servers:")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
    });

    let mut out = String::with_capacity(text.len() + 128);
    match servers_line {
        None => {
            out.push_str(text);
            if !text.is_empty() && !text.ends_with('\n') {
                out.push_str(newline);
            }
            out.push_str("servers:");
            out.push_str(newline);
            out.push_str(&entry("  "));
        }
        Some(index) => {
            let value = content(lines[index])["servers:".len()..].trim().to_string();
            let (value, comment) = match value.find('#') {
                Some(pos) => (
                    value[..pos].trim().to_string(),
                    Some(value[pos..].to_string()),
                ),
                None => (value, None),
            };
            let is_empty_flow = value
                .strip_prefix('[')
                .and_then(|v| v.strip_suffix(']'))
                .is_some_and(|inner| inner.trim().is_empty());
            if is_empty_flow {
                // Replace `servers: []` with block style (keep any trailing comment)
                out.extend(lines[..index].iter().copied());
                out.push_str("servers:");
                if let Some(comment) = comment {
                    out.push(' ');
                    out.push_str(&comment);
                }
                out.push_str(newline);
                out.push_str(&entry("  "));
                out.extend(lines[index + 1..].iter().copied());
            } else if value.is_empty() {
                // Block style. The list range = indented lines plus lines starting with `-`
                // (a sequence at indent 0). Blank lines and comment lines never extend the range,
                // regardless of indentation (as in the template, a comment explaining the next key
                // can
                // follow). If a `#` line inside a block scalar is misread, the final verification
                // rejects
                // it as a value difference
                let mut last = index;
                let mut indent: Option<String> = None;
                for (offset, line) in lines[index + 1..].iter().enumerate() {
                    let line = content(line);
                    let trimmed = line.trim_start();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    let indented = line.starts_with([' ', '\t']);
                    let is_item = trimmed == "-" || trimmed.starts_with("- ");
                    if !indented && !is_item {
                        break;
                    }
                    last = index + 1 + offset;
                    if indent.is_none() && is_item {
                        indent = Some(line[..line.len() - trimmed.len()].to_string());
                    }
                }
                out.extend(lines[..=last].iter().copied());
                if !out.ends_with('\n') {
                    out.push_str(newline);
                }
                out.push_str(&entry(indent.as_deref().unwrap_or("  ")));
                out.extend(lines[last + 1..].iter().copied());
            } else {
                return Err(unsupported());
            }
        }
    }

    // Verify the result of the append: exactly one entry added to the end of servers and nothing
    // else changed
    let before = parse_mapping(text, "the config")?;
    let after = parse_mapping(&out, "the updated config").map_err(|_| unsupported())?;
    let servers_of = |doc: &serde_yaml::Mapping| match doc.get("servers") {
        None | Some(serde_yaml::Value::Null) => Some(Vec::new()),
        Some(serde_yaml::Value::Sequence(list)) => Some(list.clone()),
        Some(_) => None,
    };
    let mut expected = servers_of(&before).ok_or_else(unsupported)?;
    let mut new_entry = serde_yaml::Mapping::new();
    new_entry.insert("name".into(), name.into());
    new_entry.insert("engine".into(), engine.into());
    new_entry.insert("schema".into(), schema.into());
    expected.push(serde_yaml::Value::Mapping(new_entry));
    let mut rest_before = before.clone();
    rest_before.remove("servers");
    let mut rest_after = after.clone();
    rest_after.remove("servers");
    if servers_of(&after) != Some(expected) || rest_before != rest_after {
        return Err(unsupported());
    }
    Ok(out)
}

/// Whether config_override_command is set.
/// Used to toggle menu items. false if the config cannot be read.
pub fn has_config_override_command() -> bool {
    AppConfig::load()
        .map(|c| c.override_command().unwrap_or(None).is_some())
        .unwrap_or(false)
}

/// Runs config_override_command and returns the raw YAML it fetched.
/// For the copy view (editable at the destination, but not saved). Errors if unset.
///
/// It **intentionally does not go through** AppState's merged-config cache. This view is for
/// checking, formatting and copying the current value in the storage location (1Password etc.),
/// so it fetches the latest each time rather than the cache taken at startup. The command
/// runs once per open (with 1Password, authentication may be required each time).
pub async fn fetch_override_config_yaml() -> Result<String, AppError> {
    let config = AppConfig::load()?;
    match config.override_command()? {
        Some(command) => run_source_command(&command).await,
        None => Err(AppError::Config(format!(
            "The config has no {CONFIG_OVERRIDE_COMMAND_KEY}"
        ))),
    }
}

/// Recursively merges the fetched YAML (over) into the local config (base). over takes precedence.
/// It descends and mixes only when both values are mappings; otherwise (scalars and
/// sequences) over replaces wholesale. Lists like servers are not merged element-wise
/// because there is no stable identity to decide which elements are "the same item"
/// (merging by name match would cause unintended partial application).
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
    let mapping = doc.as_mapping().cloned().ok_or_else(|| {
        AppError::Config(format!("{source} is not a YAML mapping"))
    })?;
    reject_renamed_keys(&mapping, source)?;
    Ok(mapping)
}

/// Explicitly rejects the old key names (sql_servers / sql_server_templates).
/// Silently ignoring them would leave the state of just "no connections appear" with no clue
/// to the cause, so it returns an error that guides the rename.
fn reject_renamed_keys(doc: &serde_yaml::Mapping, source: &str) -> Result<(), AppError> {
    for (old, new) in [
        ("sql_servers", "servers"),
        ("sql_server_templates", "server_templates"),
    ] {
        let Some(value) = doc.get(old) else {
            continue;
        };
        let mut message = format!("'{old}' in {source} was renamed to '{new}'");
        if old == "sql_servers" {
            message.push_str(" (group entries use 'servers' too)");
            // The old format (sql_servers: {command|env|file: ...}) was abolished before the
            // rename.
            // Guiding only the rename would trip the user twice ("I fixed it to a list and it still
            // doesn't work"), so if the value is a mapping, also tell them where the source
            // declaration moved to
            if value.is_mapping() {
                message.push_str(&format!(
                    ". Source declarations (command / env / file) were also removed; \
                     write a list of server definitions and fetch secrets with the \
                     top-level {CONFIG_OVERRIDE_COMMAND_KEY}"
                ));
            }
        }
        return Err(AppError::Config(message));
    }
    Ok(())
}

/// Rejects old keys left in a server entry (inside or outside a group).
/// The top-level reject_renamed_keys does not reach nested positions, so each entry in
/// parse_server_entries goes through here.
fn reject_renamed_server_key(entry: &serde_yaml::Value, source: &str) -> Result<(), AppError> {
    let has_old_key = entry
        .as_mapping()
        .is_some_and(|m| m.contains_key("sql_servers"));
    if has_old_key {
        return Err(AppError::Config(format!(
            "'sql_servers' in a servers entry in {source} was renamed to 'servers'"
        )));
    }
    Ok(())
}

/// Parses the items of the servers list. Each item is one of:
/// - A server definition itself
/// - A group entry (group_name + a nested servers list).
///   Flattened into the nested servers, recording group_name on each server.
///   Recursion (a group inside a group) is forbidden (depth 1 only).
fn parse_server_entries(
    servers: &[serde_yaml::Value],
    templates: &[serde_yaml::Value],
    source: &str,
) -> Result<Vec<ServerConfig>, AppError> {
    let mut result = Vec::new();
    for entry_value in servers {
        let entry = entry_value.as_mapping().ok_or_else(|| {
            AppError::Config(format!("A servers entry in {source} is not a mapping"))
        })?;
        reject_renamed_server_key(entry_value, source)?;
        if !entry.contains_key("servers") {
            result.push(parse_server_entry(entry_value, templates, source)?);
            continue;
        }

        // Group entry. Reject unknown keys so typos are not silently swallowed
        for (key, _) in entry {
            let key = key.as_str().unwrap_or_default();
            if key != "group_name" && key != "servers" {
                return Err(AppError::Config(format!(
                    "Unknown key '{key}' in a servers group entry in {source} \
                     (only group_name / servers are allowed)"
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
                    "A servers group entry in {source} requires a non-empty group_name"
                ))
            })?;
        let grouped = entry
            .get("servers")
            .and_then(|v| v.as_sequence())
            .ok_or_else(|| {
                AppError::Config(format!(
                    "servers in group '{group_name}' in {source} must be a list"
                ))
            })?;
        for server_value in grouped {
            // Catch old keys left on servers inside a group here too. Letting them through would
            // have them dropped as an unknown field by ServerConfig, producing an unrelated error
            // such as `missing field \`name\``
            reject_renamed_server_key(server_value, source)?;
            let is_nested_group = server_value
                .as_mapping()
                .is_some_and(|m| m.contains_key("servers"));
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
            "Failed to parse a servers entry in {source}: {e}"
        ))
    })
}

/// Runs config_override_command and returns stdout.
///
/// Splits into argv with shlex and runs without a shell. Even if shell metacharacters slip in
/// they are not interpreted, so there is no room for command injection. In exchange,
/// pipes, redirects and variable expansion are unavailable (a single command is assumed).
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
            // The PATH of a GUI launched from Finder / Dock is minimal (/usr/bin:/bin etc.) and
            // Homebrew's op etc. would not be found, so add the usual paths
            .env("PATH", supplemented_path())
            // Do not leave the child process behind when the future is dropped on timeout
            // (prevents an `op` hung waiting for authentication from becoming an orphan and being
            // launched multiple times by retries)
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

/// Returns PATH with usual directories such as Homebrew's added.
fn supplemented_path() -> String {
    supplement_path(&std::env::var("PATH").unwrap_or_default())
}

pub(crate) fn supplement_path(base: &str) -> String {
    // What we add are the usual Unix directories, and the separator assumes `:`. The Windows PATH
    // is
    // `;`-separated, so joining with `:` as-is would turn the last element of PATH into a
    // nonexistent single element like `C:\last\entry:/opt/homebrew/bin` and lose it.
    // There is no directory worth adding on Windows either, so pass it through.
    if cfg!(windows) {
        return base.to_string();
    }
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

/// Makes a server entry with `template: <name>` inherit the server_templates entry of the
/// same name by shallow merge.
/// Keys specified on the server side override the same-named keys of the template.
fn expand_template(
    server_value: &serde_yaml::Value,
    templates: &[serde_yaml::Value],
) -> Result<serde_yaml::Value, AppError> {
    let server_map = server_value
        .as_mapping()
        .ok_or_else(|| AppError::Config("A servers entry is not a mapping".into()))?;

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
                "Template '{template_name}' not found in server_templates"
            ))
        })?;

    let mut merged = template.clone();
    // The template's own name is not a server name, so remove it
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
servers:
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
        // Group entries are flattened and each server gets a group_name.
        // A mix of groups and directly written servers is also resolved in config order
        let config = config_from_yaml(
            r#"
servers:
  - group_name: production
    servers:
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
    servers:
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
        // It also propagates to ConnectionInfo
        let info = ConnectionInfo::from(&servers[0]);
        assert_eq!(info.group_name.as_deref(), Some("production"));
    }

    #[tokio::test]
    async fn test_flat_entry_group_name_is_ignored() {
        // A group_name: directly under a server entry could bypass the group entry validation,
        // so it is not deserialized (it is ignored)
        let config = config_from_yaml(
            r#"
servers:
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
servers:
  - group_name: ""
    servers:
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
servers:
  - group_name: outer
    servers:
      - group_name: inner
        servers:
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
        // Do not silently ignore typos in a group entry (servers: etc.)
        let config = config_from_yaml(
            r#"
servers:
  - group_name: g
    servers: []
    description: typo-extra-key
"#,
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("Unknown key 'description'"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_group_with_template() {
        // Servers inside a group can also inherit server_templates
        let config = config_from_yaml(
            r#"
servers:
  - group_name: shared
    servers:
      - name: db-a
        template: base
        schema: a_db
server_templates:
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
        // readonly is optional (default false). Specifying true propagates to ConnectionInfo
        let config = config_from_yaml(
            r#"
servers:
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
        // ConnectionInfo passes host/port/user and SSH tunnel info (secrets excluded) to the
        // frontend. Passwords and keys are not included.
        let config = config_from_yaml(
            r#"
servers:
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
        // Verify that secrets do not leak into the serialization
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
servers:
  - template: shared-host
    name: app-db
    schema: app_db
  - template: shared-host
    name: log-db
    schema: log_db
    port: 3307
server_templates:
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
        // A server-side specification overrides the template
        assert_eq!(servers[1].port, Some(3307));
    }

    /// For testing the path that fetches the override YAML via a command and recursively merges it
    /// into the config, builds the equivalent of load_merged from the local config + fetched YAML.
    /// (load_merged itself reads real files, so here we verify the merge part)
    fn merged_from(local_yaml: &str, fetched_yaml: &str) -> AppConfig {
        let mut config = config_from_yaml(local_yaml);
        let overrides = parse_mapping(fetched_yaml, "test override").unwrap();
        merge_mapping(&mut config.doc, &overrides);
        config.doc.remove(CONFIG_OVERRIDE_COMMAND_KEY);
        config.applied_override = Some("test-command".to_string());
        config
    }

    /// Builds a config_override_command that just prints the given YAML as one line.
    ///
    /// Windows has no `/bin/echo`, so use cmd.exe's echo (these two tests run on the same OS as
    /// the release build = they run on Windows too).
    ///
    /// The key point is that the arguments contain no whitespace. With whitespace, std wraps the
    /// argument in quotes, and cmd.exe's echo prints the quotes as well, breaking the YAML. echo
    /// prints the arguments it receives joined by spaces, so letting shlex split them yields the
    /// same single line.
    /// Therefore the caller writes the YAML as a **double-quoted scalar** (a string containing
    /// `: ` cannot be written as a YAML plain scalar, but quoting it needs no backslash escapes,
    /// and shlex also splits it cleanly on whitespace).
    fn echo_command(yaml: &str) -> String {
        if cfg!(windows) {
            format!("cmd /c echo {yaml}")
        } else {
            format!("/bin/echo {yaml}")
        }
    }

    /// Goes through the real path of load_merged (load config -> run command -> merge).
    /// It uses QUERYFOLIO_CONFIG_YAML, so this is the only test in this process that touches env
    /// (other tests use config_from_yaml and do not read env).
    #[tokio::test]
    async fn test_load_merged_runs_command_and_merges_result() {
        let command = echo_command("default_limit: 7");
        std::env::set_var(
            "QUERYFOLIO_CONFIG_YAML",
            format!("servers: []\ndefault_limit: 500\nconfig_override_command: \"{command}\"\n"),
        );
        let config = AppConfig::load_merged().await.unwrap();
        std::env::remove_var("QUERYFOLIO_CONFIG_YAML");

        // The fetched YAML's values are applied, and the key itself has been dropped
        assert_eq!(config.default_limit(), 7);
        assert!(config.override_command().unwrap().is_none());
        assert!(config.info().unwrap().source.contains("echo"));
    }

    #[tokio::test]
    async fn test_override_command_is_executed_and_merged() {
        // Have echo print the override YAML and go through the same path as load_merged.
        // Write it in flow style to fit on one line (echo cannot emit newlines)
        let yaml = run_source_command(&echo_command(
            "servers: [{name: fetched, engine: sqlite, schema: /tmp/x.db}]",
        ))
        .await
        .unwrap();
        assert!(yaml.contains("fetched"));
    }

    #[test]
    fn test_override_replaces_servers_wholesale() {
        // servers is a list, so it is replaced wholesale rather than merged element-wise
        let config = merged_from(
            r#"
servers:
  - name: local-a
    engine: sqlite
    schema: /tmp/a.db
  - name: local-b
    engine: sqlite
    schema: /tmp/b.db
"#,
            r#"
servers:
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
        // Keys other than servers can be overridden too (the biggest difference from the old
        // format)
        let config = merged_from(
            "servers: []\ndefault_limit: 500\nsqlfiles_dir: ~/local\n",
            "default_limit: 42\n",
        );
        assert_eq!(config.default_limit(), 42);
        // Keys absent from the override YAML keep the local values
        assert!(config
            .resolve_sqlfiles_dir()
            .unwrap()
            .to_string_lossy()
            .ends_with("local"));
    }

    #[test]
    fn test_override_merges_mappings_recursively() {
        // Mappings are mixed recursively (the local model stays and only api_key is overridden)
        let config = merged_from(
            "servers: []\nai:\n  provider: openai\n  model: local-model\n  api_key: sk-local\n",
            "ai:\n  api_key: sk-fetched\n",
        );
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-fetched"));
        assert_eq!(ai.get("model").and_then(serde_yaml::Value::as_str), Some("local-model"));
        assert_eq!(ai.get("provider").and_then(serde_yaml::Value::as_str), Some("openai"));
    }

    #[test]
    fn test_override_ai_wins_over_local_ai() {
        // Setup where the API key is kept on the 1Password side: the fetched YAML's ai takes
        // precedence
        let config = merged_from(
            "servers: []\nai:\n  api_key: sk-local\n",
            "ai:\n  api_key: sk-fetched\n",
        );
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-fetched"));
    }

    #[test]
    fn test_local_ai_survives_without_override_ai() {
        let config = merged_from("servers: []\nai:\n  api_key: sk-local\n", "default_limit: 10\n");
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-local"));
    }

    #[test]
    fn test_override_key_is_dropped_after_merge() {
        // Do not fetch recursively even if the fetched YAML has config_override_command
        let config = merged_from(
            "servers: []\nconfig_override_command: local-cmd\n",
            "config_override_command: fetched-cmd\nservers: []\n",
        );
        assert!(config.override_command().unwrap().is_none());
        // The applied command remains for the info display
        assert!(config.info().unwrap().source.contains("test-command"));
    }

    #[test]
    fn test_no_override_command_reports_inline() {
        let config = config_from_yaml("servers: []\n");
        assert!(config.override_command().unwrap().is_none());
        assert_eq!(config.info().unwrap().source, "inline");
    }

    #[test]
    fn test_override_command_is_read_from_config() {
        let config = config_from_yaml("servers: []\nconfig_override_command: op read x\n");
        assert_eq!(config.override_command().unwrap().as_deref(), Some("op read x"));
        assert!(config.info().unwrap().source.contains("op read x"));
    }

    #[test]
    fn test_blank_override_command_is_error() {
        // If an empty string were silently treated as "unset", you would not notice that the
        // override has no effect and the app is running on the local config
        let config = config_from_yaml("servers: []\nconfig_override_command: \"   \"\n");
        let err = config.override_command().unwrap_err().to_string();
        assert!(err.contains("is empty"));
    }

    #[test]
    fn test_non_string_override_command_is_error() {
        // Type errors are not tolerated, including when the old format's mapping shape was written
        for yaml in [
            "servers: []\nconfig_override_command: 123\n",
            "servers: []\nconfig_override_command:\n  command: op read x\n",
        ] {
            let config = config_from_yaml(yaml);
            let err = config.override_command().unwrap_err().to_string();
            assert!(err.contains("must be a string"), "unexpected error: {err}");
        }
    }

    #[test]
    fn test_old_servers_source_declaration_explains_migration() {
        // Tell users who upgraded with the old-format config where to migrate to
        let config = config_from_yaml("servers:\n  file: ~/secrets/servers.yaml\n");
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("config_override_command"), "unexpected error: {err}");
    }

    #[test]
    fn test_servers_mapping_is_rejected() {
        // Do not support an old-format source declaration even if only its key was renamed
        let config = config_from_yaml("servers:\n  command: op read x\n");
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("must be a list"));
    }

    #[test]
    fn test_renamed_keys_are_rejected_with_guidance() {
        // sql_servers / sql_server_templates have been renamed to servers / server_templates.
        // Silently ignoring them would give zero connections with no clue to the cause, so it
        // errors
        let err = parse_mapping("sql_servers: []\n", "test").unwrap_err().to_string();
        assert!(err.contains("renamed to 'servers'"), "unexpected error: {err}");

        let err = parse_mapping("servers: []\nsql_server_templates: []\n", "test")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("renamed to 'server_templates'"),
            "unexpected error: {err}"
        );
        // Templates cannot be written in a group entry, so do not add that note
        assert!(!err.contains("group entries"), "unexpected error: {err}");
    }

    #[test]
    fn test_old_source_declaration_under_old_key_explains_migration() {
        // Old key + old-format source declaration. Guiding only the rename would trip the user
        // twice ("I fixed it to a list and it still doesn't work"), so also tell them where to
        // migrate
        let err = parse_mapping("sql_servers:\n  file: ~/secrets/servers.yaml\n", "test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("renamed to 'servers'"), "unexpected error: {err}");
        assert!(
            err.contains(CONFIG_OVERRIDE_COMMAND_KEY),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_renamed_key_in_group_entry_is_rejected() {
        let config = config_from_yaml(
            "\
servers:
  - group_name: Production
    sql_servers:
      - name: a
        engine: sqlite
        schema: /tmp/a.db
",
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("renamed to 'servers'"), "unexpected error: {err}");
    }

    #[test]
    fn test_renamed_key_inside_a_group_is_rejected() {
        // Old keys left on servers inside a group. ServerConfig silently drops unknown fields,
        // so without rejecting them an unrelated error would result
        let config = config_from_yaml(
            "\
servers:
  - group_name: Production
    servers:
      - group_name: Nested
        sql_servers:
          - name: a
            engine: sqlite
            schema: /tmp/a.db
",
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("renamed to 'servers'"), "unexpected error: {err}");
    }

    #[test]
    fn test_default_limit() {
        let config = config_from_yaml("servers: []\n");
        assert_eq!(config.default_limit(), 500);
        let config = config_from_yaml("servers: []\ndefault_limit: 100\n");
        assert_eq!(config.default_limit(), 100);
        let config = config_from_yaml("servers: []\ndefault_limit: 0\n");
        assert_eq!(config.default_limit(), 0);
    }

    #[test]
    fn test_sqlfiles_dir_default_and_custom() {
        let config = config_from_yaml("servers: []\n");
        let default_dir = config.resolve_sqlfiles_dir().unwrap();
        assert!(default_dir.ends_with(".config/queryfolio/sqlfiles"));

        let config = config_from_yaml("servers: []\nsqlfiles_dir: ~/my-queries\n");
        let custom = config.resolve_sqlfiles_dir().unwrap();
        assert_eq!(custom, dirs::home_dir().unwrap().join("my-queries"));

        // A relative path is based on the config directory, not cwd (independent of the process).
        // Because the CLI (the writing side) and the running instance (the opening side) have
        // different cwds.
        let config = config_from_yaml("servers: []\nsqlfiles_dir: my-queries\n");
        let relative = config.resolve_sqlfiles_dir().unwrap();
        assert_eq!(relative, app_config_dir().unwrap().join("my-queries"));

        let config = config_from_yaml("servers: []\nsqlfiles_dir: ./a/../b\n");
        let relative = config.resolve_sqlfiles_dir().unwrap();
        assert_eq!(relative, app_config_dir().unwrap().join("./a/../b"));
    }

    #[test]
    fn test_ensure_config_file_in() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-ensure-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // If absent, create it and return Some(path)
        let created = ensure_config_file_in(&dir).unwrap();
        assert!(created.is_some());
        assert!(dir.join("config.yml").exists());

        // A new file is created with 600 (not the umask-dependent 644)
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

        // If it already exists, None (do not overwrite)
        assert!(ensure_config_file_in(&dir).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// If an existing config.yml was left with loose permissions (644), ensure_config_file_in
    /// at startup tightens it to 600 (contents unchanged).
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

        // Simulate an existing file created by hand with 644, readable by other users
        let path = dir.join("config.yml");
        std::fs::write(&path, "servers: []\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // It already exists, so None is returned while the permissions are tightened to 600
        assert!(ensure_config_file_in(&dir).unwrap().is_none());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Contents are not rewritten
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "servers: []\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// If config.yml is a directory, tighten does not change the permissions
    /// (setting 600 would drop the search bit and make it inaccessible, so leave it alone).
    #[cfg(unix)]
    #[test]
    fn test_tighten_config_permissions_skips_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "queryfolio-tighten-dir-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // Create a directory named config.yml (an abnormal state)
        let as_dir = dir.join("config.yml");
        std::fs::create_dir_all(&as_dir).unwrap();
        std::fs::set_permissions(&as_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        tighten_config_permissions(&as_dir).unwrap();

        // The directory's permissions are unchanged (not set to 600)
        let mode = std::fs::metadata(&as_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reading and writing in the config editor. If absent, creates the template and then reads;
    /// what was saved can be read back as-is.
    #[test]
    fn test_read_write_config_file_in() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // Even with no file, the template is created and can be read
        let initial = read_config_file_in(&dir).unwrap();
        assert!(initial.contains("servers"));

        let edited = "servers:\n  - name: edited\n    engine: sqlite\n    schema: /tmp/a.db\n";
        let saved_path = write_config_file_in(&dir, edited).unwrap();
        assert_eq!(saved_path, dir.join("config.yml").display().to_string());
        assert_eq!(read_config_file_in(&dir).unwrap(), edited);
        // No temp file is left behind
        assert!(!dir.join("config.yml.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Broken YAML is refused on save and the existing config is kept.
    #[test]
    fn test_write_config_file_in_rejects_invalid_yaml() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-invalid-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let valid = "servers: []\n";
        write_config_file_in(&dir, valid).unwrap();

        // Content that cannot be parsed as a mapping
        assert!(write_config_file_in(&dir, "servers: [\n").is_err());
        // Valid YAML but not a mapping
        assert!(write_config_file_in(&dir, "- just\n- a list\n").is_err());
        // The existing content is not broken
        assert_eq!(read_config_file_in(&dir).unwrap(), valid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On save it is always written with 600 (both new files and tightening loose existing
    /// permissions).
    #[cfg(unix)]
    #[test]
    fn test_write_config_file_in_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-perm-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // A new file gets 600
        write_config_file_in(&dir, "servers: []\n").unwrap();
        let path = dir.join("config.yml");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Even if the existing permissions are loose (640), save tightens them to 600
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_config_file_in(&dir, "servers: []\n# edited\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Even when config.yaml (extension yaml) is in use, save to that file.
    #[test]
    fn test_write_config_file_in_keeps_yaml_extension() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-yaml-ext-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "servers: []\n").unwrap();

        let edited = "servers: []\n# edited\n";
        let saved_path = write_config_file_in(&dir, edited).unwrap();
        assert_eq!(saved_path, dir.join("config.yaml").display().to_string());
        assert!(!dir.join("config.yml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yaml")).unwrap(),
            edited
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Resolves the text after the append into a list of (name, engine, schema, group_name).
    fn resolved_servers(yaml: &str) -> Vec<(String, String, Option<String>, Option<String>)> {
        config_from_yaml(yaml)
            .resolve_servers()
            .unwrap()
            .into_iter()
            .map(|s| (s.name, s.engine, s.schema, s.group_name))
            .collect()
    }

    /// Appending to the first-launch template (`servers: []`) replaces that line with block
    /// style, and all surrounding comments are kept.
    #[test]
    fn test_append_server_entry_replaces_empty_servers() {
        let updated =
            append_server_entry(CONFIG_TEMPLATE, "a.sqlite3", "sqlite", "/data/a.sqlite3").unwrap();
        assert!(updated.contains(
            "servers:\n  - name: \"a.sqlite3\"\n    engine: sqlite\n    schema: \"/data/a.sqlite3\"\n"
        ));
        assert!(!updated.contains("servers: []"));
        // Not a single comment line is lost
        for line in CONFIG_TEMPLATE.lines().filter(|l| l.starts_with('#')) {
            assert!(updated.contains(line), "lost comment: {line}");
        }
        assert_eq!(
            resolved_servers(&updated),
            vec![(
                "a.sqlite3".to_string(),
                "sqlite".to_string(),
                Some("/data/a.sqlite3".to_string()),
                None
            )]
        );
    }

    /// `servers: []` with a trailing comment is also replaced, keeping the comment.
    #[test]
    fn test_append_server_entry_keeps_trailing_comment_of_empty_servers() {
        let text = "default_limit: 100\nservers: [ ]  # none yet\nsqlfiles_dir: ~/q\n";
        let updated = append_server_entry(text, "x.duckdb", "duckdb", "/x.duckdb").unwrap();
        assert_eq!(
            updated,
            "default_limit: 100\nservers: # none yet\n  - name: \"x.duckdb\"\n    engine: duckdb\n    schema: \"/x.duckdb\"\nsqlfiles_dir: ~/q\n"
        );
    }

    /// If servers already exists, add right after the last item. Match the indentation of the
    /// existing items; the positions of following keys and explanatory comments do not change.
    #[test]
    fn test_append_server_entry_after_existing_servers() {
        let text = "# head\nservers:\n    - name: pg  # main db\n      engine: postgres\n      host: localhost\n\n    # - name: old\n    - name: local\n      engine: sqlite\n      schema: ~/a.db\n\n# about ai\nai:\n  provider: openai\n";
        let updated = append_server_entry(text, "b.db", "sqlite", "/b.db").unwrap();
        assert_eq!(
            updated,
            "# head\nservers:\n    - name: pg  # main db\n      engine: postgres\n      host: localhost\n\n    # - name: old\n    - name: local\n      engine: sqlite\n      schema: ~/a.db\n    - name: \"b.db\"\n      engine: sqlite\n      schema: \"/b.db\"\n\n# about ai\nai:\n  provider: openai\n"
        );
        let names: Vec<String> = resolved_servers(&updated)
            .into_iter()
            .map(|s| s.0)
            .collect();
        assert_eq!(names, vec!["pg", "local", "b.db"]);
    }

    /// Indented comments after the list are regarded as the explanation of the next key, and the
    /// entry is added before them. If the range is misread because of a `#` line inside a block
    /// scalar, it errors without writing.
    #[test]
    fn test_append_server_entry_indented_comment_before_next_key() {
        let text = "servers:\n  - name: a\n    engine: sqlite\n    schema: /a.db\n  # AI settings\nai:\n  provider: openai\n";
        let updated = append_server_entry(text, "b.db", "sqlite", "/b.db").unwrap();
        assert_eq!(
            updated,
            "servers:\n  - name: a\n    engine: sqlite\n    schema: /a.db\n  - name: \"b.db\"\n    engine: sqlite\n    schema: \"/b.db\"\n  # AI settings\nai:\n  provider: openai\n"
        );

        let block = "servers:\n  - name: a\n    engine: sqlite\n    description: |\n      foo\n      # bar\n";
        let err = append_server_entry(block, "b.db", "sqlite", "/b.db")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Edit config.yml"), "{err}");
    }

    /// Can also add to a list where `- ` is in the same column as the key (a sequence at indent 0).
    #[test]
    fn test_append_server_entry_zero_indent_sequence() {
        let text = "servers:\n- name: pg\n  engine: postgres\n  host: h\ndefault_limit: 10\n";
        let updated = append_server_entry(text, "c.db", "sqlite", "/c.db").unwrap();
        assert_eq!(
            updated,
            "servers:\n- name: pg\n  engine: postgres\n  host: h\n- name: \"c.db\"\n  engine: sqlite\n  schema: \"/c.db\"\ndefault_limit: 10\n"
        );
    }

    /// With group-style servers, add at the end of the top level, not inside a group
    /// (it appears last in the list as a connection outside any group).
    #[test]
    fn test_append_server_entry_with_groups() {
        let text = "servers:\n  - group_name: prod\n    servers:\n      - name: p1\n        engine: postgres\n        host: h\n  - group_name: dev\n    servers:\n      - name: d1\n        engine: sqlite\n        schema: ~/d.db\n";
        let updated = append_server_entry(text, "e.duckdb", "duckdb", "/e.duckdb").unwrap();
        assert!(updated.ends_with(
            "        schema: ~/d.db\n  - name: \"e.duckdb\"\n    engine: duckdb\n    schema: \"/e.duckdb\"\n"
        ));
        assert_eq!(
            resolved_servers(&updated),
            vec![
                ("p1".into(), "postgres".into(), None, Some("prod".into())),
                (
                    "d1".into(),
                    "sqlite".into(),
                    Some("~/d.db".into()),
                    Some("dev".into())
                ),
                (
                    "e.duckdb".into(),
                    "duckdb".into(),
                    Some("/e.duckdb".into()),
                    None
                ),
            ]
        );
    }

    /// If there is no servers key, add it at the end. Does not break even without a trailing
    /// newline.
    #[test]
    fn test_append_server_entry_without_servers_key() {
        let updated = append_server_entry("default_limit: 5", "f.db", "sqlite", "/f.db").unwrap();
        assert_eq!(
            updated,
            "default_limit: 5\nservers:\n  - name: \"f.db\"\n    engine: sqlite\n    schema: \"/f.db\"\n"
        );
    }

    /// Even if the last item's line has no newline (end of file), add without joining the lines.
    #[test]
    fn test_append_server_entry_without_trailing_newline() {
        let text = "servers:\n  - name: pg\n    engine: postgres";
        let updated = append_server_entry(text, "g.db", "sqlite", "/g.db").unwrap();
        assert_eq!(
            updated,
            "servers:\n  - name: pg\n    engine: postgres\n  - name: \"g.db\"\n    engine: sqlite\n    schema: \"/g.db\"\n"
        );
    }

    /// Add to a CRLF file with CRLF.
    #[test]
    fn test_append_server_entry_keeps_crlf() {
        let text = "# c\r\nservers: []\r\n";
        let updated = append_server_entry(text, "h.db", "sqlite", "/h.db").unwrap();
        assert_eq!(
            updated,
            "# c\r\nservers:\r\n  - name: \"h.db\"\r\n    engine: sqlite\r\n    schema: \"/h.db\"\r\n"
        );
    }

    /// Names and paths containing characters special in YAML can also be read back as the exact
    /// values.
    #[test]
    fn test_append_server_entry_quotes_special_characters() {
        let name = "a: b # c \"d\" 'e'.db";
        let path = "C:\\Users\\me\\#data: x.db";
        let updated = append_server_entry("servers: []\n", name, "sqlite", path).unwrap();
        assert_eq!(
            resolved_servers(&updated),
            vec![(
                name.to_string(),
                "sqlite".to_string(),
                Some(path.to_string()),
                None
            )]
        );
    }

    /// Shapes that cannot be safely appended line by line, such as flow style with content or
    /// anchors, error out without being rewritten.
    #[test]
    fn test_append_server_entry_rejects_unsupported_forms() {
        for text in [
            "servers: [{name: a, engine: sqlite, schema: /a.db}]\n",
            "servers: &s\n  - name: a\n    engine: sqlite\n",
            "servers:\n  [\n    {name: a, engine: sqlite}\n  ]\n",
        ] {
            let err = append_server_entry(text, "z.db", "sqlite", "/z.db")
                .unwrap_err()
                .to_string();
            assert!(err.contains("Edit config.yml"), "{text}: {err}");
        }
    }

    #[test]
    fn test_unique_connection_name() {
        let existing: std::collections::HashSet<&str> =
            ["a.db", "a.db (2)", "b.db"].into_iter().collect();
        assert_eq!(unique_connection_name("c.db", &existing), "c.db");
        assert_eq!(unique_connection_name("b.db", &existing), "b.db (2)");
        assert_eq!(unique_connection_name("a.db", &existing), "a.db (3)");
    }

    #[test]
    fn test_file_connection_engine() {
        let engine = |p: &str| file_connection_engine(std::path::Path::new(p));
        assert_eq!(engine("/a/x.sqlite"), Some("sqlite"));
        assert_eq!(engine("/a/x.SQLITE3"), Some("sqlite"));
        assert_eq!(engine("/a/x.db"), Some("sqlite"));
        assert_eq!(engine("/a/x.duckdb"), Some("duckdb"));
        assert_eq!(engine("/a/x.csv"), None);
        assert_eq!(engine("/a/noext"), None);
    }

    fn add_file_test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-add-file-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        dir
    }

    /// Adding from a state with no config file creates the template and then appends.
    /// A connection with the same name (including inside groups) gets a sequence number, and
    /// nothing is appended for the same file.
    #[test]
    fn test_add_file_connection_in() {
        let dir = add_file_test_dir("flow");
        let db = dir.join("data").join("sales.sqlite3");
        std::fs::write(&db, b"").unwrap();
        let db_path = db.display().to_string();

        let first = add_file_connection_in(&dir, &db_path).unwrap();
        assert_eq!(
            first,
            FileConnection {
                name: "sales.sqlite3".into(),
                added: true
            }
        );
        let text = std::fs::read_to_string(dir.join("config.yml")).unwrap();
        assert!(text.starts_with("# Queryfolio config file"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("config.yml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Choosing the same file again does not add it twice
        let again = add_file_connection_in(&dir, &db_path).unwrap();
        assert_eq!(
            again,
            FileConnection {
                name: "sales.sqlite3".into(),
                added: false
            }
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yml")).unwrap(),
            text
        );

        // A same-named file in a different directory is added with a sequence number
        std::fs::create_dir_all(dir.join("other")).unwrap();
        let other = dir.join("other").join("sales.sqlite3");
        std::fs::write(&other, b"").unwrap();
        let second = add_file_connection_in(&dir, &other.display().to_string()).unwrap();
        assert_eq!(second.name, "sales.sqlite3 (2)");
        assert!(second.added);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A same name inside a group is also regarded as a duplicate.
    #[test]
    fn test_add_file_connection_in_avoids_names_in_groups() {
        let dir = add_file_test_dir("group");
        std::fs::write(
            dir.join("config.yml"),
            "servers:\n  - group_name: g\n    servers:\n      - name: w.duckdb\n        engine: postgres\n        host: h\n",
        )
        .unwrap();
        let db = dir.join("data").join("w.duckdb");
        std::fs::write(&db, b"").unwrap();
        let added = add_file_connection_in(&dir, &db.display().to_string()).unwrap();
        assert_eq!(added.name, "w.duckdb (2)");
        let text = std::fs::read_to_string(dir.join("config.yml")).unwrap();
        let servers = resolved_servers(&text);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].1, "duckdb");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Connections inheriting engine / schema from a template, or with a path written in host,
    /// are also treated as the same file and not added twice.
    #[test]
    fn test_add_file_connection_in_detects_resolved_duplicates() {
        let dir = add_file_test_dir("resolved");
        let a = dir.join("data").join("a.db");
        let b = dir.join("data").join("b.sqlite3");
        std::fs::write(&a, b"").unwrap();
        std::fs::write(&b, b"").unwrap();
        // A Windows path contains `\`, so write it as a JSON string (= YAML double quotes)
        let quote = |p: &PathBuf| serde_json::to_string(&p.display().to_string()).unwrap();
        let yaml = format!(
            "server_templates:\n  - name: t\n    engine: sqlite3\n    schema: {}\nservers:\n  - name: via-template\n    template: t\n  - name: via-host\n    engine: sqlite\n    host: {}\n",
            quote(&a),
            quote(&b)
        );
        std::fs::write(dir.join("config.yml"), &yaml).unwrap();

        let found = add_file_connection_in(&dir, &a.display().to_string()).unwrap();
        assert_eq!(
            found,
            FileConnection {
                name: "via-template".into(),
                added: false
            }
        );
        let found = add_file_connection_in(&dir, &b.display().to_string()).unwrap();
        assert_eq!(
            found,
            FileConnection {
                name: "via-host".into(),
                added: false
            }
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yml")).unwrap(),
            yaml
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Choosing the same file via a path containing `..` or via a symlink is not duplicated.
    #[cfg(unix)]
    #[test]
    fn test_add_file_connection_in_compares_canonical_paths() {
        let dir = add_file_test_dir("canonical");
        let db = dir.join("data").join("c.db");
        std::fs::write(&db, b"").unwrap();
        let first = add_file_connection_in(&dir, &db.display().to_string()).unwrap();
        assert!(first.added);

        let dotted = dir.join("data").join("..").join("data").join("c.db");
        let again = add_file_connection_in(&dir, &dotted.display().to_string()).unwrap();
        assert_eq!(
            again,
            FileConnection {
                name: "c.db".into(),
                added: false
            }
        );

        let link = dir.join("link.db");
        std::os::unix::fs::symlink(&db, &link).unwrap();
        let via_link = add_file_connection_in(&dir, &link.display().to_string()).unwrap();
        assert_eq!(
            via_link,
            FileConnection {
                name: "c.db".into(),
                added: false
            }
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unsupported extensions, nonexistent files and relative paths are not written to the config.
    #[test]
    fn test_add_file_connection_in_rejects_bad_paths() {
        let dir = add_file_test_dir("reject");
        let csv = dir.join("data").join("x.csv");
        std::fs::write(&csv, b"").unwrap();
        for path in [
            csv.display().to_string(),
            dir.join("data").join("missing.db").display().to_string(),
            "relative.db".to_string(),
        ] {
            assert!(add_file_connection_in(&dir, &path).is_err(), "{path}");
        }
        assert!(!dir.join("config.yml").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_config_template_is_valid() {
        // The template must parse as a valid config as-is (zero connections)
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
    #[cfg(unix)]
    fn test_supplement_path() {
        // If absent, it is added
        let path = supplement_path("/usr/bin:/bin");
        assert!(path.split(':').any(|p| p == "/opt/homebrew/bin"));
        assert!(path.split(':').any(|p| p == "/usr/local/bin"));
        // If already present, it is not added again
        let path = supplement_path("/opt/homebrew/bin:/usr/bin");
        let count = path.split(':').filter(|p| *p == "/opt/homebrew/bin").count();
        assert_eq!(count, 1);
    }

    #[test]
    #[cfg(windows)]
    fn test_supplement_path_windows_passthrough() {
        // The Windows PATH is `;`-separated, so pass it through adding nothing.
        // In particular, check that the last element is not broken (it does not become
        // `C:\tools:/opt/homebrew/bin`).
        let base = r"C:\Windows\system32;C:\tools";
        assert_eq!(supplement_path(base), base);
        // Even an empty PATH stays empty (no value like `:/opt/homebrew/bin` is produced).
        assert_eq!(supplement_path(""), "");
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
            ssl_mode: None,
            ssl_root_cert: None,
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
            ssl_mode: None,
            ssl_root_cert: None,
            aws_profile: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
        }
    }

    /// The effective TLS mode is "explicit ssl_mode -> verify-full if tls: true -> prefer".
    /// That the default is prefer (may fall back to plaintext) is a deliberate choice for
    /// backward compatibility, so pin it in a test so that a change gets noticed.
    #[test]
    fn test_sql_ssl_mode() {
        let mut s = server_with(None, Some("h"), "postgres", Some("db"), Some("u"));

        // If nothing is specified, prefer, same as the sqlx default
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::Prefer);
        assert!(s.sql_ssl_mode().unwrap().allows_plaintext());

        // tls: true is equivalent to verify-full
        s.tls = true;
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::VerifyFull);
        assert!(!s.sql_ssl_mode().unwrap().allows_plaintext());

        // ssl_mode takes precedence over tls
        s.ssl_mode = Some("require".into());
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::Require);

        // Uppercase, underscores and surrounding whitespace are tolerated
        s.ssl_mode = Some("  VERIFY_CA ".into());
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::VerifyCa);

        s.ssl_mode = Some("disable".into());
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::Disable);
        assert!(s.sql_ssl_mode().unwrap().allows_plaintext());

        // Unknown values are an error rather than silently falling back to the default
        s.ssl_mode = Some("verify".into());
        assert!(s.sql_ssl_mode().is_err());
        s.ssl_mode = Some("".into());
        assert!(s.sql_ssl_mode().is_err());
    }

    /// ssl_root_cert is only meaningful in modes that verify.
    /// sqlx silently ignores the root CA in non-verifying modes, so a config that specifies
    /// both creates the misconception that "it is verified". Make it an error so that it gets
    /// noticed.
    #[test]
    fn test_sql_ssl_root_cert_requires_verifying_mode() {
        let mut s = server_with(None, Some("h"), "postgres", Some("db"), Some("u"));
        s.ssl_root_cert = Some("~/certs/ca.pem".into());

        // Specifying it together with a non-verifying mode is an error
        assert!(s.sql_ssl_root_cert().is_err()); // default = prefer
        s.ssl_mode = Some("require".into());
        assert!(s.sql_ssl_root_cert().is_err());
        s.ssl_mode = Some("disable".into());
        assert!(s.sql_ssl_root_cert().is_err());

        // It passes in a verifying mode
        s.ssl_mode = Some("verify-ca".into());
        assert_eq!(s.sql_ssl_root_cert().unwrap(), Some("~/certs/ca.pem"));
        s.ssl_mode = None;
        s.tls = true; // = verify-full
        assert_eq!(s.sql_ssl_root_cert().unwrap(), Some("~/certs/ca.pem"));

        // An empty string is an error; unset is None
        s.ssl_root_cert = Some("  ".into());
        assert!(s.sql_ssl_root_cert().is_err());
        s.ssl_root_cert = None;
        assert_eq!(s.sql_ssl_root_cert().unwrap(), None);
    }

    /// The sql_ssl_mode of ConnectionInfo is set for SQL engines and redis
    /// (the frontend shows this value in the connection details tooltip).
    #[test]
    fn test_connection_info_sql_ssl_mode() {
        let s = server_with(None, Some("h"), "postgres", Some("db"), Some("u"));
        assert_eq!(
            ConnectionInfo::from(&s).sql_ssl_mode.as_deref(),
            Some("prefer")
        );

        // Engine name aliases are also picked up
        let mut s = server_with(None, Some("h"), "mariadb", Some("db"), Some("u"));
        s.tls = true;
        assert_eq!(
            ConnectionInfo::from(&s).sql_ssl_mode.as_deref(),
            Some("verify-full")
        );

        // Engines without a TLS mode get null
        let s = server_with(None, None, "sqlite", Some("/tmp/x.sqlite3"), None);
        assert!(ConnectionInfo::from(&s).sql_ssl_mode.is_none());

        // An invalid ssl_mode gives up on display and is null (it errors at connect time)
        let mut s = server_with(None, Some("h"), "postgres", Some("db"), Some("u"));
        s.ssl_mode = Some("bogus".into());
        assert!(ConnectionInfo::from(&s).sql_ssl_mode.is_none());

        // redis reports whether tls is set as-is. Reporting plaintext as disable too lets the user
        // notice that a connection where they thought they had written tls is connecting in
        // plaintext
        // (CYBERNEURA-DEV-420)
        let s = server_with(None, Some("h"), "redis", Some("0"), None);
        assert_eq!(
            ConnectionInfo::from(&s).sql_ssl_mode.as_deref(),
            Some("disable")
        );

        let mut s = server_with(None, Some("h"), "valkey", Some("0"), None);
        s.tls = true;
        assert_eq!(
            ConnectionInfo::from(&s).sql_ssl_mode.as_deref(),
            Some("verify-full")
        );
    }

    #[test]
    fn test_sqlfiles_folder_name() {
        // If folder_name is set, use it (name is not used)
        let s = server_with(Some("my-folder"), Some("h"), "mysql", Some("db"), Some("u"));
        assert_eq!(s.sqlfiles_folder_name(), "my-folder");

        // If folder_name is an empty string, fall back
        let s = server_with(Some("   "), Some("h"), "mysql", Some("db"), Some("u"));
        assert_eq!(s.sqlfiles_folder_name(), "h_mysql_db_u");

        // No folder_name -> <host>_<engine>_<schema>_<user>
        let s = server_with(
            None,
            Some("db.example.com"),
            "postgres",
            Some("prod"),
            Some("app"),
        );
        assert_eq!(s.sqlfiles_folder_name(), "db.example.com_postgres_prod_app");

        // sqlite: no host/user, schema is a file path -> separators are sanitized
        let s = server_with(None, None, "sqlite", Some("/Users/me/data.db"), None);
        assert_eq!(s.sqlfiles_folder_name(), "_sqlite__Users_me_data.db_");

        // Avoid a leading dot (prevents it becoming hidden / a relative path)
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
            ssl_mode: None,
            ssl_root_cert: None,
            aws_profile: None,
        };
        // The access key ID is not put in the folder name; it is told apart by a short hash
        let folder = server.sqlfiles_folder_name();
        assert!(!folder.contains("AKIAEXAMPLEKEYID"), "{folder}");
        assert!(folder.contains("key-"), "{folder}");
        // A different key gives a different folder
        let mut other = server.clone();
        other.user = Some("AKIAOTHERKEYID".into());
        assert_ne!(folder, other.sqlfiles_folder_name());
        // If aws_profile is set, use the profile name (non-sensitive)
        server.user = None;
        server.password = None;
        server.aws_profile = Some("myprofile".into());
        let folder = server.sqlfiles_folder_name();
        assert!(folder.contains("myprofile"), "{folder}");
        // When both are present, follow the authentication priority and use the static key side
        // (hash)
        server.user = Some("AKIAEXAMPLEKEYID".into());
        server.password = Some("secret".into());
        let folder = server.sqlfiles_folder_name();
        assert!(folder.contains("key-"), "{folder}");
        assert!(!folder.contains("myprofile"), "{folder}");
        // Stability of the same hash
        assert_eq!(stable_hash_hex("abc"), stable_hash_hex("abc"));
        assert_ne!(stable_hash_hex("abc"), stable_hash_hex("abd"));
    }
}
