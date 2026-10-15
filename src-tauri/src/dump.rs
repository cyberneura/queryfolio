//! Dumping a database / table with the engine's own CLI tool (mysqldump / pg_dump).
//!
//! Queryfolio does not implement a dump format itself: the native tools are the ones whose
//! output the user can restore with `mysql` / `psql` / `pg_restore`, and they already handle
//! every object type. What this module adds is the part that makes the CLI tedious to use by
//! hand (CYBERNEURA-DEV-1070):
//!
//! - **Finding the binary.** GUI apps do not inherit the shell's PATH, and the tools are often
//!   keg-only in Homebrew (`mysql-client`, `libpq`) or bundled with another app (Postgres.app,
//!   MySQL Workbench). `detect_tools` searches the usual places and reports each version, and
//!   for pg_dump puts the newest first (pg_dump refuses to dump a server newer than itself).
//! - **Building the arguments from the connection config**, including TLS and the SSH tunnel.
//!   The tunnel is a dedicated one opened for this dump and closed when it ends, not the one
//!   cached by `DbManager` (a disconnect / schema switch / config reload drops that one, which
//!   would cut the dump off in the middle).
//! - **Never putting the password on the command line** (it would be visible to every user of
//!   the machine through `ps`). mysqldump reads it from a temporary option file
//!   (`--defaults-file`, mode 600, deleted when the dump ends) and pg_dump from
//!   `PGPASSWORD` in the child's environment. pg_dump also gets `--no-password` so that a
//!   missing password fails instead of waiting forever for input nobody can type.
//! - **Not leaving half-written files.** The tool writes to a temporary `.partial` file next to
//!   the output, which is renamed to the chosen path only when the tool exits successfully, and
//!   removed on failure / cancel.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::config::{expand_tilde, supplement_path, ServerConfig, SqlSslMode};
use crate::db::{default_port, parse_engine, ssl_root_cert_path, Engine};
use crate::error::AppError;
use crate::tunnel::SshTunnel;

/// Upper bound for `<tool> --version` (a hung binary must not block the dialog)
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the size of the output file is reported while dumping
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
/// How much of the tool's stderr is kept (the tail; the end usually has the actual error)
const MAX_STDERR_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DumpTool {
    Mysqldump,
    PgDump,
}

impl DumpTool {
    pub fn for_engine(engine: Engine) -> Option<Self> {
        match engine {
            Engine::MySql => Some(DumpTool::Mysqldump),
            Engine::Postgres => Some(DumpTool::PgDump),
            _ => None,
        }
    }

    /// Executable names to look for, in order of preference (MariaDB 11 renamed mysqldump to
    /// mariadb-dump and may not install the old name)
    fn binary_names(self) -> &'static [&'static str] {
        match self {
            DumpTool::Mysqldump => &["mysqldump", "mariadb-dump"],
            DumpTool::PgDump => &["pg_dump"],
        }
    }

    /// Directories (glob patterns) where the tool is commonly installed but which are not on the
    /// PATH a GUI app gets
    fn extra_dir_patterns(self) -> &'static [&'static str] {
        if cfg!(windows) {
            return match self {
                DumpTool::Mysqldump => &[
                    "C:\\Program Files\\MySQL\\*\\bin",
                    "C:\\Program Files\\MariaDB*\\bin",
                ],
                DumpTool::PgDump => &["C:\\Program Files\\PostgreSQL\\*\\bin"],
            };
        }
        match self {
            DumpTool::Mysqldump => &[
                "/opt/homebrew/opt/mysql-client/bin",
                "/opt/homebrew/opt/mysql-client@*/bin",
                "/opt/homebrew/opt/mysql/bin",
                "/opt/homebrew/opt/mysql@*/bin",
                "/opt/homebrew/opt/mariadb/bin",
                "/opt/homebrew/opt/mariadb@*/bin",
                "/usr/local/opt/mysql-client/bin",
                "/usr/local/opt/mysql-client@*/bin",
                "/usr/local/opt/mysql/bin",
                "/usr/local/opt/mysql@*/bin",
                "/usr/local/opt/mariadb/bin",
                "/usr/local/mysql/bin",
                "/Applications/MySQLWorkbench.app/Contents/MacOS",
            ],
            DumpTool::PgDump => &[
                "/opt/homebrew/opt/libpq/bin",
                "/opt/homebrew/opt/postgresql/bin",
                "/opt/homebrew/opt/postgresql@*/bin",
                "/usr/local/opt/libpq/bin",
                "/usr/local/opt/postgresql/bin",
                "/usr/local/opt/postgresql@*/bin",
                "/Applications/Postgres.app/Contents/Versions/*/bin",
                "/usr/lib/postgresql/*/bin",
                "/usr/pgsql-*/bin",
            ],
        }
    }
}

/// A tool binary found on this machine
#[derive(Debug, Clone, Serialize)]
pub struct DetectedTool {
    pub path: String,
    /// First line of `--version` (None if it could not be run)
    pub version: Option<String>,
}

/// What the dump dialog needs to know about a connection
#[derive(Debug, Clone, Serialize)]
pub struct DumpInfo {
    pub tool: DumpTool,
    /// Candidates, the recommended one first
    pub tools: Vec<DetectedTool>,
    /// Database dumped when the request does not name one (the active schema)
    pub default_database: Option<String>,
    pub via_ssh_tunnel: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DumpContent {
    /// Structure and data
    All,
    /// Structure only (CREATE statements)
    Schema,
    /// Data only (INSERT / COPY)
    Data,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DumpFormat {
    /// Plain SQL
    Plain,
    /// pg_dump's custom archive (restored with pg_restore). PostgreSQL only
    Custom,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DumpTable {
    /// Owning schema (PostgreSQL). Unquoted
    pub schema: Option<String>,
    /// Table name. Unquoted
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpRequest {
    pub tool_path: String,
    /// Database to dump. None = the active schema of the connection
    pub database: Option<String>,
    /// None = the whole database
    pub table: Option<DumpTable>,
    pub content: DumpContent,
    pub format: DumpFormat,
    /// mysqldump --single-transaction (consistent snapshot of InnoDB without locking)
    pub single_transaction: bool,
    /// mysqldump --routines --events (whole-database dumps only)
    pub routines: bool,
    /// DROP before CREATE (mysqldump's default --add-drop-table / pg_dump --clean --if-exists)
    pub add_drop: bool,
    /// pg_dump --no-owner --no-privileges (to restore as a different user)
    pub no_owner: bool,
    /// Additional arguments, split like a shell command line (no shell is involved)
    pub extra_args: String,
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DumpOutcome {
    pub path: String,
    pub bytes: u64,
    /// The tool's stderr when it exited successfully (warnings), trimmed. Empty if none
    pub warnings: String,
}

/// Returns the dump tool of a connection, or an error for engines without one
pub fn tool_for_server(server: &ServerConfig) -> Result<DumpTool, AppError> {
    let engine = parse_engine(&server.engine)?;
    DumpTool::for_engine(engine).ok_or_else(|| {
        AppError::Dump(format!(
            "Dumping is not supported for engine '{}' (only mysql and postgres)",
            server.engine
        ))
    })
}

/// Finds the tool binaries on this machine. PATH comes first (what the user would get in a
/// terminal), then the known install locations. For pg_dump the newest version comes first.
pub async fn detect_tools(tool: DumpTool) -> Vec<DetectedTool> {
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&supplement_path(&std::env::var("PATH").unwrap_or_default()))
            .collect();
    for pattern in tool.extra_dir_patterns() {
        if let Ok(paths) = glob::glob(pattern) {
            dirs.extend(paths.flatten());
        }
    }

    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for dir in dirs {
        for name in tool.binary_names() {
            let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            if !is_executable(&path) {
                continue;
            }
            // Homebrew links the same binary into several directories
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if seen.insert(key) {
                paths.push(path);
            }
        }
    }

    let versions = futures::future::join_all(paths.iter().map(|path| tool_version(path))).await;
    let mut tools: Vec<DetectedTool> = paths
        .into_iter()
        .zip(versions)
        .map(|(path, version)| DetectedTool {
            path: path.to_string_lossy().into_owned(),
            version,
        })
        .collect();
    if tool == DumpTool::PgDump {
        // Stable sort: equal versions keep the PATH-first order
        tools.sort_by(|a, b| {
            let va = a.version.as_deref().map(parse_version).unwrap_or_default();
            let vb = b.version.as_deref().map(parse_version).unwrap_or_default();
            vb.cmp(&va)
        });
    }
    tools
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// First line of `<path> --version`, or None if it cannot be run
async fn tool_version(path: &Path) -> Option<String> {
    let mut command = tokio::process::Command::new(path);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    hide_console_window(&mut command);
    let output = tokio::time::timeout(VERSION_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// The first dotted number in a version line ("pg_dump (PostgreSQL) 16.3 (Homebrew)" -> [16, 3])
fn parse_version(line: &str) -> Vec<u32> {
    line.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|token| token.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|token| {
            token
                .split('.')
                .map_while(|part| part.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The database the request dumps: the one it names, otherwise the active schema
pub fn resolve_database(request: &DumpRequest, active_schema: Option<&str>) -> Option<String> {
    request
        .database
        .as_deref()
        .or(active_schema)
        // Not trimmed: the name comes from the server's list or the config, and " shop " is a
        // different (valid) database from "shop"
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A name that becomes a positional argument of mysqldump must not look like an option
fn check_positional(kind: &str, name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::Dump(format!("The {kind} name is empty")));
    }
    if name.starts_with('-') {
        return Err(AppError::Dump(format!(
            "Cannot dump a {kind} whose name starts with '-': {name}"
        )));
    }
    Ok(())
}

/// Splits the extra arguments and rejects the ones that send the dump somewhere other than
/// stdout (`-f` / `--file`, `-r` / `--result-file`, `-T` / `--tab`): the output would bypass the
/// partial file, so a failed dump would be left behind and an empty file saved as the result.
/// Long options are matched by prefix because both tools accept unambiguous abbreviations.
fn split_extra_args(tool: DumpTool, extra: &str) -> Result<Vec<String>, AppError> {
    let args = shlex::split(extra).ok_or_else(|| {
        AppError::Dump("Could not parse the extra arguments (check the quotes)".into())
    })?;
    // Short options that redirect the output, and short options that take a value (the rest
    // of a cluster after one of them is that value: `-tfoo` is the table "foo", not `-f`)
    let (redirect, with_value, long): (&str, &str, &[&str]) = match tool {
        DumpTool::PgDump => ("f", "dEeFfhjnNpStTUZ", &["file"]),
        DumpTool::Mysqldump => ("rT", "hPpSuwrT", &["result-file", "tab"]),
    };
    for arg in &args {
        let redirects = if let Some(name) = arg.strip_prefix("--") {
            let name = name.split('=').next().unwrap_or_default();
            name.len() >= 2 && long.iter().any(|l| l.starts_with(name))
        } else if let Some(cluster) = arg.strip_prefix('-') {
            // Short options can be combined (`-vf out.sql`)
            cluster
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .scan(false, |value_follows, c| {
                    if *value_follows {
                        return None;
                    }
                    *value_follows = with_value.contains(c);
                    Some(c)
                })
                .any(|c| redirect.contains(c))
        } else {
            false
        };
        if redirects {
            return Err(AppError::Dump(format!(
                "Remove '{arg}' from the extra arguments: the output file is chosen in the dialog"
            )));
        }
    }
    Ok(args)
}

/// Arguments for mysqldump. The password is not among them (see write_mysql_option_file).
/// `defaults_file` must be the first argument (mysqldump ignores it anywhere else).
///
/// It is `--defaults-file`, not `--defaults-extra-file`: the extra file is read *before* the
/// user's `~/.my.cnf`, whose `password` would then override the configured one, and whose
/// `[mysqldump] result-file=` / `tab=` would send the output past the partial file. With
/// `--defaults-file` no other option file is read (`.mylogin.cnf` is shut out separately with
/// MYSQL_TEST_LOGIN_FILE in run_dump). Command-line options override option files anyway.
pub fn mysqldump_args(
    server: &ServerConfig,
    request: &DumpRequest,
    database: &str,
    host: &str,
    port: u16,
    mariadb: bool,
    defaults_file: Option<&Path>,
) -> Result<Vec<String>, AppError> {
    check_positional("database", database)?;
    if request.format == DumpFormat::Custom {
        return Err(AppError::Dump(
            "The custom archive format is only available for PostgreSQL".into(),
        ));
    }
    let mut args = Vec::new();
    if let Some(path) = defaults_file {
        args.push(format!("--defaults-file={}", path.display()));
    }
    args.push(format!("--host={host}"));
    args.push(format!("--port={port}"));
    // Without this, "localhost" means the Unix socket to the mysql client, while the app
    // (sqlx) and the SSH tunnel always use TCP
    args.push("--protocol=TCP".into());
    // Without a user, sqlx (the app's connection) logs in as root, while the mysql client would
    // use the OS user name
    let user = server
        .user
        .as_deref()
        .filter(|u| !u.is_empty())
        .unwrap_or("root");
    args.push(format!("--user={user}"));
    args.extend(mysql_tls_args(server, mariadb)?);
    if request.single_transaction {
        args.push("--single-transaction".into());
    }
    // Routines / events / triggers are definitions (DDL): a data-only dump restored into an
    // existing schema must not create them again
    if request.routines && request.table.is_none() && request.content != DumpContent::Data {
        args.push("--routines".into());
        args.push("--events".into());
    }
    if !request.add_drop {
        args.push("--skip-add-drop-table".into());
    }
    match request.content {
        DumpContent::All => {}
        DumpContent::Schema => args.push("--no-data".into()),
        DumpContent::Data => {
            args.push("--no-create-info".into());
            args.push("--skip-triggers".into());
        }
    }
    args.extend(split_extra_args(DumpTool::Mysqldump, &request.extra_args)?);
    args.push(database.to_string());
    if let Some(table) = &request.table {
        check_positional("table", &table.name)?;
        args.push(table.name.clone());
    }
    Ok(args)
}

/// TLS options of the mysql client for the connection's ssl_mode.
/// MySQL's client has --ssl-mode; MariaDB's does not and uses --ssl / --skip-ssl /
/// --ssl-verify-server-cert. For `prefer` (the default), MySQL's client already tries TLS by
/// default, so nothing is passed; MariaDB's client before 11.4 does not (its default is
/// plaintext), so `--ssl` asks it to try TLS like the app's own connection does, without the
/// certificate verification 11.4+ would add.
fn mysql_tls_args(server: &ServerConfig, mariadb: bool) -> Result<Vec<String>, AppError> {
    let mode = server.sql_ssl_mode()?;
    let mut args: Vec<String> = match (mode, mariadb) {
        (SqlSslMode::Prefer, false) => vec![],
        (SqlSslMode::Disable, false) => vec!["--ssl-mode=DISABLED".into()],
        (SqlSslMode::Require, false) => vec!["--ssl-mode=REQUIRED".into()],
        (SqlSslMode::VerifyCa, false) => vec!["--ssl-mode=VERIFY_CA".into()],
        (SqlSslMode::VerifyFull, false) => vec!["--ssl-mode=VERIFY_IDENTITY".into()],
        (SqlSslMode::Disable, true) => vec!["--skip-ssl".into()],
        // Try / require encryption without verification. MariaDB 11.4+ clients verify the
        // certificate by default, so turn that off explicitly (older clients accept the --skip-
        // form too, as it is a boolean option). MariaDB's client cannot tell prefer from require
        (SqlSslMode::Prefer | SqlSslMode::Require, true) => {
            vec!["--ssl".into(), "--skip-ssl-verify-server-cert".into()]
        }
        // MariaDB's client has no "chain only" mode, so verify-ca also checks the host name
        (SqlSslMode::VerifyCa | SqlSslMode::VerifyFull, true) => {
            vec!["--ssl".into(), "--ssl-verify-server-cert".into()]
        }
    };
    if let Some(path) = ssl_root_cert_path(server)? {
        args.push(format!("--ssl-ca={}", path.display()));
    }
    Ok(args)
}

/// Environment variables for the child process
type EnvVars = Vec<(String, String)>;

/// Where the dump tool connects
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Host name as configured (also the name the TLS certificate is checked against)
    pub host: String,
    /// The address actually connected to when it differs from `host` (the local end of the
    /// SSH tunnel)
    pub tunnel_addr: Option<String>,
    pub port: u16,
}

/// A value in a libpq connection string: single-quoted, with `\` and `'` escaped
fn conninfo_value(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Arguments and environment for pg_dump. The password is not among them (PGPASSWORD is added
/// when the process is started).
///
/// The connection goes in one libpq connection string (`--dbname=host=... dbname=...`) rather
/// than `--host` / `--dbname <name>`: a database name containing `=` (they come from the
/// server's database list) would otherwise be parsed as a connection string itself and could
/// redirect the connection, together with PGPASSWORD, to another host. Through an SSH tunnel,
/// `hostaddr` is the tunnel and `host` stays the configured name, so verify-full checks the
/// certificate against the real server name.
pub fn pg_dump_args(
    server: &ServerConfig,
    request: &DumpRequest,
    database: Option<&str>,
    endpoint: &Endpoint,
) -> Result<(Vec<String>, EnvVars), AppError> {
    let mut conninfo = vec![format!("host={}", conninfo_value(&endpoint.host))];
    if let Some(addr) = &endpoint.tunnel_addr {
        conninfo.push(format!("hostaddr={}", conninfo_value(addr)));
    }
    conninfo.push(format!("port={}", endpoint.port));
    if let Some(user) = server.user.as_deref().filter(|u| !u.is_empty()) {
        conninfo.push(format!("user={}", conninfo_value(user)));
    }
    if let Some(database) = database {
        conninfo.push(format!("dbname={}", conninfo_value(database)));
    }
    let mut args = vec![format!("--dbname={}", conninfo.join(" "))];
    // Fail instead of prompting: there is no terminal to type into
    args.push("--no-password".into());
    match request.format {
        DumpFormat::Plain => {
            // pg_dump rejects --clean together with --data-only
            if request.add_drop && request.content != DumpContent::Data {
                args.push("--clean".into());
                args.push("--if-exists".into());
            }
            if request.no_owner {
                args.push("--no-owner".into());
                args.push("--no-privileges".into());
            }
        }
        // --clean / --no-owner are ignored for archives: pg_restore decides them when restoring
        DumpFormat::Custom => args.push("--format=custom".into()),
    }
    match request.content {
        DumpContent::All => {}
        DumpContent::Schema => args.push("--schema-only".into()),
        DumpContent::Data => args.push("--data-only".into()),
    }
    if let Some(table) = &request.table {
        if table.name.is_empty() {
            return Err(AppError::Dump("The table name is empty".into()));
        }
        args.push(format!("--table={}", pg_table_pattern(table)));
    }
    args.extend(split_extra_args(DumpTool::PgDump, &request.extra_args)?);

    let mut env = vec![
        (
            "PGSSLMODE".to_string(),
            server.sql_ssl_mode()?.as_str().to_string(),
        ),
        ("PGCONNECT_TIMEOUT".to_string(), "15".to_string()),
    ];
    if let Some(path) = ssl_root_cert_path(server)? {
        env.push(("PGSSLROOTCERT".into(), path.to_string_lossy().into_owned()));
    }
    Ok((args, env))
}

/// `--table` takes a psql pattern: unquoted `*` / `?` / `.` and upper case are interpreted.
/// Quoting every part makes it match exactly this one table (`"` is doubled inside quotes).
fn pg_table_pattern(table: &DumpTable) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    match table.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(schema) => format!("{}.{}", quote(schema), quote(&table.name)),
        None => quote(&table.name),
    }
}

/// Contents of the option file given to mysqldump: the password, if any.
/// Inside double quotes the option file format recognizes backslash escapes.
fn mysql_option_file_contents(password: Option<&str>) -> String {
    let Some(password) = password else {
        return "[client]\n".to_string();
    };
    let mut escaped = String::with_capacity(password.len());
    for c in password.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c => escaped.push(c),
        }
    }
    format!("[client]\npassword=\"{escaped}\"\n")
}

/// Deletes the file when dropped (temporary password file, partial output)
struct RemoveOnDrop(Option<PathBuf>);

impl RemoveOnDrop {
    fn keep(&mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Creates a new file that only the current user can read (mode 600 on Unix), named by
/// `make_name(attempt)` in `dir`. `create_new` (O_EXCL) never truncates an existing file and
/// refuses to follow a symlink planted at the name; a taken name is retried with the next attempt.
fn create_private_file(
    dir: &Path,
    make_name: impl Fn(u32) -> String,
) -> Result<(std::fs::File, PathBuf), AppError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut attempt = 0u32;
    loop {
        let path = dir.join(make_name(attempt));
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 100 => {
                attempt += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// A token that makes temporary names unique per process and moment
fn unique_stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

/// Writes mysqldump's option file to a new file only the current user can read
fn write_mysql_option_file(password: Option<&str>) -> Result<RemoveOnDrop, AppError> {
    use std::io::Write;
    let stamp = unique_stamp();
    let (mut file, path) = create_private_file(&std::env::temp_dir(), |attempt| {
        format!("queryfolio-dump-{stamp}-{attempt}.cnf")
    })?;
    let guard = RemoveOnDrop(Some(path));
    file.write_all(mysql_option_file_contents(password).as_bytes())?;
    Ok(guard)
}

/// Creates the file the tool writes to until it succeeds: next to the output (so the final
/// rename stays on one file system), under a unique name (two dumps to the same path, or a
/// leftover from a crash, must not share it), readable only by the user (the dump is the
/// database's contents). The rename keeps that mode for the final file.
fn create_partial_file(output: &Path) -> Result<(std::fs::File, PathBuf), AppError> {
    let dir = match output.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let name = output
        .file_name()
        .ok_or_else(|| AppError::Dump(format!("Not a file path: {}", output.display())))?
        .to_string_lossy()
        .into_owned();
    let stamp = unique_stamp();
    create_private_file(dir, |attempt| format!(".{name}.{stamp}-{attempt}.partial"))
}

/// Whether the mysqldump at `path` is MariaDB's (its TLS options differ). Decided from the
/// version line, falling back to the file name when it cannot be run
pub async fn is_mariadb_client(path: &Path) -> bool {
    match tool_version(path).await {
        Some(version) => version.contains("MariaDB"),
        None => path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains("mariadb")),
    }
}

/// The host / port the tool should use, without opening anything (the tunnel port is filled
/// in by run_dump)
fn configured_endpoint(server: &ServerConfig, engine: Engine) -> Endpoint {
    Endpoint {
        host: server.host.clone().unwrap_or_else(|| "localhost".into()),
        tunnel_addr: server.ssh_tunnel.as_ref().map(|_| "127.0.0.1".to_string()),
        port: server.port.unwrap_or(default_port(engine)),
    }
}

/// Builds the tool's arguments and environment for an endpoint
fn build_command_line(
    tool: DumpTool,
    server: &ServerConfig,
    request: &DumpRequest,
    database: Option<&str>,
    endpoint: &Endpoint,
    mariadb: bool,
    defaults_file: Option<&Path>,
) -> Result<(Vec<String>, EnvVars), AppError> {
    match tool {
        DumpTool::Mysqldump => {
            let database = database.ok_or_else(no_database)?;
            // The mysql client has no separate "connect to / verify as" address, so through
            // a tunnel it connects to the tunnel (as the app's own connection does)
            let host = endpoint.tunnel_addr.as_deref().unwrap_or(&endpoint.host);
            let args = mysqldump_args(
                server,
                request,
                database,
                host,
                endpoint.port,
                mariadb,
                defaults_file,
            )?;
            Ok((args, vec![]))
        }
        DumpTool::PgDump => pg_dump_args(server, request, database, endpoint),
    }
}

/// The command line shown in the dialog. Built from the same function as the real one; the
/// password never appears (it is not an argument), and the option file / tunnel port are
/// replaced with readable placeholders.
pub fn preview_command(
    server: &ServerConfig,
    request: &DumpRequest,
    active_schema: Option<&str>,
    mariadb: bool,
) -> Result<String, AppError> {
    let tool = tool_for_server(server)?;
    let engine = parse_engine(&server.engine)?;
    let mut endpoint = configured_endpoint(server, engine);
    if endpoint.tunnel_addr.is_some() {
        // Replaced with a placeholder below (the real port is only known once the tunnel is up)
        endpoint.port = 0;
    }
    let database = resolve_database(request, active_schema);
    let placeholder = (tool == DumpTool::Mysqldump).then(|| {
        PathBuf::from(if server.password.is_some() {
            "<temporary file with the password>"
        } else {
            "<temporary empty option file>"
        })
    });
    let (args, env) = build_command_line(
        tool,
        server,
        request,
        database.as_deref(),
        &endpoint,
        mariadb,
        placeholder.as_deref(),
    )?;
    let mut parts: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", shell_quote(v)))
        .collect();
    if tool == DumpTool::PgDump && server.password.is_some() {
        parts.push("PGPASSWORD=<hidden>".into());
    }
    parts.push(shell_quote(&request.tool_path));
    parts.extend(args.iter().map(|a| shell_quote(a)));
    let mut line = parts.join(" ");
    line.push_str(" > ");
    line.push_str(&shell_quote(&request.output_path));
    if endpoint.tunnel_addr.is_some() {
        line = line
            .replace("--port=0", "--port=<ssh tunnel port>")
            .replace("port=0", "port=<ssh tunnel port>");
    }
    Ok(line)
}

/// Quotes for display like a shell would read it. Arguments that contain single quotes (the
/// pg_dump connection string) read better in double quotes when nothing in them is special there
fn shell_quote(s: &str) -> String {
    if s.contains('\'') && !s.contains(['"', '$', '`', '\\', '!']) {
        return format!("\"{s}\"");
    }
    shlex::try_quote(s)
        .map(|q| q.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

fn no_database() -> AppError {
    AppError::Dump("Choose a database to dump (this connection has no database set)".into())
}

/// Runs the dump. `on_progress` receives the size written so far. Returns
/// AppError::Cancelled when `cancel` fires (the tool is killed and the partial file removed).
pub async fn run_dump(
    server: &ServerConfig,
    request: &DumpRequest,
    active_schema: Option<&str>,
    cancel: CancellationToken,
    on_progress: impl Fn(u64),
) -> Result<DumpOutcome, AppError> {
    let tool = tool_for_server(server)?;
    let engine = parse_engine(&server.engine)?;
    let tool_path = expand_tilde(request.tool_path.trim());
    if !is_executable(&tool_path) {
        return Err(AppError::Dump(format!(
            "The {} binary was not found or is not executable: {}",
            tool.binary_names()[0],
            tool_path.display()
        )));
    }
    let output = expand_tilde(request.output_path.trim());
    if request.output_path.trim().is_empty() {
        return Err(AppError::Dump("Choose where to save the dump".into()));
    }
    let database = resolve_database(request, active_schema);
    if tool == DumpTool::Mysqldump && database.is_none() {
        return Err(no_database());
    }

    let mariadb = tool == DumpTool::Mysqldump && is_mariadb_client(&tool_path).await;
    // Validate the arguments before opening anything (the real port is set below)
    let mut endpoint = configured_endpoint(server, engine);
    build_command_line(
        tool,
        server,
        request,
        database.as_deref(),
        &endpoint,
        mariadb,
        None,
    )?;

    // A dedicated tunnel that lives exactly as long as this dump
    let mut _tunnel = None;
    if let Some(tunnel_config) = &server.ssh_tunnel {
        let target_host = endpoint.host.clone();
        let target_port = endpoint.port;
        let tunnel_config = tunnel_config.clone();
        let tunnel = tokio::task::spawn_blocking(move || {
            SshTunnel::start(&tunnel_config, &target_host, target_port)
        })
        .await
        .map_err(|e| AppError::SshTunnel(format!("SSH tunnel task failed: {e}")))??;
        endpoint.port = tunnel.local_port;
        _tunnel = Some(tunnel);
    }
    if cancel.is_cancelled() {
        return Err(AppError::Cancelled);
    }

    // Always written for mysqldump, even without a password: --defaults-file is what keeps
    // the user's option files out (see mysqldump_args)
    let option_file = match tool {
        DumpTool::Mysqldump => Some(write_mysql_option_file(server.password.as_deref())?),
        DumpTool::PgDump => None,
    };
    let (args, env) = build_command_line(
        tool,
        server,
        request,
        database.as_deref(),
        &endpoint,
        mariadb,
        option_file.as_ref().and_then(|g| g.0.as_deref()),
    )?;
    let mut command = tokio::process::Command::new(&tool_path);
    command.args(args).envs(env);
    if tool == DumpTool::Mysqldump {
        // .mylogin.cnf (mysql_config_editor) is read even with --defaults-file and could
        // override the password; point the client at a login file that does not exist
        command.env(
            "MYSQL_TEST_LOGIN_FILE",
            std::env::temp_dir().join(format!("queryfolio-no-login-{}.cnf", unique_stamp())),
        );
    }
    if let (Some(password), DumpTool::PgDump) = (&server.password, tool) {
        command.env("PGPASSWORD", password);
    }

    let (file, partial) = create_partial_file(&output)?;
    let mut partial_guard = RemoveOnDrop(Some(partial.clone()));

    // The tool may need helpers from the same PATH a terminal would have
    command
        .env(
            "PATH",
            supplement_path(&std::env::var("PATH").unwrap_or_default()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_console_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| AppError::Dump(format!("Failed to start {}: {e}", tool_path.display())))?;

    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_task = tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > MAX_STDERR_BYTES {
                        tail.drain(..tail.len() - MAX_STDERR_BYTES);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).trim().to_string()
    });

    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(AppError::Cancelled);
            }
            _ = ticker.tick() => {
                if let Ok(meta) = std::fs::metadata(&partial) {
                    on_progress(meta.len());
                }
            }
        }
    };
    let stderr_text = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        return Err(AppError::Dump(if stderr_text.is_empty() {
            format!(
                "{} exited with an error (code {code})",
                tool.binary_names()[0]
            )
        } else {
            format!(
                "{} exited with an error (code {code}):\n{stderr_text}",
                tool.binary_names()[0]
            )
        }));
    }
    let bytes = std::fs::metadata(&partial)?.len();
    // Replaces an existing file (the save dialog already asked) on Windows too: std's rename
    // is MoveFileExW with MOVEFILE_REPLACE_EXISTING there (same as query_files::write_file_atomic)
    std::fs::rename(&partial, &output)?;
    partial_guard.keep();
    on_progress(bytes);
    Ok(DumpOutcome {
        path: output.to_string_lossy().into_owned(),
        bytes,
        warnings: stderr_text,
    })
}

/// Windows: a console program started from a GUI app would open a console window
fn hide_console_window(command: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(engine: &str) -> ServerConfig {
        serde_yaml::from_str(&format!(
            "name: s\nengine: {engine}\nhost: db.example.com\nport: 3307\nuser: app\npassword: 'p\"w'\nschema: shop\n"
        ))
        .unwrap()
    }

    fn request() -> DumpRequest {
        DumpRequest {
            tool_path: "/usr/bin/mysqldump".into(),
            database: None,
            table: None,
            content: DumpContent::All,
            format: DumpFormat::Plain,
            single_transaction: true,
            routines: true,
            add_drop: true,
            no_owner: false,
            extra_args: String::new(),
            output_path: "/tmp/out.sql".into(),
        }
    }

    #[test]
    fn mysqldump_database_dump() {
        let s = server("mysql");
        let args = mysqldump_args(
            &s,
            &request(),
            "shop",
            "db.example.com",
            3307,
            false,
            Some(Path::new("/tmp/pw.cnf")),
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "--defaults-file=/tmp/pw.cnf",
                "--host=db.example.com",
                "--port=3307",
                "--protocol=TCP",
                "--user=app",
                "--single-transaction",
                "--routines",
                "--events",
                "shop",
            ]
        );
        // The password is never an argument
        assert!(!args.iter().any(|a| a.contains("p\"w")));
    }

    #[test]
    fn mysqldump_table_dump_options() {
        let s = server("mysql");
        let mut req = request();
        req.table = Some(DumpTable {
            schema: None,
            name: "orders".into(),
        });
        req.content = DumpContent::Schema;
        req.add_drop = false;
        req.single_transaction = false;
        req.extra_args = "--column-statistics=0 --set-gtid-purged='OFF'".into();
        let args = mysqldump_args(&s, &req, "shop", "h", 1, false, None).unwrap();
        assert_eq!(
            args,
            vec![
                "--host=h",
                "--port=1",
                "--protocol=TCP",
                "--user=app",
                // --routines is for whole-database dumps only
                "--skip-add-drop-table",
                "--no-data",
                "--column-statistics=0",
                "--set-gtid-purged=OFF",
                "shop",
                "orders",
            ]
        );
    }

    #[test]
    fn mysqldump_rejects_option_like_names_and_custom_format() {
        let s = server("mysql");
        assert!(mysqldump_args(&s, &request(), "-x", "h", 1, false, None).is_err());
        let mut req = request();
        req.table = Some(DumpTable {
            schema: None,
            name: "--all-databases".into(),
        });
        assert!(mysqldump_args(&s, &req, "shop", "h", 1, false, None).is_err());
        let mut req = request();
        req.format = DumpFormat::Custom;
        assert!(mysqldump_args(&s, &req, "shop", "h", 1, false, None).is_err());
        let mut req = request();
        req.extra_args = "'unterminated".into();
        assert!(mysqldump_args(&s, &req, "shop", "h", 1, false, None).is_err());
    }

    #[test]
    fn extra_args_must_not_redirect_the_output() {
        for extra in [
            "-vr out.sql",
            "-r out.sql",
            "--result-file=x",
            "--result x",
            "--res=x",
            "-T /tmp",
            "--tab=/tmp",
        ] {
            assert!(
                split_extra_args(DumpTool::Mysqldump, extra).is_err(),
                "{extra}"
            );
        }
        for extra in ["-f x", "-fx", "--file=x", "--fil x", "-vf x", "-vcf x"] {
            assert!(
                split_extra_args(DumpTool::PgDump, extra).is_err(),
                "{extra}"
            );
        }
        assert!(split_extra_args(DumpTool::Mysqldump, "--routines --hex-blob -R").is_ok());
        assert!(split_extra_args(
            DumpTool::PgDump,
            "-F c --format=plain --exclude-table-data=x"
        )
        .is_ok());
        // The rest of a cluster after an option that takes a value is that value
        assert!(split_extra_args(DumpTool::PgDump, "-tfoo -Tfx").is_ok());
        assert!(split_extra_args(DumpTool::Mysqldump, "-urroot -wfoo=1").is_ok());
    }

    #[test]
    fn mysqldump_data_only_leaves_out_definitions() {
        let s = server("mysql");
        let mut req = request();
        req.content = DumpContent::Data;
        let args = mysqldump_args(&s, &req, "shop", "h", 1, false, None).unwrap();
        assert!(args.contains(&"--no-create-info".to_string()));
        assert!(args.contains(&"--skip-triggers".to_string()));
        assert!(!args.contains(&"--routines".to_string()));
    }

    #[test]
    fn mysql_tls_flags_follow_ssl_mode_and_client() {
        let mut s = server("mysql");
        assert!(mysql_tls_args(&s, false).unwrap().is_empty());
        assert_eq!(
            mysql_tls_args(&s, true).unwrap(),
            vec!["--ssl", "--skip-ssl-verify-server-cert"]
        );
        s.ssl_mode = Some("require".into());
        assert_eq!(
            mysql_tls_args(&s, false).unwrap(),
            vec!["--ssl-mode=REQUIRED"]
        );
        assert_eq!(
            mysql_tls_args(&s, true).unwrap(),
            vec!["--ssl", "--skip-ssl-verify-server-cert"]
        );
        s.ssl_mode = Some("verify-full".into());
        assert_eq!(
            mysql_tls_args(&s, false).unwrap(),
            vec!["--ssl-mode=VERIFY_IDENTITY"]
        );
        assert_eq!(
            mysql_tls_args(&s, true).unwrap(),
            vec!["--ssl", "--ssl-verify-server-cert"]
        );
        s.ssl_mode = Some("disable".into());
        assert_eq!(mysql_tls_args(&s, true).unwrap(), vec!["--skip-ssl"]);
        // --ssl-ca would turn TLS back on for MariaDB, but a root CA is only accepted with the
        // verifying modes in the first place
        s.ssl_root_cert = Some("/etc/ssl/ca.pem".into());
        assert!(mysql_tls_args(&s, true).is_err());
    }

    #[test]
    fn pg_dump_args_and_env() {
        let s = server("postgres");
        let mut req = request();
        req.no_owner = true;
        req.table = Some(DumpTable {
            schema: Some("Sales".into()),
            name: "a\"b*".into(),
        });
        let endpoint = Endpoint {
            host: "h".into(),
            tunnel_addr: None,
            port: 5433,
        };
        let (args, env) = pg_dump_args(&s, &req, Some("shop"), &endpoint).unwrap();
        assert_eq!(
            args,
            vec![
                "--dbname=host='h' port=5433 user='app' dbname='shop'",
                "--no-password",
                "--clean",
                "--if-exists",
                "--no-owner",
                "--no-privileges",
                "--table=\"Sales\".\"a\"\"b*\"",
            ]
        );
        assert!(env.contains(&("PGSSLMODE".into(), "prefer".into())));
        assert!(!env.iter().any(|(k, _)| k == "PGPASSWORD"));
    }

    #[test]
    fn pg_dump_custom_format_and_data_only() {
        let s = server("postgres");
        let mut req = request();
        req.format = DumpFormat::Custom;
        req.content = DumpContent::Data;
        // --no-owner only applies to plain dumps (pg_restore decides it for archives)
        req.no_owner = true;
        let endpoint = Endpoint {
            host: "db.example.com".into(),
            tunnel_addr: Some("127.0.0.1".into()),
            port: 40001,
        };
        let (args, _) = pg_dump_args(&s, &req, None, &endpoint).unwrap();
        assert_eq!(
            args,
            vec![
                // Through a tunnel: connect to hostaddr, verify the certificate against host
                "--dbname=host='db.example.com' hostaddr='127.0.0.1' port=40001 user='app'",
                "--no-password",
                "--format=custom",
                "--data-only",
            ]
        );
    }

    #[test]
    fn pg_database_names_cannot_inject_connection_parameters() {
        let s = server("postgres");
        let endpoint = Endpoint {
            host: "h".into(),
            tunnel_addr: None,
            port: 5432,
        };
        let (args, _) = pg_dump_args(
            &s,
            &request(),
            Some("x host=evil.example.com o'k\\"),
            &endpoint,
        )
        .unwrap();
        assert_eq!(
            args[0],
            "--dbname=host='h' port=5432 user='app' dbname='x host=evil.example.com o\\'k\\\\'"
        );
    }

    #[test]
    fn option_file_escapes_the_password() {
        assert_eq!(
            mysql_option_file_contents(Some("a\"b\\c\nd")),
            "[client]\npassword=\"a\\\"b\\\\c\\nd\"\n"
        );
    }

    #[test]
    fn mysqldump_defaults_to_root_like_sqlx() {
        let mut s = server("mysql");
        s.user = None;
        let args = mysqldump_args(&s, &request(), "shop", "h", 1, false, None).unwrap();
        assert!(args.contains(&"--user=root".to_string()), "{args:?}");
        assert_eq!(mysql_option_file_contents(None), "[client]\n");
    }

    #[test]
    fn version_parsing_orders_pg_dump() {
        assert_eq!(
            parse_version("pg_dump (PostgreSQL) 16.3 (Homebrew)"),
            vec![16, 3]
        );
        assert_eq!(parse_version("pg_dump (PostgreSQL) 9.6.24"), vec![9, 6, 24]);
        assert_eq!(
            parse_version("mysqldump  Ver 8.4.0 for macos14 on arm64 (Homebrew)"),
            vec![8, 4, 0]
        );
        assert!(parse_version("no version").is_empty());
        assert!(
            parse_version("pg_dump (PostgreSQL) 17.0") > parse_version("pg_dump (PostgreSQL) 16.9")
        );
    }

    #[test]
    fn database_resolution() {
        let mut req = request();
        assert_eq!(
            resolve_database(&req, Some("shop")).as_deref(),
            Some("shop")
        );
        assert_eq!(resolve_database(&req, Some("")), None);
        assert_eq!(
            resolve_database(&req, Some(" shop ")).as_deref(),
            Some(" shop ")
        );
        req.database = Some("other".into());
        assert_eq!(
            resolve_database(&req, Some("shop")).as_deref(),
            Some("other")
        );
    }

    #[test]
    fn preview_hides_the_password() {
        let s = server("mysql");
        let line = preview_command(&s, &request(), Some("shop"), false).unwrap();
        assert!(
            line.contains("'--defaults-file=<temporary file with the password>'"),
            "{line}"
        );
        assert!(!line.contains("p\"w"));
        assert!(line.ends_with("> /tmp/out.sql"));

        let s = server("postgres");
        let mut req = request();
        req.tool_path = "/opt/homebrew/opt/libpq/bin/pg_dump".into();
        let line = preview_command(&s, &req, Some("shop"), false).unwrap();
        assert!(
            line.contains("\"--dbname=host='db.example.com' port=3307 user='app' dbname='shop'\""),
            "{line}"
        );
        assert!(line.contains("PGPASSWORD=<hidden>"));
        assert!(!line.contains("p\"w"));
    }

    #[test]
    fn unsupported_engines_are_rejected() {
        assert!(tool_for_server(&server("sqlite")).is_err());
        assert!(tool_for_server(&server("mssql")).is_err());
        assert_eq!(
            tool_for_server(&server("postgres")).unwrap(),
            DumpTool::PgDump
        );
    }

    /// End to end with a fake tool: arguments, environment, partial file and rename
    #[cfg(unix)]
    #[tokio::test]
    async fn run_dump_with_a_fake_pg_dump() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("pg_dump");
        std::fs::write(
            &tool,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'pg_dump (PostgreSQL) 16.0'; exit 0; fi\n\
             echo \"-- args: $*\"\necho \"-- pw: $PGPASSWORD\"\necho 'a warning' >&2\n",
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = dir.path().join("out.sql");
        let mut req = request();
        req.tool_path = tool.to_string_lossy().into_owned();
        req.output_path = output.to_string_lossy().into_owned();
        let s = server("postgres");
        let outcome = run_dump(&s, &req, Some("shop"), CancellationToken::new(), |_| {})
            .await
            .unwrap();
        let text = std::fs::read_to_string(&output).unwrap();
        assert!(text.contains("dbname='shop'"), "{text}");
        assert!(text.contains("-- pw: p\"w"));
        assert_eq!(outcome.warnings, "a warning");
        assert_eq!(outcome.bytes, text.len() as u64);
        // The tool and the dump; no partial file left
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // The dump is the database's contents: readable only by the user
        let mode = std::fs::metadata(&output).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_dump_leaves_no_file_and_reports_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("mysqldump");
        std::fs::write(
            &tool,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'mysqldump  Ver 8.4.0'; exit 0; fi\n\
             case \"$1\" in --defaults-file=*) f=\"${1#*=}\";; esac\n\
             echo partial\ncat \"$f\" >&2\nexit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = dir.path().join("out.sql");
        let mut req = request();
        req.tool_path = tool.to_string_lossy().into_owned();
        req.output_path = output.to_string_lossy().into_owned();
        let s = server("mysql");
        let err = run_dump(&s, &req, Some("shop"), CancellationToken::new(), |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("code 2"), "{err}");
        // The option file held the password and was readable by the tool...
        assert!(err.contains("password=\"p\\\"w\""), "{err}");
        // ...and neither it nor the partial output is left behind
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let leftovers = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("queryfolio-dump-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    /// Against a real server with a real pg_dump. Runs only when both are given:
    ///   QUERYFOLIO_TEST_PG_DUMP=/path/to/pg_dump \
    ///   QUERYFOLIO_TEST_PG_DUMP_SERVER='{host: 127.0.0.1, port: 5432, user: u, password: p, schema: db}' \
    ///   cargo test dump::tests::real_pg_dump
    /// The database must have a table "Sales"."order*s" (the name exercises the pattern quoting:
    /// unquoted, `*` would also match other tables).
    #[tokio::test]
    async fn real_pg_dump() {
        let (Ok(tool), Ok(server_yaml)) = (
            std::env::var("QUERYFOLIO_TEST_PG_DUMP"),
            std::env::var("QUERYFOLIO_TEST_PG_DUMP_SERVER"),
        ) else {
            return;
        };
        let mut server: serde_yaml::Mapping = serde_yaml::from_str(&server_yaml).unwrap();
        server.insert("name".into(), "pg".into());
        server.insert("engine".into(), "postgres".into());
        let server: ServerConfig = serde_yaml::from_value(server.into()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("table.sql");
        let mut req = request();
        req.tool_path = tool;
        req.output_path = output.to_string_lossy().into_owned();
        req.table = Some(DumpTable {
            schema: Some("Sales".into()),
            name: "order*s".into(),
        });
        run_dump(
            &server,
            &req,
            server.schema.as_deref(),
            CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        let text = std::fs::read_to_string(&output).unwrap();
        assert!(
            text.contains("CREATE TABLE \"Sales\".\"order*s\""),
            "{text}"
        );
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("DROP TABLE IF EXISTS"), "{text}");
        assert!(!text.contains("orderxs"), "{text}");

        // A wrong password fails with pg_dump's own message instead of waiting for input
        let mut wrong = server.clone();
        wrong.password = Some("wrong".into());
        let err = run_dump(
            &wrong,
            &req,
            wrong.schema.as_deref(),
            CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("password authentication failed"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_the_tool() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("pg_dump");
        std::fs::write(&tool, "#!/bin/sh\necho start\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = dir.path().join("out.sql");
        let mut req = request();
        req.tool_path = tool.to_string_lossy().into_owned();
        req.output_path = output.to_string_lossy().into_owned();
        let s = server("postgres");
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let err = run_dump(&s, &req, None, cancel, |_| {}).await.unwrap_err();
        assert!(matches!(err, AppError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
