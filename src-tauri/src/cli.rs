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
use crate::db::Engine;
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

/// 接続設定でエンドポイントを上書きしているか (dynamodb-local 等を指しているか)。
///
/// 判定は [`crate::engines::dynamodb::build_client`] の分岐と揃える。
/// あちらが `endpoint_url` を組み立てる条件そのものなので、ずれると
/// 「上書きしていないのに `tls` を出す」「上書きしているのに `on` と出す」の
/// どちらかになる。
fn has_endpoint_override(server: &ServerConfig) -> bool {
    server
        .host
        .as_deref()
        .map(str::trim)
        .is_some_and(|host| !host.is_empty())
}

/// AWS SDK がエンドポイント上書きとして読む**環境変数**を、SDK と同じ優先順で解決する。
///
/// 接続設定に `host` を書いていない dynamodb 接続でも、SDK は地域エンドポイントとは
/// 限らない: `aws_config::defaults` は環境変数とプロファイルの `endpoint_url` 設定を
/// 見るため、`AWS_ENDPOINT_URL=http://localhost:8000` が効いていれば**平文**で繋がる。
/// 「`host` が無い = https」と決め打つと、その環境で平文の接続を `on` と見せることになる。
///
/// 優先順は aws-config の `endpoint_url` / `env_service_config` に合わせる:
/// `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` が真なら上書きは無視され、そうでなければ
/// サービス個別 (`AWS_ENDPOINT_URL_DYNAMODB`) → 全体 (`AWS_ENDPOINT_URL`) の順。
///
/// **プロファイルファイル (`~/.aws/config`) の `endpoint_url` / `services` セクションは
/// 見ていない。** ここはファイルシステムに触らない純粋な経路に保ちたいうえ、SDK の
/// プロファイル解決 (プロファイル名の決定・`services` セクションの参照・
/// `AWS_CONFIG_FILE` 等) を写すと本体とずれた第二の実装になる。プロファイルで
/// エンドポイントを上書きしている環境では、この列は地域エンドポイント (`on`) を出す。
///
/// 環境変数の読み取りを引数にしているのはテストのため (プロセスの環境変数を
/// 書き換えるテストは並列実行で干渉する)。
pub fn aws_endpoint_override(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let non_empty = |value: String| {
        let trimmed = value.trim().to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    };
    let ignored = get("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS")
        .and_then(non_empty)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    if ignored {
        return None;
    }
    get("AWS_ENDPOINT_URL_DYNAMODB")
        .and_then(non_empty)
        .or_else(|| get("AWS_ENDPOINT_URL").and_then(non_empty))
}

/// プロセスの環境変数から [`aws_endpoint_override`] を解決する。
pub fn aws_endpoint_override_from_env() -> Option<String> {
    aws_endpoint_override(|key| std::env::var(key).ok())
}

/// エンドポイント URL のスキームから SSL 欄の値を決める。
///
/// **`https://` でない限り `on` を出さない。** この列の `on` は「暗号化されている」の
/// 意味なので、確認できない値を丸め込む先にしてはいけない。
///
/// `aws_config` の `parse_url` は `url::Url::parse` が通れば受理するため、
/// `ftp://...` のような http(s) 以外のスキームもそのまま SDK へ渡り、接続時に失敗する。
/// 繋がらない設定なので `invalid` を出す (`ssl_mode` の不正値と同じ扱い)。
/// 逆に URL として解釈できない値は SDK が警告して捨て、地域エンドポイント (https) に
/// 戻すので `on`。
fn scheme_summary(endpoint: &str) -> &'static str {
    let lower = endpoint.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        "on"
    } else if lower.starts_with("http://") {
        "off"
    } else if lower.contains("://") {
        // http(s) 以外のスキーム。SDK は受理するが、この URL では接続できない
        INVALID
    } else {
        // URL として解釈できない値。parse_url がエラーにするので上書きは効かない
        "on"
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
/// **ただし `tls` が実際の接続方式を決めていないエンジンでは、そのまま出さない。**
/// dynamodb の `host` / `port` / `tls` は dynamodb-local 向けのエンドポイント上書き
/// 専用で、`host` を書かない通常の AWS 接続では SDK が地域エンドポイントを
/// **常に https で解決する** (`engines::dynamodb::build_client` は `host` がある時しか
/// `endpoint_url` を組み立てない)。`tls` の既定値 `false` をそのまま出すと、
/// 暗号化されている接続を `off` = 平文と読ませることになる。
///
/// **`sql_ssl_mode` が `None` でも、そのまま `tls` に落としてはいけない。**
/// `ConnectionInfo::from` は「実効モードを持たないエンジン」だけでなく
/// 「`engine` / `ssl_mode` の値が不正で解決できなかった」場合も `None` にする。
/// 後者を `tls` の `on` / `off` として出すと、**接続時にエラーになる設定を
/// 有効な TLS 設定として見せる**ことになる (`ssl_mode: requre` の書き間違いが
/// `off` = 平文で繋がる、と読める)。この列は接続の安全性を確認するためのものなので、
/// 決められない時は `invalid` と出して隠さない。
///
/// **逆に、`invalid` を出す範囲は「その値で実際に接続が失敗するエンジン」に限る。**
/// `ssl_mode` を読むのは `db::connect` の mysql / postgres の分岐だけで、
/// elasticsearch / sqlite / duckdb / dynamodb の接続経路は見ない。共有テンプレート等で
/// 不正な `ssl_mode` が紛れ込んでいても**それらは普通に繋がる**ので、`invalid` と
/// 出すと使える接続を壊れているように見せることになる。`invalid` の意味は
/// 「この設定では繋がらない」であって「設定に無効な値が書いてある」ではない。
///
/// **`ssl_mode` が解決できても、TLS 設定の組み合わせが拒否される場合がある。**
/// `ssl_root_cert` を検証しないモード (`disable` / `prefer` / `require`) と併記した
/// 設定は `sql_ssl_root_cert` がエラーにするため (sqlx が CA を黙って無視するので、
/// 「CA を指定したから検証されている」という誤解を放置しない設計)、`db::connect` は
/// 必ず失敗する。実効モードだけ見て `prefer` と出すと、繋がらない接続を有効な設定と
/// して見せることになる。**ファイルとして開けるか (`db::ssl_root_cert_path` の
/// `is_file`) までは見ない** — この一覧の組み立てはファイルシステムに触らない純粋な
/// 関数として単体テストで固めてあり、後から置ける不在ファイルは設定の誤りとも違う。
fn ssl_summary(server: &ServerConfig, info: &ConnectionInfo, aws_endpoint: Option<&str>) -> String {
    // エンジン名自体が解決できない接続は、どの経路でも繋がらない
    let Ok(engine) = crate::db::parse_engine(&server.engine) else {
        return INVALID.to_string();
    };
    // sql_ssl_mode() が Err になるのは ssl_mode が設定されていて解決できない時だけ
    // (未設定なら tls から既定値を返す) なので、未設定の接続を誤って invalid にはしない。
    // sql_ssl_root_cert() は ssl_root_cert が未設定なら mode を見ないので、
    // 2 つとも呼ぶ必要がある (前者は値の解決、後者は組み合わせの検証)
    if matches!(engine, Engine::MySql | Engine::Postgres)
        && (server.sql_ssl_mode().is_err() || server.sql_ssl_root_cert().is_err())
    {
        return INVALID.to_string();
    }
    if let Some(mode) = &info.sql_ssl_mode {
        return mode.clone();
    }
    // 接続設定でエンドポイントを上書きしていない dynamodb は、tls フラグではなく
    // SDK が実際に解決するエンドポイントの方式を出す (環境変数の上書きが無ければ
    // 地域エンドポイントの https)
    if engine == Engine::DynamoDb && !has_endpoint_override(server) {
        return match aws_endpoint {
            Some(endpoint) => scheme_summary(endpoint),
            None => "on",
        }
        .to_string();
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
pub fn format_server_list(
    servers: &[ServerConfig],
    sqlfiles_dir: &Path,
    aws_endpoint: Option<&str>,
) -> String {
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
                ssl_summary(server, &info, aws_endpoint),
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
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"), None);
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
        let out = format_server_list(&[with_tunnel], Path::new("/tmp/sqlfiles"), None);

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

    /// dynamodb の `tls` は dynamodb-local 向けのエンドポイント上書き専用なので、
    /// `host` を書かない通常の AWS 接続にそのまま出さない。
    ///
    /// SDK は地域エンドポイントを常に https で解決するため、`tls` の既定値
    /// (false) を出すと**暗号化されている接続を平文と読ませる**ことになる。
    /// この列は接続の安全性を確認するためのものなので、誤りの向きとしては最悪。
    #[test]
    fn test_format_server_list_reports_https_for_the_aws_dynamodb_endpoint() {
        // host 無し = AWS の地域エンドポイント。tls を書いていなくても https
        let aws: ServerConfig =
            serde_yaml::from_str("name: events\nengine: dynamodb\nschema: ap-northeast-1\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[aws], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" on"), "{out}");
        assert!(!out.contains(" off"), "{out}");

        // 空白だけの host も「未指定」(build_client の trim と揃える)
        let blank_host: ServerConfig = serde_yaml::from_str(
            "name: events\nengine: dynamodb\nschema: ap-northeast-1\nhost: \"   \"\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[blank_host], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" on"), "{out}");

        // host を書いた = dynamodb-local 等のエンドポイント上書き。ここでは tls が効く
        let local: ServerConfig = serde_yaml::from_str(
            "name: local\nengine: dynamodb\nschema: ap-northeast-1\n\
             host: 127.0.0.1\nport: 8000\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[local], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" off"), "{out}");

        let local_tls: ServerConfig = serde_yaml::from_str(
            "name: local\nengine: dynamodb\nschema: ap-northeast-1\n\
             host: 127.0.0.1\nport: 8000\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[local_tls], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" on"), "{out}");

        // 実効モードを持たない他のエンジンは今までどおり tls をそのまま出す
        let es: ServerConfig =
            serde_yaml::from_str("name: search\nengine: elasticsearch\nhost: es.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[es], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(" off"), "{out}");
    }

    /// `host` を書いていない dynamodb でも、環境変数でエンドポイントを上書きして
    /// いれば地域エンドポイントとは限らない。SDK が実際に使う方を出す。
    #[test]
    fn test_format_server_list_follows_the_aws_endpoint_environment() {
        let aws: ServerConfig =
            serde_yaml::from_str("name: events\nengine: dynamodb\nschema: ap-northeast-1\n")
                .expect("test fixture should parse");

        // AWS_ENDPOINT_URL=http://... は平文。これを on と出すと、平文の接続を
        // 暗号化されていると読ませることになる
        let out = format_server_list(
            std::slice::from_ref(&aws),
            Path::new("/tmp/sqlfiles"),
            Some("http://localhost:8000"),
        );
        assert!(out.contains(" off"), "{out}");

        let out = format_server_list(
            std::slice::from_ref(&aws),
            Path::new("/tmp/sqlfiles"),
            Some("https://dynamodb.ap-northeast-1.amazonaws.com"),
        );
        assert!(out.contains(" on"), "{out}");
        assert!(!out.contains(" off"), "{out}");

        // 大文字のスキームでも判定できること (SDK は大小を区別しない)
        let out = format_server_list(
            std::slice::from_ref(&aws),
            Path::new("/tmp/sqlfiles"),
            Some("HTTP://localhost:8000"),
        );
        assert!(out.contains(" off"), "{out}");

        // SDK は解釈できない値を警告して捨て、地域エンドポイントへ戻す
        let out = format_server_list(
            std::slice::from_ref(&aws),
            Path::new("/tmp/sqlfiles"),
            Some("not-a-url"),
        );
        assert!(out.contains(" on"), "{out}");

        // http(s) 以外のスキームは SDK が受理するが接続できない。
        // **確認できない値を on に丸めない** (この列の on は「暗号化されている」の意味)
        for endpoint in ["ftp://localhost:8000", "ws://localhost:8000"] {
            let out = format_server_list(
                std::slice::from_ref(&aws),
                Path::new("/tmp/sqlfiles"),
                Some(endpoint),
            );
            assert!(out.contains(INVALID), "{endpoint}:\n{out}");
            assert!(!out.contains(" on"), "{endpoint}:\n{out}");
        }

        // 接続設定に host がある = 明示のエンドポイント上書き。こちらが優先されるので
        // 環境変数ではなく tls を見る (build_client が endpoint_url を組み立てる)
        let local: ServerConfig = serde_yaml::from_str(
            "name: local\nengine: dynamodb\nschema: ap-northeast-1\n\
             host: 127.0.0.1\nport: 8000\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(
            &[local],
            Path::new("/tmp/sqlfiles"),
            Some("http://localhost:9999"),
        );
        assert!(out.contains(" on"), "{out}");

        // 他のエンジンは AWS の環境変数と無関係
        let es: ServerConfig =
            serde_yaml::from_str("name: search\nengine: elasticsearch\nhost: es.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(
            &[es],
            Path::new("/tmp/sqlfiles"),
            Some("https://localhost:8000"),
        );
        assert!(out.contains(" off"), "{out}");
    }

    /// 環境変数の優先順は aws-config に合わせる。
    #[test]
    fn test_aws_endpoint_override_precedence() {
        let env = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            move |key: &str| {
                owned
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.to_string())
            }
        };

        assert_eq!(aws_endpoint_override(env(&[])), None);

        // サービス個別が全体より優先
        assert_eq!(
            aws_endpoint_override(env(&[
                ("AWS_ENDPOINT_URL", "http://global:1"),
                ("AWS_ENDPOINT_URL_DYNAMODB", "http://service:2"),
            ])),
            Some("http://service:2".to_string())
        );
        assert_eq!(
            aws_endpoint_override(env(&[("AWS_ENDPOINT_URL", "http://global:1")])),
            Some("http://global:1".to_string())
        );

        // AWS_IGNORE_CONFIGURED_ENDPOINT_URLS=true なら上書きは効かない
        assert_eq!(
            aws_endpoint_override(env(&[
                ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "TRUE"),
                ("AWS_ENDPOINT_URL_DYNAMODB", "http://service:2"),
            ])),
            None
        );
        // true 以外の値は無視 (false / 空文字で上書きを殺さない)
        assert_eq!(
            aws_endpoint_override(env(&[
                ("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS", "false"),
                ("AWS_ENDPOINT_URL", "http://global:1"),
            ])),
            Some("http://global:1".to_string())
        );

        // 空 / 空白だけの値は未設定として扱い、次の候補へ落とす
        assert_eq!(
            aws_endpoint_override(env(&[
                ("AWS_ENDPOINT_URL_DYNAMODB", "   "),
                ("AWS_ENDPOINT_URL", " http://global:1 "),
            ])),
            Some("http://global:1".to_string())
        );
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
        let out = format_server_list(&[dynamo], Path::new("/tmp/sqlfiles"), None);

        // engine の綴りが DynamoDB / dynamodb のどちらでも伏せる
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "{out}");
        assert!(!out.contains("wJalrXUtnFEMI"), "{out}");
        assert!(out.contains(HIDDEN), "{out}");
        // 接続そのものは一覧に出る (行ごと消してしまわない)
        assert!(out.contains("events"), "{out}");

        // 他のエンジンの user はユーザー名なので今までどおり出す
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"), None);
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
        let out = format_server_list(&[hostile], Path::new("/tmp/sqlfiles"), None);

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
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"), None);
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
        let out = format_server_list(&[server("reporting")], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("prefer"), "{out}");

        let tls: ServerConfig = serde_yaml::from_str(
            "name: secure\nengine: postgres\nhost: db.example.com\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[tls], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("verify-full"), "{out}");

        // 実効モードを持たないエンジンは tls をそのまま出す
        let es: ServerConfig = serde_yaml::from_str(
            "name: search\nengine: elasticsearch\nhost: es.example.com\ntls: true\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[es], Path::new("/tmp/sqlfiles"), None);
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
        let out = format_server_list(&[bad_mode], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(INVALID), "{out}");
        // 「平文で繋がる」と読める表示にはしない
        assert!(!out.contains(" off"), "{out}");

        let bad_engine: ServerConfig =
            serde_yaml::from_str("name: unknown\nengine: mysqll\nhost: db.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[bad_engine], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(INVALID), "{out}");

        // `ssl_mode` を読まないエンジンは巻き込まない。共有テンプレート等で不正な値が
        // 紛れ込んでいても、これらの接続経路 (db::connect の該当分岐 / engines/) は
        // sql_ssl_mode を見ないので普通に繋がる。`invalid` の意味は「この設定では
        // 繋がらない」なので、使える接続を壊れているように見せてはいけない
        // ssl_mode が解決できても、組み合わせが拒否される設定は繋がらない。
        // ssl_root_cert を検証しないモードと併記すると sql_ssl_root_cert が
        // エラーにするため、db::connect は必ず失敗する
        for mode in ["disable", "prefer", "require"] {
            let cert_without_verify: ServerConfig = serde_yaml::from_str(&format!(
                "name: ca-{mode}\nengine: postgres\nhost: db.example.com\n\
                 ssl_mode: {mode}\nssl_root_cert: /etc/ssl/ca.pem\n"
            ))
            .expect("test fixture should parse");
            let out = format_server_list(&[cert_without_verify], Path::new("/tmp/sqlfiles"), None);
            assert!(out.contains(INVALID), "{mode}:\n{out}");
            // 解決できる実効モードの方を出してはいけない (繋がらないので)
            assert!(!out.contains(&format!(" {mode}")), "{mode}:\n{out}");
        }

        // ssl_mode を省略した場合の既定は prefer なので、これも検証されない
        let cert_without_mode: ServerConfig = serde_yaml::from_str(
            "name: ca-default\nengine: postgres\nhost: db.example.com\n\
             ssl_root_cert: /etc/ssl/ca.pem\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[cert_without_mode], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(INVALID), "{out}");

        // 空の ssl_root_cert も同じくエラーになる (黙って未設定に倒さない)
        let empty_cert: ServerConfig = serde_yaml::from_str(
            "name: ca-empty\nengine: postgres\nhost: db.example.com\n\
             ssl_mode: verify-full\nssl_root_cert: \"  \"\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[empty_cert], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains(INVALID), "{out}");

        // 検証するモードとの併記は正しい設定なので、実効モードをそのまま出す。
        // **ファイルが実在するかは見ない** (この関数はファイルシステムに触らない)
        let verifying: ServerConfig = serde_yaml::from_str(
            "name: ca-ok\nengine: postgres\nhost: db.example.com\n\
             ssl_mode: verify-ca\nssl_root_cert: /nonexistent/ca.pem\n",
        )
        .expect("test fixture should parse");
        let out = format_server_list(&[verifying], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("verify-ca"), "{out}");
        assert!(!out.contains(INVALID), "{out}");

        for engine in ["elasticsearch", "sqlite", "duckdb", "dynamodb"] {
            let ignores_ssl_mode: ServerConfig = serde_yaml::from_str(&format!(
                "name: {engine}-conn\nengine: {engine}\nhost: h.example.com\n\
                 schema: s\nssl_mode: requre\n"
            ))
            .expect("test fixture should parse");
            let out = format_server_list(&[ignores_ssl_mode], Path::new("/tmp/sqlfiles"), None);
            assert!(!out.contains(INVALID), "{engine}:\n{out}");
        }

        // ssl_mode を書いていない接続を巻き込まないこと (実効モードを持たない
        // エンジンは今までどおり tls の on / off)
        let es: ServerConfig =
            serde_yaml::from_str("name: search\nengine: elasticsearch\nhost: es.example.com\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[es], Path::new("/tmp/sqlfiles"), None);
        assert!(!out.contains(INVALID), "{out}");
        assert!(out.contains(" off"), "{out}");
    }

    #[test]
    fn test_format_server_list_fills_missing_values() {
        let sqlite: ServerConfig =
            serde_yaml::from_str("name: local\nengine: sqlite\nschema: /tmp/a.db\n")
                .expect("test fixture should parse");
        let out = format_server_list(&[sqlite], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("local"));
        // host / port / user が無い行でも列がずれない
        assert!(out.contains(EMPTY));
    }

    #[test]
    fn test_format_server_list_with_no_connection() {
        let out = format_server_list(&[], Path::new("/tmp/sqlfiles"), None);
        assert!(out.contains("No connection is configured."), "{out}");
    }
}
