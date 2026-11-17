//! The Settings window's editing of the general (non-connection) settings in config.yml
//! (CYBERNEURA-DEV-1298).
//!
//! The rules this module keeps (see the "Settings window" section of AGENTS.md):
//!
//! - **Only the local file is read and written.** The values that `config_override_command`
//!   merges in (API keys kept in 1Password, etc.) are never written back: a save re-reads the
//!   local file and applies only the fields the user changed. A key that the override provides
//!   is read-only, and changing it is rejected here as well as in the UI.
//! - **The format stays YAML.** A save rewrites the whole file from its structure, so comments
//!   are lost. Before the first overwrite the original text is kept as `config.yml.bak`, and a
//!   file that has comments is only rewritten after the user confirmed it once
//!   (`SaveOutcome::NeedsCommentConfirmation`).
//! - **A file that cannot be read or parsed is never written** (the user would lose every
//!   connection). Writes are serialized by `CONFIG_WRITE_LOCK`, go through a temp file + rename
//!   and keep a symlinked config.yml a symlink (the rename happens next to the link's target).
//! - **Secrets are not returned to the frontend.** `ai.api_key` is reported only as "set or
//!   not".
//!
//! Everything here is independent of Tauri and of the rest of the config module so that it can
//! be unit tested on its own.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::error::AppError;

/// Serializes every write to config.yml from this process (the text editor, the "Open SQLite /
/// DuckDB file…" append and the Settings window). Each writer reads, changes and renames under
/// the lock, so two saves never interleave and the later one never works from a stale read.
pub(crate) static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// `default_limit` when the key is absent (`AppConfig::default_limit` uses this).
pub const DEFAULT_LIMIT: u64 = 500;

/// Largest `default_limit` the form accepts. Rows travel through the WebView, so a huge limit
/// only freezes the app; 0 (= no limit) is the way to turn it off.
pub const MAX_DEFAULT_LIMIT: u64 = 1_000_000;

/// What to treat as overridden when the local file has `config_override_command` but the merged
/// config could not be built (the command failed, 1Password is locked, ...): every general
/// setting except the command itself. Which keys the override provides is unknown then, and
/// letting the user "change" a value that the override replaces would only look like it
/// worked. The command stays editable so the user can fix it.
pub const OVERRIDE_UNKNOWN_KEYS: [&str; 3] = ["default_limit", "sqlfiles_dir", "ai"];

/// The only AI provider supported today (see ai.rs).
pub const AI_PROVIDER_OPENAI: &str = "openai";

/// Locks `CONFIG_WRITE_LOCK`. A panic while holding it cannot leave the file half-written
/// (writes are rename-based), so a poisoned lock is simply taken over.
pub(crate) fn lock_config_writes() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The local `ai:` section as the form shows it. The API key itself is never included.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct AiSettings {
    /// Whether the local file has an `ai:` mapping at all
    pub present: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// `Some("")` is meaningful (never send reasoning_effort), so it is kept distinct from None
    pub tool_reasoning_effort: Option<String>,
    /// Whether the local file has a non-empty `ai.api_key`
    pub api_key_set: bool,
}

/// The local values of the general settings, read from the file (not the merged config).
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct LocalSettings {
    pub default_limit: Option<u64>,
    pub sqlfiles_dir: Option<String>,
    pub config_override_command: Option<String>,
    pub ai: AiSettings,
    /// Keys whose local value has an unexpected type, with a message. The form shows the
    /// message and leaves the value alone unless the user replaces it.
    pub invalid: Vec<FieldMessage>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FieldMessage {
    /// Dotted key path (`default_limit`, `ai.model`, ...)
    pub field: String,
    pub message: String,
}

/// One field of a save request. A field that is absent from the request is kept as it is.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "op", content = "value", rename_all = "snake_case")]
pub enum Change<T> {
    /// Write this value
    Set(T),
    /// Remove the key (= back to the default)
    Unset,
}

/// A save request from the Settings window. Only the fields the user touched are present.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct SettingsPatch {
    pub default_limit: Option<Change<u64>>,
    pub sqlfiles_dir: Option<Change<String>>,
    pub config_override_command: Option<Change<String>>,
    pub ai_provider: Option<Change<String>>,
    pub ai_model: Option<Change<String>>,
    pub ai_base_url: Option<Change<String>>,
    pub ai_tool_reasoning_effort: Option<Change<String>>,
    pub ai_api_key: Option<Change<String>>,
}

/// Result of a save.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SaveOutcome {
    /// Written (or nothing changed: `changed` is false and the file was not touched)
    Saved {
        path: String,
        changed: bool,
        /// Path of the backup if this save created it
        backup_created: Option<String>,
    },
    /// The file has comments that a save would drop. Nothing was written; the frontend asks
    /// the user and saves again with `allow_drop_comments`.
    NeedsCommentConfirmation,
}

/// Reads the general settings out of a parsed local config.
pub fn read_local_settings(doc: &Mapping) -> LocalSettings {
    let mut out = LocalSettings::default();
    match doc.get("default_limit") {
        None | Some(Value::Null) => {}
        Some(value) => match value.as_u64() {
            Some(n) => out.default_limit = Some(n),
            None => out.invalid.push(FieldMessage {
                field: "default_limit".into(),
                message: "default_limit in the file is not a non-negative integer, so the default (500) is used".into(),
            }),
        },
    }
    read_string(
        doc,
        "sqlfiles_dir",
        "sqlfiles_dir",
        &mut out.sqlfiles_dir,
        &mut out.invalid,
    );
    read_string(
        doc,
        "config_override_command",
        "config_override_command",
        &mut out.config_override_command,
        &mut out.invalid,
    );
    match doc.get("ai") {
        None | Some(Value::Null) => {}
        Some(Value::Mapping(ai)) => {
            out.ai.present = true;
            read_string(
                ai,
                "provider",
                "ai.provider",
                &mut out.ai.provider,
                &mut out.invalid,
            );
            read_string(ai, "model", "ai.model", &mut out.ai.model, &mut out.invalid);
            read_string(
                ai,
                "base_url",
                "ai.base_url",
                &mut out.ai.base_url,
                &mut out.invalid,
            );
            read_string(
                ai,
                "tool_reasoning_effort",
                "ai.tool_reasoning_effort",
                &mut out.ai.tool_reasoning_effort,
                &mut out.invalid,
            );
            out.ai.api_key_set = ai
                .get("api_key")
                .and_then(Value::as_str)
                .is_some_and(|key| !key.trim().is_empty());
        }
        Some(_) => out.invalid.push(FieldMessage {
            field: "ai".into(),
            message: "ai in the file is not a mapping; fix it in the text editor".into(),
        }),
    }
    out
}

fn read_string(
    doc: &Mapping,
    key: &str,
    field: &str,
    slot: &mut Option<String>,
    invalid: &mut Vec<FieldMessage>,
) {
    match doc.get(key) {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => *slot = Some(s.clone()),
        Some(_) => invalid.push(FieldMessage {
            field: field.into(),
            message: format!("{field} in the file is not a string"),
        }),
    }
}

/// Dotted paths of every value the override YAML sets. A mapping is descended into (the merge
/// is recursive there); anything else is one leaf (scalars and lists replace wholesale).
pub fn override_key_paths(overrides: &Mapping) -> Vec<String> {
    fn walk(map: &Mapping, prefix: &str, out: &mut Vec<String>) {
        for (key, value) in map {
            let key = match key {
                Value::String(s) => s.clone(),
                other => match serde_yaml::to_string(other) {
                    Ok(s) => s.trim().to_string(),
                    Err(_) => continue,
                },
            };
            let path = if prefix.is_empty() {
                key
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                Value::Mapping(inner) if !inner.is_empty() => walk(inner, &path, out),
                _ => out.push(path),
            }
        }
    }
    let mut out = Vec::new();
    walk(overrides, "", &mut out);
    out
}

/// Whether `field` (a dotted path) gets its value from the override: the override sets the field
/// itself or one of its parents (`ai` replaced as a whole covers `ai.model`).
pub fn is_overridden(override_keys: &[String], field: &str) -> bool {
    override_keys.iter().any(|key| {
        key == field
            || field
                .strip_prefix(key.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// Whether the text has YAML comments that a structural rewrite would drop. A `#` that starts
/// a line, or follows whitespace, is a comment unless it is inside a quoted scalar; this check
/// does not parse quotes, so a `" #"` inside a string also counts. That only costs one extra
/// confirmation, which is the safe direction.
pub fn has_comments(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('#') || line.contains(" #") || line.contains("\t#")
    })
}

/// Validates `patch` and applies it to the local config `doc`.
///
/// Returns every problem at once (field + message) so the form can show them next to the
/// fields. `doc` is only changed when there are none.
pub fn apply_patch(
    doc: &mut Mapping,
    patch: &SettingsPatch,
    override_keys: &[String],
) -> Result<(), Vec<FieldMessage>> {
    let mut errors = Vec::new();
    let mut check = |field: &str, present: bool, result: Result<(), String>| {
        if !present {
            return;
        }
        if is_overridden(override_keys, field) {
            errors.push(FieldMessage {
                field: field.into(),
                message: format!(
                    "{field} is (or may be) provided by config_override_command (1Password etc.); change it there"
                ),
            });
        } else if let Err(message) = result {
            errors.push(FieldMessage {
                field: field.into(),
                message,
            });
        }
    };

    check(
        "default_limit",
        patch.default_limit.is_some(),
        match &patch.default_limit {
            Some(Change::Set(n)) => validate_default_limit(*n),
            _ => Ok(()),
        },
    );
    check(
        "sqlfiles_dir",
        patch.sqlfiles_dir.is_some(),
        validate_set(&patch.sqlfiles_dir, validate_sqlfiles_dir),
    );
    check(
        "config_override_command",
        patch.config_override_command.is_some(),
        validate_set(&patch.config_override_command, validate_command),
    );
    check(
        "ai.provider",
        patch.ai_provider.is_some(),
        validate_set(&patch.ai_provider, validate_provider),
    );
    check(
        "ai.model",
        patch.ai_model.is_some(),
        validate_set(&patch.ai_model, validate_model),
    );
    check(
        "ai.base_url",
        patch.ai_base_url.is_some(),
        validate_set(&patch.ai_base_url, validate_base_url),
    );
    check(
        "ai.tool_reasoning_effort",
        patch.ai_tool_reasoning_effort.is_some(),
        validate_set(&patch.ai_tool_reasoning_effort, validate_reasoning_effort),
    );
    check(
        "ai.api_key",
        patch.ai_api_key.is_some(),
        validate_set(&patch.ai_api_key, validate_api_key),
    );

    let touches_ai = patch.ai_provider.is_some()
        || patch.ai_model.is_some()
        || patch.ai_base_url.is_some()
        || patch.ai_tool_reasoning_effort.is_some()
        || patch.ai_api_key.is_some();
    if touches_ai {
        if let Some(value) = doc.get("ai") {
            if !matches!(value, Value::Mapping(_) | Value::Null) {
                errors.push(FieldMessage {
                    field: "ai".into(),
                    message: "ai in the file is not a mapping; fix it in the text editor".into(),
                });
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    apply_top(
        doc,
        "default_limit",
        patch
            .default_limit
            .as_ref()
            .map(|c| map_change(c, |n| Value::from(*n))),
    );
    apply_top(
        doc,
        "sqlfiles_dir",
        patch.sqlfiles_dir.as_ref().map(string_change),
    );
    apply_top(
        doc,
        "config_override_command",
        patch.config_override_command.as_ref().map(string_change),
    );

    if touches_ai {
        let mut ai = match doc.get("ai") {
            Some(Value::Mapping(map)) => map.clone(),
            _ => Mapping::new(),
        };
        apply_top(
            &mut ai,
            "provider",
            patch.ai_provider.as_ref().map(string_change),
        );
        apply_top(&mut ai, "model", patch.ai_model.as_ref().map(string_change));
        apply_top(
            &mut ai,
            "base_url",
            patch.ai_base_url.as_ref().map(string_change),
        );
        // tool_reasoning_effort is not trimmed into "unset": "" means "never send it"
        apply_top(
            &mut ai,
            "tool_reasoning_effort",
            patch
                .ai_tool_reasoning_effort
                .as_ref()
                .map(|c| map_change(c, |s| Value::String(s.trim().to_string()))),
        );
        apply_top(
            &mut ai,
            "api_key",
            patch.ai_api_key.as_ref().map(string_change),
        );
        if ai.is_empty() {
            doc.remove("ai");
        } else {
            set_keeping_position(doc, "ai", Value::Mapping(ai));
        }
    }
    Ok(())
}

fn validate_set(
    change: &Option<Change<String>>,
    validate: fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    match change {
        Some(Change::Set(value)) => validate(value),
        _ => Ok(()),
    }
}

fn map_change<T>(change: &Change<T>, to_value: impl Fn(&T) -> Value) -> Option<Value> {
    match change {
        Change::Set(value) => Some(to_value(value)),
        Change::Unset => None,
    }
}

fn string_change(change: &Change<String>) -> Option<Value> {
    map_change(change, |s| Value::String(s.trim().to_string()))
}

/// `change`: None = the field was not in the patch, Some(None) = remove, Some(Some(v)) = set.
fn apply_top(doc: &mut Mapping, key: &str, change: Option<Option<Value>>) {
    match change {
        None => {}
        Some(None) => {
            doc.remove(key);
        }
        Some(Some(value)) => set_keeping_position(doc, key, value),
    }
}

/// Replaces the value in place if the key exists (so the file keeps its order), otherwise
/// appends it.
fn set_keeping_position(doc: &mut Mapping, key: &str, value: Value) {
    match doc.get_mut(key) {
        Some(slot) => *slot = value,
        None => {
            doc.insert(Value::String(key.to_string()), value);
        }
    }
}

fn single_line(field: &str, value: &str) -> Result<(), String> {
    if value.contains(['\n', '\r', '\0']) {
        return Err(format!("{field} must be a single line"));
    }
    Ok(())
}

pub fn validate_default_limit(n: u64) -> Result<(), String> {
    if n > MAX_DEFAULT_LIMIT {
        return Err(format!(
            "Use a number up to {MAX_DEFAULT_LIMIT} (0 turns the automatic LIMIT off)"
        ));
    }
    Ok(())
}

pub fn validate_sqlfiles_dir(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("Enter a folder, or reset it to the default".into());
    }
    single_line("The folder", value)
}

pub fn validate_command(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("Enter a command, or reset it to remove the override".into());
    }
    single_line("The command", value)?;
    match shlex::split(value) {
        Some(parts) if !parts.is_empty() => Ok(()),
        _ => Err("The command has an unclosed quote".into()),
    }
}

pub fn validate_provider(value: &str) -> Result<(), String> {
    if value.trim() != AI_PROVIDER_OPENAI {
        return Err(format!("Only '{AI_PROVIDER_OPENAI}' is supported"));
    }
    Ok(())
}

pub fn validate_model(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("Enter a model name, or reset it to the default".into());
    }
    single_line("The model", value)?;
    if value.trim().contains(char::is_whitespace) {
        return Err("A model name has no spaces".into());
    }
    Ok(())
}

pub fn validate_base_url(value: &str) -> Result<(), String> {
    let value = value.trim();
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return Err("The base URL starts with https:// (or http://)".into());
    };
    if value.contains(char::is_whitespace) {
        return Err("The base URL has no spaces".into());
    }
    if rest.is_empty() || rest.starts_with('/') {
        return Err("The base URL has no host".into());
    }
    Ok(())
}

pub fn validate_reasoning_effort(value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.len() > 32
        || !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c == '-')
    {
        return Err("Use a value such as none, low, medium or high (empty = never send)".into());
    }
    Ok(())
}

pub fn validate_api_key(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("Enter the API key, or remove it".into());
    }
    single_line("The API key", value)?;
    if value.trim().contains(char::is_whitespace) {
        return Err("The API key has no spaces".into());
    }
    Ok(())
}

/// The path to actually write when `path` may be a symlink (a config kept in dotfiles):
/// the link's target, so the rename replaces the target file and the link survives.
/// A dangling link is followed too (the target is created).
pub(crate) fn resolve_write_target(path: &Path) -> Result<PathBuf, AppError> {
    if let Ok(real) = std::fs::canonicalize(path) {
        return Ok(real);
    }
    // A dangling link: follow it to the last link target (following one level only would make
    // the rename replace an intermediate link of a chain with a plain file). A loop stops at
    // the hop limit
    let mut target = path.to_path_buf();
    let mut hops = 0;
    while let Ok(link) = std::fs::read_link(&target) {
        hops += 1;
        if hops > 40 {
            return Err(AppError::Config(format!(
                "Too many levels of symbolic links: {}",
                path.display()
            )));
        }
        target = if link.is_absolute() {
            link
        } else {
            target.parent().map(|p| p.join(&link)).unwrap_or(link)
        };
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    Ok(target)
}

/// Writes `content` to `path` (following a symlink) through a temp file + rename, always with
/// owner-only permissions (the config may hold passwords). Callers hold `CONFIG_WRITE_LOCK`.
pub(crate) fn write_atomically(path: &Path, content: &str) -> Result<PathBuf, AppError> {
    let target = resolve_write_target(path)?;
    write_file_atomically(&target, content)?;
    Ok(target)
}

/// A temp file name next to `target` that does not exist yet. A fixed name would let a symlink
/// planted at that name redirect the write (and truncate whatever it points to, the config
/// itself included), so the name is unique (pid + time + counter) and the file is opened with
/// `create_new`.
fn unique_temp_path(target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.yml".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    target.with_file_name(format!(
        ".{name}.{}-{nanos}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Writes the whole content to a fresh temp file (`create_new` = O_EXCL, so it never opens an
/// existing file or symlink), then renames it over `target`. Owner-only permissions.
fn write_file_atomically(target: &Path, content: &str) -> std::io::Result<()> {
    let temp = unique_temp_path(target);
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        // mode() is only narrowed further by umask; guard against an umask that drops the
        // owner bits too
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Path of the backup kept before the Settings window first rewrites the file.
pub fn backup_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.yml".into());
    target.with_file_name(format!("{name}.bak"))
}

/// Writes the backup if there is none yet. Never overwrites an existing one (it holds the
/// state from before the first save, which is the one worth keeping).
///
/// The copy is written to a fresh temp file and renamed into place only once it is complete, so a
/// failed write (disk full, ...) never leaves a truncated `.bak` that a later save would take
/// for the finished backup. Callers hold `CONFIG_WRITE_LOCK`, so the existence check and the
/// rename do not race with another save from this process.
fn write_backup_once(target: &Path, original: &str) -> Result<Option<PathBuf>, AppError> {
    let backup = backup_path_for(target);
    if std::fs::symlink_metadata(&backup).is_ok() {
        return Ok(None);
    }
    write_file_atomically(&backup, original)?;
    Ok(Some(backup))
}

/// Parses the local config text. A document that is not a mapping is an error (the caller then
/// writes nothing).
pub fn parse_local(text: &str) -> Result<Mapping, AppError> {
    let value: Value = serde_yaml::from_str(text).map_err(|e| {
        AppError::Config(format!(
            "config.yml could not be parsed, so the Settings window will not save over it. Fix it with Edit config.yml first: {e}"
        ))
    })?;
    match value {
        Value::Mapping(map) => Ok(map),
        // An empty file parses as null
        Value::Null => Ok(Mapping::new()),
        _ => Err(AppError::Config(
            "config.yml is not a YAML mapping, so the Settings window will not save over it".into(),
        )),
    }
}

/// Saves `patch` into the config file at `path` (which must exist).
///
/// Reads the file again under the lock, so the change is applied to what is on disk now
/// (not to what the form loaded), and only the touched keys change.
pub fn save_patch(
    path: &Path,
    patch: &SettingsPatch,
    override_keys: &[String],
    allow_drop_comments: bool,
) -> Result<SaveOutcome, AppError> {
    save_doc_change(path, allow_drop_comments, |doc| {
        apply_patch(doc, patch, override_keys).map_err(|errors| {
            AppError::Config(
                errors
                    .iter()
                    .map(|e| format!("{}: {}", e.field, e.message))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })
    })
}

/// The write path shared by every structural save (the general settings and the connection
/// editor): under the lock, re-read the file, let `change` edit the parsed mapping, and write the
/// result with the same rules (unchanged = untouched, comments confirmed once, round-trip check,
/// `.bak` before the first overwrite, atomic write through symlinks).
pub fn save_doc_change(
    path: &Path,
    allow_drop_comments: bool,
    change: impl FnOnce(&mut Mapping) -> Result<(), AppError>,
) -> Result<SaveOutcome, AppError> {
    let _guard = lock_config_writes();
    let original = std::fs::read_to_string(path)?;
    let mut doc = parse_local(&original)?;
    let before = doc.clone();
    change(&mut doc)?;
    if doc == before {
        return Ok(SaveOutcome::Saved {
            path: path.display().to_string(),
            changed: false,
            backup_created: None,
        });
    }
    if has_comments(&original) && !allow_drop_comments {
        return Ok(SaveOutcome::NeedsCommentConfirmation);
    }
    let text = serde_yaml::to_string(&Value::Mapping(doc.clone()))
        .map_err(|e| AppError::Config(format!("Failed to write the settings as YAML: {e}")))?;
    // The output must read back as the same structure; otherwise do not replace the file
    if parse_local(&text)? != doc {
        return Err(AppError::Config(
            "The settings did not survive a YAML round trip, so nothing was saved".into(),
        ));
    }
    let target = resolve_write_target(path)?;
    let backup = write_backup_once(&target, &original)?;
    let written = write_atomically(path, &text)?;
    Ok(SaveOutcome::Saved {
        path: written.display().to_string(),
        changed: true,
        backup_created: backup.map(|p| p.display().to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> Mapping {
        parse_local(text).unwrap()
    }

    fn set<T>(value: T) -> Option<Change<T>> {
        Some(Change::Set(value))
    }

    fn temp_config(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_local_values_without_the_api_key() {
        let doc = yaml(
            "default_limit: 200\nsqlfiles_dir: ~/q\nconfig_override_command: op read x\nai:\n  api_key: sk-secret\n  model: gpt-x\n",
        );
        let local = read_local_settings(&doc);
        assert_eq!(local.default_limit, Some(200));
        assert_eq!(local.sqlfiles_dir.as_deref(), Some("~/q"));
        assert_eq!(local.config_override_command.as_deref(), Some("op read x"));
        assert!(local.ai.present);
        assert!(local.ai.api_key_set);
        assert_eq!(local.ai.model.as_deref(), Some("gpt-x"));
        let json = serde_json::to_string(&local).unwrap();
        assert!(
            !json.contains("sk-secret"),
            "the API key must not reach the frontend"
        );
    }

    #[test]
    fn reports_values_of_the_wrong_type() {
        let local = read_local_settings(&yaml("default_limit: lots\nai: oops\n"));
        assert_eq!(local.default_limit, None);
        let fields: Vec<_> = local.invalid.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(fields, ["default_limit", "ai"]);
    }

    #[test]
    fn override_paths_descend_into_mappings_only() {
        let keys = override_key_paths(&yaml(
            "ai:\n  api_key: sk\nservers:\n  - name: a\ndefault_limit: 5\nempty: {}\n",
        ));
        assert_eq!(keys, ["ai.api_key", "servers", "default_limit", "empty"]);
        assert!(is_overridden(&keys, "ai.api_key"));
        assert!(!is_overridden(&keys, "ai.model"));
        assert!(!is_overridden(&keys, "ai.api_key_extra"));
        assert!(is_overridden(&["ai".to_string()], "ai.model"));
        assert!(!is_overridden(&["ai".to_string()], "aim"));
    }

    #[test]
    fn patch_changes_only_the_touched_keys_and_keeps_order() {
        let mut doc = yaml("servers: []\ndefault_limit: 10\nsqlfiles_dir: ~/q\nextra: keep\n");
        let patch = SettingsPatch {
            default_limit: set(20),
            sqlfiles_dir: Some(Change::Unset),
            ..Default::default()
        };
        apply_patch(&mut doc, &patch, &[]).unwrap();
        let text = serde_yaml::to_string(&doc).unwrap();
        assert_eq!(text, "servers: []\ndefault_limit: 20\nextra: keep\n");
    }

    #[test]
    fn patch_rejects_overridden_keys() {
        let mut doc = yaml("servers: []\n");
        let before = doc.clone();
        let patch = SettingsPatch {
            ai_api_key: set("sk-new".into()),
            default_limit: set(5),
            ..Default::default()
        };
        let errors = apply_patch(&mut doc, &patch, &["ai.api_key".into()]).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "ai.api_key");
        assert_eq!(doc, before, "nothing is applied when a field is rejected");
    }

    #[test]
    fn patch_reports_every_invalid_field() {
        let mut doc = yaml("servers: []\n");
        let patch = SettingsPatch {
            default_limit: set(MAX_DEFAULT_LIMIT + 1),
            config_override_command: set("op read \"unclosed".into()),
            ai_provider: set("anthropic".into()),
            ai_base_url: set("example.com".into()),
            ai_tool_reasoning_effort: set("High Effort".into()),
            ai_api_key: set("  ".into()),
            sqlfiles_dir: set("a\nb".into()),
            ai_model: set("gpt 5".into()),
        };
        let errors = apply_patch(&mut doc, &patch, &[]).unwrap_err();
        assert_eq!(errors.len(), 8, "{errors:?}");
    }

    #[test]
    fn ai_section_is_created_and_removed() {
        let mut doc = yaml("servers: []\n");
        let patch = SettingsPatch {
            ai_api_key: set(" sk-1 ".into()),
            ai_tool_reasoning_effort: set("".into()),
            ..Default::default()
        };
        apply_patch(&mut doc, &patch, &[]).unwrap();
        assert_eq!(
            serde_yaml::to_string(&doc).unwrap(),
            "servers: []\nai:\n  tool_reasoning_effort: ''\n  api_key: sk-1\n"
        );
        let patch = SettingsPatch {
            ai_api_key: Some(Change::Unset),
            ai_tool_reasoning_effort: Some(Change::Unset),
            ..Default::default()
        };
        apply_patch(&mut doc, &patch, &[]).unwrap();
        assert_eq!(serde_yaml::to_string(&doc).unwrap(), "servers: []\n");
    }

    #[test]
    fn ai_that_is_not_a_mapping_is_not_replaced() {
        let mut doc = yaml("ai: legacy\n");
        let patch = SettingsPatch {
            ai_model: set("gpt-x".into()),
            ..Default::default()
        };
        let errors = apply_patch(&mut doc, &patch, &[]).unwrap_err();
        assert_eq!(errors[0].field, "ai");
    }

    #[test]
    fn patch_deserializes_from_the_frontend_shape() {
        let patch: SettingsPatch = serde_json::from_str(
            r#"{"default_limit":{"op":"set","value":100},"ai_api_key":{"op":"unset"}}"#,
        )
        .unwrap();
        assert_eq!(patch.default_limit, Some(Change::Set(100)));
        assert_eq!(patch.ai_api_key, Some(Change::Unset));
        assert_eq!(patch.sqlfiles_dir, None);
    }

    #[test]
    fn comments_are_detected() {
        assert!(has_comments("# header\nservers: []\n"));
        assert!(has_comments("servers: [] # trailing\n"));
        assert!(!has_comments("servers: []\nurl: https://example.com/#x\n"));
    }

    #[test]
    fn save_round_trips_and_keeps_a_backup_once() {
        let original =
            "servers:\n- name: a\n  engine: sqlite\n  schema: /tmp/a.db\ndefault_limit: 10\n";
        let (_dir, path) = temp_config(original);
        let patch = SettingsPatch {
            default_limit: set(25),
            ..Default::default()
        };
        let outcome = save_patch(&path, &patch, &[], false).unwrap();
        let SaveOutcome::Saved {
            changed,
            backup_created,
            ..
        } = outcome
        else {
            panic!("expected a save");
        };
        assert!(changed);
        let backup = PathBuf::from(backup_created.expect("first save keeps a backup"));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);

        let saved = parse_local(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut expected = parse_local(original).unwrap();
        expected.insert("default_limit".into(), 25.into());
        assert_eq!(
            saved, expected,
            "structure -> YAML -> structure is lossless"
        );

        // The second save leaves the backup from before the first one alone
        let patch = SettingsPatch {
            default_limit: set(30),
            ..Default::default()
        };
        let outcome = save_patch(&path, &patch, &[], false).unwrap();
        assert!(matches!(
            outcome,
            SaveOutcome::Saved {
                backup_created: None,
                ..
            }
        ));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
        let leftovers = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "temp files are renamed away");
    }

    #[test]
    fn save_asks_before_dropping_comments() {
        let original = "# my notes\nservers: []\n";
        let (_dir, path) = temp_config(original);
        let patch = SettingsPatch {
            default_limit: set(1),
            ..Default::default()
        };
        assert_eq!(
            save_patch(&path, &patch, &[], false).unwrap(),
            SaveOutcome::NeedsCommentConfirmation
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        save_patch(&path, &patch, &[], true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "servers: []\ndefault_limit: 1\n"
        );
    }

    #[test]
    fn save_without_changes_does_not_touch_the_file() {
        let original = "# notes\ndefault_limit: 5\n";
        let (_dir, path) = temp_config(original);
        let patch = SettingsPatch {
            default_limit: set(5),
            ..Default::default()
        };
        let outcome = save_patch(&path, &patch, &[], false).unwrap();
        assert!(matches!(outcome, SaveOutcome::Saved { changed: false, .. }));
        assert!(!backup_path_for(&path).exists());
    }

    #[test]
    fn save_refuses_a_broken_file() {
        let original = "servers: [\n";
        let (_dir, path) = temp_config(original);
        let patch = SettingsPatch {
            default_limit: set(1),
            ..Default::default()
        };
        assert!(save_patch(&path, &patch, &[], true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path_for(&path).exists());
    }

    #[test]
    fn save_never_writes_override_values() {
        // The override provides the API key; the local file has none. Saving another field
        // must not bring the merged key into the local file.
        let (_dir, path) = temp_config("servers: []\nai:\n  model: gpt-x\n");
        let keys = override_key_paths(&yaml("ai:\n  api_key: sk-from-1password\n"));
        let patch = SettingsPatch {
            default_limit: set(7),
            ..Default::default()
        };
        save_patch(&path, &patch, &keys, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-from-1password"));
        assert!(!text.contains("api_key"));
        let patch = SettingsPatch {
            ai_api_key: set("sk-local".into()),
            ..Default::default()
        };
        assert!(save_patch(&path, &patch, &keys, false).is_err());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("sk-local"));
    }

    #[test]
    fn an_existing_backup_is_never_replaced() {
        let (_dir, path) = temp_config("default_limit: 1\n");
        std::fs::write(backup_path_for(&path), "older\n").unwrap();
        let patch = SettingsPatch {
            default_limit: set(2),
            ..Default::default()
        };
        let outcome = save_patch(&path, &patch, &[], false).unwrap();
        assert!(matches!(
            outcome,
            SaveOutcome::Saved {
                backup_created: None,
                ..
            }
        ));
        assert_eq!(
            std::fs::read_to_string(backup_path_for(&path)).unwrap(),
            "older\n"
        );
    }

    #[test]
    fn unknown_override_locks_everything_but_the_command() {
        let keys: Vec<String> = OVERRIDE_UNKNOWN_KEYS
            .iter()
            .map(|k| k.to_string())
            .collect();
        let mut doc = yaml("config_override_command: op read x\n");
        let patch = SettingsPatch {
            ai_model: set("m".into()),
            ..Default::default()
        };
        assert!(apply_patch(&mut doc, &patch, &keys).is_err());
        let patch = SettingsPatch {
            config_override_command: set("op read y".into()),
            ..Default::default()
        };
        apply_patch(&mut doc, &patch, &keys).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_is_never_written_through() {
        // A symlink at a predictable temp name must not redirect the write
        let (dir, path) = temp_config("default_limit: 1\n");
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, "keep me\n").unwrap();
        for name in ["config.yml.tmp", ".config.yml.tmp", "config.yml.bak.tmp"] {
            std::os::unix::fs::symlink(&victim, dir.path().join(name)).unwrap();
        }
        let patch = SettingsPatch {
            default_limit: set(2),
            ..Default::default()
        };
        save_patch(&path, &patch, &[], false).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me\n");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "default_limit: 2\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_through_a_symlink_keeps_the_link_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("dotfiles");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real = real_dir.join("queryfolio.yml");
        std::fs::write(&real, "servers: []\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.path().join("config.yml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let patch = SettingsPatch {
            default_limit: set(3),
            ..Default::default()
        };
        save_patch(&link, &patch, &[], false).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "servers: []\ndefault_limit: 3\n"
        );
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let backup = backup_path_for(&real);
        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the backup can hold passwords too");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_chain_is_followed_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.yml");
        let alias = dir.path().join("alias.yml");
        let real = dir.path().join("sub").join("real.yml");
        std::os::unix::fs::symlink(&alias, &first).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert_eq!(resolve_write_target(&first).unwrap(), real);
        let looped = dir.path().join("loop.yml");
        std::os::unix::fs::symlink(&looped, &looped).unwrap();
        assert!(resolve_write_target(&looped).is_err());
    }
}
