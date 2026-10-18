use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::AppError;

/// Validate that a name is safe as a path component and return it.
/// Guards against path traversal and hidden files.
/// (Also used by history.rs to validate connection names.)
pub(crate) fn validate_component(name: &str) -> Result<&str, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::QueryFile("The name is empty".into()));
    }
    if name.starts_with('.') {
        return Err(AppError::QueryFile(format!(
            "Names starting with a dot are not allowed: {name}"
        )));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(AppError::QueryFile(format!(
            "The name contains invalid characters: {name}"
        )));
    }
    Ok(name)
}

/// Normalize a query file name (ensures the connection engine's extension).
/// `ext` is an extension without the dot, such as "sql" / "redis" (the
/// file_extension of engines::EngineCapabilities).
pub(crate) fn normalize_file_name(name: &str, ext: &str) -> Result<String, AppError> {
    let name = validate_component(name)?;
    let suffix = format!(".{}", ext.to_ascii_lowercase());
    if name.to_ascii_lowercase().ends_with(&suffix) {
        Ok(name.to_string())
    } else {
        Ok(format!("{name}{suffix}"))
    }
}

/// Return the directory where query files are stored for a connection name.
pub(crate) fn connection_dir(
    sqlfiles_dir: &Path,
    connection: &str,
) -> Result<PathBuf, AppError> {
    let connection = validate_component(connection)?;
    Ok(sqlfiles_dir.join(connection))
}

fn file_path(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<PathBuf, AppError> {
    let file_name = normalize_file_name(file_name, ext)?;
    Ok(connection_dir(sqlfiles_dir, connection)?.join(file_name))
}

/// Comparison that treats runs of digits as numbers (natural order).
///
/// Plain lexicographic order makes numbered suffixes of different digit counts disagree with
/// creation order: the sequence numbers appended when several files are created within the same
/// minute are not zero-padded (FilesPane's defaultFileName), so for "-9" vs "-10" the '1' < '9'
/// rule puts "-9" first in descending order, i.e. the older file ends up on top.
/// Comparing each run of digits as a number avoids this.
///
/// Digit strings with the same value ("02" and "2") are ordered with the one having fewer digits
/// first, to keep the comparison deterministic.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    /// Consume and return the leading run of ASCII digits.
    fn take_digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
        let mut out = String::new();
        while let Some(c) = it.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            out.push(*c);
            it.next();
        }
        out
    }

    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            // The shorter one that is a prefix is considered smaller (same as lexicographic order)
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let da = take_digits(&mut ai);
                    let db = take_digits(&mut bi);
                    // Once leading zeros are stripped, "digit count, then lexicographic" gives numeric order
                    // (parsing into u64 would overflow on extremely long digit strings)
                    let ta = da.trim_start_matches('0');
                    let tb = db.trim_start_matches('0');
                    let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    let ord = da.len().cmp(&db.len());
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = x.cmp(&y);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
}

/// Return the file names directly under the directory that have extension `ext`, in **descending**
/// order. Empty if the directory does not exist.
/// (list_query_files and search_query_files share the enumeration criteria so that hidden-file /
/// extension checks and sorting cannot drift apart in only one of them.)
///
/// Hidden dot-files are excluded. This is consistent with validate_component rejecting names that
/// start with a dot (= they cannot be opened via CRUD), and keeps the contents of manually placed
/// hidden files from leaking into search previews.
///
/// Descending order is used so that "new files appear at the top of the list". The default file
/// name has the form `YYYYMMDD-HHMM`, designed so that name order is chronological order
/// (FilesPane's defaultFileName). Hence descending name order = newest first.
/// Names rather than modification time are used so the list does not reshuffle on every save and
/// jump around while the user is working.
fn list_query_file_names(dir: &Path, ext: &str) -> Result<Vec<String>, AppError> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let suffix = format!(".{}", ext.to_ascii_lowercase());
    let mut names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .filter(|name| name.to_ascii_lowercase().ends_with(&suffix))
        .collect();
    // Descending (newest first). The comparison is done on the **part without the extension** in natural order.
    //
    // Comparing with the extension attached would reverse the order of numbered files created in
    // the same minute ("20260804-1200.sql" and "20260804-1200-2.sql"):
    // '.' (0x2E) > '-' (0x2D), so in descending order the older un-numbered one would come first.
    // Dropping the extension gives "20260804-1200-2" > "20260804-1200" (the shorter prefix-match is
    // smaller), so the newest comes first in creation order.
    //
    // The filter above guarantees every element ends with the suffix, and the suffix is ASCII, so
    // the position that drops the trailing suffix.len() bytes is always a char boundary.
    let ext_len = suffix.len();
    names.sort_unstable_by(|a, b| {
        let a_stem = &a[..a.len() - ext_len];
        let b_stem = &b[..b.len() - ext_len];
        // Equal stems only happen when the extensions differ just in case.
        // In that case, decide by the whole name to keep the order deterministic.
        natural_cmp(b_stem, a_stem).then_with(|| b.cmp(a))
    });
    Ok(names)
}

/// Return the query file list of a connection (descending by name = newest first).
/// The app's list moved to list_query_file_entries (by modification time); this is kept only so
/// tests can verify the name-based order.
#[cfg(test)]
pub fn list_query_files(
    sqlfiles_dir: &Path,
    connection: &str,
    ext: &str,
) -> Result<Vec<String>, AppError> {
    list_query_file_names(&connection_dir(sqlfiles_dir, connection)?, ext)
}

/// One row of the FILES pane (file name + modification time + size).
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct QueryFileEntry {
    /// File name (with extension)
    pub file_name: String,
    /// Last modification time (milliseconds since the UNIX epoch). None if the OS cannot provide it
    pub modified_ms: Option<i64>,
    /// File size (bytes)
    pub size: u64,
}

/// List for the FILES pane. Returned in **descending modification time** (most recently edited
/// first) (CYBERNEURA-DEV-774).
///
/// The enumeration criteria (hidden files, extension) are shared with list_query_file_names, and
/// its name order is used as the initial order for a stable sort. Entries with equal modification
/// time (and those whose time is unavailable) stay in name order within their group, and entries
/// without a time go to the end.
///
/// Search (search_query_files) and list_query_files stay in name order. Only the FILES pane,
/// which shows the timestamps, is allowed to reorder on every save.
pub fn list_query_file_entries(
    sqlfiles_dir: &Path,
    connection: &str,
    ext: &str,
) -> Result<Vec<QueryFileEntry>, AppError> {
    let dir = connection_dir(sqlfiles_dir, connection)?;
    let mut entries: Vec<QueryFileEntry> = list_query_file_names(&dir, ext)?
        .into_iter()
        // Files that vanished between enumeration and stat are left out of the list (they cannot be opened)
        .filter_map(|file_name| {
            let meta = fs::metadata(dir.join(&file_name)).ok()?;
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_millis()).ok());
            Some(QueryFileEntry {
                file_name,
                modified_ms,
                size: meta.len(),
            })
        })
        .collect();
    // Option orders None < Some, so descending puts entries without a time at the end.
    // sort_by_key is a stable sort, so name order is preserved within the same time
    entries.sort_by_key(|e| std::cmp::Reverse(e.modified_ms));
    Ok(entries)
}

/// One hit of a query file search.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct FileSearchHit {
    /// Matched file name (with extension)
    pub file_name: String,
    /// Whether the file name matched the query
    pub name_match: bool,
    /// First line whose content matched (for preview; None if only the name matched)
    pub content_preview: Option<String>,
}

/// Maximum number of characters in the preview line (anything longer is cut and ends with an ellipsis).
const PREVIEW_MAX_CHARS: usize = 120;

/// Maximum number of search results. The search stops after this many from the top in descending
/// name order (newest first) (keeps the modal list short and bounds read cost with many files).
const MAX_SEARCH_HITS: usize = 50;

/// Format a preview line by trimming surrounding whitespace and limiting its length.
fn truncate_preview(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= PREVIEW_MAX_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(PREVIEW_MAX_CHARS).collect();
    format!("{cut}…")
}

/// Search a connection's query files by file name and content.
/// Case-insensitive substring match. For content, the first matching line is returned as the
/// preview. Only files whose name or content matched are returned, in descending name order
/// (newest first).
/// (No external process such as rg/grep is used. Query files are few, so reading them in pure
/// Rust is more robust and has no external dependency or injection surface.)
pub fn search_query_files(
    sqlfiles_dir: &Path,
    connection: &str,
    query: &str,
    ext: &str,
) -> Result<Vec<FileSearchHit>, AppError> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(vec![]);
    }
    let dir = connection_dir(sqlfiles_dir, connection)?;
    let names = list_query_file_names(&dir, ext)?;

    let mut hits = Vec::new();
    for name in names {
        let name_match = name.to_lowercase().contains(&needle);
        // Content search. Unreadable files (binary etc.) are skipped and picked up by name match only
        let content_preview = fs::read_to_string(dir.join(&name))
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find(|line| line.to_lowercase().contains(&needle))
                    .map(truncate_preview)
            });
        if name_match || content_preview.is_some() {
            hits.push(FileSearchHit {
                file_name: name,
                name_match,
                content_preview,
            });
            // Descending name order (newest first), up to the limit from the top. Later files are not read; stop here
            if hits.len() >= MAX_SEARCH_HITS {
                break;
            }
        }
    }
    Ok(hits)
}

/// Return the absolute path of a query file as a string (for "Copy full path").
/// The name is validated and normalized before building the path, as a path-traversal defense.
/// It is only called for files shown in the list, so no existence check is done.
/// If sqlfiles_dir is configured as a relative path, the built path is relative too.
/// Since "Copy full path" must always return an absolute path, a relative one is made absolute
/// against the current directory (std::path::absolute is a lexical absolutization that neither
/// checks existence nor resolves symlinks).
pub fn query_file_path(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    let path = std::path::absolute(&path)?;
    Ok(path.to_string_lossy().into_owned())
}

pub fn read_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    if !path.exists() {
        return Err(AppError::QueryFile(format!(
            "File not found: {}",
            path.display()
        )));
    }
    Ok(fs::read_to_string(&path)?)
}

/// Create the storage directories. **On Unix only the levels created here** get mode 0700.
///
/// Query files contain actual WHERE values, table names and DDL, and the execution history
/// (history.rs), which stores the same content, is explicitly restricted to 0700 / 0600. Without
/// specifying the mode at creation, the umask default (usually 0755 / 0644) applies and other
/// users on the same host could read them (CYBERNEURA-DEV-510). Windows has no mode concept, and
/// ACLs are left to inheritance from the parent directory as before.
///
/// `create_dir_all` + `set_permissions` is not used, for two reasons:
/// - `create_dir_all` also creates intermediate levels (the storage root etc.), but the mode can
///   only be set on the final directory, leaving the levels in between with the umask default
/// - If we check "does it exist" first and then create, we would also tighten a directory that
///   another process created in between (or loosen it, if it had been created more strictly)
///
/// Instead `create_dir` is used per level. "Create if missing" completes in one system call, and
/// `AlreadyExists` is returned if it already exists, so the mode can be set **only when we created
/// it**. The mode of an existing directory is not changed (a setting the user intentionally
/// loosened is not tightened later; the same idea as write_file_atomic, which inherits existing
/// permissions on write).
fn ensure_dir_700(dir: &Path) -> Result<(), AppError> {
    // Collect the levels that need creating, from the deepest one upward.
    let mut missing = Vec::new();
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.as_os_str().is_empty() || path.is_dir() {
            break;
        }
        missing.push(path);
        current = path.parent();
    }

    // Create from the shallowest level. **Specify 0700 at creation time** (`DirBuilder::mode`).
    // If we chmod after creating, a directory left behind by a crash in between would remain with the
    // umask default and be treated as "existing" from then on, never tightened.
    // The umask can only drop bits from the specified mode, so it never opens the directory to other
    // users at creation. The owner bits it dropped are restored right afterward.
    for path in missing.iter().rev() {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
                }
            }
            // Another process won the race and created it first. Respect its mode.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Common options for creating a new file (`O_EXCL`). **On Unix the mode is 600.**
/// `create_new` never clobbers an existing file. Same intent as `open_options_600` in history.rs:
/// only the mode of newly created files is tightened. On Windows the mode has no effect and
/// ACLs stay as before.
fn create_new_options_600() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Create a new file with `create_new_options_600`.
/// `mode` has bits dropped by the umask, so under a umask that drops even the owner bits (0700
/// etc.) a 000 file that cannot be read or written would be left. Set 0600 right after creation
/// to restore them (same form as config.rs when it creates config.yml).
fn create_new_file_600(path: &Path) -> std::io::Result<fs::File> {
    let file = create_new_options_600().open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // If the mode cannot be applied, fail without leaving the created file behind.
        // Failing creation itself is better than leaving a half-permissive file
        // (fchmod is refused only on some network filesystems etc.).
        if let Err(e) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(e);
        }
    }
    Ok(file)
}

/// Write to a temporary file in the same directory, then replace with rename.
///
/// `fs::write` truncates the destination and then appends, so the intermediate state is visible
/// on disk. The CLI (`queryfolio write`) is a **separate process from the app**, so concurrent
/// writes to the same file, or reads by the running instance's external-change watcher, can
/// overlap a write. In that window half-written content may be read, or a broken query mixing the
/// contents of two writers may be left behind.
/// rename is atomic within the same filesystem, so readers see either "before the write" or
/// "after the write" (and among competing writers, the last one wins as a whole).
///
/// The temporary file is **always placed in the same directory** (rename fails with EXDEV across
/// filesystems). Its name starts with a dot and has a `.tmp` extension, so it does not show up in
/// the list or search (list_query_file_names).
///
/// Windows `fs::rename` also replaces an existing file, equivalent to `MOVEFILE_REPLACE_EXISTING`
/// (the same premise as `move_query_file` crushing a reserved empty file with rename).
fn write_file_atomic(path: &Path, content: &str) -> Result<(), AppError> {
    /// Upper limit on retries when the temporary file name is already taken.
    /// The name is pid + sequence number, so the first attempt normally succeeds (it advances only
    /// when there are leftovers or links).
    const MAX_TMP_FILE_ATTEMPTS: usize = 8;

    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    /// Keeps temporary file names from colliding when the same file is written concurrently within
    /// one process (processes are separated by pid).
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .ok_or_else(|| AppError::QueryFile(format!("Invalid file path: {}", path.display())))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AppError::QueryFile(format!("Invalid file name: {}", path.display())))?;
    // The temporary file is created with `create_new` (`O_EXCL`). `File::create` follows symlinks
    // and truncates, so a same-name file left by a crash or a link placed beforehand could corrupt
    // files outside the storage area. If the name is in use, advance the sequence number and retry
    // (collisions with concurrent writes in the same process or with leftovers).
    let mut tmp_path = PathBuf::new();
    let mut file = None;
    for _ in 0..MAX_TMP_FILE_ATTEMPTS {
        let candidate = parent.join(format!(
            ".{file_name}.{}-{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        match create_new_file_600(&candidate) {
            Ok(f) => {
                tmp_path = candidate;
                file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let Some(mut file) = file else {
        return Err(AppError::QueryFile(format!(
            "Could not create a temporary file to write: {}",
            path.display()
        )));
    };

    let written = (|| -> std::io::Result<()> {
        // Inherit the permissions of the existing file (rename swaps the attributes along with the
        // contents, so without this a query file the user tightened to 600 would revert to the umask default)
        #[cfg(unix)]
        if let Ok(meta) = fs::metadata(path) {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(meta.permissions().mode()));
        }
        file.write_all(content.as_bytes())?;
        // Flush the contents to disk before the rename (if the rename became durable first, a crash
        // could leave a file that "has a name but empty contents")
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

pub fn write_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    content: &str,
    ext: &str,
) -> Result<(), AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    if let Some(parent) = path.parent() {
        ensure_dir_700(parent)?;
    }
    write_file_atomic(&path, content)?;
    Ok(())
}

/// Write with optimistic locking. Writes only when the base the caller knows (expected_base)
/// matches the current on-disk content read right before writing.
/// - Returns Ok(true) if written.
/// - If the file exists but differs from expected_base (= changed outside the app), nothing is
///   written and Ok(false) is returned (so the caller can go to merge/conflict handling).
/// - If the file does not exist, it is (re)created regardless of expected_base and Ok(true) is
///   returned (so local edits are reliably kept when the file was deleted externally).
///
/// Doing the check and the write adjacently inside the same backend call removes the TOCTOU
/// window of an asynchronous round trip with the frontend (not fully OS-level atomic, but the
/// read->write gap is narrowed to adjacent system calls).
pub fn write_query_file_if_unchanged(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    content: &str,
    expected_base: &str,
    ext: &str,
) -> Result<bool, AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    match fs::read_to_string(&path) {
        Ok(current) => {
            if current != expected_base {
                // Changed outside the app. Do not overwrite.
                return Ok(false);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Deleted externally, etc. Recreate below.
        }
        Err(e) => return Err(e.into()),
    }
    if let Some(parent) = path.parent() {
        ensure_dir_700(parent)?;
    }
    write_file_atomic(&path, content)?;
    Ok(true)
}

/// Create a new empty query file and return the normalized file name.
pub fn create_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let normalized = normalize_file_name(file_name, ext)?;
    let path = file_path(sqlfiles_dir, connection, &normalized, ext)?;
    if path.exists() {
        return Err(AppError::QueryFile(format!(
            "A file with the same name already exists: {normalized}"
        )));
    }
    if let Some(parent) = path.parent() {
        ensure_dir_700(parent)?;
    }
    // Using `create_new` (`O_EXCL`) avoids clobbering a file another process created between the
    // existence check above and creation (`fs::write` truncates an existing file).
    create_new_file_600(&path)?;
    Ok(normalized)
}

/// Canonicalize `file`, resolving symlinks, and verify that it stays under `base` (also
/// canonicalized). A defense in depth that rejects links pointing to targets outside the storage
/// area. The target should be an existing file, so if it cannot be canonicalized (does not exist
/// etc.) it is rejected.
pub fn verify_within_dir(base: &Path, file: &Path) -> Result<(), AppError> {
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

/// Create the query file empty if it does not exist (contents are left as is if it does).
/// Return the normalized file name.
///
/// The difference from `create_query_file` is that it **neither overwrites nor fails on an
/// existing file**. Used when the CLI (`queryfolio write <connection> <file-name>`) omits the
/// content: the expected behavior is "create and open if missing / open as is if present", and
/// existing contents must not be wiped to empty.
///
/// Creation uses `create_new` (`O_EXCL`), and if another process creates a same-name file
/// between the existence check and creation, it is treated as existing (contents are not erased).
/// However, an error is returned if the existing one is not a regular file (see below).
pub fn ensure_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let normalized = normalize_file_name(file_name, ext)?;
    let path = file_path(sqlfiles_dir, connection, &normalized, ext)?;
    if let Some(parent) = path.parent() {
        ensure_dir_700(parent)?;
    }
    match create_new_file_600(&path) {
        Ok(_) => Ok(normalized),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // `O_EXCL` returns AlreadyExists not only when a "regular file already exists" but also when a
            // directory, broken symlink, FIFO etc. with the same name exists.
            // Returning success as is would make the CLI (`queryfolio write`) think the write succeeded and
            // continue launching, with the real failure delayed until the frontend loads it = the calling
            // agent cannot tell the failure from the exit status. Verify here that an openable query file
            // really exists (metadata follows links, so a broken link gives Err).
            let meta = fs::metadata(&path).map_err(|e| {
                AppError::QueryFile(format!(
                    "The existing entry cannot be opened as a query file: {} ({e})",
                    path.display()
                ))
            })?;
            if !meta.is_file() {
                return Err(AppError::QueryFile(format!(
                    "The path already exists and is not a regular file: {}",
                    path.display()
                )));
            }
            // `metadata` follows links, so a symlink pointing to a **regular file outside the storage area**
            // passes the is_file() above. The running instance rejects the same target via
            // verify_within_dir, so letting it through here yields "the CLI exits with status 0 but the file
            // is not opened".
            // Apply the same check as the opening side so that the meaning of success is consistent.
            verify_within_dir(sqlfiles_dir, &path)?;
            Ok(normalized)
        }
        Err(e) => Err(e.into()),
    }
}

pub fn delete_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<(), AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    if !path.exists() {
        return Err(AppError::QueryFile(format!(
            "File not found: {}",
            path.display()
        )));
    }
    fs::remove_file(&path)?;
    Ok(())
}

/// Rename a query file and return the normalized new file name.
/// If old and new are the same name (after normalization) it is a no-op and returns the new name.
pub fn rename_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    old_name: &str,
    new_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let old_normalized = normalize_file_name(old_name, ext)?;
    let new_normalized = normalize_file_name(new_name, ext)?;
    if old_normalized == new_normalized {
        return Ok(new_normalized);
    }
    let old_path = file_path(sqlfiles_dir, connection, &old_normalized, ext)?;
    if !old_path.exists() {
        return Err(AppError::QueryFile(format!(
            "File not found: {}",
            old_path.display()
        )));
    }
    // Collision detection is case-insensitive (matches the real behavior of case-insensitive
    // filesystems and agrees with the frontend's check). The rename target itself (old) is excluded,
    // so a rename that only changes case (Test.sql -> test.sql) is allowed.
    let new_lower = new_normalized.to_ascii_lowercase();
    let dir = connection_dir(sqlfiles_dir, connection)?;
    if dir.exists() {
        for entry in fs::read_dir(&dir)?.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name != old_normalized && name.to_ascii_lowercase() == new_lower {
                return Err(AppError::QueryFile(format!(
                    "A file with the same name already exists: {new_normalized}"
                )));
            }
        }
    }
    let new_path = file_path(sqlfiles_dir, connection, &new_normalized, ext)?;
    fs::rename(&old_path, &new_path)?;
    Ok(new_normalized)
}

/// Move a query file to another connection's folder and return the normalized file name.
/// If the source and destination point to the same folder, it is a no-op and returns the name
/// (different connections can still share the same folder when folder_name is the same).
///
/// The extension differs per engine, so the caller (lib.rs) confirms the source and destination
/// have the same one before calling. Here it is treated as a single ext.
pub fn move_query_file(
    sqlfiles_dir: &Path,
    from_connection: &str,
    to_connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let normalized = normalize_file_name(file_name, ext)?;
    let from_dir = connection_dir(sqlfiles_dir, from_connection)?;
    let to_dir = connection_dir(sqlfiles_dir, to_connection)?;

    // The existence check is done before the same-folder check. Doing it afterward would make
    // moving a nonexistent file return "success".
    let from_path = from_dir.join(&normalized);
    if !from_path.exists() {
        return Err(AppError::QueryFile(format!(
            "File not found: {}",
            from_path.display()
        )));
    }
    if from_dir == to_dir {
        return Ok(normalized);
    }

    ensure_dir_700(&to_dir)?;

    // Reject a same-name file that differs only in case first (matches the real behavior of
    // case-insensitive filesystems and agrees with rename_query_file's check). If enumeration fails,
    // it is not treated as "no collision" but as an error (do not move after missing one).
    let lower = normalized.to_ascii_lowercase();
    for entry in fs::read_dir(&to_dir)? {
        let Ok(name) = entry?.file_name().into_string() else {
            continue;
        };
        if name.to_ascii_lowercase() == lower {
            return Err(AppError::QueryFile(format!(
                "A file with the same name already exists at the destination: {normalized}"
            )));
        }
    }

    // **Atomically reserve the destination name, then rename.**
    // Unix rename silently replaces an existing destination, so if a same-name file is created
    // between the existence check above and the rename (a concurrent move or external creation),
    // that file would be lost. With an O_EXCL creation, "create if missing" is atomic, so a
    // successful reservation means the name is certainly ours.
    let to_path = to_dir.join(&normalized);
    match create_new_file_600(&to_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(AppError::QueryFile(format!(
                "A file with the same name already exists at the destination: {normalized}"
            )));
        }
        Err(e) => return Err(e.into()),
    }

    // Source and destination are both directly under sqlfiles_dir, so they are on the same
    // filesystem and rename works (no need for the EXDEV fallback of copy + delete).
    // The only thing replaced here is the empty file we reserved.
    if let Err(e) = fs::rename(&from_path, &to_path) {
        // Do not leave the reserved empty file behind (it would make the next move keep failing with a collision)
        let _ = fs::remove_file(&to_path);
        return Err(e.into());
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "queryfolio-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn test_validate_component() {
        assert!(validate_component("normal-name").is_ok());
        assert!(validate_component("").is_err());
        assert!(validate_component("   ").is_err());
        assert!(validate_component("..").is_err());
        assert!(validate_component(".hidden").is_err());
        assert!(validate_component("a/b").is_err());
        assert!(validate_component("a\\b").is_err());
        assert!(validate_component("../../etc/passwd").is_err());
    }

    #[test]
    fn test_normalize_file_name() {
        assert_eq!(normalize_file_name("query", "sql").unwrap(), "query.sql");
        assert_eq!(normalize_file_name("query.sql", "sql").unwrap(), "query.sql");
        assert_eq!(normalize_file_name("query.SQL", "sql").unwrap(), "query.SQL");
        assert!(normalize_file_name("../evil", "sql").is_err());
        // Engine-specific extension (redis)
        assert_eq!(normalize_file_name("keys", "redis").unwrap(), "keys.redis");
        assert_eq!(
            normalize_file_name("keys.redis", "redis").unwrap(),
            "keys.redis"
        );
        // Replace another engine's extension with this one (keys.sql is a different name on a redis connection)
        assert_eq!(
            normalize_file_name("keys.sql", "redis").unwrap(),
            "keys.sql.redis"
        );
    }

    #[test]
    fn test_ensure_query_file_creates_and_keeps_existing() {
        let dir = test_dir().join("ensure");
        let connection = "conn";

        // Create empty if missing (the extension is supplied too)
        assert_eq!(
            ensure_query_file(&dir, connection, "report", "sql").unwrap(),
            "report.sql"
        );
        assert_eq!(read_query_file(&dir, connection, "report", "sql").unwrap(), "");

        // Existing contents are not erased (unlike create_query_file, it is not an error either)
        write_query_file(&dir, connection, "report.sql", "SELECT 1;", "sql").unwrap();
        assert_eq!(
            ensure_query_file(&dir, connection, "report.sql", "sql").unwrap(),
            "report.sql"
        );
        assert_eq!(
            read_query_file(&dir, connection, "report", "sql").unwrap(),
            "SELECT 1;"
        );

        // An invalid name is not created; it is an error
        assert!(ensure_query_file(&dir, connection, "../evil", "sql").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    /// When a directory / broken symlink with the same name is squatting there, it is an error
    /// rather than "already exists, so OK" (because there is no openable query file).
    #[test]
    fn test_ensure_query_file_rejects_non_file() {
        let dir = test_dir().join("ensure-non-file");
        let connection = "conn";
        let conn_dir = connection_dir(&dir, connection).unwrap();
        fs::create_dir_all(&conn_dir).unwrap();

        // Directory
        fs::create_dir(conn_dir.join("dir-entry.sql")).unwrap();
        assert!(ensure_query_file(&dir, connection, "dir-entry", "sql").is_err());

        // Broken symlink (the link target is missing)
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                conn_dir.join("missing-target.sql"),
                conn_dir.join("dangling.sql"),
            )
            .unwrap();
            assert!(ensure_query_file(&dir, connection, "dangling", "sql").is_err());
        }

        // A link pointing to an **existing regular file** outside the storage area.
        // metadata follows links, so is_file() passes, but the running instance rejects it via
        // verify_within_dir. Succeeding here would give "the CLI exited with 0 but the file is not
        // opened".
        #[cfg(unix)]
        {
            let outside = test_dir().join("ensure-non-file-outside");
            fs::create_dir_all(&outside).unwrap();
            let target = outside.join("real.sql");
            fs::write(&target, "SELECT 1;").unwrap();
            std::os::unix::fs::symlink(&target, conn_dir.join("escaping.sql")).unwrap();
            assert!(ensure_query_file(&dir, connection, "escaping", "sql").is_err());

            // A link pointing **inside** the storage area is accepted by the opening side
            // (verify_within_dir), so accept it here too (do not let the checks disagree).
            let inside = conn_dir.join("inside.sql");
            fs::write(&inside, "SELECT 1;").unwrap();
            std::os::unix::fs::symlink(&inside, conn_dir.join("linked.sql")).unwrap();
            assert!(ensure_query_file(&dir, connection, "linked", "sql").is_ok());

            let _ = fs::remove_dir_all(&outside);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    /// Newly created items are restricted to 0600 for files / 0700 for directories
    /// (CYBERNEURA-DEV-510). Aligned with history.rs, which stores the same content.
    #[cfg(unix)]
    #[test]
    fn test_new_query_files_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir().join("permissions");
        let connection = "conn";

        let mode_of = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

        // create_query_file (new empty file). Starts from a state where even the storage root is not created yet
        assert!(!dir.exists());
        create_query_file(&dir, connection, "created", "sql").unwrap();
        let conn_dir = connection_dir(&dir, connection).unwrap();
        assert_eq!(
            mode_of(&dir),
            0o700,
            "保存ルートも 0700 (途中の階層を残さない)"
        );
        assert_eq!(mode_of(&conn_dir), 0o700, "接続ディレクトリは 0700");
        assert_eq!(mode_of(&conn_dir.join("created.sql")), 0o600);

        // ensure_query_file (the path the CLI uses for "create if missing")
        ensure_query_file(&dir, connection, "ensured", "sql").unwrap();
        assert_eq!(mode_of(&conn_dir.join("ensured.sql")), 0o600);

        // write_query_file (temporary file + rename. A new creation, so there is nothing to inherit from)
        write_query_file(&dir, connection, "written", "SELECT 1;", "sql").unwrap();
        assert_eq!(mode_of(&conn_dir.join("written.sql")), 0o600);

        // The mode of an existing directory is not changed (a setting the user loosened is not tightened back on its own)
        fs::set_permissions(&conn_dir, fs::Permissions::from_mode(0o755)).unwrap();
        write_query_file(&dir, connection, "second", "SELECT 2;", "sql").unwrap();
        assert_eq!(
            mode_of(&conn_dir),
            0o755,
            "既存ディレクトリのパーミッションは変更しないこと"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Writes use a temporary file + rename and do not expose the destination in an intermediate
    /// state. Also verifies that no temporary file is left and that existing permissions are inherited.
    #[test]
    fn test_write_query_file_is_atomic() {
        let dir = test_dir().join("atomic");
        let connection = "conn";

        write_query_file(&dir, connection, "report", "SELECT 1;", "sql").unwrap();
        let conn_dir = connection_dir(&dir, connection).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = conn_dir.join("report.sql");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            write_query_file(&dir, connection, "report", "SELECT 2;", "sql").unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "既存ファイルのパーミッションを引き継ぐこと"
            );
        }

        assert_eq!(
            read_query_file(&dir, connection, "report", "sql").unwrap(),
            if cfg!(unix) { "SELECT 2;" } else { "SELECT 1;" }
        );

        // No temporary file is left (if one remained it would not show in list/search, but would be garbage)
        let leftovers: Vec<String> = fs::read_dir(&conn_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "一時ファイルが残っている: {leftovers:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_query_files_filters_by_extension() {
        let dir = test_dir().join("ext");
        let connection = "redis-conn";

        create_query_file(&dir, connection, "commands", "redis").unwrap();
        // A manually placed file with a different extension does not appear in the list
        fs::write(
            connection_dir(&dir, connection).unwrap().join("other.sql"),
            "SELECT 1;",
        )
        .unwrap();

        assert_eq!(
            list_query_files(&dir, connection, "redis").unwrap(),
            vec!["commands.redis"]
        );
        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            vec!["other.sql"]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_natural_cmp() {
        // Digit runs are compared as numbers (creation order even with different digit counts)
        assert_eq!(natural_cmp("a-9", "a-10"), Ordering::Less);
        assert_eq!(natural_cmp("a-10", "a-9"), Ordering::Greater);
        assert_eq!(natural_cmp("a-100", "a-99"), Ordering::Greater);
        // Digit runs are compared numerically even with leading zeros. With equal values, the one with fewer digits comes first
        assert_eq!(natural_cmp("a-02", "a-9"), Ordering::Less);
        assert_eq!(natural_cmp("a-2", "a-02"), Ordering::Less);
        // Non-digits use ordinary character comparison. The shorter prefix-match is smaller
        assert_eq!(natural_cmp("20260102-1200", "20260102-1200-2"), Ordering::Less);
        assert_eq!(natural_cmp("report", "report"), Ordering::Equal);
        assert_eq!(natural_cmp("apple", "banana"), Ordering::Less);
        // The date part is also compared as a number (same digit count, so it agrees with lexicographic order)
        assert_eq!(natural_cmp("20260102-1200", "20260315-1830"), Ordering::Less);
    }

    #[test]
    fn test_list_query_file_entries_sorts_by_modified_desc() {
        use std::time::{Duration, UNIX_EPOCH};
        let dir = test_dir().join("entries");
        let connection = "entries-conn";
        create_query_file(&dir, connection, "b-old", "sql").unwrap();
        create_query_file(&dir, connection, "a-new", "sql").unwrap();
        create_query_file(&dir, connection, "c-mid", "sql").unwrap();
        create_query_file(&dir, connection, "z-tie", "sql").unwrap();
        create_query_file(&dir, connection, "y-tie", "sql").unwrap();
        write_query_file(&dir, connection, "c-mid.sql", "select 1;", "sql").unwrap();
        let conn_dir = connection_dir(&dir, connection).unwrap();
        let set = |name: &str, secs: u64| {
            fs::File::options()
                .write(true)
                .open(conn_dir.join(name))
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        };
        set("b-old.sql", 1_700_000_000);
        set("c-mid.sql", 1_750_000_000);
        set("a-new.sql", 1_800_000_000);
        set("z-tie.sql", 1_600_000_000);
        set("y-tie.sql", 1_600_000_000);

        let entries = list_query_file_entries(&dir, connection, "sql").unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| e.file_name.as_str())
                .collect::<Vec<_>>(),
            // Two with the same time stay in name order (descending)
            vec![
                "a-new.sql",
                "c-mid.sql",
                "b-old.sql",
                "z-tie.sql",
                "y-tie.sql"
            ]
        );
        assert_eq!(entries[0].modified_ms, Some(1_800_000_000_000));
        assert_eq!(entries[1].size, "select 1;".len() as u64);
        assert_eq!(entries[0].size, 0);
        // The name-ordered list (shared with search) is unchanged
        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            vec![
                "z-tie.sql",
                "y-tie.sql",
                "c-mid.sql",
                "b-old.sql",
                "a-new.sql"
            ]
        );
    }

    #[test]
    fn test_list_query_files_sorts_newest_first() {
        let dir = test_dir().join("order");
        let connection = "order-conn";

        // The default file name is YYYYMMDD-HHMM, so descending name order = newest first.
        // To check that the order is independent of creation order, files are deliberately created out of
        // chronological order. The trailing -2 / -10 are sequence numbers for several files created in
        // the same minute (FilesPane).
        // This covers both regressions: comparing with the extension puts the un-numbered (oldest) one
        // before the numbered ones, and lexicographic comparison puts -2 before -10.
        create_query_file(&dir, connection, "20260101-0900", "sql").unwrap();
        create_query_file(&dir, connection, "20260315-1830", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200-2", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200-10", "sql").unwrap();

        let expected = vec![
            "20260315-1830.sql",
            "20260102-1200-10.sql",
            "20260102-1200-2.sql",
            "20260102-1200.sql",
            "20260101-0900.sql",
        ];
        assert_eq!(list_query_files(&dir, connection, "sql").unwrap(), expected);

        // Search results have the same order (the enumeration is shared via list_query_file_names)
        let hits = search_query_files(&dir, connection, "2026", "sql").unwrap();
        assert_eq!(
            hits.iter().map(|h| h.file_name.as_str()).collect::<Vec<_>>(),
            expected
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_query_file_crud() {
        let dir = test_dir();
        let connection = "test-conn";

        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            Vec::<String>::new()
        );

        let name = create_query_file(&dir, connection, "my query", "sql").unwrap();
        assert_eq!(name, "my query.sql");

        // Recreating the same name is an error
        assert!(create_query_file(&dir, connection, "my query", "sql").is_err());

        write_query_file(&dir, connection, &name, "SELECT 1;", "sql").unwrap();
        assert_eq!(
            read_query_file(&dir, connection, &name, "sql").unwrap(),
            "SELECT 1;"
        );

        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            vec!["my query.sql"]
        );

        delete_query_file(&dir, connection, &name, "sql").unwrap();
        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            Vec::<String>::new()
        );

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_query_file_if_unchanged() {
        let dir = test_dir().join("cas");
        let connection = "test-conn";
        let name = "q.sql";

        // If the file does not exist, create it regardless of expected_base (recovery from external deletion)
        assert_eq!(
            write_query_file_if_unchanged(&dir, connection, name, "V1", "", "sql").unwrap(),
            true
        );
        assert_eq!(read_query_file(&dir, connection, name, "sql").unwrap(), "V1");

        // Write if base matches the current on-disk content.
        assert_eq!(
            write_query_file_if_unchanged(&dir, connection, name, "V2", "V1", "sql").unwrap(),
            true
        );
        assert_eq!(read_query_file(&dir, connection, name, "sql").unwrap(), "V2");

        // If it was changed to "EXTERNAL" outside the app while our base is stale ("V2"), nothing is
        // written and false is returned (external changes are not silently overwritten).
        write_query_file(&dir, connection, name, "EXTERNAL", "sql").unwrap();
        assert_eq!(
            write_query_file_if_unchanged(&dir, connection, name, "MINE", "V2", "sql").unwrap(),
            false
        );
        assert_eq!(read_query_file(&dir, connection, name, "sql").unwrap(), "EXTERNAL");

        // Once base is set to the current value, writing works again.
        assert_eq!(
            write_query_file_if_unchanged(&dir, connection, name, "MINE", "EXTERNAL", "sql")
                .unwrap(),
            true
        );
        assert_eq!(read_query_file(&dir, connection, name, "sql").unwrap(), "MINE");

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_query_file_path() {
        let dir = test_dir().join("fullpath");
        let connection = "test-conn";

        // The absolute path is returned with .sql appended, the connection folder, and the directory joined
        let path = query_file_path(&dir, connection, "report", "sql").unwrap();
        let expected = dir
            .join(connection)
            .join("report.sql")
            .to_string_lossy()
            .into_owned();
        assert_eq!(path, expected);

        // A name already ending in .sql does not get a second one
        let path = query_file_path(&dir, connection, "report.sql", "sql").unwrap();
        assert_eq!(path, expected);

        // Path traversal is rejected
        assert!(query_file_path(&dir, connection, "../evil", "sql").is_err());
        assert!(query_file_path(&dir, connection, "a/b", "sql").is_err());
    }

    #[test]
    fn test_rename_query_file() {
        let dir = test_dir().join("rename");
        let connection = "test-conn";

        create_query_file(&dir, connection, "old", "sql").unwrap();
        write_query_file(&dir, connection, "old", "SELECT 1;", "sql").unwrap();

        // Rename succeeds (contents are kept)
        let renamed = rename_query_file(&dir, connection, "old", "new", "sql").unwrap();
        assert_eq!(renamed, "new.sql");
        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            vec!["new.sql"]
        );
        assert_eq!(
            read_query_file(&dir, connection, "new", "sql").unwrap(),
            "SELECT 1;"
        );

        // Renaming to an existing name is rejected
        create_query_file(&dir, connection, "other", "sql").unwrap();
        assert!(rename_query_file(&dir, connection, "new", "other", "sql").is_err());

        // Renaming to the same name (after normalization) is a no-op
        assert_eq!(
            rename_query_file(&dir, connection, "new", "new.sql", "sql").unwrap(),
            "new.sql"
        );

        // Renaming a nonexistent file is an error
        assert!(rename_query_file(&dir, connection, "missing", "x", "sql").is_err());

        // An invalid new name is rejected (path traversal)
        assert!(rename_query_file(&dir, connection, "new", "../evil", "sql").is_err());
        assert!(rename_query_file(&dir, connection, "new", "a/b", "sql").is_err());

        // Renaming to a file differing only in case is rejected (case-insensitive check)
        assert!(rename_query_file(&dir, connection, "new", "OTHER", "sql").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rename_query_file_case_only() {
        let dir = test_dir().join("rename-case");
        let connection = "test-conn";

        create_query_file(&dir, connection, "Report", "sql").unwrap();
        write_query_file(&dir, connection, "Report", "SELECT 2;", "sql").unwrap();

        // A rename that only changes the file's own case is allowed
        let renamed =
            rename_query_file(&dir, connection, "Report", "report", "sql").unwrap();
        assert_eq!(renamed, "report.sql");
        assert_eq!(
            read_query_file(&dir, connection, "report", "sql").unwrap(),
            "SELECT 2;"
        );
        // On a case-insensitive FS it stays one file; on a case-sensitive FS too
        // the old name does not remain (it was renamed).
        let files = list_query_files(&dir, connection, "sql").unwrap();
        assert!(files.iter().any(|f| f.eq_ignore_ascii_case("report.sql")));
        assert!(!files.contains(&"Report.sql".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_move_query_file() {
        let dir = test_dir().join("move");
        let from = "from-conn";
        let to = "to-conn";

        create_query_file(&dir, from, "report", "sql").unwrap();
        write_query_file(&dir, from, "report", "SELECT 1;", "sql").unwrap();

        // The destination folder is created even if it does not exist yet. Contents are kept
        assert_eq!(
            move_query_file(&dir, from, to, "report", "sql").unwrap(),
            "report.sql"
        );
        assert!(list_query_files(&dir, from, "sql").unwrap().is_empty());
        assert_eq!(list_query_files(&dir, to, "sql").unwrap(), vec!["report.sql"]);
        assert_eq!(
            read_query_file(&dir, to, "report", "sql").unwrap(),
            "SELECT 1;"
        );

        // A file missing from the source is an error (not a success even for the same folder)
        assert!(move_query_file(&dir, from, to, "report", "sql").is_err());
        assert!(move_query_file(&dir, from, from, "report", "sql").is_err());

        // If the destination has a same-name file it is an error (the source remains)
        create_query_file(&dir, from, "report", "sql").unwrap();
        assert!(move_query_file(&dir, from, to, "report", "sql").is_err());
        assert_eq!(
            list_query_files(&dir, from, "sql").unwrap(),
            vec!["report.sql"]
        );
        // A name differing only in case also counts as the same name (source and destination are
        // different folders, so both can be created even on a case-insensitive FS)
        create_query_file(&dir, from, "Sales", "sql").unwrap();
        create_query_file(&dir, to, "sales", "sql").unwrap();
        assert!(move_query_file(&dir, from, to, "Sales", "sql").is_err());

        // Moving to the same folder is a no-op (different connections can have the same folder_name)
        assert_eq!(
            move_query_file(&dir, from, from, "report", "sql").unwrap(),
            "report.sql"
        );
        assert_eq!(
            read_query_file(&dir, from, "report", "sql").unwrap(),
            ""
        );

        // Path traversal is rejected
        assert!(move_query_file(&dir, from, "../evil", "report", "sql").is_err());
        assert!(move_query_file(&dir, "../evil", to, "report", "sql").is_err());
        assert!(move_query_file(&dir, from, to, "../evil", "sql").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_search_query_files() {
        let dir = test_dir().join("search");
        let connection = "test-conn";

        create_query_file(&dir, connection, "users report", "sql").unwrap();
        write_query_file(
            &dir,
            connection,
            "users report",
            "SELECT * FROM users WHERE active = 1;",
            "sql",
        )
        .unwrap();
        create_query_file(&dir, connection, "orders", "sql").unwrap();
        write_query_file(
            &dir,
            connection,
            "orders",
            "SELECT id, total FROM orders;",
            "sql",
        )
        .unwrap();

        // An empty query returns empty
        assert!(search_query_files(&dir, connection, "  ", "sql").unwrap().is_empty());

        // File name match (case-insensitive)
        let hits = search_query_files(&dir, connection, "USERS", "sql").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_name, "users report.sql");
        assert!(hits[0].name_match);
        // "users" is also in the contents, so a preview is attached
        assert!(hits[0].content_preview.as_deref().unwrap().contains("users"));

        // Content-only match (the file name is "orders" but the contents contain total)
        let hits = search_query_files(&dir, connection, "total", "sql").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_name, "orders.sql");
        assert!(!hits[0].name_match);
        assert_eq!(
            hits[0].content_preview.as_deref(),
            Some("SELECT id, total FROM orders;")
        );

        // A word in neither gives 0 results
        assert!(search_query_files(&dir, connection, "zzz", "sql").unwrap().is_empty());

        // A manually placed hidden .sql is excluded from search (its content preview must not leak).
        // validate_component rejects dot-prefixed names, so it cannot be made via create; reproduce it by
        // writing the file directly
        fs::write(
            connection_dir(&dir, connection).unwrap().join(".secret.sql"),
            "SELECT secret_total FROM vault;",
        )
        .unwrap();
        assert!(search_query_files(&dir, connection, "secret", "sql")
            .unwrap()
            .is_empty());
        assert!(!list_query_files(&dir, connection, "sql")
            .unwrap()
            .iter()
            .any(|f| f.starts_with('.')));

        // A nonexistent connection directory gives 0 results
        assert!(search_query_files(&dir, "no-such-conn", "users", "sql")
            .unwrap()
            .is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_truncate_preview() {
        assert_eq!(truncate_preview("  SELECT 1  "), "SELECT 1");
        let long = "x".repeat(200);
        let out = truncate_preview(&long);
        assert_eq!(out.chars().count(), PREVIEW_MAX_CHARS + 1); // +1 is for the ellipsis
        assert!(out.ends_with('…'));
    }
}
