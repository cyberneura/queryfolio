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

/// SSH の blocking 操作 (handshake / auth 等) のタイムアウト (ミリ秒)。
const SSH_TIMEOUT_MS: u32 = 30_000;

/// 非同期ポンプループのアイドル時スリープ。
const PUMP_IDLE_SLEEP: Duration = Duration::from_millis(5);

/// SSH ローカルポートフォワードトンネル。
///
/// 127.0.0.1 の空きポートで listen し、接続を受けるたびに新しい SSH セッションを
/// 確立して direct-tcpip チャンネルで転送先へ中継する。
/// libssh2 のセッションはスレッド間の同時操作が安全でないため、
/// 転送 1 本ごとに独立したセッションを持たせる (sqlx のプールは少数接続なので
/// セッション確立のオーバーヘッドは許容範囲)。
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
    /// トンネルを開始する。認証エラーを早期に検出するため、
    /// 最初に 1 度セッション確立を試してから listener を立てる。
    pub fn start(
        ssh_config: &SshTunnelConfig,
        target_host: &str,
        target_port: u16,
    ) -> Result<Self, AppError> {
        // ssh_tunnel.ssh_config が設定されていれば system ssh に委譲する
        // (ProxyJump / 多段トンネル / ~/.ssh/config 解決のため)。
        if let Some(alias) = ssh_config.ssh_config.as_deref() {
            let alias = alias.trim();
            if alias.is_empty() {
                return Err(AppError::Config(
                    "ssh_tunnel.ssh_config must not be empty".into(),
                ));
            }
            // `-` 始まりのエイリアスは ssh にオプションとして解釈される
            // (`-oProxyCommand=...` で任意コマンド実行になる)。ssh_config は
            // config_override_command が取得する YAML からも設定できるため、
            // 取得 YAML にコマンドを実行させないという境界を守るために弾く。
            // argv 側でも `--` で区切っている (start_system_ssh 参照)。
            if alias.starts_with('-') {
                return Err(AppError::Config(
                    "ssh_tunnel.ssh_config must not start with '-'".into(),
                ));
            }
            return start_system_ssh(alias, target_host, target_port);
        }

        // libssh2 経路: host / user は必須 (ssh_config 委譲時のみ省略可)。
        // serde default で空文字になり得るため明示的に弾く。空のまま進むと
        // userauth で分かりにくい認証失敗になり、設定漏れだと気付けない。
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

        // 認証情報の検証を兼ねた接続テスト
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

/// system の `ssh` クライアントで `-N -L` ローカルフォワードトンネルを張る。
///
/// `ssh_tunnel.ssh_config` (=~/.ssh/config の Host エイリアス) 指定時に使う。
/// HostName / User / Port / ProxyJump / 多段トンネルの解決は OpenSSH と
/// ~/.ssh/config に委譲する (libssh2 経路は使わない)。認証・ホスト鍵検証も
/// OpenSSH 任せ。BatchMode=yes でパスワード/パスフレーズ/ホスト鍵確認の
/// 対話プロンプトを禁じ (GUI 起動で TTY が無く固まるのを防ぐ)、agent 認証
/// (1Password 等) は agent 側で処理されるため影響しない。
fn start_system_ssh(
    alias: &str,
    target_host: &str,
    target_port: u16,
) -> Result<SshTunnel, AppError> {
    // ローカルの空きポートを自分で確保してから ssh に渡す。
    // (ssh -L の port 0 動的割当は割当ポートの読み取りが面倒なため)
    let local_port = {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        // listener を閉じてから ssh に bind させる (わずかな race はあるが許容)
        drop(listener);
        port
    };

    // IPv6 リテラル (`::1` / `fd00::10` 等) の転送先は `-L` 仕様上ブラケットで
    // 囲む必要がある (`[::1]`)。囲まないと `...:::1:...` となり OpenSSH が不正な
    // local-forward として拒否する。ホスト名 / IPv4 は `:` を含まないので素通し。
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
        // これ以降をオプションとして解釈させない (エイリアスによる引数注入の防止)
        .arg("--")
        .arg(alias)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("PATH", path)
        .spawn()
        .map_err(|e| AppError::SshTunnel(format!("Failed to launch ssh: {e}")))?;

    // stderr は別スレッドで吸い出す (パイプ詰まりで ssh が止まるのを防ぎ、
    // 失敗時の診断メッセージも拾えるようにする)。
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

    // 失敗時の診断メッセージを組む前に drain スレッドの完了を待つ。
    // ssh が即終了すると try_wait() が drain スレッドの書き込みより先に exit を
    // 観測し得るため、join せずに読むとメッセージが空になる (診断が最も要る場面で)。
    // 呼び出し時点で child は既に終了 (= stderr が EOF) しているので join は即返る。
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

    // ローカルのフォワードポートが接続を受け付けるまで待つ。
    // OpenSSH は接続先への認証・フォワード確立に成功して初めてローカルの
    // listen ソケットを開く (ExitOnForwardFailure=yes で失敗時は即終了する)
    // ため、接続できた時点で認証成功とみなせる (libssh2 経路の
    // establish_session による事前検証と同じ役割)。
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

/// SSH セッションを確立して認証まで行う。
fn establish_session(config: &SshTunnelConfig) -> Result<Session, AppError> {
    // session.set_timeout は接続確立後にしか効かないため、
    // TCP 接続自体にも同じタイムアウトを適用する (ブラックホール化した
    // ホストで OS の TCP タイムアウトまで待たされるのを防ぐ)
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

/// ホストキーを ~/.ssh/known_hosts と照合する。
/// 不一致は MITM の可能性があるためエラー。未登録ホストは known_hosts に
/// 追記して許可する (OpenSSH の StrictHostKeyChecking=accept-new 相当)。
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
            // known_hosts のエントリ形式: 標準ポートはホスト名のみ、
            // 非標準ポートは [host]:port
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

    // password も private_key_path も無い場合は ssh-agent を試す。
    // GUI 起動時はシェルの SSH_AUTH_SOCK を継承しないことがあるため、使う agent socket を
    // config の identity_agent → ~/.ssh/config の IdentityAgent → SSH_AUTH_SOCK の順で解決する。
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

/// どの ssh-agent socket を使うか。
enum AgentSocket {
    /// 明示的な socket パス (libssh2 の set_identity_path で指定)。
    Path(PathBuf),
    /// IdentityAgent none 相当。agent 認証を行わない。
    Disabled,
    /// 解決できなかった。libssh2 の既定 (SSH_AUTH_SOCK) に任せる。
    Default,
}

/// 使用する ssh-agent socket を優先順位に従って解決する。
///
/// 1. config の `ssh_tunnel.identity_agent`
/// 2. `~/.ssh/config` の `IdentityAgent` (対象ホストにマッチするもの)
/// 3. `SSH_AUTH_SOCK` (= `AgentSocket::Default`)
fn resolve_agent_socket(config: &SshTunnelConfig) -> AgentSocket {
    if let Some(explicit) = &config.identity_agent {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            // 空文字は未指定扱いとし、ssh_config へフォールバックする。
            let home = dirs::home_dir().unwrap_or_default();
            return parse_agent_socket_value(trimmed, &home);
        }
    }
    ssh_config_identity_agent(&config.host).unwrap_or(AgentSocket::Default)
}

/// IdentityAgent の値 (config フィールドまたは ssh_config) を AgentSocket に変換する。
/// `none` は無効化、`SSH_AUTH_SOCK` は既定 (env)、それ以外はパスとして展開する。
fn parse_agent_socket_value(value: &str, home: &Path) -> AgentSocket {
    if value.eq_ignore_ascii_case("none") {
        AgentSocket::Disabled
    } else if value.eq_ignore_ascii_case("SSH_AUTH_SOCK") {
        AgentSocket::Default
    } else {
        AgentSocket::Path(expand_ssh_path(value, home))
    }
}

/// ssh-agent 経由で認証する。socket が Some ならその unix socket を使う。
/// libssh2 の set_identity_path は SSH_AUTH_SOCK 環境変数を書き換えないので
/// スレッド間で安全に呼べる。
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

/// `~/.ssh/config` を読み、`host` に対する実効的な IdentityAgent を返す。
/// OpenSSH のセマンティクスに倣い、最初にマッチした値を採用する。
/// Host の glob パターン・`IdentityAgent none`/`SSH_AUTH_SOCK`・`~`/`%d`/環境変数展開を尊重する。
/// (Match ブロックは未対応で、その中の設定は無視する。)
fn ssh_config_identity_agent(host: &str) -> Option<AgentSocket> {
    let home = dirs::home_dir()?;
    let config_path = home.join(".ssh").join("config");
    ssh_config_identity_agent_at(&config_path, host, &home)
}

/// `ssh_config_identity_agent` の本体。config パスと home を注入できるようにして
/// テスト可能にしたもの。
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

/// ssh_config を 1 ファイル解析し、最初にマッチした IdentityAgent を `result` に書き込む。
///
/// OpenSSH のセマンティクス (`ssh -G` で確認) に忠実に:
/// - Host/Match ブロックの外 (冒頭) の設定は全ホストに適用される。
/// - `Include` は **enclosing block がマッチする時だけ** 展開する。マッチしない
///   Host ブロック内の Include は、たとえ include 先が自前の `Host *` を持っていても
///   適用しない (Case A で確認)。
/// - include 先の Host コンテキストは return 後の親には波及しない (再帰の局所変数で自然に実現)。
/// - Match ブロックは未対応で、その中の設定は無視する。
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
                // Match ブロックは未対応。誤判定を避けるためブロックごと無視する。
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

/// OpenSSH に倣い行内コメントを除去する。`#` はダブルクオートの外側かつ
/// 直前が空白 (または行頭) のときだけコメント開始とみなす。
/// (`/a#b.sock` の `#` は値の一部、`"/a#b.sock"` の `#` はクオート内なので残す。
///  `ssh -G` の挙動で確認済み。)
fn strip_inline_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let mut prev_is_ws = true; // 行頭は「空白の後」とみなす
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

/// ssh_config の 1 行を「キーワード」と「残り」に分割する。
/// OpenSSH は `Keyword value` と `Keyword=value` の両方を許す。
fn split_ssh_config_line(line: &str) -> (&str, &str) {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let keyword = &line[..end];
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').map(str::trim_start).unwrap_or(rest);
    (keyword, rest)
}

/// Host 行のパターン列 (空白区切り、`!` 否定あり) が host にマッチするか。
/// ホスト名は OpenSSH に倣い大文字小文字を区別しない。
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

/// `*` と `?` のみ対応するワイルドカードマッチ。
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

/// 前後のダブルクオートを除去する。
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

/// IdentityAgent の値のパスを展開する:
/// バックスラッシュエスケープ復号 → 環境変数 (`${VAR}`/`$VAR`) → `%d` (ホーム) → `~`。
///
/// 接続依存の percent トークン (`%h`/`%n`/`%r`/`%u`/`%l`/`%C`) は IdentityAgent の
/// socket パスでは実質使われず、完全対応には接続コンテキスト一式が要るため、未知の
/// `%X` はそのまま残す (best-effort)。解決不能時は呼び出し側が SSH_AUTH_SOCK に倒す。
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

/// ssh_config のバックスラッシュエスケープを復号する。OpenSSH は `\ ` (空白) と
/// `\\` (バックスラッシュ) を復号し、その他の `\X` はそのまま残す (`ssh -G` で確認)。
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
                chars = lookahead; // エスケープ対象を消費
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// `${VAR}` と `$VAR` を環境変数で展開する。未定義変数は空文字に展開する
/// (OpenSSH / シェル準拠)。
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
            out.push('$'); // `$` 単独等はそのまま残す
        } else if let Ok(value) = std::env::var(&name) {
            out.push_str(&value);
        }
    }
    out
}

/// Include のパス列を展開する。相対パスは `~/.ssh/` からの相対とみなす。
/// glob パターン (`*` `?` `[...]`) は glob crate で展開する (OpenSSH は glob(3) 準拠)。
/// トークン分割はダブルクオート/バックスラッシュエスケープを尊重するため、
/// `Include /tmp/with\ space/inc.conf` のような空白入りパスも 1 つとして扱う。
fn expand_include_paths(rest: &str, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for token in split_config_tokens(rest) {
        // split_config_tokens はクオート除去・エスケープ復号済みのトークンを返す。
        let resolved = if let Some(tail) = token.strip_prefix("~/") {
            home.join(tail)
        } else if token.starts_with('/') {
            PathBuf::from(&token)
        } else {
            home.join(".ssh").join(&token)
        };
        if token.contains(['*', '?', '[']) {
            // glob crate は既定でソート済みの順で返す。
            if let Ok(paths) = glob::glob(&resolved.to_string_lossy()) {
                out.extend(paths.flatten());
            }
        } else {
            out.push(resolved);
        }
    }
    out
}

/// ssh_config の引数リストを空白区切りのトークンに分割する。
/// ダブルクオートとバックスラッシュエスケープ (`\ `→空白 / `\\`→`\`) を解釈し、
/// クオート/エスケープされた空白ではトークンを分割しない。
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

/// ローカル TCP 接続 1 本を SSH direct-tcpip チャンネルへ中継する。
///
/// libssh2 は同一セッションへの並行操作が安全でないため、
/// セッションを non-blocking にして単一スレッドで双方向にポンプする。
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
                // チャンネル側 EOF: 中継終了
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

/// non-blocking な writer に対して全バイト書き込む。WouldBlock はリトライする。
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

/// non-blocking チャンネルへの EOF 送信。WouldBlock はリトライする。
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
        // 否定パターンが優先してマッチを打ち消す
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
        // より具体的な Host ブロックが先にあり none を指定していれば、後続の Host * より優先される。
        let body = "Host irene.example.com\n  IdentityAgent none\n\nHost *\n  IdentityAgent ~/agent.sock\n";
        assert!(matches!(
            resolve(body, "irene.example.com", home),
            Some(AgentSocket::Disabled)
        ));
        // マッチしないホストは Host * にフォールバックする。
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
        // Include を Host ブロック内に置いた場合、そのブロックがマッチしないホストには
        // Include 先の IdentityAgent を適用してはならない (OpenSSH のインライン展開)。
        let home = Path::new("/home/u");
        let inc = unique_temp("ssh_inc");
        std::fs::write(&inc, "IdentityAgent none\n").unwrap();
        let body = format!(
            "Host prod\n  Include {}\n\nHost *\n  IdentityAgent ~/agent.sock\n",
            inc.display()
        );
        // dev は Host prod にマッチしないので Host * の agent.sock を得る。
        assert!(matches!(
            resolve(&body, "dev", home),
            Some(AgentSocket::Path(_))
        ));
        // prod は Host prod にマッチするので Include 先の none を得る。
        assert!(matches!(
            resolve(&body, "prod", home),
            Some(AgentSocket::Disabled)
        ));
        let _ = std::fs::remove_file(&inc);
    }

    #[test]
    fn include_in_nonmatching_block_is_not_expanded_even_with_own_host() {
        // Include が非マッチ Host ブロック内にある場合、include 先が自前の `Host *` を
        // 持っていても展開してはならない (OpenSSH の実挙動、ssh -G Case A で確認)。
        let home = Path::new("/home/u");
        let inc = unique_temp("ssh_inc_own_host");
        std::fs::write(&inc, "Host *\n  IdentityAgent ~/from-include.sock\n").unwrap();
        let body = format!(
            "Host prod\n  Include {}\nHost *\n  IdentityAgent ~/main-star.sock\n",
            inc.display()
        );
        // dev は Host prod 非マッチ → include を無視し main の Host * を得る。
        match resolve(&body, "dev", home) {
            Some(AgentSocket::Path(p)) => assert_eq!(p, PathBuf::from("/home/u/main-star.sock")),
            other => panic!("expected main-star, resolved={}", other.is_some()),
        }
        // prod は Host prod マッチ → include を展開しその Host * が勝つ。
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
        // ディレクトリ名に空白。バックスラッシュでエスケープして Include。
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
        // 未定義変数は空に展開される
        std::env::remove_var("QF_TEST_UNDEFINED");
        assert_eq!(expand_env_vars("x${QF_TEST_UNDEFINED}y"), "xy");
    }

    #[test]
    fn strips_inline_comments_like_openssh() {
        // クオート外かつ空白前の `#` はコメント。
        assert_eq!(strip_inline_comment("IdentityAgent none # disable"), "IdentityAgent none");
        // トークン途中の `#` は値の一部。
        assert_eq!(strip_inline_comment("IdentityAgent /a#b.sock"), "IdentityAgent /a#b.sock");
        // クオート内の `#` は残す。
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
        // OpenSSH は `\ ` と `\\` のみ復号し、その他の `\X` は残す (ssh -G で確認)。
        assert_eq!(decode_ssh_escapes("a\\ b"), "a b");
        assert_eq!(decode_ssh_escapes("a\\\\b"), "a\\b");
        assert_eq!(decode_ssh_escapes("plain\\tab"), "plain\\tab");
        assert_eq!(decode_ssh_escapes("noescape"), "noescape");
    }

    #[test]
    fn escaped_space_in_identity_agent_path() {
        let home = Path::new("/home/u");
        // クオートなしで空白をエスケープした 1Password 風パス。
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
        // `[...]` bracket glob (OpenSSH は glob(3) の文字クラスに対応)
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

    /// ssh_config だけ書いた設定が host / user 無しでデシリアライズできること。
    #[test]
    fn ssh_config_only_deserializes_without_host_user() {
        let cfg: SshTunnelConfig =
            serde_yaml::from_str("ssh_config: pop-three-ec2-staging\n").unwrap();
        assert_eq!(cfg.ssh_config.as_deref(), Some("pop-three-ec2-staging"));
        assert!(cfg.host.is_empty());
        assert!(cfg.user.is_empty());
    }

    /// ssh_config が空文字なら spawn せずにエラーを返すこと。
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

    /// `-` 始まりの ssh_config は ssh のオプションとして解釈されるため、
    /// spawn せずに設定エラーを返すこと (取得 YAML からの引数注入の回帰テスト)。
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

    /// ssh_config も host も無ければ (libssh2 経路で host 必須) エラーを返すこと。
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

    /// ssh_config 無しで user が空なら (libssh2 経路で user 必須) エラーを返すこと。
    /// serde default で "" になっても userauth まで進ませない。
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
