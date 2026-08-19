//! GUI を起動せずに終わる CLI オプション (`--help` / `--version` / `--list-servers`)。
//!
//! `open` / `write` のサブコマンド ([`crate::router`]) は「アプリを起動して
//! ファイルを開く」ためのものだが、ここで扱うのは**標準出力に書いて終了する**
//! だけのものなので、ルーターとは別に持つ。lib.rs の `run()` が Tauri を組み立てる
//! 前に [`info_command_from_args`] を見て、該当すれば表示して終了する。
//!
//! 表示の組み立ては Tauri にもファイルシステムにも依存しない純粋な関数にして、
//! 単体テストで固める (特に `--list-servers` は**パスワードを出さない**ことが
//! 要件なので、テストで担保する)。

use crate::config::{ConnectionInfo, ServerConfig};
use std::path::Path;

/// GUI を起動せず、標準出力に書いて終わるオプション。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoCommand {
    /// 使い方を表示する。
    Help,
    /// バージョンを表示する。
    Version,
    /// 設定されている接続の一覧を表示する。
    ListServers,
}

/// 起動時引数から [`InfoCommand`] を取り出す。
///
/// **`open` / `write` サブコマンドが先に現れたら `None` を返す。**
/// `write` の第 3 引数 (クエリの内容) に `--help` のような文字列が入っていても
/// それは書き出す中身であって、オプションではないため
/// (`queryfolio write conn a.sql "-- help"` のようなケースを取り違えない)。
///
/// 位置固定ではなく走査にしているのは、macOS が `.app` を起動する時に
/// `-psn_0_12345` のような引数を先頭へ差し込むことがあるため
/// (`router::route_from_cli_args` が同じ理由で走査している)。
pub fn info_command_from_args<S: AsRef<str>>(args: &[S]) -> Option<InfoCommand> {
    for arg in args {
        match arg.as_ref() {
            // サブコマンドが先に来たら、以降は全てその引数なので見ない
            "open" | "write" => return None,
            "--help" | "-h" | "help" => return Some(InfoCommand::Help),
            "--version" | "-V" => return Some(InfoCommand::Version),
            "--list-servers" => return Some(InfoCommand::ListServers),
            _ => {}
        }
    }
    None
}

/// アプリの版番号。
///
/// `build.rs` が `tauri.conf.json` の `version` から埋め込む。
/// **`CARGO_PKG_VERSION` を使ってはいけない** — リリースの版番号は
/// `tauri.conf.json` 側で管理されており、Cargo.toml の version は追随していない
/// (配布物が 0.1.4 でも `--version` が 0.1.0 と答えることになる)。
const APP_VERSION: &str = env!("QUERYFOLIO_VERSION");

/// `--help` で表示する使い方。
pub fn help_text() -> String {
    let version = APP_VERSION;
    format!(
        "QueryFolio {version} - a multi-purpose SQL GUI client

USAGE:
    queryfolio                                       Launch the app
    queryfolio open <path>                           Open a saved query file by path
    queryfolio write <connection> <file> [content]   Write a query file and open it
    queryfolio --list-servers                        List the configured connections
    queryfolio --help                                Show this help
    queryfolio --version                             Show the version

OPEN
    <path> has to be a query file directly under a connection folder in the
    query files directory. Paths outside it are rejected.

WRITE
    <connection> is the connection name in the config, not a folder name.
    The file extension of the connection's engine is added when it is missing,
    and the connection folder is created when it does not exist yet.
    [content] can also be piped in on stdin. When no content is given, an empty
    file is created and an existing file is left as it is.

    echo 'SELECT 1;' | queryfolio write reporting check.sql

    Both subcommands hand the file to the running window when one is open.

CONFIG
    ~/.config/queryfolio/config.yml (config.yaml is used when it is missing).
    QUERYFOLIO_CONFIG_YAML overrides it with the YAML in the variable itself.

On macOS the app bundle takes the same arguments after --args:

    open -a QueryFolio --args open /path/to/query.sql
"
    )
}

/// `--version` で表示する 1 行。
pub fn version_text() -> String {
    format!("QueryFolio {APP_VERSION}")
}

/// Windows で、情報系オプションの出力を呼び出し元の端末へ届ける。
///
/// `main.rs` の `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` により、
/// **Windows の release ビルドは GUI サブシステムとしてリンクされ、プロセスにコンソールが
/// 割り当てられない**。この状態では Rust の std が書き込み先とする
/// `GetStdHandle(STD_OUTPUT_HANDLE)` が無効ハンドルを返し、`print!` の内容が
/// 黙って捨てられる (端末には何も出ないまま終了する)。表示そのものが目的の
/// `--help` / `--version` / `--list-servers` では機能が成立しないので、表示の前に
/// 親プロセス (起動した cmd.exe / PowerShell) のコンソールへ繋ぎ直す。
///
/// 標準出力が既に有効な場合 (パイプやファイルへのリダイレクト、親にコンソールが無い等) は
/// `AttachConsole` が失敗するだけで、元の書き込み先がそのまま使われる。戻り値を見ないのは
/// そのため — ここでの失敗は「今までどおり」であって、報告できることが無い。
/// C ランタイム流の `freopen("CONOUT$")` に相当する処理は要らない
/// (Rust の std は書き込みのたびに `GetStdHandle` を引き直すため)。
///
/// **この経路は実機の Windows では未検証** (開発ホストにも CI にも Windows が無い)。
/// 呼ぶのは情報系オプションの表示直前だけなので、失敗しても現状どおり出力が出ないだけで、
/// GUI 起動の経路には影響しない。
#[cfg(windows)]
pub fn attach_parent_console() {
    // (DWORD)-1 = ATTACH_PARENT_PROCESS
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

    extern "system" {
        fn AttachConsole(dwProcessId: u32) -> i32;
    }

    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Windows 以外では何もしない (常にコンソールへ書ける)。
#[cfg(not(windows))]
pub fn attach_parent_console() {}

/// `--list-servers` の表の列。
const COLUMNS: [&str; 9] = [
    "NAME", "ENGINE", "HOST", "PORT", "USER", "DATABASE", "SSL", "SSH", "FOLDER",
];

/// 値が無い欄の表示。
const EMPTY: &str = "-";

/// 設定が壊れていて TLS の状態を決められない欄の表示。
/// 有効なモード名 (`disable` / `prefer` / ...) とも `on` / `off` とも被らない語にする。
const INVALID: &str = "invalid";

/// 値そのものが資格情報なので伏せた欄の表示。
/// `EMPTY` (未設定) と区別できる語にする — 「AWS のキーを設定していない」と
/// 「設定しているが出していない」は別の話なので、同じ `-` にはしない。
const HIDDEN: &str = "(hidden)";

/// USER 欄に出す値。
///
/// **dynamodb の `user` は AWS のアクセスキー ID で、DB のユーザー名ではない。**
/// 秘密鍵ほどではないが資格情報の片割れの識別子なので、端末やその履歴・
/// ログに残す値ではない。`folder_meta.rs` が同じ理由で `(aws access key, hidden)`
/// に差し替えているのと同じ扱いにする (こちらは表の 1 列なので短い語にする)。
///
/// 他のエンジンの `user` はただのユーザー名なので、そのまま出す
/// (どのアカウントで繋ぐ設定なのかは、この一覧を見る目的そのもの)。
fn user_cell(server: &ServerConfig, info: &ConnectionInfo) -> String {
    match info.user.as_deref() {
        Some(_) if server.engine.eq_ignore_ascii_case("dynamodb") => HIDDEN.to_string(),
        Some(user) => user.to_string(),
        None => EMPTY.to_string(),
    }
}

/// TLS / SSL の状態を 1 語で表す。
///
/// mysql / postgres / redis は実効モード ([`ConnectionInfo::sql_ssl_mode`]) を
/// そのまま出す (`disable` / `prefer` / `require` / `verify-ca` / `verify-full`)。
/// 「`prefer` は暗号化されないことがある」という区別が消えると、この一覧で
/// 接続の安全性を確認できなくなるため、yes / no には丸めない。
///
/// それ以外のエンジン (elasticsearch / dynamodb 等) には実効モードが無いので、
/// 設定の `tls` をそのまま `on` / `off` で出す。
///
/// **`sql_ssl_mode` が `None` でも、そのまま `tls` に落としてはいけない。**
/// `ConnectionInfo::from` は「実効モードを持たないエンジン」だけでなく
/// 「`engine` / `ssl_mode` の値が不正で解決できなかった」場合も `None` にする。
/// 後者を `tls` の `on` / `off` として出すと、**接続時にエラーになる設定を
/// 有効な TLS 設定として見せる**ことになる (`ssl_mode: requre` の書き間違いが
/// `off` = 平文で繋がる、と読める)。この列は接続の安全性を確認するためのものなので、
/// 決められない時は `invalid` と出して隠さない。
fn ssl_summary(server: &ServerConfig, info: &ConnectionInfo) -> String {
    if let Some(mode) = &info.sql_ssl_mode {
        return mode.clone();
    }
    // sql_ssl_mode() が Err になるのは ssl_mode が設定されていて解決できない時だけ
    // (未設定なら tls から既定値を返す) なので、未設定の接続を誤って invalid にはしない
    if server.sql_ssl_mode().is_err() || crate::db::parse_engine(&server.engine).is_err() {
        return INVALID.to_string();
    }
    if server.tls { "on" } else { "off" }.to_string()
}

/// 表のセルに出す前に制御文字を可視表現へ落とす。
///
/// 設定の値は `config.yml` だけでなく `config_override_command` の出力からも来る。
/// 改行やタブが混ざると「1 接続 = 1 行」の形が崩れて他の接続の行を偽装でき、
/// ANSI / OSC のエスケープシーケンスが混ざると端末側で解釈されてしまう。
/// 値の中身を見せることより、行の形が崩れないことを優先する。
fn sanitize_cell(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control() {
                // 見えない文字が「消える」と何が入っていたか分からないので、
                // 落とさずに可視の記号へ置き換える
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// `--list-servers` の本文を組み立てる。
///
/// **パスワード・SSH の鍵やパスフレーズ・AWS のアクセスキー ID は出さない。**
/// 出す項目は [`ConnectionInfo`] (フロントへ渡す「機密を含まない」射影) と
/// フォルダ名だけに限り、[`ServerConfig`] のフィールドを直接読むのは
/// TLS の判定 (`tls`) と USER 欄の伏せ字判定 (`engine`) だけにしている。
/// 項目を増やす時もこの経路を守ること。
///
/// なお [`ConnectionInfo`] は「フロント (自分の画面) に渡してよい」射影であって
/// 「端末に出してよい」射影ではない。`user` のように**エンジンによって意味が
/// 変わるフィールド**があるので、そのまま流さず [`user_cell`] のような
/// 用途別の判断を挟むこと。
pub fn format_server_list(servers: &[ServerConfig], sqlfiles_dir: &Path) -> String {
    let mut out = format!(
        "Query files directory: {}\n",
        sanitize_cell(&sqlfiles_dir.display().to_string())
    );
    if servers.is_empty() {
        out.push_str("\nNo connection is configured.\n");
        return out;
    }

    let rows: Vec<[String; 9]> = servers
        .iter()
        .map(|server| {
            let info = ConnectionInfo::from(server);
            let cells = [
                info.name.clone(),
                info.engine.clone(),
                info.host.clone().unwrap_or_else(|| EMPTY.to_string()),
                info.port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| EMPTY.to_string()),
                user_cell(server, &info),
                info.schema.clone().unwrap_or_else(|| EMPTY.to_string()),
                ssl_summary(server, &info),
                if info.has_ssh_tunnel { "yes" } else { EMPTY }.to_string(),
                server.sqlfiles_folder_name(),
            ];
            cells.map(|cell| sanitize_cell(&cell))
        })
        .collect();

    // 列幅は見出しと値の最大長 (文字数) に合わせる。
    let widths: Vec<usize> = (0..COLUMNS.len())
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain(std::iter::once(COLUMNS[i].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();

    out.push('\n');
    out.push_str(&join_row(&COLUMNS.map(|c| c.to_string()), &widths));
    for row in &rows {
        out.push_str(&join_row(row, &widths));
    }
    out
}

/// 1 行を列幅に合わせて連結する (末尾の余白は落とす)。
fn join_row(cells: &[String; 9], widths: &[usize]) -> String {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        if i + 1 < cells.len() {
            // chars().count() で数えるのは、見出しも値も表示幅ではなく
            // 文字数で揃える割り切り (全角を含む名前では多少ずれる)。
            let pad = widths[i].saturating_sub(cell.chars().count());
            line.push_str(&" ".repeat(pad));
        }
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> ServerConfig {
        serde_yaml::from_str(&format!(
            "name: {name}\nengine: postgres\nhost: db.example.com\nport: 5432\n\
             user: app\nschema: appdb\npassword: s3cret\n"
        ))
        .expect("test fixture should parse")
    }

    #[test]
    fn test_info_command_from_args() {
        assert_eq!(info_command_from_args(&["--help"]), Some(InfoCommand::Help));
        assert_eq!(info_command_from_args(&["-h"]), Some(InfoCommand::Help));
        assert_eq!(info_command_from_args(&["help"]), Some(InfoCommand::Help));
        assert_eq!(
            info_command_from_args(&["--version"]),
            Some(InfoCommand::Version)
        );
        assert_eq!(info_command_from_args(&["-V"]), Some(InfoCommand::Version));
        assert_eq!(
            info_command_from_args(&["--list-servers"]),
            Some(InfoCommand::ListServers)
        );
        // .app 起動で先頭に入る引数があっても拾える
        assert_eq!(
            info_command_from_args(&["-psn_0_12345", "--list-servers"]),
            Some(InfoCommand::ListServers)
        );
        // 引数なしは GUI 起動
        assert_eq!(info_command_from_args::<&str>(&[]), None);
        assert_eq!(info_command_from_args(&["--unknown"]), None);
    }

    #[test]
    fn test_subcommand_wins_over_option_like_argument() {
        // write の内容にオプションらしき文字列が入っていても、それは中身
        assert_eq!(
            info_command_from_args(&["write", "conn", "a.sql", "--help"]),
            None
        );
        assert_eq!(
            info_command_from_args(&["write", "conn", "a.sql", "--list-servers"]),
            None
        );
        assert_eq!(info_command_from_args(&["open", "--help"]), None);
        // サブコマンドより前にあれば拾う
        assert_eq!(
            info_command_from_args(&["--help", "write", "conn", "a.sql"]),
            Some(InfoCommand::Help)
        );
    }

    /// 版番号はリリースの基準である tauri.conf.json のものを出す。
    /// `CARGO_PKG_VERSION` へ戻すと、この 2 つがずれた時に気付けなくなる。
    #[test]
    fn test_version_comes_from_the_tauri_config() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json should be valid JSON");
        let expected = conf["version"]
            .as_str()
            .expect("version should be a string");

        assert_eq!(version_text(), format!("QueryFolio {expected}"));
        assert!(help_text().starts_with(&format!("QueryFolio {expected} ")));
    }

    #[test]
    fn test_help_text_lists_every_entry_point() {
        let help = help_text();
        for expected in [
            "queryfolio open <path>",
            "queryfolio write <connection> <file> [content]",
            "--list-servers",
            "--help",
            "--version",
        ] {
            assert!(help.contains(expected), "help should mention {expected}");
        }
    }

    #[test]
    fn test_format_server_list_never_shows_the_password() {
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"));
        assert!(
            !out.contains("s3cret"),
            "the password must not be printed:\n{out}"
        );
        assert!(!out.to_lowercase().contains("password"));
    }

    /// SSH トンネルの機密 (パスワード / 秘密鍵のパス / パスフレーズ /
    /// エージェントのソケット) も出さない。`ServerConfig` に機密フィールドが
    /// 増えた時に、この一覧へ漏れる回帰をここで止める。
    #[test]
    fn test_format_server_list_never_shows_the_ssh_secrets() {
        let with_tunnel: ServerConfig = serde_yaml::from_str(
            "name: through-bastion\n\
             engine: postgres\n\
             host: db.internal\n\
             port: 5432\n\
             user: app\n\
             password: db-p4ssword\n\
             ssh_tunnel:\n\
             \x20 host: bastion.example.com\n\
             \x20 port: 22\n\
             \x20 user: jump-user\n\
             \x20 password: ssh-p4ssword\n\
             \x20 private_key_path: /home/me/.ssh/id_ed25519_secret\n\
             \x20 private_key_passphrase: k3y-passphrase\n\
             \x20 identity_agent: /run/user/1000/secret-agent.sock\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[with_tunnel], Path::new("/tmp/sqlfiles"));

        for secret in [
            "db-p4ssword",
            "ssh-p4ssword",
            "id_ed25519_secret",
            "k3y-passphrase",
            "secret-agent.sock",
        ] {
            assert!(
                !out.contains(secret),
                "{secret} must not be printed:\n{out}"
            );
        }
        // トンネルを使っていることは分かる (SSH 列)
        assert!(out.contains("yes"), "{out}");
    }

    /// dynamodb の `user` は AWS のアクセスキー ID なので USER 欄に出さない。
    ///
    /// `ConnectionInfo` は「機密を含まない」射影だが、それは**フロントへ渡す**
    /// 基準であって端末に出す基準ではない。ここを素通しにすると、資格情報の
    /// 識別子が端末とシェル履歴・ログに残る。
    #[test]
    fn test_format_server_list_hides_the_aws_access_key_id() {
        let dynamo: ServerConfig = serde_yaml::from_str(
            "name: events\nengine: DynamoDB\nschema: ap-northeast-1\n\
             user: AKIAIOSFODNN7EXAMPLE\npassword: wJalrXUtnFEMI\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[dynamo], Path::new("/tmp/sqlfiles"));

        // engine の綴りが DynamoDB / dynamodb のどちらでも伏せる
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "{out}");
        assert!(!out.contains("wJalrXUtnFEMI"), "{out}");
        assert!(out.contains(HIDDEN), "{out}");
        // 接続そのものは一覧に出る (行ごと消してしまわない)
        assert!(out.contains("events"), "{out}");

        // 他のエンジンの user はユーザー名なので今までどおり出す
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"));
        assert!(out.contains("app"), "{out}");
        assert!(!out.contains(HIDDEN), "{out}");
    }

    #[test]
    fn test_format_server_list_neutralizes_control_characters() {
        // 設定は config_override_command の出力からも来る。改行が通ると
        // 「1 接続 = 1 行」の形が崩れて別の接続の行を偽装でき、ANSI / OSC の
        // エスケープが通ると端末側で解釈される
        let hostile: ServerConfig = serde_yaml::from_str(
            "name: \"evil\\nfake-row  postgres\"\nengine: postgres\n\
             host: \"h\\u001b[31mred\\u001b[0m\"\nuser: \"a\\tb\"\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[hostile], Path::new("/tmp/sqlfiles"));

        // 見出し 1 行 + 接続 1 行 + 先頭の情報行 + 空行 だけ
        assert_eq!(
            out.lines().count(),
            4,
            "a value must not be able to add a row:\n{out}"
        );
        assert!(!out.contains('\t'), "{out}");
        assert!(!out.contains('\u{1b}'), "{out}");
    }

    #[test]
    fn test_format_server_list_shows_the_requested_columns() {
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"));
        assert!(out.contains("Query files directory: /tmp/sqlfiles"));
        for expected in [
            "NAME", "ENGINE", "HOST", "PORT", "USER", "DATABASE", "SSL", "SSH", "FOLDER",
        ] {
            assert!(out.contains(expected), "header should contain {expected}");
        }
        assert!(out.contains("reporting"));
        assert!(out.contains("db.example.com"));
        assert!(out.contains("5432"));
        assert!(out.contains("app"));
        // フォルダ名は <host>_<engine>_<schema>_<user>
        assert!(out.contains("db.example.com_postgres_appdb_app"));
    }

    #[test]
    fn test_format_server_list_reports_the_effective_ssl_mode() {
        // ssl_mode も tls も無い postgres は prefer (平文に降格しうる)。
        // yes/no に丸めるとこの区別が消えるので、実効モードをそのまま出す
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"));
        assert!(out.contains("prefer"), "{out}");

        let tls: ServerConfig = serde_yaml::from_str(
            "name: secure\nengine: postgres\nhost: db.example.com\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[tls], Path::new("/tmp/sqlfiles"));
        assert!(out.contains("verify-full"), "{out}");

        // 実効モードを持たないエンジンは tls をそのまま出す
        let es: ServerConfig = serde_yaml::from_str(
            "name: search\nengine: elasticsearch\nhost: es.example.com\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[es], Path::new("/tmp/sqlfiles"));
        assert!(out.contains(" on"), "{out}");
    }

    /// 解決できない設定を「有効な TLS 設定」として見せない。
    ///
    /// `ConnectionInfo::sql_ssl_mode` は値が不正な時も `None` になるため、素直に
    /// `tls` へ落とすと `ssl_mode: requre` (書き間違い) が `off` = 平文で繋がる、と
    /// 読めてしまう。実際には接続時にエラーになるだけで、そんなモードは存在しない。
    #[test]
    fn test_format_server_list_marks_unresolvable_ssl_settings() {
        let bad_mode: ServerConfig = serde_yaml::from_str(
            "name: typo\nengine: postgres\nhost: db.example.com\nssl_mode: requre\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[bad_mode], Path::new("/tmp/sqlfiles"));
        assert!(out.contains(INVALID), "{out}");
        // 「平文で繋がる」と読める表示にはしない
        assert!(!out.contains(" off"), "{out}");

        let bad_engine: ServerConfig =
            serde_yaml::from_str("name: unknown\nengine: mysqll\nhost: db.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[bad_engine], Path::new("/tmp/sqlfiles"));
        assert!(out.contains(INVALID), "{out}");

        // ssl_mode を書いていない接続を巻き込まないこと (実効モードを持たない
        // エンジンは今までどおり tls の on / off)
        let es: ServerConfig =
            serde_yaml::from_str("name: search\nengine: elasticsearch\nhost: es.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[es], Path::new("/tmp/sqlfiles"));
        assert!(!out.contains(INVALID), "{out}");
        assert!(out.contains(" off"), "{out}");
    }

    #[test]
    fn test_format_server_list_fills_missing_values() {
        let sqlite: ServerConfig =
            serde_yaml::from_str("name: local\nengine: sqlite\nschema: /tmp/a.db\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[sqlite], Path::new("/tmp/sqlfiles"));
        assert!(out.contains("local"));
        // host / port / user が無い行でも列がずれない
        assert!(out.contains(EMPTY));
    }

    #[test]
    fn test_format_server_list_with_no_connection() {
        let out = format_server_list(&[], Path::new("/tmp/sqlfiles"));
        assert!(out.contains("No connection is configured."), "{out}");
    }
}
