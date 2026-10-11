//! CLI options that finish without launching the GUI (`--help` / `--version` / `--license` / `--list-servers`).
//!
//! The `open` / `write` subcommands ([`crate::router`]) are for "launching the app and
//! opening a file", whereas what is handled here only **writes to standard output and exits**,
//! so it is kept separate from the router. Before `run()` in lib.rs assembles Tauri, it looks at
//! [`info_command_from_args`] and, if it matches, prints and exits.
//!
//! The output is built by pure functions that depend on neither Tauri nor the filesystem and
//! is pinned by unit tests (in particular `--list-servers` must **not print passwords**, which
//! is a requirement, so the tests guarantee it).

use crate::config::{ConnectionInfo, ServerConfig};
use crate::db::Engine;
use std::path::Path;

/// Options that write to standard output and finish without launching the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoCommand {
    /// Print the usage.
    Help,
    /// Print the version.
    Version,
    /// Print the list of licenses of the bundled dependency libraries.
    License,
    /// Print the list of configured connections.
    ListServers,
}

/// Extract an [`InfoCommand`] from the startup arguments.
///
/// **Returns `None` if an `open` / `write` subcommand appears first.**
/// Even if the third argument of `write` (the query content) contains a string like `--help`,
/// that is the content to be written out, not an option
/// (so a case like `queryfolio write conn a.sql "-- help"` is not mistaken).
///
/// It scans instead of fixing the position because macOS may insert an argument such as
/// `-psn_0_12345` at the front when launching the `.app`
/// (`router::route_from_cli_args` scans for the same reason).
pub fn info_command_from_args<S: AsRef<str>>(args: &[S]) -> Option<InfoCommand> {
    for arg in args {
        match arg.as_ref() {
            // If a subcommand comes first, everything after it is its arguments, so do not look at them
            "open" | "write" => return None,
            "--help" | "-h" | "help" => return Some(InfoCommand::Help),
            "--version" | "-V" => return Some(InfoCommand::Version),
            "--license" => return Some(InfoCommand::License),
            "--list-servers" => return Some(InfoCommand::ListServers),
            _ => {}
        }
    }
    None
}

/// The version number of the app.
///
/// `build.rs` embeds it from `version` in `tauri.conf.json`.
/// **Do not use `CARGO_PKG_VERSION`** -- the release version is managed on the
/// `tauri.conf.json` side, and the version in Cargo.toml does not follow it
/// (even if the distributed build is 0.1.4, `--version` would answer 0.1.0).
const APP_VERSION: &str = env!("QUERYFOLIO_VERSION");

/// Usage printed by `--help`.
pub fn help_text() -> String {
    let version = APP_VERSION;
    format!(
        "Queryfolio {version} - a multi-purpose SQL GUI client

USAGE:
    queryfolio                                       Launch the app
    queryfolio open <path>                           Open a saved query file by path
    queryfolio write <connection> <file> [content]   Write a query file and open it
    queryfolio --list-servers                        List the configured connections
    queryfolio --help                                Show this help
    queryfolio --version                             Show the version
    queryfolio --license                             Show the licenses of the bundled libraries

OPEN
    <path> has to be a query file directly under a connection folder in the
    query files directory. Paths outside it are rejected.

WRITE
    <connection> is the connection name in the config, not a folder name.
    The file extension of the connection's engine is added when it is missing,
    and the connection folder is created when it does not exist yet.
    [content] can also be piped in on stdin. When no content is given, an empty
    file is created and an existing file is left as it is.

    echo 'SELECT 1;' | queryfolio write reporting check.sql

    Both subcommands hand the file to the running window when one is open.

CONFIG
    ~/.config/queryfolio/config.yml (config.yaml is used when it is missing).
    QUERYFOLIO_CONFIG_YAML overrides it with the YAML in the variable itself.

On macOS the app bundle takes the same arguments after --args:

    open -a Queryfolio --args open /path/to/query.sql
"
    )
}

/// The single line printed by `--version`.
pub fn version_text() -> String {
    format!("Queryfolio {APP_VERSION}")
}

/// On Windows, deliver the output of the info options to the caller's terminal.
///
/// Because of `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` in `main.rs`,
/// **a Windows release build is linked as a GUI subsystem and the process is not assigned a
/// console**. In that state `GetStdHandle(STD_OUTPUT_HANDLE)`, which Rust's std uses as the
/// write destination, returns an invalid handle and the content of `print!` is silently
/// discarded (the process exits with nothing shown in the terminal). The feature cannot work for
/// `--help` / `--version` / `--list-servers`, whose whole purpose is printing, so before printing
/// we re-attach to the console of the parent process (the cmd.exe / PowerShell that launched us).
///
/// If standard output is already valid (redirected to a pipe or file, the parent has no console,
/// etc.), `AttachConsole` merely fails and the original destination is used as-is. That is why
/// the return value is not checked -- a failure here means "the same as before", and there is
/// nothing to report. The equivalent of the C runtime's `freopen("CONOUT$")` is not needed
/// (Rust's std looks up `GetStdHandle` again on every write).
///
/// **This path is unverified on a real Windows machine** (neither the dev host nor CI has Windows).
/// It is called only right before printing the info options, so even if it fails the output
/// simply stays absent as before, and the GUI launch path is unaffected.
#[cfg(windows)]
pub fn attach_parent_console() {
    // (DWORD)-1 = ATTACH_PARENT_PROCESS
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

    extern "system" {
        fn AttachConsole(dwProcessId: u32) -> i32;
    }

    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Does nothing on non-Windows platforms (it can always write to the console).
#[cfg(not(windows))]
pub fn attach_parent_console() {}

/// Columns of the `--list-servers` table.
const COLUMNS: [&str; 9] = [
    "NAME", "ENGINE", "HOST", "PORT", "USER", "DATABASE", "SSL", "SSH", "FOLDER",
];

/// What is shown for a field with no value.
const EMPTY: &str = "-";

/// What is shown for a field whose TLS state cannot be determined because the config is broken.
/// Use a word that collides with neither a valid mode name (`disable` / `prefer` / ...) nor `on` / `off`.
const INVALID: &str = "invalid";

/// What is shown for a field that is hidden because the value itself is a credential.
/// Use a word distinguishable from `EMPTY` (unset) -- "the AWS key is not set" and
/// "it is set but not shown" are different things, so do not use the same `-` for both.
const HIDDEN: &str = "(hidden)";

/// The value shown in the USER column.
///
/// **The `user` of dynamodb is an AWS access key ID, not a DB user name.**
/// It is not as sensitive as a secret key, but it is an identifier that is half of a credential,
/// so it should not remain in a terminal, its history, or logs. Treat it the same way as
/// `folder_meta.rs`, which replaces it with `(aws access key, hidden)` (this is one column of a table, so use a short word).
///
/// The `user` of other engines is just a user name, so it is shown as is
/// (which account the config connects with is the very purpose of looking at this list).
fn user_cell(server: &ServerConfig, info: &ConnectionInfo) -> String {
    match info.user.as_deref() {
        Some(_) if server.engine.eq_ignore_ascii_case("dynamodb") => HIDDEN.to_string(),
        Some(user) => user.to_string(),
        None => EMPTY.to_string(),
    }
}

/// Whether the connection config overrides the endpoint (points at dynamodb-local etc.).
///
/// The decision is aligned with the branch in [`crate::engines::dynamodb::build_client`].
/// That is exactly the condition under which it builds `endpoint_url`, so if they diverge we get
/// either "showing `tls` although it is not overriding" or
/// "showing `on` although it is overriding".
fn has_endpoint_override(server: &ServerConfig) -> bool {
    server
        .host
        .as_deref()
        .map(str::trim)
        .is_some_and(|host| !host.is_empty())
}

/// Resolve the **environment variables** that the AWS SDK reads as an endpoint override, in the same precedence as the SDK.
///
/// Even for a dynamodb connection with no `host` in its config, the SDK does not necessarily use
/// the regional endpoint: `aws_config::defaults` looks at the environment variables and the
/// `endpoint_url` setting of the profile, so if `AWS_ENDPOINT_URL=http://localhost:8000` is in effect it connects in **plaintext**.
/// Assuming "no `host` = https" would show a plaintext connection as `on` in that environment.
///
/// The precedence follows aws-config's `endpoint_url` / `env_service_config`:
/// if `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` is true the override is ignored; otherwise the order is
/// per-service (`AWS_ENDPOINT_URL_DYNAMODB`) then global (`AWS_ENDPOINT_URL`).
///
/// **The `endpoint_url` / `services` sections of the profile file (`~/.aws/config`) are not
/// looked at.** We want to keep this a pure path that does not touch the filesystem, and mirroring
/// the SDK's profile resolution (deciding the profile name, referring to the `services` section,
/// `AWS_CONFIG_FILE`, etc.) would create a second implementation that diverges from the real one.
/// In an environment that overrides the endpoint through a profile, this column shows the regional endpoint (`on`).
///
/// The environment variable reader is a parameter for the sake of tests (tests that
/// modify the process's environment variables interfere with each other when run in parallel).
pub fn aws_endpoint_override(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let non_empty = |value: String| {
        let trimmed = value.trim().to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    };
    let ignored = get("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS")
        .and_then(non_empty)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    if ignored {
        return None;
    }
    get("AWS_ENDPOINT_URL_DYNAMODB")
        .and_then(non_empty)
        .or_else(|| get("AWS_ENDPOINT_URL").and_then(non_empty))
}

/// Resolve [`aws_endpoint_override`] from the process's environment variables.
pub fn aws_endpoint_override_from_env() -> Option<String> {
    aws_endpoint_override(|key| std::env::var(key).ok())
}

/// Decide the value of the SSL column from the scheme of the endpoint URL.
///
/// **Do not show `on` unless it is `https://`.** The `on` in this column means "encrypted", so
/// it must not become the place where unverifiable values are rounded off to.
///
/// `aws_config`'s `parse_url` accepts anything that `url::Url::parse` can parse, so a
/// non-http(s) scheme such as `ftp://...` is also passed to the SDK as is and fails at connection time.
/// Since that config cannot connect, show `invalid` (the same treatment as an invalid `ssl_mode`).
/// Conversely, a value that cannot be interpreted as a URL is warned about and discarded by the SDK,
/// which falls back to the regional endpoint (https), so `on`.
fn scheme_summary(endpoint: &str) -> &'static str {
    let lower = endpoint.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        "on"
    } else if lower.starts_with("http://") {
        "off"
    } else if lower.contains("://") {
        // A scheme other than http(s). The SDK accepts it, but this URL cannot be connected to
        INVALID
    } else {
        // A value that cannot be interpreted as a URL. parse_url makes it an error, so the override has no effect
        "on"
    }
}

/// Express the TLS / SSL state in one word.
///
/// For mysql / postgres / redis, show the effective mode ([`ConnectionInfo::sql_ssl_mode`])
/// as is (`disable` / `prefer` / `require` / `verify-ca` / `verify-full`).
/// It is not rounded to yes / no, because losing the distinction that "`prefer` may not be encrypted"
/// would make it impossible to check the safety of a connection from this list.
///
/// For other engines (elasticsearch / dynamodb, etc.) there is no effective mode, so
/// the `tls` of the config is shown as `on` / `off` as is.
///
/// **However, for engines where `tls` does not decide the actual connection method, it is not shown as is.**
/// The `host` / `port` / `tls` of dynamodb exist only to override the endpoint for dynamodb-local;
/// for a normal AWS connection without `host`, the SDK **always resolves the regional endpoint
/// over https** (`engines::dynamodb::build_client` builds `endpoint_url` only when there is a `host`).
/// Showing the default value `false` of `tls` as is would make an encrypted connection look like
/// `off` = plaintext.
///
/// **Even when `sql_ssl_mode` is `None`, it must not simply fall back to `tls`.**
/// `ConnectionInfo::from` also makes it `None` not only for "engines with no effective mode" but
/// also when "the value of `engine` / `ssl_mode` is invalid and could not be resolved".
/// Showing the latter as `on` / `off` of `tls` would **present a config that errors at connection
/// time as a valid TLS config** (a typo like `ssl_mode: requre` would read as
/// `off` = connects in plaintext). This column is for checking the safety of a connection, so
/// when it cannot be decided it shows `invalid` rather than hiding it.
///
/// **Conversely, the range in which `invalid` is shown is limited to "engines where the connection actually fails with that value".**
/// Only the mysql / postgres / mssql branches of `db::connect` read `ssl_mode`; the connection paths of
/// elasticsearch / sqlite / duckdb / dynamodb do not look at it. Even if an invalid `ssl_mode` has crept in
/// through a shared template or the like, **those connect normally**, so showing `invalid`
/// would make a usable connection look broken. The meaning of `invalid` is
/// "this config will not connect", not "an invalid value is written in the config".
///
/// **Even if `ssl_mode` can be resolved, some TLS setting combinations are rejected.**
/// A config that lists `ssl_root_cert` together with a mode that does not verify it (`disable` / `prefer` / `require`)
/// is made an error by `sql_ssl_root_cert` (the design avoids leaving the misunderstanding
/// that "specifying a CA means it is verified", since sqlx silently ignores the CA), so `db::connect`
/// always fails. Looking only at the effective mode and showing `prefer` would present a config that
/// cannot connect as a valid one. **We do not go as far as checking whether it can be opened as a file (`is_file` in
/// `db::ssl_root_cert_path`)** -- building this list is a pure function that does not touch the
/// filesystem and is pinned by unit tests, and a missing file that can be placed later is different from a config mistake.
fn ssl_summary(server: &ServerConfig, info: &ConnectionInfo, aws_endpoint: Option<&str>) -> String {
    // A connection whose engine name itself cannot be resolved does not connect by any path
    let Ok(engine) = crate::db::parse_engine(&server.engine) else {
        return INVALID.to_string();
    };
    // sql_ssl_mode() returns Err only when ssl_mode is set and cannot be resolved
    // (when unset it returns a default derived from tls), so a connection with it unset is not wrongly marked invalid.
    // sql_ssl_root_cert() does not look at the mode when ssl_root_cert is unset,
    // so both need to be called (the former resolves the value, the latter validates the combination)
    if matches!(engine, Engine::MySql | Engine::Postgres | Engine::MsSql)
        && (server.sql_ssl_mode().is_err() || server.sql_ssl_root_cert().is_err())
    {
        return INVALID.to_string();
    }
    if let Some(mode) = &info.sql_ssl_mode {
        return mode.clone();
    }
    // For dynamodb whose connection config does not override the endpoint, show not the tls flag but the scheme of the
    // endpoint the SDK actually resolves (the https regional endpoint unless an environment
    // variable overrides it)
    if engine == Engine::DynamoDb && !has_endpoint_override(server) {
        return match aws_endpoint {
            Some(endpoint) => scheme_summary(endpoint),
            None => "on",
        }
        .to_string();
    }
    if server.tls { "on" } else { "off" }.to_string()
}

/// Turn control characters into visible representations before putting them in a table cell.
///
/// Config values come not only from `config.yml` but also from the output of `config_override_command`.
/// If newlines or tabs are mixed in, the "1 connection = 1 line" shape breaks and rows of other connections can be
/// forged, and if ANSI / OSC escape sequences are mixed in they get interpreted by the terminal.
/// Keeping the row shape intact takes priority over showing the contents of the value.
fn sanitize_cell(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() {
                // If invisible characters simply "disappeared" we could not tell what was in there,
                // so replace them with visible symbols instead of dropping them
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// Build the body of `--list-servers`.
///
/// **Passwords, SSH keys and passphrases, and AWS access key IDs are not shown.**
/// The items shown are limited to [`ConnectionInfo`] (the projection passed to the frontend that "contains no secrets")
/// and the folder name, and the only places that read fields of [`ServerConfig`] directly are
/// the TLS decision (`tls`) and the USER column masking decision (`engine`).
/// Keep to this path when adding items.
///
/// Note that [`ConnectionInfo`] is a projection that "may be passed to the frontend (your own screen)"
/// and not one that "may be shown in a terminal". There are **fields whose meaning
/// changes depending on the engine**, like `user`, so do not pass them through as they are;
/// insert a purpose-specific decision such as [`user_cell`].
pub fn format_server_list(
    servers: &[ServerConfig],
    sqlfiles_dir: &Path,
    aws_endpoint: Option<&str>,
) -> String {
    let mut out = format!(
        "Query files directory: {}\n",
        sanitize_cell(&sqlfiles_dir.display().to_string())
    );
    if servers.is_empty() {
        out.push_str("\nNo connection is configured.\n");
        return out;
    }

    let rows: Vec<[String; 9]> = servers
        .iter()
        .map(|server| {
            let info = ConnectionInfo::from(server);
            let cells = [
                info.name.clone(),
                info.engine.clone(),
                info.host.clone().unwrap_or_else(|| EMPTY.to_string()),
                info.port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| EMPTY.to_string()),
                user_cell(server, &info),
                info.schema.clone().unwrap_or_else(|| EMPTY.to_string()),
                ssl_summary(server, &info, aws_endpoint),
                if info.has_ssh_tunnel { "yes" } else { EMPTY }.to_string(),
                server.sqlfiles_folder_name(),
            ];
            cells.map(|cell| sanitize_cell(&cell))
        })
        .collect();

    // Column widths match the longest of the header and the values (in characters).
    let widths: Vec<usize> = (0..COLUMNS.len())
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain(std::iter::once(COLUMNS[i].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();

    out.push('\n');
    out.push_str(&join_row(&COLUMNS.map(|c| c.to_string()), &widths));
    for row in &rows {
        out.push_str(&join_row(row, &widths));
    }
    out
}

/// Join one line according to the column widths (trailing padding is dropped).
fn join_row(cells: &[String; 9], widths: &[usize]) -> String {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        if i + 1 < cells.len() {
            // chars().count() is used because both header and values are aligned by
            // character count, not display width (a compromise; names containing full-width characters are slightly off).
            let pad = widths[i].saturating_sub(cell.chars().count());
            line.push_str(&" ".repeat(pad));
        }
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> ServerConfig {
        serde_yaml::from_str(&format!(
            "name: {name}\nengine: postgres\nhost: db.example.com\nport: 5432\n\
             user: app\nschema: appdb\npassword: s3cret\n"
        ))
        .expect("test fixture should parse")
    }

    #[test]
    fn test_info_command_from_args() {
        assert_eq!(info_command_from_args(&["--help"]), Some(InfoCommand::Help));
        assert_eq!(info_command_from_args(&["-h"]), Some(InfoCommand::Help));
        assert_eq!(info_command_from_args(&["help"]), Some(InfoCommand::Help));
        assert_eq!(
            info_command_from_args(&["--version"]),
            Some(InfoCommand::Version)
        );
        assert_eq!(info_command_from_args(&["-V"]), Some(InfoCommand::Version));
        assert_eq!(
            info_command_from_args(&["--license"]),
            Some(InfoCommand::License)
        );
        assert_eq!(
            info_command_from_args(&["--list-servers"]),
            Some(InfoCommand::ListServers)
        );
        // The option is still picked up even when .app launch inserts arguments in front of it
        assert_eq!(
            info_command_from_args(&["-psn_0_12345", "--list-servers"]),
            Some(InfoCommand::ListServers)
        );
        // No arguments means a GUI launch
        assert_eq!(info_command_from_args::<&str>(&[]), None);
        assert_eq!(info_command_from_args(&["--unknown"]), None);
    }

    #[test]
    fn test_subcommand_wins_over_option_like_argument() {
        // Even if the content of write contains something that looks like an option, it is just the content
        assert_eq!(
            info_command_from_args(&["write", "conn", "a.sql", "--help"]),
            None
        );
        assert_eq!(
            info_command_from_args(&["write", "conn", "a.sql", "--list-servers"]),
            None
        );
        assert_eq!(info_command_from_args(&["open", "--help"]), None);
        // Picked up if it comes before the subcommand
        assert_eq!(
            info_command_from_args(&["--help", "write", "conn", "a.sql"]),
            Some(InfoCommand::Help)
        );
    }

    /// Output the version of tauri.conf.json, which is the basis of releases.
    /// If it were reverted to `CARGO_PKG_VERSION`, we would fail to notice when the two diverge.
    #[test]
    fn test_version_comes_from_the_tauri_config() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json should be valid JSON");
        let expected = conf["version"]
            .as_str()
            .expect("version should be a string");

        assert_eq!(version_text(), format!("Queryfolio {expected}"));
        assert!(help_text().starts_with(&format!("Queryfolio {expected} ")));
    }

    #[test]
    fn test_help_text_lists_every_entry_point() {
        let help = help_text();
        for expected in [
            "queryfolio open <path>",
            "queryfolio write <connection> <file> [content]",
            "--list-servers",
            "--help",
            "--version",
            "--license",
        ] {
            assert!(help.contains(expected), "help should mention {expected}");
        }
    }

    #[test]
    fn test_format_server_list_never_shows_the_password() {
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"), None);
        assert!(
            !out.contains("s3cret"),
            "the password must not be printed:\n{out}"
        );
        assert!(!out.to_lowercase().contains("password"));
    }

    /// The secrets of an SSH tunnel (password / path of the private key / passphrase /
    /// agent socket) are not shown either. Stops here the regression where a secret field
    /// added to `ServerConfig` leaks into this list.
    #[test]
    fn test_format_server_list_never_shows_the_ssh_secrets() {
        let with_tunnel: ServerConfig = serde_yaml::from_str(
            "name: through-bastion\n\
             engine: postgres\n\
             host: db.internal\n\
             port: 5432\n\
             user: app\n\
             password: db-p4ssword\n\
             ssh_tunnel:\n\
             \x20 host: bastion.example.com\n\
             \x20 port: 22\n\
             \x20 user: jump-user\n\
             \x20 password: ssh-p4ssword\n\
             \x20 private_key_path: /home/me/.ssh/id_ed25519_secret\n\
             \x20 private_key_passphrase: k3y-passphrase\n\
             \x20 identity_agent: /run/user/1000/secret-agent.sock\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[with_tunnel], Path::new("/tmp/sqlfiles"), None);

        for secret in [
            "db-p4ssword",
            "ssh-p4ssword",
            "id_ed25519_secret",
            "k3y-passphrase",
            "secret-agent.sock",
        ] {
            assert!(
                !out.contains(secret),
                "{secret} must not be printed:\n{out}"
            );
        }
        // It can be seen that a tunnel is used (SSH column)
        assert!(out.contains("yes"), "{out}");
    }

    /// The `tls` of dynamodb exists only to override the endpoint for dynamodb-local, so
    /// it is not shown as is for a normal AWS connection without `host`.
    ///
    /// The SDK always resolves the regional endpoint over https, so showing the default value of `tls`
    /// (false) would **make an encrypted connection look like plaintext**.
    /// This column is for checking the safety of a connection, so this is the worst direction to be wrong in.
    #[test]
    fn test_format_server_list_reports_https_for_the_aws_dynamodb_endpoint() {
        // No host = AWS regional endpoint. https even if tls is not written
        let aws: ServerConfig =
            serde_yaml::from_str("name: events\nengine: dynamodb\nschema: ap-northeast-1\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[aws], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" on"), "{out}");
        assert!(!out.contains(" off"), "{out}");

        // A host that is only whitespace also counts as "unspecified" (aligned with the trim in build_client)
        let blank_host: ServerConfig = serde_yaml::from_str(
            "name: events\nengine: dynamodb\nschema: ap-northeast-1\nhost: \"   \"\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[blank_host], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" on"), "{out}");

        // A host is written = endpoint override for dynamodb-local etc. Here tls takes effect
        let local: ServerConfig = serde_yaml::from_str(
            "name: local\nengine: dynamodb\nschema: ap-northeast-1\n\
             host: 127.0.0.1\nport: 8000\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[local], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" off"), "{out}");

        let local_tls: ServerConfig = serde_yaml::from_str(
            "name: local\nengine: dynamodb\nschema: ap-northeast-1\n\
             host: 127.0.0.1\nport: 8000\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[local_tls], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" on"), "{out}");

        // Other engines without an effective mode show tls as is, same as before
        let es: ServerConfig =
            serde_yaml::from_str("name: search\nengine: elasticsearch\nhost: es.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[es], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" off"), "{out}");
    }

    /// Even for dynamodb with no `host`, if the endpoint is overridden by an environment variable
    /// it is not necessarily the regional endpoint. Show the one the SDK actually uses.
    #[test]
    fn test_format_server_list_follows_the_aws_endpoint_environment() {
        let aws: ServerConfig =
            serde_yaml::from_str("name: events\nengine: dynamodb\nschema: ap-northeast-1\n")
                .expect("test fixture should parse");

        // AWS_ENDPOINT_URL=http://... is plaintext. Showing this as on would make a plaintext
        // connection look encrypted
        let out = format_server_list(
            std::slice::from_ref(&aws),
            Path::new("/tmp/sqlfiles"),
            Some("http://localhost:8000"),
        );
        assert!(out.contains(" off"), "{out}");

        let out = format_server_list(
            std::slice::from_ref(&aws),
            Path::new("/tmp/sqlfiles"),
            Some("https://dynamodb.ap-northeast-1.amazonaws.com"),
        );
        assert!(out.contains(" on"), "{out}");
        assert!(!out.contains(" off"), "{out}");

        // A scheme in upper case can also be judged (the SDK does not distinguish case)
        let out = format_server_list(
            std::slice::from_ref(&aws),
            Path::new("/tmp/sqlfiles"),
            Some("HTTP://localhost:8000"),
        );
        assert!(out.contains(" off"), "{out}");

        // The SDK warns about and discards values it cannot interpret, and falls back to the regional endpoint
        let out = format_server_list(
            std::slice::from_ref(&aws),
            Path::new("/tmp/sqlfiles"),
            Some("not-a-url"),
        );
        assert!(out.contains(" on"), "{out}");

        // A scheme other than http(s) is accepted by the SDK but cannot connect.
        // **Do not round unverifiable values to on** (the on of this column means "encrypted")
        for endpoint in ["ftp://localhost:8000", "ws://localhost:8000"] {
            let out = format_server_list(
                std::slice::from_ref(&aws),
                Path::new("/tmp/sqlfiles"),
                Some(endpoint),
            );
            assert!(out.contains(INVALID), "{endpoint}:\n{out}");
            assert!(!out.contains(" on"), "{endpoint}:\n{out}");
        }

        // A host in the connection config = explicit endpoint override. That takes precedence, so
        // look at tls rather than the environment variable (build_client builds endpoint_url)
        let local: ServerConfig = serde_yaml::from_str(
            "name: local\nengine: dynamodb\nschema: ap-northeast-1\n\
             host: 127.0.0.1\nport: 8000\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(
            &[local],
            Path::new("/tmp/sqlfiles"),
            Some("http://localhost:9999"),
        );
        assert!(out.contains(" on"), "{out}");

        // Other engines are unrelated to the AWS environment variables
        let es: ServerConfig =
            serde_yaml::from_str("name: search\nengine: elasticsearch\nhost: es.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(
            &[es],
            Path::new("/tmp/sqlfiles"),
            Some("https://localhost:8000"),
        );
        assert!(out.contains(" off"), "{out}");
    }

    /// The precedence of environment variables follows aws-config.
    #[test]
    fn test_aws_endpoint_override_precedence() {
        let env = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            move |key: &str| {
                owned
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.to_string())
            }
        };

        assert_eq!(aws_endpoint_override(env(&[])), None);

        // Per-service takes precedence over global
        assert_eq!(
            aws_endpoint_override(env(&[
                ("AWS_ENDPOINT_URL", "http://global:1"),
                ("AWS_ENDPOINT_URL_DYNAMODB", "http://service:2"),
            ])),
            Some("http://service:2".to_string())
        );
        assert_eq!(
            aws_endpoint_override(env(&[("AWS_ENDPOINT_URL", "http://global:1")])),
            Some("http://global:1".to_string())
        );

        // If AWS_IGNORE_CONFIGURED_ENDPOINT_URLS=true the override has no effect
        assert_eq!(
            aws_endpoint_override(env(&[
                ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "TRUE"),
                ("AWS_ENDPOINT_URL_DYNAMODB", "http://service:2"),
            ])),
            None
        );
        // Values other than true are ignored (false / an empty string must not kill the override)
        assert_eq!(
            aws_endpoint_override(env(&[
                ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "false"),
                ("AWS_ENDPOINT_URL", "http://global:1"),
            ])),
            Some("http://global:1".to_string())
        );

        // An empty / whitespace-only value is treated as unset and falls through to the next candidate
        assert_eq!(
            aws_endpoint_override(env(&[
                ("AWS_ENDPOINT_URL_DYNAMODB", "   "),
                ("AWS_ENDPOINT_URL", " http://global:1 "),
            ])),
            Some("http://global:1".to_string())
        );
    }

    /// The `user` of dynamodb is an AWS access key ID, so it is not shown in the USER column.
    ///
    /// `ConnectionInfo` is a projection that "contains no secrets", but that is the standard for
    /// **passing to the frontend**, not for showing in a terminal. Passing it through here would leave
    /// an identifier of a credential in the terminal, shell history, and logs.
    #[test]
    fn test_format_server_list_hides_the_aws_access_key_id() {
        let dynamo: ServerConfig = serde_yaml::from_str(
            "name: events\nengine: DynamoDB\nschema: ap-northeast-1\n\
             user: AKIAIOSFODNN7EXAMPLE\npassword: wJalrXUtnFEMI\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[dynamo], Path::new("/tmp/sqlfiles"), None);

        // Masked whether engine is spelled DynamoDB or dynamodb
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "{out}");
        assert!(!out.contains("wJalrXUtnFEMI"), "{out}");
        assert!(out.contains(HIDDEN), "{out}");
        // The connection itself still appears in the list (the row is not removed entirely)
        assert!(out.contains("events"), "{out}");

        // The user of other engines is a user name, so it is shown as before
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("app"), "{out}");
        assert!(!out.contains(HIDDEN), "{out}");
    }

    #[test]
    fn test_format_server_list_neutralizes_control_characters() {
        // Settings also come from the output of config_override_command. If newlines get through,
        // the "1 connection = 1 line" shape breaks and rows of other connections can be forged, and if ANSI / OSC
        // escapes get through they are interpreted by the terminal
        let hostile: ServerConfig = serde_yaml::from_str(
            "name: \"evil\\nfake-row  postgres\"\nengine: postgres\n\
             host: \"h\\u001b[31mred\\u001b[0m\"\nuser: \"a\\tb\"\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[hostile], Path::new("/tmp/sqlfiles"), None);

        // Only the header line + one connection line + the leading info line + a blank line
        assert_eq!(
            out.lines().count(),
            4,
            "a value must not be able to add a row:\n{out}"
        );
        assert!(!out.contains('\t'), "{out}");
        assert!(!out.contains('\u{1b}'), "{out}");
    }

    #[test]
    fn test_format_server_list_shows_the_requested_columns() {
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("Query files directory: /tmp/sqlfiles"));
        for expected in [
            "NAME", "ENGINE", "HOST", "PORT", "USER", "DATABASE", "SSL", "SSH", "FOLDER",
        ] {
            assert!(out.contains(expected), "header should contain {expected}");
        }
        assert!(out.contains("reporting"));
        assert!(out.contains("db.example.com"));
        assert!(out.contains("5432"));
        assert!(out.contains("app"));
        // The folder name is <host>_<engine>_<schema>_<user>
        assert!(out.contains("db.example.com_postgres_appdb_app"));
    }

    #[test]
    fn test_format_server_list_reports_the_effective_ssl_mode() {
        // A postgres with neither ssl_mode nor tls is prefer (it can downgrade to plaintext).
        // Rounding to yes/no would lose this distinction, so the effective mode is shown as is
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("prefer"), "{out}");

        let tls: ServerConfig = serde_yaml::from_str(
            "name: secure\nengine: postgres\nhost: db.example.com\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[tls], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("verify-full"), "{out}");

        // Engines without an effective mode show tls as is
        let es: ServerConfig = serde_yaml::from_str(
            "name: search\nengine: elasticsearch\nhost: es.example.com\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[es], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" on"), "{out}");
    }

    /// Do not present a config that cannot be resolved as a "valid TLS config".
    ///
    /// `ConnectionInfo::sql_ssl_mode` is also `None` when the value is invalid, so naively
    /// falling back to `tls` would let `ssl_mode: requre` (a typo) read as `off` = connects in plaintext.
    /// In reality it only errors at connection time, and no such mode exists.
    #[test]
    fn test_format_server_list_marks_unresolvable_ssl_settings() {
        let bad_mode: ServerConfig = serde_yaml::from_str(
            "name: typo\nengine: postgres\nhost: db.example.com\nssl_mode: requre\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[bad_mode], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(INVALID), "{out}");
        // Do not make a display that can be read as "connects in plaintext"
        assert!(!out.contains(" off"), "{out}");

        let bad_engine: ServerConfig =
            serde_yaml::from_str("name: unknown\nengine: mysqll\nhost: db.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[bad_engine], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(INVALID), "{out}");

        // Do not drag in engines that do not read `ssl_mode`. Even if an invalid value has crept in through a shared template or the like,
        // their connection paths (the corresponding branch of db::connect / engines/) do not look at
        // sql_ssl_mode, so they connect normally. The meaning of `invalid` is "this config
        // will not connect", so a usable connection must not be made to look broken
        // Even if ssl_mode can be resolved, a config whose combination is rejected does not connect.
        // Listing ssl_root_cert together with a mode that does not verify it makes sql_ssl_root_cert
        // return an error, so db::connect always fails
        for mode in ["disable", "prefer", "require"] {
            let cert_without_verify: ServerConfig = serde_yaml::from_str(&format!(
                "name: ca-{mode}\nengine: postgres\nhost: db.example.com\n\
                 ssl_mode: {mode}\nssl_root_cert: /etc/ssl/ca.pem\n"
            ))
            .expect("test fixture should parse");
            let out = format_server_list(&[cert_without_verify], Path::new("/tmp/sqlfiles"), None);
            assert!(out.contains(INVALID), "{mode}:\n{out}");
            // The resolvable effective mode must not be shown (because it cannot connect)
            assert!(!out.contains(&format!(" {mode}")), "{mode}:\n{out}");
        }

        // The default when ssl_mode is omitted is prefer, so this is not verified either
        let cert_without_mode: ServerConfig = serde_yaml::from_str(
            "name: ca-default\nengine: postgres\nhost: db.example.com\n\
             ssl_root_cert: /etc/ssl/ca.pem\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[cert_without_mode], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(INVALID), "{out}");

        // An empty ssl_root_cert is also an error in the same way (it is not silently treated as unset)
        let empty_cert: ServerConfig = serde_yaml::from_str(
            "name: ca-empty\nengine: postgres\nhost: db.example.com\n\
             ssl_mode: verify-full\nssl_root_cert: \"  \"\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[empty_cert], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(INVALID), "{out}");

        // Listing it together with a verifying mode is a correct config, so the effective mode is shown as is.
        // **Whether the file actually exists is not checked** (this function does not touch the filesystem)
        let verifying: ServerConfig = serde_yaml::from_str(
            "name: ca-ok\nengine: postgres\nhost: db.example.com\n\
             ssl_mode: verify-ca\nssl_root_cert: /nonexistent/ca.pem\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[verifying], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("verify-ca"), "{out}");
        assert!(!out.contains(INVALID), "{out}");

        for engine in ["elasticsearch", "sqlite", "duckdb", "dynamodb"] {
            let ignores_ssl_mode: ServerConfig = serde_yaml::from_str(&format!(
                "name: {engine}-conn\nengine: {engine}\nhost: h.example.com\n\
                 schema: s\nssl_mode: requre\n"
            ))
            .expect("test fixture should parse");
            let out = format_server_list(&[ignores_ssl_mode], Path::new("/tmp/sqlfiles"), None);
            assert!(!out.contains(INVALID), "{engine}:\n{out}");
        }

        // Must not drag in connections that do not write ssl_mode (engines without an
        // effective mode show tls on / off as before)
        let es: ServerConfig =
            serde_yaml::from_str("name: search\nengine: elasticsearch\nhost: es.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[es], Path::new("/tmp/sqlfiles"), None);
        assert!(!out.contains(INVALID), "{out}");
        assert!(out.contains(" off"), "{out}");
    }

    #[test]
    fn test_format_server_list_fills_missing_values() {
        let sqlite: ServerConfig =
            serde_yaml::from_str("name: local\nengine: sqlite\nschema: /tmp/a.db\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[sqlite], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("local"));
        // Columns must not shift even for rows with no host / port / user
        assert!(out.contains(EMPTY));
    }

    #[test]
    fn test_format_server_list_with_no_connection() {
        let out = format_server_list(&[], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("No connection is configured."), "{out}");
    }
}
