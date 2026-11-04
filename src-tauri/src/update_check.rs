//! "Check for Updates…" menu (CYBERNEURA-DEV-1079).
//!
//! Only runs when the user picks the menu item: there is no check at startup or in the
//! background, so Queryfolio sends nothing to GitHub unless asked.
//!
//! - Asks the GitHub releases API for the latest published release (drafts and prereleases
//!   are never returned by `releases/latest`) and compares its tag with our version.
//! - When the app was installed with Homebrew, it offers to run `brew upgrade --cask
//!   queryfolio` and restart. Otherwise it opens the Release page for a manual download.
//!
//! tauri-plugin-updater is not used: it needs an update-signing key registered as a CI
//! secret, which is a human task. Homebrew already verifies the download (sha256 in the cask),
//! so the brew path updates the app without that key.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_dialog::{
    DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult,
};

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/cyberneura/queryfolio/releases/latest";
/// Opened instead of the API's `html_url`, so the app only ever opens this fixed page
const RELEASES_PAGE: &str = "https://github.com/cyberneura/queryfolio/releases/latest";
const CASK: &str = "queryfolio";

const UPDATE_WITH_BREW: &str = "Update with Homebrew";
const OPEN_RELEASE_PAGE: &str = "Open Release Page";
const LATER: &str = "Later";
const RESTART_NOW: &str = "Restart Now";

/// Set while a check, a brew upgrade or one of their dialogs is in progress
static IN_PROGRESS: AtomicBool = AtomicBool::new(false);

#[derive(Debug, serde::Deserialize)]
struct LatestRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Parses `v1.2.3` / `1.2.3` into (major, minor, patch). Anything else (pre-release suffixes,
/// missing parts) is rejected: our releases are always plain `v<major>.<minor>.<patch>`.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Returns the latest version (without the `v`) when it is newer than `current`.
fn newer_version(current: &str, latest: &LatestRelease) -> Option<String> {
    if latest.draft || latest.prerelease {
        return None;
    }
    let latest_v = parse_version(&latest.tag_name)?;
    let current_v = parse_version(current)?;
    (latest_v > current_v).then(|| format!("{}.{}.{}", latest_v.0, latest_v.1, latest_v.2))
}

async fn fetch_latest() -> Result<LatestRelease, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let res = client
        .get(LATEST_RELEASE_API)
        // GitHub rejects API requests without a User-Agent
        .header("User-Agent", "Queryfolio update check")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = res.status();
    if !status.is_success() {
        return Err(format!("GitHub returned HTTP {}", status.as_u16()));
    }
    res.json::<LatestRelease>().await.map_err(|e| e.to_string())
}

/// `brew` of each Homebrew prefix that has the cask in its Caskroom (GUI apps do not inherit
/// the shell PATH, so `brew` is called by its full path).
fn brew_candidates(prefixes: &[&Path]) -> Vec<PathBuf> {
    prefixes
        .iter()
        .filter(|prefix| prefix.join("Caskroom").join(CASK).is_dir())
        .map(|prefix| prefix.join("bin/brew"))
        .filter(|brew| brew.is_file())
        .collect()
}

/// The `.app` bundle the running executable lives in
/// (`/Applications/Queryfolio.app/Contents/MacOS/queryfolio` -> `/Applications/Queryfolio.app`).
fn running_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

/// The cask's appdir (where it puts apps). `Caskroom/queryfolio/.metadata/config.json` is the
/// configuration Homebrew writes at install time; `explicit` (`--appdir`) wins over `env`
/// (`HOMEBREW_CASK_OPTS`), which wins over `default` (Homebrew's `Cask::Config`). When it cannot
/// be read, Homebrew's default `/Applications`.
fn cask_appdir(config_json: Option<&str>, home: Option<&Path>) -> PathBuf {
    let config = config_json.and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok());
    let appdir = config.as_ref().and_then(|config| {
        ["explicit", "env", "default"].iter().find_map(|level| {
            config
                .get(level)?
                .get("appdir")?
                .as_str()
                .filter(|dir| !dir.is_empty())
        })
    });
    match appdir {
        Some(dir) => match (dir.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(dir),
        },
        None => PathBuf::from("/Applications"),
    }
}

/// Whether the appdir was set by `HOMEBREW_CASK_OPTS` (`env`) alone.
///
/// `brew upgrade` carries over only the `explicit` part of the saved configuration and takes
/// `env` from the environment it runs in (Homebrew's `Cask::Upgrade` / `Config#merge`). A GUI app
/// does not inherit the shell's variables, so an upgrade started from here would reinstall into
/// the default `/Applications` and the running location would never get the new version (a
/// restart would bring back the old one). In that case brew is not offered; the Release page is.
fn appdir_set_only_by_env(config_json: Option<&str>) -> bool {
    let Some(config) =
        config_json.and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
    else {
        return false;
    };
    let appdir = |level: &str| {
        config
            .get(level)
            .and_then(|c| c.get("appdir"))
            .and_then(|v| v.as_str())
            .filter(|dir| !dir.is_empty())
    };
    appdir("explicit").is_none() && appdir("env").is_some_and(|env| Some(env) != appdir("default"))
}

/// Where the cask put its app, from `brew info --cask --json=v2 queryfolio` (CYBERNEURA-DEV-1147).
///
/// Current Homebrew adds to the `app` artifact a `target` already resolved with the install-time
/// configuration (`{"app":["Queryfolio.app"],"target":"/Applications/Queryfolio.app"}`). Older
/// brew versions without it get the path built from the appdir and the artifact name (with
/// `app "Foo.app", target: "Bar.app"` the installed name is `Bar.app`).
///
/// `brew list --cask queryfolio` is not used: its artifact list
/// (`/Applications/Queryfolio.app (...)`) is printed only when stdout is a terminal. Through a
/// pipe, which is what `Command::output()` gives it, it prints a `find` of the Caskroom instead,
/// so the old check never matched and the cask install was always sent to the Release page.
fn cask_app_paths(info_json: &str, appdir: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let Ok(info) = serde_json::from_str::<serde_json::Value>(info_json) else {
        return Vec::new();
    };
    let artifacts = info["casks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|cask| cask["token"] == CASK)
        .filter_map(|cask| cask["artifacts"].as_array())
        .flatten();
    let mut paths = Vec::new();
    for artifact in artifacts {
        let Some(args) = artifact["app"].as_array() else {
            continue;
        };
        if let Some(target) = artifact["target"].as_str().map(Path::new) {
            if target.is_absolute() {
                paths.push(target.to_path_buf());
                continue;
            }
        }
        // The same resolution as Homebrew's Relocated: the target if given (`~/` expanded,
        // relative ones under the appdir), otherwise the basename of the source under the appdir
        let source = args.first().and_then(|v| v.as_str()).map(Path::new);
        let renamed = args
            .iter()
            .find_map(|v| v.get("target").and_then(|t| t.as_str()));
        let path = match (renamed, source) {
            (Some(target), _) => match (target.strip_prefix("~/"), home) {
                (Some(rest), Some(home)) => home.join(rest),
                (Some(_), None) => continue,
                _ => appdir.join(target),
            },
            (None, Some(source)) => match source.file_name() {
                Some(name) => appdir.join(name),
                None => continue,
            },
            (None, None) => continue,
        };
        paths.push(path);
    }
    paths
}

/// Whether one of the places the cask installed to is `bundle`.
///
/// Compared exactly, case included: APFS is case-insensitive by default, and a loose match could
/// send brew to a copy installed under a different name. The running path is canonicalized
/// (= spelled as on disk).
fn cask_installs_bundle(app_paths: &[PathBuf], bundle: &Path) -> bool {
    app_paths.iter().any(|path| path == bundle)
}

/// Highest version in the output of `brew list --cask --versions queryfolio`
/// (`queryfolio 0.10.0`).
fn parse_cask_version(output: &str) -> Option<(u64, u64, u64)> {
    output
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == CASK).then_some(words)
        })
        .flat_map(|words| words.filter_map(parse_version))
        .max()
}

/// Runs brew without a terminal (it must not wait for input). Returns stdout on success and
/// the tail of the output on failure.
fn run_brew(brew: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
    let output = std::process::Command::new(brew)
        .args(args)
        .envs(envs.iter().copied())
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        // Homebrew's ask mode (the default in current versions) would prompt for confirmation
        // and, with stdin closed, abort. The variable is used instead of `--no-ask` because
        // older brew versions reject unknown flags but ignore unknown variables
        .env("HOMEBREW_NO_ASK", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run {}: {e}", brew.display()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&output.stdout).into_owned();
    }
    Err(tail_lines(&text, 10))
}

/// `brew` that manages the running app, if any. A Caskroom entry alone is not enough: the
/// running copy may be a manually installed one elsewhere, and upgrading the cask would then
/// update a different app. So the place the cask installed to must be our own bundle.
/// Blocking (runs `brew info`).
fn homebrew_brew_for_running_app() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let bundle = running_app_bundle()?;
    let home = dirs::home_dir();
    brew_candidates(&[Path::new("/opt/homebrew"), Path::new("/usr/local")])
        .into_iter()
        .find(|brew| {
            // brew_candidates returns <prefix>/bin/brew
            let Some(prefix) = brew.parent().and_then(Path::parent) else {
                return false;
            };
            let config = std::fs::read_to_string(
                prefix
                    .join("Caskroom")
                    .join(CASK)
                    .join(".metadata/config.json"),
            )
            .ok();
            if appdir_set_only_by_env(config.as_deref()) {
                return false;
            }
            let appdir = cask_appdir(config.as_deref(), home.as_deref());
            run_brew(brew, &["info", "--cask", "--json=v2", CASK], &[]).is_ok_and(|out| {
                cask_installs_bundle(&cask_app_paths(&out, &appdir, home.as_deref()), &bundle)
            })
        })
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Shows a dialog and waits until it is closed. The wait runs on a blocking thread so the
/// async runtime's workers are not held while the user reads the dialog.
async fn ask(
    app: &AppHandle,
    kind: MessageDialogKind,
    text: String,
    buttons: MessageDialogButtons,
) -> MessageDialogResult {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .message(text)
            .title("Check for Updates")
            .kind(kind)
            .buttons(buttons)
            .blocking_show_with_result()
    })
    .await
    .unwrap_or_default()
}

async fn message(app: &AppHandle, kind: MessageDialogKind, text: String) {
    ask(app, kind, text, MessageDialogButtons::Ok).await;
}

fn open_release_page() {
    if let Err(e) = tauri_plugin_opener::open_url(RELEASES_PAGE, None::<&str>) {
        eprintln!("[update] failed to open the release page: {e}");
    }
}

/// Entry point from the menu. Returns immediately; the check and its dialogs run in a
/// background task.
pub fn check_for_updates(app: &AppHandle) {
    // Held until the last dialog is closed, so repeated clicks do not stack dialogs
    if IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        run(&app).await;
        IN_PROGRESS.store(false, Ordering::SeqCst);
    });
}

async fn run(app: &AppHandle) {
    let current = app.package_info().version.to_string();
    let latest = match fetch_latest().await {
        Ok(latest) => latest,
        Err(e) => {
            let text = format!("Could not check for updates.\n\n{e}");
            message(app, MessageDialogKind::Error, text).await;
            return;
        }
    };
    let Some(newer) = newer_version(&current, &latest) else {
        let text = format!("Queryfolio {current} is the latest version.");
        message(app, MessageDialogKind::Info, text).await;
        return;
    };

    let brew = tauri::async_runtime::spawn_blocking(homebrew_brew_for_running_app)
        .await
        .ok()
        .flatten();
    let buttons = if brew.is_some() {
        MessageDialogButtons::YesNoCancelCustom(
            UPDATE_WITH_BREW.into(),
            OPEN_RELEASE_PAGE.into(),
            LATER.into(),
        )
    } else {
        MessageDialogButtons::OkCancelCustom(OPEN_RELEASE_PAGE.into(), LATER.into())
    };
    let text = format!("Queryfolio {newer} is available. You have {current}.");
    let choice = ask(app, MessageDialogKind::Info, text, buttons).await;

    match (choice, brew) {
        (MessageDialogResult::Custom(label), Some(brew)) if label == UPDATE_WITH_BREW => {
            upgrade_with_brew(app, brew, newer).await;
        }
        (MessageDialogResult::Custom(label), _) if label == OPEN_RELEASE_PAGE => {
            open_release_page();
        }
        _ => {}
    }
}

async fn upgrade_with_brew(app: &AppHandle, brew: PathBuf, newer: String) {
    let result = tauri::async_runtime::spawn_blocking(move || {
        // brew refreshes the tap only when its last update is older than
        // HOMEBREW_AUTO_UPDATE_SECS (a day by default); 0 makes it look at the tap now.
        // brew quits running cask apps before upgrading them, which would kill this process
        // before it can verify the result and offer the restart; HOMEBREW_NO_UPGRADE_QUIT_CASKS
        // (= --no-quit, as a variable for the same reason as HOMEBREW_NO_ASK) prevents that
        run_brew(
            &brew,
            &["upgrade", "--cask", CASK],
            &[
                ("HOMEBREW_AUTO_UPDATE_SECS", "0"),
                ("HOMEBREW_NO_UPGRADE_QUIT_CASKS", "1"),
            ],
        )?;
        // A successful upgrade does not mean the new version is in: when the tap has not
        // picked up the release yet, brew skips the cask as already up to date
        run_brew(&brew, &["list", "--cask", "--versions", CASK], &[])
            .map(|out| parse_cask_version(&out))
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));

    let installed = match result {
        Ok(installed) => installed,
        Err(e) => {
            let text = format!(
                "brew upgrade failed.\n\n{e}\n\nRun `brew upgrade --cask {CASK}` in a terminal to see the details."
            );
            message(app, MessageDialogKind::Error, text).await;
            return;
        }
    };
    if installed.is_none() || installed < parse_version(&newer) {
        let text = format!(
            "Homebrew does not offer Queryfolio {newer} yet. The tap follows new releases within an hour; try again later, or download it from the Release page."
        );
        let buttons = MessageDialogButtons::OkCancelCustom(OPEN_RELEASE_PAGE.into(), LATER.into());
        let choice = ask(app, MessageDialogKind::Info, text, buttons).await;
        if matches!(&choice, MessageDialogResult::Custom(label) if label == OPEN_RELEASE_PAGE) {
            open_release_page();
        }
        return;
    }
    let text = format!("Queryfolio {newer} was installed. Restart Queryfolio to use it.");
    let buttons = MessageDialogButtons::OkCancelCustom(RESTART_NOW.into(), LATER.into());
    let choice = ask(app, MessageDialogKind::Info, text, buttons).await;
    if matches!(&choice, MessageDialogResult::Custom(label) if label == RESTART_NOW) {
        app.restart();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> LatestRelease {
        LatestRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
        }
    }

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("v0.9.0"), Some((0, 9, 0)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version(" v0.10.1 "), Some((0, 10, 1)));
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("v1.2.3.4"), None);
        assert_eq!(parse_version("v1.2.3-beta.1"), None);
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn test_newer_version_compares_numerically() {
        assert_eq!(
            newer_version("0.9.0", &release("v0.10.0")),
            Some("0.10.0".into())
        );
        assert_eq!(
            newer_version("0.9.0", &release("v0.9.1")),
            Some("0.9.1".into())
        );
        assert_eq!(
            newer_version("0.9.9", &release("v1.0.0")),
            Some("1.0.0".into())
        );
    }

    #[test]
    fn test_newer_version_none_when_same_or_older() {
        assert_eq!(newer_version("0.9.0", &release("v0.9.0")), None);
        assert_eq!(newer_version("0.10.0", &release("v0.9.5")), None);
    }

    #[test]
    fn test_newer_version_ignores_drafts_prereleases_and_bad_tags() {
        let mut r = release("v9.0.0");
        r.draft = true;
        assert_eq!(newer_version("0.9.0", &r), None);
        let mut r = release("v9.0.0");
        r.prerelease = true;
        assert_eq!(newer_version("0.9.0", &r), None);
        assert_eq!(newer_version("0.9.0", &release("nightly")), None);
    }

    #[test]
    fn test_latest_release_deserializes_github_payload() {
        let json = r#"{"tag_name":"v0.9.0","name":"Queryfolio v0.9.0","draft":false,"prerelease":false,"html_url":"https://github.com/cyberneura/queryfolio/releases/tag/v0.9.0"}"#;
        let r: LatestRelease = serde_json::from_str(json).unwrap();
        assert_eq!(r.tag_name, "v0.9.0");
        assert!(!r.draft && !r.prerelease);
    }

    #[test]
    fn test_brew_candidates_require_the_cask_and_brew() {
        let dir = std::env::temp_dir().join(format!("queryfolio-brew-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let with_cask = dir.join("with-cask");
        let without_cask = dir.join("without-cask");
        let without_brew = dir.join("without-brew");
        for prefix in [&with_cask, &without_cask] {
            std::fs::create_dir_all(prefix.join("bin")).unwrap();
            std::fs::write(prefix.join("bin/brew"), "").unwrap();
        }
        std::fs::create_dir_all(with_cask.join("Caskroom/queryfolio")).unwrap();
        std::fs::create_dir_all(without_brew.join("Caskroom/queryfolio")).unwrap();

        assert!(brew_candidates(&[&without_cask, &without_brew]).is_empty());
        assert_eq!(
            brew_candidates(&[&without_cask, &with_cask, &without_brew]),
            vec![with_cask.join("bin/brew")]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_cask_appdir() {
        let home = Path::new("/Users/me");
        // explicit > env > default
        let json = r#"{"default":{"appdir":"/Applications"},"env":{"appdir":"/Env"},"explicit":{"appdir":"/Explicit"}}"#;
        assert_eq!(
            cask_appdir(Some(json), Some(home)),
            PathBuf::from("/Explicit")
        );
        let json = r#"{"default":{"appdir":"/Applications"},"env":{"appdir":"~/Applications"},"explicit":{}}"#;
        assert_eq!(
            cask_appdir(Some(json), Some(home)),
            PathBuf::from("/Users/me/Applications")
        );
        let json = r#"{"default":{"appdir":"/Applications"},"env":{},"explicit":{}}"#;
        assert_eq!(
            cask_appdir(Some(json), Some(home)),
            PathBuf::from("/Applications")
        );
        // Unreadable or broken: Homebrew's default
        assert_eq!(
            cask_appdir(None, Some(home)),
            PathBuf::from("/Applications")
        );
        assert_eq!(
            cask_appdir(Some("{"), Some(home)),
            PathBuf::from("/Applications")
        );
    }

    // Homebrew is macOS-only, and /Users/... is not an absolute path on Windows
    #[cfg(unix)]
    #[test]
    fn test_cask_app_paths() {
        let appdir = Path::new("/Applications");
        let home = Some(Path::new("/Users/me"));
        // Current brew: the resolved target is used as is
        let json = r#"{"formulae":[],"casks":[{"token":"queryfolio","artifacts":[{"uninstall":[{"quit":"com.cyberneura.queryfolio"}]},{"app":["Queryfolio.app"],"target":"/Users/me/Applications/Queryfolio.app"},{"zap":[{"trash":["~/.config/queryfolio"]}]}]}]}"#;
        assert_eq!(
            cask_app_paths(json, appdir, home),
            vec![PathBuf::from("/Users/me/Applications/Queryfolio.app")]
        );
        // Older brew without target: appdir + name
        let json = r#"{"casks":[{"token":"queryfolio","artifacts":[{"app":["Queryfolio.app"]}]}]}"#;
        assert_eq!(
            cask_app_paths(json, appdir, home),
            vec![PathBuf::from("/Applications/Queryfolio.app")]
        );
        let json = r#"{"casks":[{"token":"queryfolio","artifacts":[{"app":["queryfolio.app",{"target":"Renamed.app"}]}]}]}"#;
        assert_eq!(
            cask_app_paths(json, appdir, home),
            vec![PathBuf::from("/Applications/Renamed.app")]
        );
        // The source's directory is not part of the installed location (basename only)
        let json =
            r#"{"casks":[{"token":"queryfolio","artifacts":[{"app":["dist/Queryfolio.app"]}]}]}"#;
        assert_eq!(
            cask_app_paths(json, appdir, home),
            vec![PathBuf::from("/Applications/Queryfolio.app")]
        );
        // ~/ in a target is the home directory
        let json = r#"{"casks":[{"token":"queryfolio","artifacts":[{"app":["Queryfolio.app",{"target":"~/Apps/Queryfolio.app"}]}]}]}"#;
        assert_eq!(
            cask_app_paths(json, appdir, home),
            vec![PathBuf::from("/Users/me/Apps/Queryfolio.app")]
        );
        // Another cask's artifacts are ignored
        let json = r#"{"casks":[{"token":"other","artifacts":[{"app":["Queryfolio.app"]}]}]}"#;
        assert!(cask_app_paths(json, appdir, home).is_empty());
        assert!(cask_app_paths("not json", appdir, None).is_empty());
    }

    #[test]
    fn test_appdir_set_only_by_env() {
        let env_only = r#"{"default":{"appdir":"/Applications"},"env":{"appdir":"/Users/me/Applications"},"explicit":{}}"#;
        assert!(appdir_set_only_by_env(Some(env_only)));
        // explicit (--appdir) is carried over by upgrade
        let explicit = r#"{"default":{"appdir":"/Applications"},"env":{"appdir":"/Users/me/Applications"},"explicit":{"appdir":"/Users/me/Applications"}}"#;
        assert!(!appdir_set_only_by_env(Some(explicit)));
        // env equal to the default lands in the same place on upgrade
        let same = r#"{"default":{"appdir":"/Applications"},"env":{"appdir":"/Applications"},"explicit":{}}"#;
        assert!(!appdir_set_only_by_env(Some(same)));
        let plain = r#"{"default":{"appdir":"/Applications"},"env":{},"explicit":{}}"#;
        assert!(!appdir_set_only_by_env(Some(plain)));
        assert!(!appdir_set_only_by_env(None));
    }

    #[test]
    fn test_cask_installs_bundle() {
        let paths = vec![PathBuf::from("/Applications/Queryfolio.app")];
        assert!(cask_installs_bundle(
            &paths,
            Path::new("/Applications/Queryfolio.app")
        ));
        // A manually installed copy elsewhere is not the cask's
        assert!(!cask_installs_bundle(
            &paths,
            Path::new("/Users/me/Apps/Queryfolio.app")
        ));
        // Prefix of another path is not a match
        assert!(!cask_installs_bundle(
            &paths,
            Path::new("/Applications/Queryfolio.app.old")
        ));
        // Case matters (APFS would open either spelling)
        assert!(!cask_installs_bundle(
            &paths,
            Path::new("/Applications/queryfolio.app")
        ));
        assert!(!cask_installs_bundle(
            &[],
            Path::new("/Applications/Queryfolio.app")
        ));
    }

    #[test]
    fn test_parse_cask_version() {
        assert_eq!(parse_cask_version("queryfolio 0.10.0\n"), Some((0, 10, 0)));
        assert_eq!(
            parse_cask_version("queryfolio 0.9.0 0.10.0\n"),
            Some((0, 10, 0))
        );
        assert_eq!(parse_cask_version("other 1.0.0\n"), None);
        assert_eq!(parse_cask_version(""), None);
    }

    #[test]
    fn test_tail_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_lines("a", 5), "a");
    }
}
