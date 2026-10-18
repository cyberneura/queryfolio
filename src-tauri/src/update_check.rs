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

/// Whether the output of `brew list --cask queryfolio` lists `bundle` as an installed
/// artifact. Artifact lines look like `/Applications/Queryfolio.app (1,234 files, 56.7MB)`.
fn cask_lists_bundle(list_output: &str, bundle: &Path) -> bool {
    let bundle = bundle.to_string_lossy();
    list_output.lines().map(str::trim).any(|line| {
        line.strip_prefix(bundle.as_ref())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(" ("))
    })
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
/// update a different app. So the cask's installed artifacts must include our own bundle.
/// Blocking (runs `brew list`).
fn homebrew_brew_for_running_app() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let bundle = running_app_bundle()?;
    brew_candidates(&[Path::new("/opt/homebrew"), Path::new("/usr/local")])
        .into_iter()
        .find(|brew| {
            run_brew(brew, &["list", "--cask", CASK], &[])
                .is_ok_and(|out| cask_lists_bundle(&out, &bundle))
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
        // HOMEBREW_AUTO_UPDATE_SECS (a day by default); 0 makes it look at the tap now
        run_brew(
            &brew,
            &["upgrade", "--cask", CASK],
            &[("HOMEBREW_AUTO_UPDATE_SECS", "0")],
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
    fn test_cask_lists_bundle() {
        let out = "==> App\n/Applications/Queryfolio.app (1,234 files, 56.7MB)\n";
        assert!(cask_lists_bundle(
            out,
            Path::new("/Applications/Queryfolio.app")
        ));
        // A manually installed copy elsewhere is not the cask's
        assert!(!cask_lists_bundle(
            out,
            Path::new("/Users/me/Apps/Queryfolio.app")
        ));
        // Prefix of another path is not a match
        let out = "/Applications/Queryfolio.app.old (12 files, 1MB)\n";
        assert!(!cask_lists_bundle(
            out,
            Path::new("/Applications/Queryfolio.app")
        ));
        assert!(cask_lists_bundle(
            "/Applications/Queryfolio.app\n",
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
