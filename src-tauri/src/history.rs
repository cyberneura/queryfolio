//! Recording and searching the query execution history.
//!
//! Entries are appended per connection in JSONL format
//! (~/.config/queryfolio/history/<connection>.jsonl); once the line limit is exceeded,
//! the oldest lines are dropped (rotation). SQL text may contain secrets such as
//! passwords, so the history directory is created with mode 700 and history files
//! with mode 600.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::AppError;
use crate::query_files::validate_component;

/// Maximum number of history lines per connection.
pub const MAX_HISTORY_LINES: usize = 10_000;

/// Number of lines to keep after rotation. Keeping fewer than the limit prevents a full
/// rewrite on every append once the limit is reached (until the next rotation, only
/// appends are needed for MAX_HISTORY_LINES - ROTATED_KEEP_LINES entries).
const ROTATED_KEEP_LINES: usize = 9_000;

/// Number of entries returned by list_query_history when no limit is given.
pub const DEFAULT_LIST_LIMIT: usize = 200;

/// A single history record. Corresponds to one line of the JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Execution time (ISO 8601 / RFC 3339)
    pub time: String,
    pub sql: String,
    /// Active schema (database) at execution time
    pub schema: Option<String>,
    /// Number of rows fetched or affected (None on failure)
    pub row_count: Option<u64>,
    /// Elapsed time (milliseconds)
    pub elapsed_ms: u64,
    pub success: bool,
    /// Set when a safety guard stopped the statement before it ran: `"readonly"` or
    /// `"dangerous"` (see [`blocked_kind`]). Lets `queryfolio --stats` tell guard stops apart
    /// from other failures such as syntax errors. Lines written before this field existed
    /// have no key and read as `None`, and `None` is not written, so ordinary lines keep
    /// their old shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// Value of [`HistoryEntry::blocked`] for a guard stop.
pub const BLOCKED_READONLY: &str = "readonly";
pub const BLOCKED_DANGEROUS: &str = "dangerous";

/// Which guard, if any, produced this error: the read-only guard ([`AppError::Readonly`])
/// or the dangerous-statement guard ([`AppError::Dangerous`]). Other errors return `None`.
pub fn blocked_kind(error: &AppError) -> Option<&'static str> {
    match error {
        AppError::Readonly(_) => Some(BLOCKED_READONLY),
        AppError::Dangerous(_) => Some(BLOCKED_DANGEROUS),
        _ => None,
    }
}

/// Manager that caches the history file line count per connection.
/// Avoids re-reading every line on each append by counting the actual file only on
/// first access. Protected by a Mutex to serialize appends within the process (this is
/// a single-user desktop app, so contention only occurs inside the process).
#[derive(Default)]
pub struct HistoryManager {
    counts: Mutex<HashMap<String, usize>>,
}

/// Default history directory (~/.config/queryfolio/history).
pub fn default_history_dir() -> Result<PathBuf, AppError> {
    Ok(config::app_config_dir()?.join("history"))
}

/// Returns the history file path for a connection name.
/// The connection name becomes a path component, so it is checked with validate_component.
fn history_file(history_dir: &Path, connection: &str) -> Result<PathBuf, AppError> {
    let connection = validate_component(connection)
        .map_err(|e| AppError::History(format!("Invalid connection name: {e}")))?;
    Ok(history_dir.join(format!("{connection}.jsonl")))
}

/// Creates the history directory and sets its permissions to 700.
fn ensure_history_dir(history_dir: &Path) -> Result<(), AppError> {
    fs::create_dir_all(history_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(history_dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Counts the lines of a file (called only on first access).
fn count_lines(path: &Path) -> Result<usize, AppError> {
    if !path.exists() {
        return Ok(0);
    }
    let reader = BufReader::new(fs::File::open(path)?);
    Ok(reader.lines().count())
}

/// Common options for opening a file for writing with permissions 600.
fn open_options_600(append: bool) -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

impl HistoryManager {
    /// Appends one history entry. When the limit is exceeded, drops old lines (rotation).
    pub fn append(
        &self,
        history_dir: &Path,
        connection: &str,
        entry: &HistoryEntry,
    ) -> Result<(), AppError> {
        self.append_with_limits(
            history_dir,
            connection,
            entry,
            MAX_HISTORY_LINES,
            ROTATED_KEEP_LINES,
        )
    }

    /// The actual implementation with a parameterized limit (split out so tests can use a small limit).
    fn append_with_limits(
        &self,
        history_dir: &Path,
        connection: &str,
        entry: &HistoryEntry,
        max_lines: usize,
        keep_lines: usize,
    ) -> Result<(), AppError> {
        let path = history_file(history_dir, connection)?;
        let line = serde_json::to_string(entry)
            .map_err(|e| AppError::History(format!("Failed to serialize an entry: {e}")))?;

        // Serialize the whole append operation with the lock to keep the counter and the real file consistent
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut count = match counts.get(connection) {
            Some(count) => *count,
            None => {
                // First access: count the existing lines and fix the permissions to 600,
                // even for files created by older versions or by hand
                let existing = count_lines(&path)?;
                #[cfg(unix)]
                if path.exists() {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                }
                existing
            }
        };

        ensure_history_dir(history_dir)?;
        let mut file = open_options_600(true).open(&path)?;
        writeln!(file, "{line}")?;
        count += 1;

        if count > max_lines {
            count = rotate_file(&path, keep_lines)?;
        }
        counts.insert(connection.to_string(), count);
        Ok(())
    }
}

/// Rewrites the history file keeping only its last keep_lines lines.
/// Writes to a temporary file and then renames it, so the original file is not
/// corrupted even if something fails midway. Returns the number of lines kept.
fn rotate_file(path: &Path, keep_lines: usize) -> Result<usize, AppError> {
    let reader = BufReader::new(fs::File::open(path)?);
    let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;
    let start = lines.len().saturating_sub(keep_lines);
    let kept = &lines[start..];

    let temp_path = path.with_extension("jsonl.tmp");
    {
        let mut file = open_options_600(false).open(&temp_path)?;
        for line in kept {
            writeln!(file, "{line}")?;
        }
        file.sync_all()?;
    }
    fs::rename(&temp_path, path)?;
    Ok(kept.len())
}

/// Reads the history of every connection, for `queryfolio --stats`.
///
/// Returns `(connection name, entry)` pairs in no particular order. The connection name is
/// the file stem (history files are named after the connection, see [`history_file`]).
/// Only `*.jsonl` files are read, so a rotation's leftover `*.jsonl.tmp` is not counted twice.
/// Broken lines are skipped as in [`list_history`]. A missing directory means no history yet.
pub fn read_all_history(history_dir: &Path) -> Result<Vec<(String, HistoryEntry)>, AppError> {
    if !history_dir.exists() {
        return Ok(vec![]);
    }
    let mut entries = vec![];
    for dir_entry in fs::read_dir(history_dir)? {
        let path = dir_entry?.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(connection) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let connection = connection.to_string();
        let reader = BufReader::new(fs::File::open(&path)?);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line) {
                entries.push((connection.clone(), entry));
            }
        }
    }
    Ok(entries)
}

/// Returns the history newest first. If search is given, filters by substring match
/// on the SQL (case-insensitive).
pub fn list_history(
    history_dir: &Path,
    connection: &str,
    search: Option<&str>,
    limit: usize,
) -> Result<Vec<HistoryEntry>, AppError> {
    let path = history_file(history_dir, connection)?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let needle = search
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let reader = BufReader::new(fs::File::open(&path)?);
    let mut entries: Vec<HistoryEntry> = vec![];
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Skip broken lines (e.g. partially written during a crash) and keep reading
        let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line) else {
            continue;
        };
        if let Some(needle) = &needle {
            if !entry.sql.to_lowercase().contains(needle) {
                continue;
            }
        }
        entries.push(entry);
    }
    // The file is in append order = oldest first, so reverse it to get newest first
    entries.reverse();
    entries.truncate(limit);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sql: &str, success: bool) -> HistoryEntry {
        HistoryEntry {
            time: "2026-07-11T12:00:00+09:00".into(),
            sql: sql.into(),
            schema: Some("main".into()),
            row_count: if success { Some(3) } else { None },
            elapsed_ms: 12,
            success,
            blocked: None,
        }
    }

    #[test]
    fn test_blocked_is_written_only_when_set() {
        let plain = serde_json::to_string(&entry("SELECT 1", true)).unwrap();
        assert!(!plain.contains("blocked"), "{plain}");

        let mut stopped = entry("DELETE FROM t", false);
        stopped.blocked = blocked_kind(&AppError::Dangerous("no".into())).map(str::to_string);
        let line = serde_json::to_string(&stopped).unwrap();
        assert!(line.contains(r#""blocked":"dangerous""#), "{line}");
    }

    #[test]
    fn test_old_lines_without_blocked_are_read() {
        // A line written before the blocked field existed
        let line = r#"{"time":"2026-07-11T12:00:00+09:00","sql":"SELECT 1","schema":null,"row_count":1,"elapsed_ms":3,"success":true}"#;
        let entry: HistoryEntry = serde_json::from_str(line).unwrap();
        assert_eq!(entry.blocked, None);
        assert!(entry.success);
    }

    #[test]
    fn test_blocked_kind() {
        assert_eq!(
            blocked_kind(&AppError::Readonly("x".into())),
            Some("readonly")
        );
        assert_eq!(
            blocked_kind(&AppError::Dangerous("x".into())),
            Some("dangerous")
        );
        assert_eq!(blocked_kind(&AppError::Config("syntax error".into())), None);
    }

    #[test]
    fn test_read_all_history() {
        let dir = tempfile::tempdir().unwrap();
        let manager = HistoryManager::default();
        manager
            .append(dir.path(), "a", &entry("SELECT 1", true))
            .unwrap();
        manager
            .append(dir.path(), "a", &entry("SELECT 2", true))
            .unwrap();
        manager
            .append(dir.path(), "b", &entry("SELECT 3", false))
            .unwrap();
        // A rotation leftover and an unrelated file are not read
        fs::write(dir.path().join("a.jsonl.tmp"), "{}\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "x\n").unwrap();

        let mut entries = read_all_history(dir.path()).unwrap();
        entries.sort_by(|x, y| x.1.sql.cmp(&y.1.sql));
        let pairs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(c, e)| (c.as_str(), e.sql.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![("a", "SELECT 1"), ("a", "SELECT 2"), ("b", "SELECT 3")]
        );

        assert!(read_all_history(&dir.path().join("missing"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_append_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let manager = HistoryManager::default();

        manager.append(dir.path(), "conn", &entry("SELECT 1", true)).unwrap();
        manager.append(dir.path(), "conn", &entry("SELECT 2", false)).unwrap();
        manager.append(dir.path(), "conn", &entry("SELECT 3", true)).unwrap();

        // Returned newest first
        let entries = list_history(dir.path(), "conn", None, 100).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sql, "SELECT 3");
        assert_eq!(entries[2].sql, "SELECT 1");
        assert!(entries[0].success);
        assert!(!entries[1].success);
        assert_eq!(entries[1].row_count, None);
        assert_eq!(entries[0].row_count, Some(3));

        // limit truncates the list, keeping the entries from the front (the newest ones)
        let entries = list_history(dir.path(), "conn", None, 2).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sql, "SELECT 3");
        assert_eq!(entries[1].sql, "SELECT 2");

        // Files are separated per connection
        assert_eq!(list_history(dir.path(), "other", None, 100).unwrap().len(), 0);
    }

    #[test]
    fn test_list_search() {
        let dir = tempfile::tempdir().unwrap();
        let manager = HistoryManager::default();

        manager
            .append(dir.path(), "conn", &entry("SELECT * FROM users", true))
            .unwrap();
        manager
            .append(dir.path(), "conn", &entry("SELECT * FROM orders", true))
            .unwrap();
        manager
            .append(dir.path(), "conn", &entry("UPDATE users SET a = 1", true))
            .unwrap();

        // Substring match (case-insensitive)
        let entries = list_history(dir.path(), "conn", Some("USERS"), 100).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sql, "UPDATE users SET a = 1");
        assert_eq!(entries[1].sql, "SELECT * FROM users");

        let entries = list_history(dir.path(), "conn", Some("orders"), 100).unwrap();
        assert_eq!(entries.len(), 1);

        // A whitespace-only search term matches everything
        let entries = list_history(dir.path(), "conn", Some("  "), 100).unwrap();
        assert_eq!(entries.len(), 3);

        let entries = list_history(dir.path(), "conn", Some("no-match"), 100).unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let manager = HistoryManager::default();

        // Append 7 times with a limit of 5 and 3 lines kept after rotation
        for i in 1..=7 {
            manager
                .append_with_limits(
                    dir.path(),
                    "conn",
                    &entry(&format!("SELECT {i}"), true),
                    5,
                    3,
                )
                .unwrap();
        }
        // The 6th append exceeds the limit -> shrinks to the last 3 lines (4,5,6); the 7th makes it 4 lines
        let entries = list_history(dir.path(), "conn", None, 100).unwrap();
        let sqls: Vec<&str> = entries.iter().map(|e| e.sql.as_str()).collect();
        assert_eq!(sqls, vec!["SELECT 7", "SELECT 6", "SELECT 5", "SELECT 4"]);

        // No temporary file is left behind
        assert!(!dir.path().join("conn.jsonl.tmp").exists());
    }

    #[test]
    fn test_count_recovery_across_instances() {
        let dir = tempfile::tempdir().unwrap();

        // Even a different instance (equivalent to a restart) recounts the existing lines and rotates
        let manager1 = HistoryManager::default();
        for i in 1..=4 {
            manager1
                .append_with_limits(dir.path(), "conn", &entry(&format!("A{i}"), true), 5, 3)
                .unwrap();
        }
        let manager2 = HistoryManager::default();
        for i in 1..=2 {
            manager2
                .append_with_limits(dir.path(), "conn", &entry(&format!("B{i}"), true), 5, 3)
                .unwrap();
        }
        // 4 + 2 = 6 lines -> exceeds 5, so it is rotated down to 3 lines
        let entries = list_history(dir.path(), "conn", None, 100).unwrap();
        let sqls: Vec<&str> = entries.iter().map(|e| e.sql.as_str()).collect();
        assert_eq!(sqls, vec!["B2", "B1", "A4"]);
    }

    #[test]
    fn test_broken_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let manager = HistoryManager::default();
        manager.append(dir.path(), "conn", &entry("SELECT 1", true)).unwrap();

        // Reading continues even if broken lines (e.g. from a crash) are mixed in
        let path = dir.path().join("conn.jsonl");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{broken json").unwrap();
        drop(file);
        manager.append(dir.path(), "conn", &entry("SELECT 2", true)).unwrap();

        let entries = list_history(dir.path(), "conn", None, 100).unwrap();
        let sqls: Vec<&str> = entries.iter().map(|e| e.sql.as_str()).collect();
        assert_eq!(sqls, vec!["SELECT 2", "SELECT 1"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let history_dir = dir.path().join("history");
        let manager = HistoryManager::default();
        manager.append(&history_dir, "conn", &entry("SELECT 1", true)).unwrap();

        let dir_mode = fs::metadata(&history_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let file_mode = fs::metadata(history_dir.join("conn.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);

        // Permissions are preserved after rotation
        for i in 0..10 {
            manager
                .append_with_limits(&history_dir, "conn", &entry(&format!("S{i}"), true), 5, 3)
                .unwrap();
        }
        let file_mode = fs::metadata(history_dir.join("conn.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn test_invalid_connection_name() {
        let dir = tempfile::tempdir().unwrap();
        let manager = HistoryManager::default();
        // Reject connection names that would cause path traversal
        assert!(manager
            .append(dir.path(), "../evil", &entry("SELECT 1", true))
            .is_err());
        assert!(list_history(dir.path(), "a/b", None, 10).is_err());
        assert!(list_history(dir.path(), ".hidden", None, 10).is_err());
    }
}
