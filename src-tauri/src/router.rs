//! Common router that interprets `queryfolio://` URIs and CLI arguments.
//!
//! Both URIs (deep links) and CLI subcommands are turned into a [`Route`] here, and
//! lib.rs dispatches the [`Route`]. To add a new action later, just add a variant and
//! its parsing here and it works for both URI and CLI (this is the point of "make it
//! possible to keep adding features through routes like queryfolio://").
//!
//! However, **the write action (`write`) is CLI-only** and is not accepted from a URI
//! (`parse_uri` returns UnknownAction). A `queryfolio://` URL can be opened from a web
//! page, so allowing writes via URI would let a page being viewed plant arbitrary SQL as
//! a user's query file (and the user might run it later). The CLI is an action by the
//! person launching it, so this path does not exist there.
//!
//! Path resolution ([`resolve_open_target`]) is security-critical, so it is written with
//! plain std only, without depending on Tauri, and its boundaries are pinned down by unit
//! tests. The only thing that can be opened is "a query file in a connection folder
//! directly under the query file storage directory (`sqlfiles_dir`) (extensions are
//! [`ALLOWED_EXTENSIONS`]: `.sql` / `.redis` / `.es`)"; traversal via `..` and paths
//! outside the storage area are rejected. Whether the extension matches the connection's
//! engine is verified additionally by lib.rs (resolve_route_target), which can resolve
//! the connection.

use std::path::{Component, Path, PathBuf};

/// URI scheme name (`queryfolio://...`).
pub const URI_SCHEME: &str = "queryfolio";

/// CLI subcommand that opens an existing file by path.
const OPEN_SUBCOMMAND: &str = "open";

/// CLI subcommand that writes a query file into a connection folder and opens it.
const WRITE_SUBCOMMAND: &str = "write";

/// An action interpreted from a URI / CLI (holds the raw, not yet validated input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Open a query file by path. `path` is an unvalidated raw path
    /// (validate with `resolve_open_target` that it is under the storage area before use).
    OpenFile { path: String },
    /// Open a query file by specifying the connection (SQL server config) name and the file name.
    /// If `content` is present, write that content first and then open (CLI-only; see below).
    ///
    /// **The write itself is not performed when resolving this Route.** The CLI process
    /// finishes writing before launching Tauri, and then hands this Route to the running
    /// instance (or its own process) as an "open" instruction. Stdin content is not included
    /// in the argv forwarded by the single-instance plugin, so if the write were not
    /// completed on the launching side it would be lost on the forwarding path.
    /// For that reason the resolving side (lib.rs) does not look at `content`.
    WriteFile {
        /// Connection name (`ServerConfig::name`). Unvalidated.
        connection: String,
        /// File name. Unvalidated (the engine's extension is added if missing).
        file_name: String,
        /// Content to write (if omitted, the existing file is opened as is).
        content: Option<String>,
    },
}

/// Identifies the query file to open by connection name and (normalized) file name.
/// The frontend selects this connection and opens this file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTarget {
    /// Name of the connection the target file belongs to (`ServerConfig::name`).
    pub connection: String,
    /// File name to open (with extension; one component inside the connection folder).
    pub file_name: String,
}

/// Routing / path resolution errors. Passed to the frontend as the Display string
/// (English, since it is an in-app message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// Does not start with `queryfolio://`.
    NotQueryfolioUri,
    /// Unknown action (anything other than `open`).
    UnknownAction(String),
    /// The path to open is empty.
    EmptyPath,
    /// Points outside the query file storage directory.
    OutsideSqlfilesDir,
    /// Not in the form "connection folder / file" directly under the storage directory.
    NotUnderConnectionFolder,
    /// A folder name that matches no connection's folder.
    UnknownFolder(String),
    /// Multiple connections map to the same folder, so which connection to open with cannot be determined uniquely.
    AmbiguousFolder(String),
    /// Invalid file name (not a known query file extension, starts with a dot, etc.).
    InvalidFileName(String),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NotQueryfolioUri => {
                write!(f, "Not a {URI_SCHEME}:// URI")
            }
            RouteError::UnknownAction(a) => write!(f, "Unknown action: {a}"),
            RouteError::EmptyPath => write!(f, "The file path is empty"),
            RouteError::OutsideSqlfilesDir => write!(
                f,
                "The path is outside the query files directory"
            ),
            RouteError::NotUnderConnectionFolder => write!(
                f,
                "The path is not a file directly under a connection folder"
            ),
            RouteError::UnknownFolder(folder) => write!(
                f,
                "No connection matches the folder: {folder}"
            ),
            RouteError::AmbiguousFolder(folder) => write!(
                f,
                "Multiple connections map to the folder, cannot decide which to open: {folder}"
            ),
            RouteError::InvalidFileName(name) => {
                write!(f, "Invalid query file name: {name}")
            }
        }
    }
}

impl std::error::Error for RouteError {}

/// Interprets a URI of the form `queryfolio://open/<path>` into a [`Route`].
///
/// The only accepted action is `open`. `write` involves writing, so it is not accepted
/// from a URI (see the module documentation).
///
/// The action is taken from right after `queryfolio://` up to the first `/`. The rest is
/// the path to open (percent-decoded if encoded). When an absolute path is passed the `/`
/// doubles up, as in `queryfolio://open//abs/path.sql`, but after extracting `open` the
/// remaining `/abs/path.sql` becomes the path as is.
pub fn parse_uri(uri: &str) -> Result<Route, RouteError> {
    let scheme_prefix = format!("{URI_SCHEME}://");
    let rest = uri
        .strip_prefix(&scheme_prefix)
        .ok_or(RouteError::NotQueryfolioUri)?;
    // The action runs up to the first `/`. If there is no `/`, there is no path (= empty path).
    let (action, raw_path) = match rest.split_once('/') {
        Some((action, raw)) => (action, raw),
        None => (rest, ""),
    };
    match action {
        "open" => {
            let path = percent_decode(raw_path);
            if path.trim().is_empty() {
                return Err(RouteError::EmptyPath);
            }
            Ok(Route::OpenFile { path })
        }
        other => Err(RouteError::UnknownAction(other.to_string())),
    }
}

/// Interprets a CLI argument list into a [`Route`].
///
/// Two forms are handled:
///
/// - `open <path>` — open a saved query file by path
/// - `write <connection> <file-name> [content]` — write a query file into the
///   connection's folder and open it (if `content` is omitted the caller reads stdin;
///   if it cannot be read, the existing file is opened as is)
///
/// The argument list may include the program name (the argv forwarded by single-instance
/// includes argv[0]), so the starting point is the **first subcommand word that appears**
/// rather than a fixed leading position.
///
/// `queryfolio://` URL arguments are handled by the deep-link plugin, so they are not
/// accepted as the path of `open` (to prevent double handling). **They are not removed
/// from the whole argument list** — removing them would drop the content itself when the
/// `write` content is a URL (`write prod a.sql 'queryfolio://open/x'`). URL arguments never
/// match a subcommand word, so they do not interfere with finding the starting point either.
pub fn route_from_cli_args<S: AsRef<str>>(args: &[S]) -> Option<Route> {
    let scheme_prefix = format!("{URI_SCHEME}://");
    let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
    // Take whichever of open / write appears first (searching for both separately would
    // pick up a word buried in the arguments of the later subcommand)
    let pos = args
        .iter()
        .position(|a| *a == OPEN_SUBCOMMAND || *a == WRITE_SUBCOMMAND)?;
    match args[pos] {
        OPEN_SUBCOMMAND => {
            let path = args.get(pos + 1)?;
            if path.trim().is_empty() || path.starts_with(&scheme_prefix) {
                return None;
            }
            Some(Route::OpenFile {
                path: (*path).to_string(),
            })
        }
        WRITE_SUBCOMMAND => {
            let connection = args.get(pos + 1)?;
            let file_name = args.get(pos + 2)?;
            if connection.trim().is_empty() || file_name.trim().is_empty() {
                return None;
            }
            // Content is optional (stdin if omitted; if there is none either, nothing is written).
            // If an empty string is passed explicitly, keep Some("") as the intent of "write it empty".
            Some(Route::WriteFile {
                connection: (*connection).to_string(),
                file_name: (*file_name).to_string(),
                content: args.get(pos + 3).map(|c| (*c).to_string()),
            })
        }
        _ => None,
    }
}

/// Resolves a raw path as a query file in a connection folder under the storage directory.
///
/// - `sqlfiles_dir`: the query file storage directory. **The caller must make it an
///   absolute path** (if relative, the `cwd` base would disagree with the raw path: the
///   raw path is resolved against the cwd of the deep link / CLI origin, while the storage
///   directory is accessed relative to the app process's cwd. To avoid mixing the two,
///   making base absolute is the caller's responsibility).
/// - `folders`: a `(folder name, connection name)` table (in config order). The folder
///   name is what `ServerConfig::sqlfiles_folder_name()` returns.
/// - `raw_path`: the raw path of the target to open (`~` / relative paths are expanded with `home` / `cwd`).
/// - `home`: home directory used for `~` expansion (if absent, `~` is not expanded).
/// - `cwd`: base directory for relative paths (if absent, relative paths are left as is).
///
/// Success condition: the expanded, lexically normalized path has the form
/// `sqlfiles_dir/<folder>/<name>.sql` (exactly 2 levels), `<folder>` exists in `folders`,
/// and `<name>.sql` is a valid file name. Traversal via `..` is collapsed by lexical
/// normalization, and going outside the storage area yields `OutsideSqlfilesDir`
/// (the filesystem is not touched).
pub fn resolve_open_target(
    sqlfiles_dir: &Path,
    folders: &[(String, String)],
    raw_path: &str,
    home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<OpenTarget, RouteError> {
    let expanded = expand_path(raw_path, home, cwd);
    // base (sqlfiles_dir) has already been made absolute by the caller. Only the raw path is resolved against cwd.
    let normalized = lexical_normalize(&expanded);
    let base = lexical_normalize(sqlfiles_dir);

    let relative = normalized
        .strip_prefix(&base)
        .map_err(|_| RouteError::OutsideSqlfilesDir)?;

    // Directly under the storage directory there are exactly 2 components: "connection folder / file".
    let components: Vec<&std::ffi::OsStr> = relative
        .components()
        .map(|c| c.as_os_str())
        .collect();
    if components.len() != 2 {
        return Err(RouteError::NotUnderConnectionFolder);
    }
    let folder = components[0].to_string_lossy().into_owned();
    let file_name = components[1].to_string_lossy().into_owned();

    // If multiple connections map to the same folder (the same folder_name, or the generated
    // host/engine/schema/user folders happen to coincide), which connection to open with
    // cannot be determined uniquely. Silently picking the first might open it with a
    // connection to a different DB / different readonly policy, so treat it as ambiguous
    // and return an error.
    let mut matches = folders.iter().filter(|(f, _)| *f == folder);
    let connection = match (matches.next(), matches.next()) {
        (None, _) => return Err(RouteError::UnknownFolder(folder)),
        (Some(_), Some(_)) => return Err(RouteError::AmbiguousFolder(folder)),
        (Some((_, conn)), None) => conn.clone(),
    };

    validate_sql_file_name(&file_name)?;

    Ok(OpenTarget {
        connection,
        file_name,
    })
}

/// Extensions that can be opened as query files (per engine; keep in sync with
/// file_extension in engines::EngineCapabilities).
const ALLOWED_EXTENSIONS: &[&str] = &["sql", "redis", "es"];

/// Validates that a name is valid as a query file name
/// (same policy as validate_component / normalize_file_name in query_files.rs: reject
/// empty names, names starting with a dot and separator characters, and require the
/// extension to be a known query file extension).
fn validate_sql_file_name(name: &str) -> Result<(), RouteError> {
    // Reject names with leading or trailing whitespace. normalize_file_name in
    // query_files.rs trims the name on load, so allowing whitespace would make the "validated
    // path (the path canonicalized by verify_within_dir)" differ from the "path actually
    // opened (after trim)", letting a different file that passed validation (e.g. via a
    // symlink) be opened. Rejecting here guarantees the validated target and the opened
    // target are always the same.
    let lower = name.to_ascii_lowercase();
    let invalid = name.is_empty()
        || name != name.trim()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || !ALLOWED_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(&format!(".{ext}")));
    if invalid {
        return Err(RouteError::InvalidFileName(name.to_string()));
    }
    Ok(())
}

/// Expands `~` / relative paths (lexical expansion that does not touch the filesystem).
fn expand_path(raw: &str, home: Option<&Path>, cwd: Option<&Path>) -> PathBuf {
    let raw = raw.trim();
    if let Some(home) = home {
        if raw == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path
    }
}

/// Resolves `.` / `..` in a path lexically (symlinks are not followed).
/// `..` removes the previous normal component. It cannot go above the root.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Remove the previous normal component. Directly under the root, it goes no further up.
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Decodes percent-encoding (`%XX`). An incomplete `%` is left as is.
/// Paths coming through a deep link may have spaces etc. encoded, so this is used for URI paths.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) =
                (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
            {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One hex digit to a number (None for anything else).
fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders() -> Vec<(String, String)> {
        vec![
            ("db1_mysql__root".to_string(), "prod".to_string()),
            ("reporting".to_string(), "reporting-conn".to_string()),
        ]
    }

    #[test]
    fn test_parse_uri_open_absolute() {
        // An absolute path ends up with `/` doubled after the scheme
        assert_eq!(
            parse_uri("queryfolio://open//home/u/.config/queryfolio/sqlfiles/reporting/a.sql"),
            Ok(Route::OpenFile {
                path: "/home/u/.config/queryfolio/sqlfiles/reporting/a.sql".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_uri_percent_decode() {
        assert_eq!(
            parse_uri("queryfolio://open//tmp/my%20query.sql"),
            Ok(Route::OpenFile {
                path: "/tmp/my query.sql".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_uri_errors() {
        assert_eq!(parse_uri("http://open/x"), Err(RouteError::NotQueryfolioUri));
        assert_eq!(
            parse_uri("queryfolio://delete/x"),
            Err(RouteError::UnknownAction("delete".to_string()))
        );
        assert_eq!(parse_uri("queryfolio://open"), Err(RouteError::EmptyPath));
        assert_eq!(parse_uri("queryfolio://open/"), Err(RouteError::EmptyPath));
    }

    #[test]
    fn test_route_from_cli_args() {
        assert_eq!(
            route_from_cli_args(&["open", "/tmp/a.sql"]),
            Some(Route::OpenFile {
                path: "/tmp/a.sql".to_string(),
            })
        );
        // Ignore the queryfolio:// URL argument (deep-link handles it)
        assert_eq!(
            route_from_cli_args(&["queryfolio://open//tmp/a.sql"]),
            None
        );
        // Even if a URL comes after open, it is not accepted as the path
        assert_eq!(
            route_from_cli_args(&["open", "queryfolio://open//tmp/a.sql"]),
            None
        );
        // None if there is no path after open
        assert_eq!(route_from_cli_args(&["open"]), None);
        // None if there are only unrelated arguments
        assert_eq!(route_from_cli_args(&["--flag", "value"]), None);
        // Can be picked up even if argv[0] (the program path) is mixed in
        assert_eq!(
            route_from_cli_args(&["/Applications/Queryfolio.app/queryfolio", "open", "/tmp/a.sql"]),
            Some(Route::OpenFile {
                path: "/tmp/a.sql".to_string(),
            })
        );
    }

    #[test]
    fn test_route_from_cli_args_write() {
        // With content
        assert_eq!(
            route_from_cli_args(&["write", "prod", "report.sql", "select 1"]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "report.sql".to_string(),
                content: Some("select 1".to_string()),
            })
        );
        // Content omitted (stdin or "just open")
        assert_eq!(
            route_from_cli_args(&["write", "prod", "report"]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "report".to_string(),
                content: None,
            })
        );
        // Keep empty-string content as the intent of "write it empty" (do not collapse it to None)
        assert_eq!(
            route_from_cli_args(&["write", "prod", "report.sql", ""]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "report.sql".to_string(),
                content: Some(String::new()),
            })
        );
        // Content is treated as content even if it is a queryfolio:// URL
        // (removing URL arguments must not drop the content)
        assert_eq!(
            route_from_cli_args(&["write", "prod", "a.sql", "queryfolio://open/x"]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "a.sql".to_string(),
                content: Some("queryfolio://open/x".to_string()),
            })
        );
        // None if arguments are missing / whitespace only
        assert_eq!(route_from_cli_args(&["write", "prod"]), None);
        assert_eq!(route_from_cli_args(&["write"]), None);
        assert_eq!(route_from_cli_args(&["write", " ", "a.sql"]), None);
        assert_eq!(route_from_cli_args(&["write", "prod", "  "]), None);
    }

    #[test]
    fn test_route_from_cli_args_first_subcommand_wins() {
        // Use the subcommand that appears first. Even if the word "open" appears in the
        // content of write, it is not misinterpreted as the open subcommand.
        assert_eq!(
            route_from_cli_args(&["write", "prod", "a.sql", "open /etc/passwd"]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "a.sql".to_string(),
                content: Some("open /etc/passwd".to_string()),
            })
        );
        assert_eq!(
            route_from_cli_args(&["open", "/tmp/a.sql", "write"]),
            Some(Route::OpenFile {
                path: "/tmp/a.sql".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_uri_rejects_write() {
        // Write actions are not accepted from a URI (a web page can
        // open a queryfolio:// URL)
        assert_eq!(
            parse_uri("queryfolio://write/prod/a.sql"),
            Err(RouteError::UnknownAction("write".to_string()))
        );
    }

    #[test]
    fn test_resolve_open_target_ok() {
        let base = Path::new("/home/u/.config/queryfolio/sqlfiles");
        let target = resolve_open_target(
            base,
            &folders(),
            "/home/u/.config/queryfolio/sqlfiles/reporting/monthly.sql",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            target,
            OpenTarget {
                connection: "reporting-conn".to_string(),
                file_name: "monthly.sql".to_string(),
            }
        );
    }

    #[test]
    fn test_resolve_open_target_tilde_and_relative() {
        let home = Path::new("/home/u");
        let base = Path::new("~/.config/queryfolio/sqlfiles"); // base is expanded too
        // Expand and compare even if base contains ~
        let target = resolve_open_target(
            &expand_path("~/.config/queryfolio/sqlfiles", Some(home), None),
            &folders(),
            "~/.config/queryfolio/sqlfiles/reporting/a.sql",
            Some(home),
            None,
        )
        .unwrap();
        assert_eq!(target.connection, "reporting-conn");
        let _ = base;
    }

    #[test]
    fn test_resolve_open_target_relative_raw_path_with_cwd() {
        // base is absolute (a contract that the caller makes it absolute). Relative input paths are resolved against cwd.
        let cwd = Path::new("/work");
        let base = Path::new("/work/queries");
        let target = resolve_open_target(
            base,
            &folders(),
            "queries/reporting/a.sql",
            None,
            Some(cwd),
        )
        .unwrap();
        assert_eq!(target.connection, "reporting-conn");
        assert_eq!(target.file_name, "a.sql");
    }

    #[test]
    fn test_resolve_open_target_traversal_rejected() {
        let base = Path::new("/data/sqlfiles");
        // Reject paths that try to go outside the storage area with ..
        let err = resolve_open_target(
            base,
            &folders(),
            "/data/sqlfiles/reporting/../../../etc/passwd",
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, RouteError::OutsideSqlfilesDir);
    }

    #[test]
    fn test_resolve_open_target_outside() {
        let base = Path::new("/data/sqlfiles");
        assert_eq!(
            resolve_open_target(base, &folders(), "/etc/passwd", None, None)
                .unwrap_err(),
            RouteError::OutsideSqlfilesDir
        );
    }

    #[test]
    fn test_resolve_open_target_unknown_folder() {
        let base = Path::new("/data/sqlfiles");
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/unknown/a.sql",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::UnknownFolder("unknown".to_string())
        );
    }

    #[test]
    fn test_resolve_open_target_ambiguous_folder() {
        let base = Path::new("/data/sqlfiles");
        // When two connections map to the same folder name, return an error as ambiguous
        let dup = vec![
            ("shared".to_string(), "conn-a".to_string()),
            ("shared".to_string(), "conn-b".to_string()),
        ];
        assert_eq!(
            resolve_open_target(base, &dup, "/data/sqlfiles/shared/a.sql", None, None)
                .unwrap_err(),
            RouteError::AmbiguousFolder("shared".to_string())
        );
    }

    #[test]
    fn test_resolve_open_target_too_deep() {
        let base = Path::new("/data/sqlfiles");
        // A subdirectory under the connection folder = not 2 levels
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/reporting/sub/a.sql",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::NotUnderConnectionFolder
        );
        // Also reject a file directly under the storage directory (no folder)
        assert_eq!(
            resolve_open_target(base, &folders(), "/data/sqlfiles/a.sql", None, None)
                .unwrap_err(),
            RouteError::NotUnderConnectionFolder
        );
    }

    #[test]
    fn test_resolve_open_target_not_sql() {
        let base = Path::new("/data/sqlfiles");
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/reporting/notes.txt",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::InvalidFileName("notes.txt".to_string())
        );
        // Also reject hidden files starting with a dot
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/reporting/.secret.sql",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::InvalidFileName(".secret.sql".to_string())
        );
        // Reject names with leading/trailing whitespace (prevents them from turning into a different file via trim).
        // expand_path trims the whole path, so trailing whitespace is dropped and leading whitespace remains.
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/reporting/ report.sql",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::InvalidFileName(" report.sql".to_string())
        );
    }

    #[test]
    fn test_percent_decode_incomplete() {
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("%2F"), "/");
    }
}
