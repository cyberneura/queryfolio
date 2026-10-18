use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ssh2::Session;

use crate::config::SshTunnelConfig;
use crate::error::AppError;
use crate::config::expand_tilde;

/// Timeout (in milliseconds) for SSH blocking operations (handshake / auth, etc.).
const SSH_TIMEOUT_MS: u32 = 30_000;

/// Sleep while idle in the async pump loop.
const PUMP_IDLE_SLEEP: Duration = Duration::from_millis(5);

/// SSH local port forwarding tunnel.
///
/// Listens on a free port on 127.0.0.1 and, for each accepted connection, establishes a new
/// SSH session and relays to the destination over a direct-tcpip channel.
/// libssh2 sessions are not safe for concurrent use across threads, so each forwarded
/// connection gets its own independent session (sqlx pools hold few connections, so the
/// session setup overhead is acceptable).
pub struct SshTunnel {
    pub local_port: u16,
    shutdown: Arc<AtomicBool>,
    /// Some when the tunnel is delegated to the system `ssh` client
    /// (ssh_tunnel.ssh_config mode). Killed on drop to tear the tunnel down.
    child: Option<Child>,
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct ForwardTarget {
    ssh_config: SshTunnelConfig,
    target_host: String,
    target_port: u16,
}

impl SshTunnel {
    /// Starts the tunnel. To detect authentication errors early,
    /// first try establishing a session once, then set up the listener.
    pub fn start(
        ssh_config: &SshTunnelConfig,
        target_host: &str,
        target_port: u16,
    ) -> Result<Self, AppError> {
        // If ssh_tunnel.ssh_config is set, delegate to the system ssh
        // (for ProxyJump / multi-hop tunnels / ~/.ssh/config resolution).
        if let Some(alias) = ssh_config.ssh_config.as_deref() {
            let alias = alias.trim();
            if alias.is_empty() {
                return Err(AppError::Config(
                    "ssh_tunnel.ssh_config must not be empty".into(),
                ));
            }
            // An alias starting with `-` would be interpreted by ssh as an option
            // (`-oProxyCommand=...` would execute an arbitrary command). ssh_config can also be set from
            // the YAML fetched by config_override_command, so reject it to keep the boundary that
            // fetched YAML must not make us execute commands.
            // The argv is also separated with `--` (see start_system_ssh).
            if alias.starts_with('-') {
                return Err(AppError::Config(
                    "ssh_tunnel.ssh_config must not start with '-'".into(),
                ));
            }
            return start_system_ssh(alias, target_host, target_port);
        }

        // libssh2 path: host / user are required (they may be omitted only when delegating to ssh_config).
        // They can be empty strings via the serde default, so reject them explicitly. Proceeding empty
        // would give a confusing authentication failure at userauth, hiding the missing config.
        if ssh_config.host.trim().is_empty() {
            return Err(AppError::Config(
                "ssh_tunnel requires 'host' (or set 'ssh_config' to delegate to system ssh)"
                    .into(),
            ));
        }
        if ssh_config.user.trim().is_empty() {
            return Err(AppError::Config(
                "ssh_tunnel requires 'user' (or set 'ssh_config' to delegate to system ssh)"
                    .into(),
            ));
        }

        // Connection test that doubles as credential validation
        establish_session(ssh_config)?;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let local_port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let target = Arc::new(ForwardTarget {
            ssh_config: ssh_config.clone(),
            target_host: target_host.to_string(),
            target_port,
        });

        let accept_shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            accept_loop(listener, target, accept_shutdown);
        });

        Ok(Self {
            local_port,
            shutdown,
            child: None,
        })
    }
}

/// Sets up a `-N -L` local forward tunnel with the system `ssh` client.
///
/// Used when `ssh_tunnel.ssh_config` (= a Host alias in ~/.ssh/config) is specified.
/// Resolution of HostName / User / Port / ProxyJump / multi-hop tunnels is delegated to
/// OpenSSH and ~/.ssh/config (the libssh2 path is not used). Authentication and host key
/// verification are also left to OpenSSH. BatchMode=yes forbids interactive prompts for
/// password / passphrase / host key confirmation (to avoid hanging without a TTY when launched
/// from the GUI); agent authentication (1Password, etc.) is handled by the agent and unaffected.
fn start_system_ssh(
    alias: &str,
    target_host: &str,
    target_port: u16,
) -> Result<SshTunnel, AppError> {
    // Allocate a free local port ourselves and then pass it to ssh.
    // (Reading back the port from ssh -L's dynamic port 0 allocation is cumbersome)
    let local_port = {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        // Close the listener before letting ssh bind (a slight race exists but is acceptable)
        drop(listener);
        port
    };

    // For an IPv6 literal destination (`::1` / `fd00::10`, etc.), the `-L` syntax requires
    // brackets (`[::1]`). Without them it becomes `...:::1:...` and OpenSSH rejects it as an invalid
    // local-forward. Host names / IPv4 contain no `:` and pass through unchanged.
    let forward_host = if target_host.contains(':') && !target_host.starts_with('[') {
        format!("[{target_host}]")
    } else {
        target_host.to_string()
    };
    let forward = format!("127.0.0.1:{local_port}:{forward_host}:{target_port}");
    let connect_timeout_secs = (SSH_TIMEOUT_MS / 1000).max(1);
    let path = crate::config::supplement_path(&std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new("ssh")
        .arg("-N")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={connect_timeout_secs}"))
        .arg("-L")
        .arg(&forward)
        // Prevent anything after this from being interpreted as options (prevents argument injection via an alias)
        .arg("--")
        .arg(alias)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("PATH", path)
        .spawn()
        .map_err(|e| AppError::SshTunnel(format!("Failed to launch ssh: {e}")))?;

    // Drain stderr in a separate thread (to keep ssh from stalling on a full pipe
    // and to capture diagnostic messages on failure).
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let mut drain = child.stderr.take().map(|mut err| {
        let buf = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            if let Ok(mut guard) = buf.lock() {
                guard.push_str(&s);
            }
        })
    });

    // Wait for the drain thread to finish before building the failure diagnostic.
    // If ssh exits immediately, try_wait() may observe the exit before the drain thread writes,
    // so reading without a join would yield an empty message (exactly when the diagnostic is needed most).
    // At this point the child has already exited (= stderr is at EOF), so the join returns immediately.
    let collect_stderr = |drain: &mut Option<std::thread::JoinHandle<()>>| {
        if let Some(handle) = drain.take() {
            let _ = handle.join();
        }
        stderr_buf
            .lock()
            .ok()
            .map(|g| g.trim().to_string())
            .unwrap_or_default()
    };

    // Wait until the local forward port accepts connections.
    // OpenSSH opens the local listen socket only after it has succeeded in authenticating to the
    // target and establishing the forward (ExitOnForwardFailure=yes makes it exit immediately on
    // failure), so a successful connection means authentication succeeded (the same role as the
    // up-front verification by establish_session on the libssh2 path).
    let start = Instant::now();
    let timeout = Duration::from_millis(SSH_TIMEOUT_MS as u64);
    let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], local_port));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(AppError::SshTunnel(format!(
                    "ssh tunnel via '{alias}' exited ({status}): {}",
                    collect_stderr(&mut drain)
                )));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AppError::SshTunnel(format!("Failed to poll ssh: {e}")));
            }
        }
        if TcpStream::connect_timeout(&loopback, Duration::from_millis(500)).is_ok() {
            break;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::SshTunnel(format!(
                "Timed out establishing ssh tunnel via '{alias}': {}",
                collect_stderr(&mut drain)
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok(SshTunnel {
        local_port,
        shutdown: Arc::new(AtomicBool::new(false)),
        child: Some(child),
    })
}

fn accept_loop(
    listener: TcpListener,
    target: Arc<ForwardTarget>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let target = Arc::clone(&target);
                let shutdown = Arc::clone(&shutdown);
                std::thread::spawn(move || {
                    if let Err(e) = forward_connection(stream, &target, shutdown) {
                        eprintln!("[SshTunnel] forwarding error: {e}");
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[SshTunnel] accept error: {e}");
                return;
            }
        }
    }
}

/// Establishes an SSH session and performs authentication.
fn establish_session(config: &SshTunnelConfig) -> Result<Session, AppError> {
    // session.set_timeout only takes effect after the connection is established, so
    // apply the same timeout to the TCP connection itself (to avoid waiting for the OS TCP
    // timeout on a blackholed host)
    let addr = format!("{}:{}", config.host, config.port);
    let socket_addrs: Vec<std::net::SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .map_err(|e| AppError::SshTunnel(format!("Failed to resolve {addr}: {e}")))?
        .collect();
    let mut tcp = None;
    let mut last_error = None;
    for socket_addr in &socket_addrs {
        match TcpStream::connect_timeout(
            socket_addr,
            Duration::from_millis(SSH_TIMEOUT_MS as u64),
        ) {
            Ok(stream) => {
                tcp = Some(stream);
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let tcp = tcp.ok_or_else(|| {
        AppError::SshTunnel(format!(
            "Failed to connect to {addr}: {}",
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no resolvable address".into())
        ))
    })?;

    let mut session = Session::new()
        .map_err(|e| AppError::SshTunnel(format!("Failed to create the SSH session: {e}")))?;
    session.set_timeout(SSH_TIMEOUT_MS);
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|e| AppError::SshTunnel(format!("SSH handshake failed: {e}")))?;

    verify_host_key(&session, config)?;
    authenticate(&session, config)?;
    Ok(session)
}

/// Checks the host key against ~/.ssh/known_hosts.
/// A mismatch is an error since it may be a MITM. An unknown host is appended to known_hosts
/// and allowed (equivalent to OpenSSH's StrictHostKeyChecking=accept-new).
fn verify_host_key(session: &Session, config: &SshTunnelConfig) -> Result<(), AppError> {
    let (key, key_type) = session.host_key().ok_or_else(|| {
        AppError::SshTunnel("Could not obtain the host key".into())
    })?;

    let mut known_hosts = session
        .known_hosts()
        .map_err(|e| AppError::SshTunnel(format!("Failed to initialize known_hosts: {e}")))?;

    let known_hosts_path = dirs::home_dir()
        .ok_or_else(|| AppError::SshTunnel("Could not determine the home directory".into()))?
        .join(".ssh")
        .join("known_hosts");

    if known_hosts_path.exists() {
        known_hosts
            .read_file(&known_hosts_path, ssh2::KnownHostFileKind::OpenSSH)
            .map_err(|e| {
                AppError::SshTunnel(format!(
                    "Failed to read known_hosts ({}): {e}",
                    known_hosts_path.display()
                ))
            })?;
    }

    match known_hosts.check_port(&config.host, config.port, key) {
        ssh2::CheckResult::Match => Ok(()),
        ssh2::CheckResult::Mismatch => Err(AppError::SshTunnel(format!(
            "Host key for {} does not match known_hosts. Connection aborted because this \
             may be a man-in-the-middle attack. If the host key was legitimately changed, \
             remove the corresponding line from known_hosts",
            config.host
        ))),
        ssh2::CheckResult::NotFound => {
            // known_hosts entry format: the host name only for the standard port,
            // [host]:port for a non-standard port
            let entry_host = if config.port == 22 {
                config.host.clone()
            } else {
                format!("[{}]:{}", config.host, config.port)
            };
            known_hosts
                .add(&entry_host, key, "added by queryfolio", key_type.into())
                .map_err(|e| {
                    AppError::SshTunnel(format!("Failed to add the host key to known_hosts: {e}"))
                })?;
            if let Some(parent) = known_hosts_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            known_hosts
                .write_file(&known_hosts_path, ssh2::KnownHostFileKind::OpenSSH)
                .map_err(|e| {
                    AppError::SshTunnel(format!(
                        "Failed to write known_hosts ({}): {e}",
                        known_hosts_path.display()
                    ))
                })?;
            Ok(())
        }
        ssh2::CheckResult::Failure => Err(AppError::SshTunnel(
            "Host key verification failed".into(),
        )),
    }
}

fn authenticate(session: &Session, config: &SshTunnelConfig) -> Result<(), AppError> {
    if let Some(key_path) = &config.private_key_path {
        let key_path: PathBuf = expand_tilde(key_path);
        session
            .userauth_pubkey_file(
                &config.user,
                None,
                &key_path,
                config.private_key_passphrase.as_deref(),
            )
            .map_err(|e| {
                AppError::SshTunnel(format!(
                    "Private key authentication failed ({}): {e}",
                    key_path.display()
                ))
            })?;
        return Ok(());
    }

    if let Some(password) = &config.password {
        session
            .userauth_password(&config.user, password)
            .map_err(|e| AppError::SshTunnel(format!("Password authentication failed: {e}")))?;
        return Ok(());
    }

    // If there is neither a password nor private_key_path, try ssh-agent.
    // A GUI launch may not inherit the shell's SSH_AUTH_SOCK, so resolve the agent socket to use in the order
    // config identity_agent -> IdentityAgent in ~/.ssh/config -> SSH_AUTH_SOCK.
    match resolve_agent_socket(config) {
        AgentSocket::Disabled => Err(AppError::SshTunnel(format!(
            "The SSH agent is disabled for {} (IdentityAgent none) and no \
             private_key_path/password is configured",
            config.host
        ))),
        AgentSocket::Path(path) => authenticate_with_agent(session, &config.user, Some(&path)),
        AgentSocket::Default => authenticate_with_agent(session, &config.user, None),
    }
}

/// Which ssh-agent socket to use.
enum AgentSocket {
    /// An explicit socket path (specified via libssh2's set_identity_path).
    Path(PathBuf),
    /// Equivalent to IdentityAgent none. Do not perform agent authentication.
    Disabled,
    /// Could not be resolved. Leave it to libssh2's default (SSH_AUTH_SOCK).
    Default,
}

/// Resolves the ssh-agent socket to use according to priority.
///
/// 1. `ssh_tunnel.identity_agent` in the config
/// 2. `IdentityAgent` in `~/.ssh/config` (the one matching the target host)
/// 3. `SSH_AUTH_SOCK` (= `AgentSocket::Default`)
fn resolve_agent_socket(config: &SshTunnelConfig) -> AgentSocket {
    if let Some(explicit) = &config.identity_agent {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            // An empty string is treated as unspecified and falls back to ssh_config.
            let home = dirs::home_dir().unwrap_or_default();
            return parse_agent_socket_value(trimmed, &home);
        }
    }
    ssh_config_identity_agent(&config.host).unwrap_or(AgentSocket::Default)
}

/// Converts an IdentityAgent value (config field or ssh_config) into an AgentSocket.
/// `none` disables, `SSH_AUTH_SOCK` means the default (env), anything else is expanded as a path.
fn parse_agent_socket_value(value: &str, home: &Path) -> AgentSocket {
    if value.eq_ignore_ascii_case("none") {
        AgentSocket::Disabled
    } else if value.eq_ignore_ascii_case("SSH_AUTH_SOCK") {
        AgentSocket::Default
    } else {
        AgentSocket::Path(expand_ssh_path(value, home))
    }
}

/// Authenticates via ssh-agent. If socket is Some, uses that unix socket.
/// libssh2's set_identity_path does not modify the SSH_AUTH_SOCK environment variable,
/// so it is safe to call across threads.
fn authenticate_with_agent(
    session: &Session,
    user: &str,
    socket: Option<&Path>,
) -> Result<(), AppError> {
    let mut agent = session
        .agent()
        .map_err(|e| AppError::SshTunnel(format!("Failed to initialize the ssh-agent: {e}")))?;
    if let Some(path) = socket {
        agent.set_identity_path(path).map_err(|e| {
            AppError::SshTunnel(format!(
                "Failed to select the ssh-agent socket ({}): {e}",
                path.display()
            ))
        })?;
    }
    agent
        .connect()
        .map_err(|e| AppError::SshTunnel(format!("Failed to connect to the ssh-agent: {e}")))?;
    agent
        .list_identities()
        .map_err(|e| AppError::SshTunnel(format!("Failed to list ssh-agent identities: {e}")))?;
    let identities = agent
        .identities()
        .map_err(|e| AppError::SshTunnel(format!("Failed to read ssh-agent identities: {e}")))?;
    if identities.is_empty() {
        let location = socket
            .map(|p| format!(" at {}", p.display()))
            .unwrap_or_default();
        return Err(AppError::SshTunnel(format!(
            "No identities found in the ssh agent{location}. Ensure your SSH agent \
             (e.g. 1Password) is unlocked and holds a key, or set \
             ssh_tunnel.private_key_path / ssh_tunnel.identity_agent"
        )));
    }
    let mut last_error = None;
    for identity in &identities {
        match agent.userauth(user, identity) {
            Ok(()) => return Ok(()),
            Err(e) => last_error = Some(e),
        }
    }
    Err(AppError::SshTunnel(format!(
        "ssh-agent authentication failed for user {user}: {}",
        last_error
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no identity was accepted".into())
    )))
}

/// Reads `~/.ssh/config` and returns the effective IdentityAgent for `host`.
/// Following OpenSSH semantics, the first matching value is used.
/// Honors Host glob patterns, `IdentityAgent none` / `SSH_AUTH_SOCK`, and `~` / `%d` / environment variable expansion.
/// (Match blocks are unsupported; settings inside them are ignored.)
fn ssh_config_identity_agent(host: &str) -> Option<AgentSocket> {
    let home = dirs::home_dir()?;
    let config_path = home.join(".ssh").join("config");
    ssh_config_identity_agent_at(&config_path, host, &home)
}

/// The body of `ssh_config_identity_agent`. Made testable by allowing the config path and
/// home to be injected.
fn ssh_config_identity_agent_at(
    config_path: &Path,
    host: &str,
    home: &Path,
) -> Option<AgentSocket> {
    let mut result = None;
    let mut depth = 0;
    parse_ssh_config_file(config_path, host, home, &mut result, &mut depth);
    result
}

/// Parses one ssh_config file and writes the first matching IdentityAgent into `result`.
///
/// Faithful to OpenSSH semantics (verified with `ssh -G`):
/// - Settings outside Host/Match blocks (at the top) apply to all hosts.
/// - `Include` is expanded **only when the enclosing block matches**. An Include inside a
///   non-matching Host block is not applied, even if the included file has its own `Host *`
///   (verified in Case A).
/// - The Host context of an included file does not leak to the parent after returning (achieved naturally by recursion-local variables).
/// - Match blocks are unsupported; settings inside them are ignored.
fn parse_ssh_config_file(
    path: &Path,
    host: &str,
    home: &Path,
    result: &mut Option<AgentSocket>,
    depth: &mut u32,
) {
    if result.is_some() || *depth > 16 {
        return;
    }
    *depth += 1;
    if let Ok(content) = std::fs::read_to_string(path) {
        let mut block_matches = true;
        for raw_line in content.lines() {
            if result.is_some() {
                break;
            }
            let line = strip_inline_comment(raw_line.trim());
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (keyword, rest) = split_ssh_config_line(line);
            match keyword.to_ascii_lowercase().as_str() {
                "host" => block_matches = host_patterns_match(rest, host),
                // Match blocks are unsupported. Ignore the whole block to avoid misjudging.
                "match" => block_matches = false,
                "include" if block_matches => {
                    for inc in expand_include_paths(rest, home) {
                        parse_ssh_config_file(&inc, host, home, result, depth);
                        if result.is_some() {
                            break;
                        }
                    }
                }
                "identityagent" if block_matches => {
                    *result = Some(parse_agent_socket_value(unquote(rest), home));
                }
                _ => {}
            }
        }
    }
    *depth -= 1;
}

/// Strips inline comments following OpenSSH. A `#` starts a comment only when it is outside
/// double quotes and preceded by whitespace (or at the line start).
/// (The `#` in `/a#b.sock` is part of the value, and the `#` in `"/a#b.sock"` is inside quotes so it is kept.
///  Verified with the behavior of `ssh -G`.)
fn strip_inline_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let mut prev_is_ws = true; // the line start counts as "after whitespace"
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes && prev_is_ws => return line[..i].trim_end(),
            _ => {}
        }
        prev_is_ws = ch.is_whitespace();
    }
    line
}

/// Splits one ssh_config line into a "keyword" and the "rest".
/// OpenSSH allows both `Keyword value` and `Keyword=value`.
fn split_ssh_config_line(line: &str) -> (&str, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let keyword = &line[..end];
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').map(str::trim_start).unwrap_or(rest);
    (keyword, rest)
}

/// Whether the pattern list on a Host line (whitespace-separated, `!` negation allowed) matches host.
/// Host names are case-insensitive, following OpenSSH.
fn host_patterns_match(patterns: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let mut matched = false;
    for pattern in patterns.split_whitespace() {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob_match(&negated.to_ascii_lowercase(), &host) {
                return false;
            }
        } else if glob_match(&pattern.to_ascii_lowercase(), &host) {
            matched = true;
        }
    }
    matched
}

/// Wildcard matching that supports only `*` and `?`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Strips surrounding double quotes.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

/// Expands the path in an IdentityAgent value:
/// backslash-escape decoding -> environment variables (`${VAR}`/`$VAR`) -> `%d` (home) -> `~`.
///
/// Connection-dependent percent tokens (`%h`/`%n`/`%r`/`%u`/`%l`/`%C`) are practically unused in an
/// IdentityAgent socket path and full support would need the whole connection context, so unknown
/// `%X` are left as-is (best-effort). When it cannot be resolved, the caller falls back to SSH_AUTH_SOCK.
fn expand_ssh_path(value: &str, home: &Path) -> PathBuf {
    let decoded = decode_ssh_escapes(value);
    let mut expanded = expand_env_vars(&decoded);
    if expanded.contains("%d") {
        expanded = expanded.replace("%d", &home.to_string_lossy());
    }
    if let Some(rest) = expanded.strip_prefix("~/") {
        return home.join(rest);
    }
    if expanded == "~" {
        return home.to_path_buf();
    }
    PathBuf::from(expanded)
}

/// Decodes ssh_config backslash escapes. OpenSSH decodes `\ ` (space) and
/// `\\` (backslash) and leaves any other `\X` as-is (verified with `ssh -G`).
fn decode_ssh_escapes(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let mut lookahead = chars.clone();
            if let Some(next @ (' ' | '\\')) = lookahead.next() {
                out.push(next);
                chars = lookahead; // consume the escaped character
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Expands `${VAR}` and `$VAR` with environment variables. Undefined variables expand to an empty string
/// (following OpenSSH / the shell).
fn expand_env_vars(input: &str) -> String {
    if !input.contains('$') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&nc) = chars.peek() {
            let is_name_char = nc.is_ascii_alphanumeric() || nc == '_';
            if braced {
                if nc == '}' {
                    chars.next();
                    break;
                }
                name.push(nc);
                chars.next();
            } else if is_name_char {
                name.push(nc);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            out.push('$'); // leave a lone `$` etc. as-is
        } else if let Ok(value) = std::env::var(&name) {
            out.push_str(&value);
        }
    }
    out
}

/// Expands a list of Include paths. Relative paths are taken as relative to `~/.ssh/`.
/// Glob patterns (`*` `?` `[...]`) are expanded with the glob crate (OpenSSH follows glob(3)).
/// Token splitting honors double quotes / backslash escapes, so
/// a path with spaces such as `Include /tmp/with\ space/inc.conf` is treated as a single token.
fn expand_include_paths(rest: &str, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for token in split_config_tokens(rest) {
        // split_config_tokens returns tokens with quotes stripped and escapes decoded.
        let resolved = if let Some(tail) = token.strip_prefix("~/") {
            home.join(tail)
        } else if token.starts_with('/') {
            PathBuf::from(&token)
        } else {
            home.join(".ssh").join(&token)
        };
        if token.contains(['*', '?', '[']) {
            // The glob crate returns results in sorted order by default.
            if let Ok(paths) = glob::glob(&resolved.to_string_lossy()) {
                out.extend(paths.flatten());
            }
        } else {
            out.push(resolved);
        }
    }
    out
}

/// Splits an ssh_config argument list into whitespace-separated tokens.
/// Interprets double quotes and backslash escapes (`\ ` -> space / `\\` -> `\`),
/// and does not split a token at quoted/escaped spaces.
fn split_config_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_quotes = false;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            '\\' if !in_quotes => {
                has_token = true;
                match chars.peek() {
                    Some(&next @ (' ' | '\\')) => {
                        current.push(next);
                        chars.next();
                    }
                    _ => current.push('\\'),
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(current);
    }
    tokens
}

/// Relays one local TCP connection to an SSH direct-tcpip channel.
///
/// libssh2 is not safe for concurrent operations on the same session, so
/// make the session non-blocking and pump both directions from a single thread.
fn forward_connection(
    tcp: TcpStream,
    target: &ForwardTarget,
    shutdown: Arc<AtomicBool>,
) -> Result<(), AppError> {
    let session = establish_session(&target.ssh_config)?;
    let mut channel = session
        .channel_direct_tcpip(&target.target_host, target.target_port, None)
        .map_err(|e| {
            AppError::SshTunnel(format!(
                "Failed to open a direct-tcpip channel ({}:{}): {e}",
                target.target_host, target.target_port
            ))
        })?;

    tcp.set_nonblocking(true)?;
    session.set_blocking(false);

    let mut tcp = tcp;
    let mut buf = [0u8; 16 * 1024];
    let mut tcp_eof = false;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let mut idle = true;

        if !tcp_eof {
            match tcp.read(&mut buf) {
                Ok(0) => {
                    tcp_eof = true;
                    let _ = write_all_nonblocking_channel_eof(&mut channel);
                }
                Ok(n) => {
                    idle = false;
                    write_all_nonblocking(
                        |data| channel.write(data),
                        &buf[..n],
                    )?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }

        match channel.read(&mut buf) {
            Ok(0) => {
                // EOF on the channel side: end the relay
                break;
            }
            Ok(n) => {
                idle = false;
                write_all_nonblocking(|data| tcp.write(data), &buf[..n])?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }

        if tcp_eof && channel.eof() {
            break;
        }
        if idle {
            std::thread::sleep(PUMP_IDLE_SLEEP);
        }
    }
    Ok(())
}

/// Writes all bytes to a non-blocking writer. Retries on WouldBlock.
fn write_all_nonblocking<W>(mut write: W, data: &[u8]) -> Result<(), std::io::Error>
where
    W: FnMut(&[u8]) -> Result<usize, std::io::Error>,
{
    let mut offset = 0;
    while offset < data.len() {
        match write(&data[offset..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "The write target was closed",
                ));
            }
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(PUMP_IDLE_SLEEP);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Sends EOF on a non-blocking channel. Retries on WouldBlock.
fn write_all_nonblocking_channel_eof(
    channel: &mut ssh2::Channel,
) -> Result<(), std::io::Error> {
    loop {
        match channel.send_eof() {
            Ok(()) => return Ok(()),
            Err(e) => {
                let io_err: std::io::Error = e.into();
                if io_err.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(PUMP_IDLE_SLEEP);
                    continue;
                }
                return Err(io_err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_basic() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("app01.*", "app01.example.com"));
        assert!(!glob_match("app01.*", "app02.example.com"));
        assert!(glob_match("host?", "host1"));
        assert!(!glob_match("host?", "host12"));
        assert!(glob_match("a*c", "abbbc"));
        assert!(!glob_match("a*c", "abbbd"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exacts"));
    }

    #[test]
    fn host_patterns_match_with_negation() {
        assert!(host_patterns_match("*", "db.example.com"));
        assert!(host_patterns_match("*.example.com", "db.example.com"));
        // A negated pattern takes priority and cancels the match
        assert!(!host_patterns_match("*.example.com !secret.example.com", "secret.example.com"));
        assert!(host_patterns_match("*.example.com !secret.example.com", "db.example.com"));
        assert!(!host_patterns_match("foo bar", "baz"));
    }

    #[test]
    fn split_line_handles_space_and_equals() {
        assert_eq!(split_ssh_config_line("IdentityAgent none"), ("IdentityAgent", "none"));
        assert_eq!(split_ssh_config_line("Host=foo"), ("Host", "foo"));
        assert_eq!(
            split_ssh_config_line("IdentityAgent  \"~/a b\""),
            ("IdentityAgent", "\"~/a b\"")
        );
    }

    #[test]
    fn unquote_and_expand() {
        assert_eq!(unquote("\"~/x\""), "~/x");
        assert_eq!(unquote("plain"), "plain");
        let home = Path::new("/home/u");
        assert_eq!(expand_ssh_path("~/a/b", home), PathBuf::from("/home/u/a/b"));
        assert_eq!(expand_ssh_path("%d/a", home), PathBuf::from("/home/u/a"));
        assert_eq!(expand_ssh_path("/abs/path", home), PathBuf::from("/abs/path"));
    }

    fn unique_temp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("qf_{tag}_{}_{n}", std::process::id()))
    }

    fn resolve(config_body: &str, host: &str, home: &Path) -> Option<AgentSocket> {
        let path = unique_temp("ssh_cfg");
        std::fs::write(&path, config_body).unwrap();
        let result = ssh_config_identity_agent_at(&path, host, home);
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn resolves_host_star_identity_agent() {
        let home = Path::new("/home/u");
        let body = "Host *\n  IdentityAgent \"~/Library/1p/agent.sock\"\n";
        match resolve(body, "db.example.com", home) {
            Some(AgentSocket::Path(p)) => {
                assert_eq!(p, PathBuf::from("/home/u/Library/1p/agent.sock"));
            }
            other => panic!("expected Path, got resolved={}", other.is_some()),
        }
    }

    #[test]
    fn first_matching_value_wins_and_none_disables() {
        let home = Path::new("/home/u");
        // A more specific Host block comes first and specifies none, so it takes priority over the later Host *.
        let body = "Host irene.example.com\n  IdentityAgent none\n\nHost *\n  IdentityAgent ~/agent.sock\n";
        assert!(matches!(
            resolve(body, "irene.example.com", home),
            Some(AgentSocket::Disabled)
        ));
        // A host that does not match falls back to Host *.
        assert!(matches!(
            resolve(body, "other.example.com", home),
            Some(AgentSocket::Path(_))
        ));
    }

    #[test]
    fn ssh_auth_sock_literal_maps_to_default() {
        let home = Path::new("/home/u");
        let body = "Host *\n  IdentityAgent SSH_AUTH_SOCK\n";
        assert!(matches!(
            resolve(body, "db.example.com", home),
            Some(AgentSocket::Default)
        ));
    }

    #[test]
    fn no_identity_agent_returns_none() {
        let home = Path::new("/home/u");
        let body = "Host *\n  User someone\n";
        assert!(resolve(body, "db.example.com", home).is_none());
    }

    #[test]
    fn include_inside_non_matching_host_block_is_ignored() {
        // When an Include is placed inside a Host block, the IdentityAgent of the included file
        // must not apply to hosts that do not match that block (OpenSSH inline expansion).
        let home = Path::new("/home/u");
        let inc = unique_temp("ssh_inc");
        std::fs::write(&inc, "IdentityAgent none\n").unwrap();
        let body = format!(
            "Host prod\n  Include {}\n\nHost *\n  IdentityAgent ~/agent.sock\n",
            inc.display()
        );
        // dev does not match Host prod, so it gets agent.sock from Host *.
        assert!(matches!(
            resolve(&body, "dev", home),
            Some(AgentSocket::Path(_))
        ));
        // prod matches Host prod, so it gets none from the included file.
        assert!(matches!(
            resolve(&body, "prod", home),
            Some(AgentSocket::Disabled)
        ));
        let _ = std::fs::remove_file(&inc);
    }

    #[test]
    fn include_in_nonmatching_block_is_not_expanded_even_with_own_host() {
        // When an Include is inside a non-matching Host block, it must not be expanded even if the included file has
        // its own `Host *` (actual OpenSSH behavior, verified with ssh -G Case A).
        let home = Path::new("/home/u");
        let inc = unique_temp("ssh_inc_own_host");
        std::fs::write(&inc, "Host *\n  IdentityAgent ~/from-include.sock\n").unwrap();
        let body = format!(
            "Host prod\n  Include {}\nHost *\n  IdentityAgent ~/main-star.sock\n",
            inc.display()
        );
        // dev does not match Host prod -> ignore the include and get main's Host *.
        match resolve(&body, "dev", home) {
            Some(AgentSocket::Path(p)) => assert_eq!(p, PathBuf::from("/home/u/main-star.sock")),
            other => panic!("expected main-star, resolved={}", other.is_some()),
        }
        // prod matches Host prod -> expand the include and its Host * wins.
        match resolve(&body, "prod", home) {
            Some(AgentSocket::Path(p)) => assert_eq!(p, PathBuf::from("/home/u/from-include.sock")),
            other => panic!("expected from-include, resolved={}", other.is_some()),
        }
        let _ = std::fs::remove_file(&inc);
    }

    #[test]
    fn split_config_tokens_respects_quotes_and_escapes() {
        assert_eq!(split_config_tokens("a b c"), vec!["a", "b", "c"]);
        assert_eq!(split_config_tokens("/tmp/with\\ space/x"), vec!["/tmp/with space/x"]);
        assert_eq!(split_config_tokens("\"/tmp/a b\" /tmp/c"), vec!["/tmp/a b", "/tmp/c"]);
        assert_eq!(split_config_tokens("a\\\\b"), vec!["a\\b"]);
    }

    #[test]
    fn include_path_with_escaped_space() {
        let home = Path::new("/home/u");
        let dir = unique_temp("ssh incdir with space");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inc.conf"), "Host *\n  IdentityAgent ~/spaced.sock\n").unwrap();
        // A space in the directory name. Include it with a backslash escape.
        let escaped = dir.to_string_lossy().replace(' ', "\\ ");
        let body = format!("Include {}/inc.conf\n", escaped);
        match resolve(&body, "any.host", home) {
            Some(AgentSocket::Path(p)) => assert_eq!(p, PathBuf::from("/home/u/spaced.sock")),
            other => panic!("expected spaced include to resolve, resolved={}", other.is_some()),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_var_is_expanded_in_path() {
        assert_eq!(expand_env_vars("plain"), "plain");
        std::env::set_var("QF_TEST_AGENT_DIR", "/tmp/qf");
        assert_eq!(expand_env_vars("${QF_TEST_AGENT_DIR}/a.sock"), "/tmp/qf/a.sock");
        assert_eq!(expand_env_vars("$QF_TEST_AGENT_DIR/a.sock"), "/tmp/qf/a.sock");
        // An undefined variable expands to empty
        std::env::remove_var("QF_TEST_UNDEFINED");
        assert_eq!(expand_env_vars("x${QF_TEST_UNDEFINED}y"), "xy");
    }

    #[test]
    fn strips_inline_comments_like_openssh() {
        // A `#` outside quotes and before whitespace is a comment.
        assert_eq!(strip_inline_comment("IdentityAgent none # disable"), "IdentityAgent none");
        // A `#` in the middle of a token is part of the value.
        assert_eq!(strip_inline_comment("IdentityAgent /a#b.sock"), "IdentityAgent /a#b.sock");
        // A `#` inside quotes is kept.
        assert_eq!(
            strip_inline_comment("IdentityAgent \"/a#b.sock\" # c"),
            "IdentityAgent \"/a#b.sock\""
        );
        assert_eq!(strip_inline_comment("# whole line"), "");
    }

    #[test]
    fn inline_comment_on_identity_agent_is_ignored() {
        let home = Path::new("/home/u");
        assert!(matches!(
            resolve("Host *\n  IdentityAgent none # disable\n", "db", home),
            Some(AgentSocket::Disabled)
        ));
        match resolve("Host *\n  IdentityAgent ~/a.sock # use this\n", "db", home) {
            Some(AgentSocket::Path(p)) => assert_eq!(p, PathBuf::from("/home/u/a.sock")),
            other => panic!("expected path, resolved={}", other.is_some()),
        }
    }

    #[test]
    fn decodes_backslash_escapes_like_openssh() {
        // OpenSSH decodes only `\ ` and `\\` and leaves other `\X` (verified with ssh -G).
        assert_eq!(decode_ssh_escapes("a\\ b"), "a b");
        assert_eq!(decode_ssh_escapes("a\\\\b"), "a\\b");
        assert_eq!(decode_ssh_escapes("plain\\tab"), "plain\\tab");
        assert_eq!(decode_ssh_escapes("noescape"), "noescape");
    }

    #[test]
    fn escaped_space_in_identity_agent_path() {
        let home = Path::new("/home/u");
        // A 1Password-style path with unquoted escaped spaces.
        match resolve(
            "Host *\n  IdentityAgent ~/Library/Group\\ Containers/x/agent.sock\n",
            "db",
            home,
        ) {
            Some(AgentSocket::Path(p)) => {
                assert_eq!(p, PathBuf::from("/home/u/Library/Group Containers/x/agent.sock"))
            }
            other => panic!("expected path with space, resolved={}", other.is_some()),
        }
    }

    #[test]
    fn host_matching_is_case_insensitive() {
        let home = Path::new("/home/u");
        let body = "Host DB.Example.COM\n  IdentityAgent ~/agent.sock\n";
        assert!(matches!(
            resolve(body, "db.example.com", home),
            Some(AgentSocket::Path(_))
        ));
    }

    #[test]
    fn glob_include_expands_star_and_bracket() {
        let home = Path::new("/home/u");
        let dir = unique_temp("ssh_incdir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("10-agent.conf"), "Host *\n  IdentityAgent ~/from-glob.sock\n").unwrap();
        // `*` glob
        let body = format!("Include {}/*.conf\n", dir.display());
        match resolve(&body, "any.host", home) {
            Some(AgentSocket::Path(p)) => assert_eq!(p, PathBuf::from("/home/u/from-glob.sock")),
            other => panic!("expected star-glob path, resolved={}", other.is_some()),
        }
        // `[...]` bracket glob (OpenSSH supports glob(3) character classes)
        let body = format!("Include {}/[0-9]*.conf\n", dir.display());
        match resolve(&body, "any.host", home) {
            Some(AgentSocket::Path(p)) => assert_eq!(p, PathBuf::from("/home/u/from-glob.sock")),
            other => panic!("expected bracket-glob path, resolved={}", other.is_some()),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_config_field_takes_priority() {
        let cfg = SshTunnelConfig {
            host: "db.example.com".into(),
            port: 22,
            user: "u".into(),
            ssh_config: None,
            password: None,
            private_key_path: None,
            private_key_passphrase: None,
            identity_agent: Some("none".into()),
        };
        assert!(matches!(resolve_agent_socket(&cfg), AgentSocket::Disabled));

        let cfg = SshTunnelConfig {
            identity_agent: Some("~/custom.sock".into()),
            ..cfg
        };
        assert!(matches!(resolve_agent_socket(&cfg), AgentSocket::Path(_)));
    }

    /// ssh_config alone must deserialize without host / user.
    #[test]
    fn ssh_config_only_deserializes_without_host_user() {
        let cfg: SshTunnelConfig =
            serde_yaml::from_str("ssh_config: pop-three-ec2-staging\n").unwrap();
        assert_eq!(cfg.ssh_config.as_deref(), Some("pop-three-ec2-staging"));
        assert!(cfg.host.is_empty());
        assert!(cfg.user.is_empty());
    }

    /// An empty ssh_config must return an error without spawning.
    #[test]
    fn start_rejects_empty_ssh_config() {
        let cfg = SshTunnelConfig {
            host: String::new(),
            port: 22,
            user: String::new(),
            ssh_config: Some("   ".into()),
            password: None,
            private_key_path: None,
            private_key_passphrase: None,
            identity_agent: None,
        };
        assert!(matches!(
            SshTunnel::start(&cfg, "localhost", 5432),
            Err(AppError::Config(_))
        ));
    }

    /// An ssh_config starting with `-` would be interpreted as an ssh option, so
    /// it must return a config error without spawning (regression test for argument injection from fetched YAML).
    #[test]
    fn start_rejects_ssh_config_starting_with_dash() {
        for alias in ["-oProxyCommand=touch /tmp/queryfolio-pwned", "  -F/dev/null"] {
            let cfg = SshTunnelConfig {
                host: String::new(),
                port: 22,
                user: String::new(),
                ssh_config: Some(alias.into()),
                password: None,
                private_key_path: None,
                private_key_passphrase: None,
                identity_agent: None,
            };
            match SshTunnel::start(&cfg, "localhost", 5432) {
                Err(AppError::Config(msg)) => assert!(msg.contains("must not start with '-'")),
                Err(e) => panic!("expected AppError::Config for {alias:?}, got {e}"),
                Ok(_) => panic!("expected AppError::Config for {alias:?}, got Ok"),
            }
        }
    }

    /// With neither ssh_config nor host, an error must be returned (host is required on the libssh2 path).
    #[test]
    fn start_requires_host_without_ssh_config() {
        let cfg = SshTunnelConfig {
            host: String::new(),
            port: 22,
            user: "u".into(),
            ssh_config: None,
            password: None,
            private_key_path: None,
            private_key_passphrase: None,
            identity_agent: None,
        };
        assert!(matches!(
            SshTunnel::start(&cfg, "localhost", 5432),
            Err(AppError::Config(_))
        ));
    }

    /// With no ssh_config and an empty user, an error must be returned (user is required on the libssh2 path).
    /// Even if serde default makes it "", do not proceed to userauth.
    #[test]
    fn start_requires_user_without_ssh_config() {
        let cfg = SshTunnelConfig {
            host: "db.example.com".into(),
            port: 22,
            user: String::new(),
            ssh_config: None,
            password: None,
            private_key_path: None,
            private_key_passphrase: None,
            identity_agent: None,
        };
        assert!(matches!(
            SshTunnel::start(&cfg, "localhost", 5432),
            Err(AppError::Config(_))
        ));
    }
}
