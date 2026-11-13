//! The `queryfolio` command on the PATH (macOS only).
//!
//! The menu's Install Command Line Tools… puts a symlink to the executable inside the `.app`
//! at [`CLI_LINK_PATH`]. A launch through that symlink is handed to LaunchServices
//! ([`launch_app`]) instead of running the GUI in the terminal.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::router::Route;

pub const CLI_LINK_PATH: &str = "/usr/local/bin/queryfolio";

/// Put in front of a relaunched `write`: the process that was started from the terminal has
/// already written the file, so the relaunched one must only open it (the content is left out
/// of the relaunch arguments anyway; stdin cannot be passed through `open`). When an instance
/// is running, writing again would run `config_override_command` (a 1Password prompt) a second
/// time just to hand the arguments over. When none is running, the relaunched app still runs it
/// once more for itself: the merged config cannot be handed to a new process.
pub const WRITE_DONE_FLAG: &str = "--cli-write-done";

/// Whether [`WRITE_DONE_FLAG`] comes before the subcommand. Anything after `write` is its
/// arguments, so a query whose content is the flag itself still gets written.
pub fn write_already_done<S: AsRef<str>>(args: &[S]) -> bool {
    args.iter()
        .map(AsRef::as_ref)
        .take_while(|a| *a != "open" && *a != "write")
        .any(|a| a == WRITE_DONE_FLAG)
}

/// Launched through a symlink such as [`CLI_LINK_PATH`], i.e. from the command line.
/// Running the `.app` executable as is would tie the GUI to the terminal (closing the
/// terminal quits the app), so such a launch is handed to LaunchServices instead.
pub fn invoked_via_symlink() -> bool {
    std::env::current_exe()
        .and_then(|path| path.symlink_metadata())
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// The arguments for the relaunched process.
///
/// - The relaunched process starts in `/`, so a relative `open` path is made absolute against
///   `cwd` here (`~` is left for the router to expand).
/// - `write` is relaunched without its content and with [`WRITE_DONE_FLAG`] (see there).
///   It stays `write <connection> <file>` rather than becoming `open <path>`: the path would
///   be resolved back to a connection through its folder, which fails when two connections
///   share a folder.
/// - Arguments that are neither (unknown options) are dropped, as the GUI ignores them.
pub fn relaunch_args(route: Option<&Route>, cwd: Option<&Path>) -> Vec<String> {
    match route {
        None => Vec::new(),
        Some(Route::OpenFile { path }) => {
            let path = match cwd {
                Some(cwd) if !path.starts_with('~') && Path::new(path).is_relative() => {
                    cwd.join(path).to_string_lossy().into_owned()
                }
                _ => path.clone(),
            };
            vec!["open".to_string(), path]
        }
        Some(Route::WriteFile {
            connection,
            file_name,
            ..
        }) => vec![
            WRITE_DONE_FLAG.to_string(),
            "write".to_string(),
            connection.clone(),
            file_name.clone(),
        ],
    }
}

pub fn launch_app(args: &[String]) -> Result<(), String> {
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|e| format!("Failed to locate the application: {e}"))?;

    let Some(bundle) = app_bundle(&executable) else {
        // A development build (outside a .app). A running instance takes the arguments
        // through single-instance and this one exits
        Command::new(&executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to start {}: {e}", executable.display()))?;
        return Ok(());
    };

    let mut command = Command::new("/usr/bin/open");
    // With arguments, -n starts a new process: open does not pass arguments to an app that is
    // already running, it only brings it to the front. The new process hands them to the
    // running instance through single-instance and exits
    if !args.is_empty() {
        command.arg("-n");
    }
    command.arg(&bundle);
    if !args.is_empty() {
        command.arg("--args").args(args);
    }
    let status = command
        .status()
        .map_err(|e| format!("Failed to run open: {e}"))?;
    if !status.success() {
        return Err(format!("open exited with {status}"));
    }
    Ok(())
}

/// The `<name>.app` of `<name>.app/Contents/MacOS/<executable>`
fn app_bundle(executable: &Path) -> Option<PathBuf> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app";
    is_bundle.then(|| bundle.to_path_buf())
}

/// The menu's Install Command Line Tools…. /usr/local/bin is often owned by root, so when it
/// cannot be written, the administrator password is asked for.
pub fn install() -> Result<String, String> {
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|e| format!("Failed to locate the application: {e}"))?;
    check_install_source(&executable)?;
    let link = Path::new(CLI_LINK_PATH);
    // Do not overwrite something else (a symlink is taken as one made earlier and replaced)
    if let Ok(metadata) = link.symlink_metadata() {
        if !metadata.file_type().is_symlink() {
            return Err(format!(
                "{CLI_LINK_PATH} already exists and is not a symlink."
            ));
        }
    }

    let script = install_script(&executable, link);
    let as_user = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to run sh: {e}"))?;
    // Replaced by something else after the check above: asking for the password would not help
    if as_user.code() == Some(EXISTS_EXIT_CODE) {
        return Err(format!(
            "{CLI_LINK_PATH} already exists and is not a symlink."
        ));
    }
    if !as_user.success() {
        let output = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!(
                "do shell script \"{}\" with administrator privileges",
                applescript_escape(&script)
            ))
            .output()
            .map_err(|e| format!("Failed to run osascript: {e}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
    }
    Ok(format!(
        "{CLI_LINK_PATH} -> {}\n\nRun `queryfolio --help` in a terminal to see the commands.",
        executable.display()
    ))
}

/// An app that macOS runs from a randomized read-only location (App Translocation: a
/// quarantined app opened where it was downloaded, including straight from the disk image)
/// gets a different path on the next launch, so a link to it would break.
fn check_install_source(executable: &Path) -> Result<(), String> {
    if executable.to_string_lossy().contains("/AppTranslocation/") {
        return Err(
            "Move Queryfolio to the Applications folder, open it from there, and try again."
                .to_string(),
        );
    }
    Ok(())
}

const EXISTS_EXIT_CODE: i32 = 3;

/// The script checks again that the link path is not something other than a symlink: the
/// check in `install` runs before the script, possibly long before (the password prompt), and
/// `ln -sf` running as root would delete a file that appeared in between (and put the link
/// inside a directory). A free path is linked without `-f`, so whatever appears there after the
/// test makes `ln` fail instead of being deleted. Only an existing symlink is replaced with
/// `-f`; that leaves a window of microseconds in which only root could swap it for a file
/// (the script runs as root only when /usr/local/bin is not writable by the user), and POSIX
/// has no "replace only if it is a symlink" operation to close it.
fn install_script(executable: &Path, link: &Path) -> String {
    let link = link.to_string_lossy();
    let directory = Path::new(link.as_ref())
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    format!(
        "if [ -L {link_q} ]; then ln -sfn {target_q} {link_q}; elif [ -e {link_q} ]; then echo {message} >&2; exit {EXISTS_EXIT_CODE}; else mkdir -p {dir_q} && ln -s {target_q} {link_q}; fi",
        dir_q = sh_quote(&directory),
        target_q = sh_quote(&executable.to_string_lossy()),
        link_q = sh_quote(&link),
        message = sh_quote(&format!("{link} already exists and is not a symlink.")),
    )
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn applescript_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// macOS-only code: the tests use Unix paths, symlinks and /bin/sh, which Windows does not have
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn relaunch_args_makes_a_relative_open_path_absolute() {
        // Arrange
        let route = Route::OpenFile {
            path: "reporting/monthly.sql".to_string(),
        };

        // Act
        let args = relaunch_args(Some(&route), Some(Path::new("/Users/me/sqlfiles")));

        // Assert
        assert_eq!(args, ["open", "/Users/me/sqlfiles/reporting/monthly.sql"]);
    }

    #[test]
    fn relaunch_args_keeps_absolute_and_home_relative_open_paths() {
        // Arrange
        let cwd = Path::new("/Users/me/work");
        let absolute = Route::OpenFile {
            path: "/tmp/a.sql".to_string(),
        };
        let home = Route::OpenFile {
            path: "~/q/a.sql".to_string(),
        };

        // Act / Assert
        assert_eq!(
            relaunch_args(Some(&absolute), Some(cwd)),
            ["open", "/tmp/a.sql"]
        );
        assert_eq!(relaunch_args(Some(&home), Some(cwd)), ["open", "~/q/a.sql"]);
    }

    #[test]
    fn relaunch_args_drops_the_content_of_write_and_marks_it_done() {
        // Arrange
        let route = Route::WriteFile {
            connection: "reporting".to_string(),
            file_name: "monthly.sql".to_string(),
            content: Some("SELECT 1;".to_string()),
        };

        // Act
        let args = relaunch_args(Some(&route), Some(Path::new("/")));

        // Assert: the relaunched process sees the flag and does not write again
        assert_eq!(args, [WRITE_DONE_FLAG, "write", "reporting", "monthly.sql"]);
        assert!(write_already_done(&args));
        assert!(relaunch_args(None, None).is_empty());
    }

    #[test]
    fn write_already_done_ignores_the_flag_inside_the_write_arguments() {
        // A query whose content happens to be the flag is content, not the flag
        assert!(!write_already_done(&[
            "write",
            "conn",
            "a.sql",
            WRITE_DONE_FLAG
        ]));
        assert!(!write_already_done(&["write", "conn", "a.sql"]));
        assert!(write_already_done(&[
            "-psn_0_1",
            WRITE_DONE_FLAG,
            "write",
            "c",
            "a.sql"
        ]));
    }

    #[test]
    fn app_bundle_finds_the_enclosing_app() {
        assert_eq!(
            app_bundle(Path::new(
                "/Applications/Queryfolio.app/Contents/MacOS/queryfolio"
            )),
            Some(PathBuf::from("/Applications/Queryfolio.app"))
        );
        assert_eq!(
            app_bundle(Path::new("/repo/src-tauri/target/debug/queryfolio")),
            None
        );
    }

    #[test]
    fn check_install_source_rejects_paths_that_change_on_the_next_launch() {
        let translocated = Path::new(
            "/private/var/folders/x/AppTranslocation/1234/d/Queryfolio.app/Contents/MacOS/queryfolio",
        );
        let installed = Path::new("/Applications/Queryfolio.app/Contents/MacOS/queryfolio");
        let external_drive =
            Path::new("/Volumes/SSD/Applications/Queryfolio.app/Contents/MacOS/queryfolio");

        assert!(check_install_source(translocated).is_err());
        assert!(check_install_source(installed).is_ok());
        assert!(check_install_source(external_drive).is_ok());
    }

    #[test]
    fn install_script_quotes_paths_for_applescript_and_sh() {
        // Arrange: the path has spaces and quotes
        let executable =
            Path::new("/Applications/My \"SQL\" Tools's/Queryfolio.app/Contents/MacOS/queryfolio");

        // Act
        let script = install_script(executable, Path::new(CLI_LINK_PATH));
        let escaped = applescript_escape(&script);

        // Assert: inside the AppleScript string, " and \ are not read as a terminator or an escape
        assert!(script.ends_with(
            "else mkdir -p '/usr/local/bin' && ln -s '/Applications/My \"SQL\" Tools'\\''s/Queryfolio.app/Contents/MacOS/queryfolio' '/usr/local/bin/queryfolio'; fi"
        ));
        assert!(!escaped
            .replace("\\\\", "")
            .replace("\\\"", "")
            .contains(['"', '\\']));
    }

    fn run_install_script(link: &Path) -> std::process::ExitStatus {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(install_script(Path::new("/target/queryfolio"), link))
            .stderr(Stdio::null())
            .status()
            .unwrap()
    }

    #[test]
    fn install_script_creates_or_replaces_a_symlink() {
        // Arrange: one path is free, the other already has a link from an earlier install
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("bin").join("queryfolio");
        let earlier = dir.path().join("earlier");
        std::os::unix::fs::symlink("/old/queryfolio", &earlier).unwrap();

        // Act
        let fresh_status = run_install_script(&fresh);
        let earlier_status = run_install_script(&earlier);

        // Assert
        assert!(fresh_status.success());
        assert!(earlier_status.success());
        assert_eq!(fs_read_link(&fresh), "/target/queryfolio");
        assert_eq!(fs_read_link(&earlier), "/target/queryfolio");
    }

    #[test]
    fn install_script_leaves_a_file_or_directory_alone() {
        // Arrange: the path holds a regular file / a real directory
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "keep").unwrap();
        let directory = dir.path().join("directory");
        std::fs::create_dir(&directory).unwrap();

        // Act
        let file_status = run_install_script(&file);
        let directory_status = run_install_script(&directory);

        // Assert: nothing is replaced, and no link is put inside the directory
        assert_eq!(file_status.code(), Some(EXISTS_EXIT_CODE));
        assert_eq!(directory_status.code(), Some(EXISTS_EXIT_CODE));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    }

    fn fs_read_link(path: &Path) -> String {
        std::fs::read_link(path)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }
}
