//! Coding agents (Claude Code / Codex) for the chat pane and the inline AI field
//! (CYBERNEURA-DEV-1354).
//!
//! The agents are the user's own CLIs (`claude` / `codex`), started as child processes and used
//! with the login they already have (a subscription, not an API key). This module holds the
//! parts that do not touch a process: the `agent:` section of config.yml, the working directory
//! of a connection, and the command lines. `agent_process.rs` runs them.

use crate::config::expand_tilde;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which CLI to run
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
        }
    }

    /// Parses the `provider` value of config.yml. `claude-code` is accepted for `claude`.
    fn parse(value: &str) -> Result<Self, AppError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claude_code" => Ok(Provider::Claude),
            "codex" => Ok(Provider::Codex),
            other => Err(AppError::Config(format!(
                "agent.provider must be \"claude\" or \"codex\" (got \"{other}\")"
            ))),
        }
    }
}

/// The `agent:` section of config.yml.
///
/// ```yaml
/// agent:
///   provider: claude            # claude | codex
///   directory: ~/repos/my-app   # optional: working directory for every connection
///   directories:                # optional: working directory per connection name
///     reporting: ~/repos/reporting
///   auto_accept_edits: true     # optional, default true
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub provider: Provider,
    /// Working directory for every connection without its own entry in `directories`
    pub directory: Option<String>,
    /// Working directory per connection name
    pub directories: BTreeMap<String, String>,
    /// Let the agent edit files without asking each time (Claude Code: `acceptEdits`).
    /// Commands the agent wants to run still ask.
    pub auto_accept_edits: bool,
}

#[derive(Deserialize)]
struct RawAgentConfig {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    directories: BTreeMap<String, String>,
    #[serde(default)]
    auto_accept_edits: Option<bool>,
}

impl AgentConfig {
    /// Parses the `agent:` value. Returns None when no agent is selected (no section, no
    /// `provider`, or an empty / "none" provider): the chat pane then uses the built-in
    /// assistant (the `ai:` section).
    pub fn from_value(value: Option<&serde_yaml::Value>) -> Result<Option<Self>, AppError> {
        let Some(value) = value else {
            return Ok(None);
        };
        if value.is_null() {
            return Ok(None);
        }
        let raw: RawAgentConfig = serde_yaml::from_value(value.clone())
            .map_err(|e| AppError::Config(format!("Invalid agent settings: {e}")))?;
        let provider = raw.provider.unwrap_or_default();
        if provider.trim().is_empty() || provider.trim().eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        let blank_to_none = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
        Ok(Some(AgentConfig {
            provider: Provider::parse(&provider)?,
            directory: blank_to_none(raw.directory),
            directories: raw
                .directories
                .into_iter()
                .filter(|(_, dir)| !dir.trim().is_empty())
                .collect(),
            auto_accept_edits: raw.auto_accept_edits.unwrap_or(true),
        }))
    }

    /// The same settings with another provider (the chat pane lets the user pick the agent
    /// for one conversation; the backend, not the webview, turns that into a command line)
    pub fn with_provider(mut self, provider: Option<Provider>) -> Self {
        if let Some(provider) = provider {
            self.provider = provider;
        }
        self
    }

    /// The directory setting that applies to a connection (per connection, then the common one)
    fn directory_setting(&self, connection: &str) -> Option<&str> {
        self.directories
            .get(connection)
            .or(self.directory.as_ref())
            .map(String::as_str)
    }
}

/// Where the agent of a connection runs, and which other directory it must be able to reach.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDirectories {
    /// The agent's working directory
    pub cwd: PathBuf,
    /// The connection's query file folder when it is not the working directory (the agent is
    /// given access to it so that it can still edit the query file that is open)
    pub query_dir: Option<PathBuf>,
}

/// Resolves the working directory of a connection's agent.
///
/// The default is the connection's query file folder (where the SQL files and `_queryfolio.md`
/// are). A configured directory (`agent.directories.<connection>`, then `agent.directory`) or
/// one chosen in the chat pane (`chosen`) replaces it — typically a clone of the application's
/// repository. `~` is expanded; the path must be absolute and an existing directory.
pub fn resolve_directories(
    config: &AgentConfig,
    connection: &str,
    query_dir: &Path,
    chosen: Option<&str>,
) -> Result<AgentDirectories, AppError> {
    let setting = chosen
        .filter(|dir| !dir.trim().is_empty())
        .or_else(|| config.directory_setting(connection));
    let Some(setting) = setting else {
        return Ok(AgentDirectories {
            cwd: query_dir.to_path_buf(),
            query_dir: None,
        });
    };
    let cwd = expand_tilde(setting.trim());
    if !cwd.is_absolute() {
        return Err(AppError::Config(format!(
            "The agent directory must be an absolute path (got \"{setting}\")"
        )));
    }
    if !cwd.is_dir() {
        return Err(AppError::Config(format!(
            "The agent directory does not exist: {}",
            cwd.display()
        )));
    }
    let same = cwd == query_dir
        || matches!(
            (cwd.canonicalize(), query_dir.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        );
    Ok(AgentDirectories {
        cwd,
        query_dir: (!same).then(|| query_dir.to_path_buf()),
    })
}

/// Command line of the long-lived agent behind the chat pane. The frontend talks to it over
/// stdin / stdout (Claude Code: stream-json, Codex: app-server JSON-RPC).
///
/// **Approvals are never bypassed**: tool approvals are routed to the chat pane
/// (`--permission-prompt-tool stdio`; Codex asks through app-server requests). With
/// `auto_accept_edits` Claude Code is started in `acceptEdits` mode, which stops asking for file
/// edits inside the working directories only.
pub fn chat_argv(config: &AgentConfig, dirs: &AgentDirectories) -> Vec<String> {
    let mut argv: Vec<String> = match config.provider {
        Provider::Claude => [
            "claude",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        Provider::Codex => vec!["codex".into(), "app-server".into()],
    };
    match config.provider {
        Provider::Claude => {
            if config.auto_accept_edits {
                argv.extend(["--permission-mode".into(), "acceptEdits".into()]);
            }
            if let Some(query_dir) = &dirs.query_dir {
                argv.extend(["--add-dir".into(), path_arg(query_dir)]);
            }
        }
        Provider::Codex => {
            // Codex keeps its own sandbox / approval settings (~/.codex/config.toml). The only
            // override: the query file folder is made writable when it is outside the working
            // directory, so that an edit of the open query file is not refused by the sandbox.
            if let Some(query_dir) = &dirs.query_dir {
                argv.extend([
                    "-c".into(),
                    format!(
                        "sandbox_workspace_write.writable_roots=[{}]",
                        toml_string(&path_arg(query_dir))
                    ),
                ]);
            }
        }
    }
    argv
}

/// Tools the one-shot Claude Code run has: reading and editing files, nothing that runs a
/// command. There is nobody to answer an approval prompt in a one-shot run.
const ONCE_CLAUDE_TOOLS: &str = "Read,Edit,Write,Glob,Grep";

/// Command line of a one-shot run (the inline AI field above the editor): the agent gets one
/// prompt **on stdin**, may edit files in its working directories, and its final message is the
/// result.
///
/// The prompt is not an argument: it is multi-line user text (a `.cmd` shim on Windows cannot
/// take a newline in an argument), it can exceed the length limit of one argument, and command
/// lines are visible to other processes while the prompt quotes the user's SQL.
///
/// - Claude Code: `--tools` limits the built-in tools to the file tools (the others do not
///   exist for the run, they are not merely unapproved), `--strict-mcp-config` without an MCP
///   config leaves it no MCP tools, and `acceptEdits` applies the edits — the instruction is
///   the request to edit, and nobody could answer a prompt. It prints the final message to
///   stdout.
/// - Codex: `--sandbox workspace-write` confines what it does (commands included) to writes
///   inside the working directory and the query file folder, without network access. It writes
///   the final message to `last_message_file` (its stdout also carries progress). `-` reads
///   the prompt from stdin.
pub fn once_argv(
    config: &AgentConfig,
    dirs: &AgentDirectories,
    last_message_file: &Path,
) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    match config.provider {
        Provider::Claude => {
            argv.extend(
                [
                    "claude",
                    "-p",
                    "--output-format",
                    "text",
                    "--permission-mode",
                    "acceptEdits",
                    "--tools",
                    ONCE_CLAUDE_TOOLS,
                    "--allowedTools",
                    ONCE_CLAUDE_TOOLS,
                    "--strict-mcp-config",
                ]
                .into_iter()
                .map(String::from),
            );
            if let Some(query_dir) = &dirs.query_dir {
                argv.extend(["--add-dir".into(), path_arg(query_dir)]);
            }
        }
        Provider::Codex => {
            argv.extend(
                ["codex", "exec", "--sandbox", "workspace-write", "--skip-git-repo-check"]
                    .into_iter()
                    .map(String::from),
            );
            if let Some(query_dir) = &dirs.query_dir {
                argv.extend(["--add-dir".into(), path_arg(query_dir)]);
            }
            argv.extend(["--output-last-message".into(), path_arg(last_message_file)]);
            argv.push("-".into());
        }
    }
    argv
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A TOML basic string (for `codex -c key=value`, whose value is parsed as TOML)
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What the frontend needs to know about the agent settings (no secrets are involved; the
/// agents use their own login)
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    /// None when no agent is configured (the built-in assistant is used)
    pub provider: Option<Provider>,
    pub provider_name: Option<String>,
    pub auto_accept_edits: bool,
}

impl AgentInfo {
    pub fn from_config(config: Option<&AgentConfig>) -> Self {
        AgentInfo {
            provider: config.map(|c| c.provider),
            provider_name: config.map(|c| c.provider.display_name().to_string()),
            auto_accept_edits: config.is_some_and(|c| c.auto_accept_edits),
        }
    }
}

/// Longest final message of a one-shot run returned to the frontend (it is shown in a toast)
pub const ONCE_MAX_OUTPUT_CHARS: usize = 4_000;

/// Trims the final message of a one-shot run and cuts it to ONCE_MAX_OUTPUT_CHARS
pub fn clip_output(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= ONCE_MAX_OUTPUT_CHARS {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(ONCE_MAX_OUTPUT_CHARS).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Result<Option<AgentConfig>, AppError> {
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        AgentConfig::from_value(Some(&value))
    }

    #[test]
    fn no_provider_means_no_agent() {
        assert_eq!(AgentConfig::from_value(None).unwrap(), None);
        assert_eq!(config("~").unwrap(), None);
        assert_eq!(config("directory: /tmp").unwrap(), None);
        assert_eq!(config("provider: ''").unwrap(), None);
        assert_eq!(config("provider: none").unwrap(), None);
    }

    #[test]
    fn provider_and_defaults_are_parsed() {
        let claude = config("provider: claude").unwrap().unwrap();
        assert_eq!(claude.provider, Provider::Claude);
        assert!(claude.auto_accept_edits);
        assert_eq!(claude.directory, None);
        assert_eq!(
            config("provider: Claude-Code").unwrap().unwrap().provider,
            Provider::Claude
        );
        let codex = config(
            "provider: codex\ndirectory: ~/app\nauto_accept_edits: false\n\
             directories:\n  reporting: /srv/reporting\n  empty: ''\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(codex.provider, Provider::Codex);
        assert!(!codex.auto_accept_edits);
        assert_eq!(codex.directory.as_deref(), Some("~/app"));
        // An empty entry is the same as no entry
        assert_eq!(codex.directories.len(), 1);
        assert_eq!(codex.directory_setting("reporting"), Some("/srv/reporting"));
        assert_eq!(codex.directory_setting("other"), Some("~/app"));
    }

    #[test]
    fn invalid_settings_are_errors() {
        assert!(config("provider: gemini").is_err());
        assert!(config("provider: [claude]").is_err());
        assert!(config("- claude").is_err());
    }

    #[test]
    fn the_query_folder_is_the_default_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let query_dir = tmp.path().join("queries");
        std::fs::create_dir(&query_dir).unwrap();
        let config = config("provider: claude").unwrap().unwrap();
        let dirs = resolve_directories(&config, "main", &query_dir, None).unwrap();
        assert_eq!(dirs.cwd, query_dir);
        assert_eq!(dirs.query_dir, None);
    }

    #[test]
    fn a_configured_directory_keeps_the_query_folder_reachable() {
        let tmp = tempfile::tempdir().unwrap();
        let query_dir = tmp.path().join("queries");
        let repo = tmp.path().join("repo");
        let other = tmp.path().join("other");
        for dir in [&query_dir, &repo, &other] {
            std::fs::create_dir(dir).unwrap();
        }
        let config = AgentConfig {
            provider: Provider::Claude,
            directory: Some(repo.to_string_lossy().into_owned()),
            directories: BTreeMap::from([(
                "reporting".to_string(),
                other.to_string_lossy().into_owned(),
            )]),
            auto_accept_edits: true,
        };
        let dirs = resolve_directories(&config, "main", &query_dir, None).unwrap();
        assert_eq!(dirs.cwd, repo);
        assert_eq!(dirs.query_dir.as_deref(), Some(query_dir.as_path()));
        // The per-connection entry wins over the common one
        let dirs = resolve_directories(&config, "reporting", &query_dir, None).unwrap();
        assert_eq!(dirs.cwd, other);
        // A directory chosen in the chat pane wins over both
        let chosen = query_dir.to_string_lossy().into_owned();
        let dirs = resolve_directories(&config, "reporting", &query_dir, Some(&chosen)).unwrap();
        assert_eq!(dirs.cwd, query_dir);
        assert_eq!(dirs.query_dir, None);
        // A blank choice means "no choice"
        let dirs = resolve_directories(&config, "main", &query_dir, Some("  ")).unwrap();
        assert_eq!(dirs.cwd, repo);
    }

    #[test]
    fn a_missing_or_relative_directory_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config("provider: claude").unwrap().unwrap();
        let missing = tmp.path().join("missing").to_string_lossy().into_owned();
        assert!(resolve_directories(&config, "main", tmp.path(), Some(&missing)).is_err());
        assert!(resolve_directories(&config, "main", tmp.path(), Some("relative/dir")).is_err());
    }

    fn dirs(query_dir: Option<&str>) -> AgentDirectories {
        AgentDirectories {
            cwd: PathBuf::from("/work"),
            query_dir: query_dir.map(PathBuf::from),
        }
    }

    fn agent(provider: Provider, auto_accept_edits: bool) -> AgentConfig {
        AgentConfig {
            provider,
            directory: None,
            directories: BTreeMap::new(),
            auto_accept_edits,
        }
    }

    #[test]
    fn chat_agents_route_approvals_to_the_pane_and_never_bypass_them() {
        for provider in [Provider::Claude, Provider::Codex] {
            for auto in [true, false] {
                let argv = chat_argv(&agent(provider, auto), &dirs(Some("/q")));
                assert!(!argv.iter().any(|a| a.contains("dangerously")), "{argv:?}");
                assert!(!argv.iter().any(|a| a == "bypassPermissions"), "{argv:?}");
            }
        }
        let claude = chat_argv(&agent(Provider::Claude, true), &dirs(None));
        assert!(claude.windows(2).any(|w| w == ["--permission-prompt-tool", "stdio"]));
        assert!(claude.windows(2).any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(!claude.contains(&"--add-dir".to_string()));
        let asking = chat_argv(&agent(Provider::Claude, false), &dirs(Some("/q")));
        assert!(!asking.contains(&"--permission-mode".to_string()));
        assert!(asking.windows(2).any(|w| w == ["--add-dir", "/q"]));
        assert_eq!(
            chat_argv(&agent(Provider::Codex, true), &dirs(None)),
            ["codex", "app-server"]
        );
        assert_eq!(
            chat_argv(&agent(Provider::Codex, true), &dirs(Some("/q \"x\""))),
            [
                "codex",
                "app-server",
                "-c",
                "sandbox_workspace_write.writable_roots=[\"/q \\\"x\\\"\"]"
            ]
        );
    }

    #[test]
    fn one_shot_runs_take_the_prompt_on_stdin_and_are_confined() {
        let out = Path::new("/tmp/last.txt");
        let claude = once_argv(&agent(Provider::Claude, false), &dirs(Some("/q")), out);
        // Only the file tools exist for the run, and no MCP server is loaded
        assert!(claude.windows(2).any(|w| w == ["--tools", "Read,Edit,Write,Glob,Grep"]));
        assert!(claude.contains(&"--strict-mcp-config".to_string()));
        assert!(!claude.iter().any(|a| a.contains("Bash")));
        assert_eq!(&claude[claude.len() - 2..], ["--add-dir", "/q"]);
        let codex = once_argv(&agent(Provider::Codex, true), &dirs(None), out);
        assert!(codex.windows(2).any(|w| w == ["--sandbox", "workspace-write"]));
        assert!(codex.windows(2).any(|w| w == ["--output-last-message", "/tmp/last.txt"]));
        // `-`: the prompt comes from stdin, never from the command line
        assert_eq!(codex.last().map(String::as_str), Some("-"));
        for argv in [&claude, &codex] {
            assert!(!argv.iter().any(|a| a.contains("dangerously")), "{argv:?}");
            assert!(!argv.iter().any(|a| a == "bypassPermissions"), "{argv:?}");
        }
    }

    #[test]
    fn long_output_is_clipped() {
        assert_eq!(clip_output("  done \n"), "done");
        let long = "あ".repeat(ONCE_MAX_OUTPUT_CHARS + 10);
        let clipped = clip_output(&long);
        assert_eq!(clipped.chars().count(), ONCE_MAX_OUTPUT_CHARS + 1);
        assert!(clipped.ends_with('…'));
    }
}
