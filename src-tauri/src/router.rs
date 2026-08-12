//! `queryfolio://` URI と CLI 引数を解釈する共通ルーター。
//!
//! URI (deep link) と CLI サブコマンドの両方をここで [`Route`] に落とし、
//! lib.rs 側が [`Route`] をディスパッチする。今後アクションを増やす時は
//! ここに variant とパースを足すだけで URI / CLI の両方に対応できる
//! (「queryfolio:// と同様のルートで機能を追加していけるように」の要)。
//!
//! ただし**書き込みを伴うアクション (`write`) は CLI 専用**で、URI からは
//! 受け付けない (`parse_uri` は UnknownAction にする)。`queryfolio://` URL は
//! Web ページからでも開かせられるため、URI で書き込みを許すと閲覧中のページが
//! 任意の SQL をユーザーのクエリファイルとして置けてしまう (ユーザーが後から
//! それを実行する危険がある)。CLI は起動する本人の操作なのでこの経路は無い。
//!
//! パス解決 ([`resolve_open_target`]) はセキュリティ上重要なので Tauri に依存
//! させず純粋な std だけで書き、単体テストで境界を固める。開けるのは
//! 「クエリファイル保存ディレクトリ (`sqlfiles_dir`) 直下の接続フォルダにある
//! クエリファイル (拡張子は [`ALLOWED_EXTENSIONS`]: `.sql` / `.redis` / `.es`)」だけで、
//! `..` によるトラバーサルや保存領域外のパスは拒否する。拡張子が接続エンジンの
//! ものと一致するかは、接続を解決できる lib.rs (resolve_route_target) 側が
//! 追加で検証する。

use std::path::{Component, Path, PathBuf};

/// URI スキーム名 (`queryfolio://...`)。
pub const URI_SCHEME: &str = "queryfolio";

/// 既存ファイルをパス指定で開く CLI サブコマンド。
const OPEN_SUBCOMMAND: &str = "open";

/// 接続フォルダにクエリファイルを書き出して開く CLI サブコマンド。
const WRITE_SUBCOMMAND: &str = "write";

/// URI / CLI から解釈されたアクション (まだ検証していない生の入力を保持する)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// クエリファイルをパス指定で開く。`path` は未検証の生パス
    /// (`resolve_open_target` で保存領域配下かを検証してから使う)。
    OpenFile { path: String },
    /// 接続 (SQL サーバー設定) の名前とファイル名を指定してクエリファイルを開く。
    /// `content` があればその内容で書き出してから開く (CLI 専用。下記参照)。
    ///
    /// **書き出し自体はこの Route の解決では行わない**。CLI プロセスが
    /// Tauri を起動する前に書き終えてから、この Route を「開く」指示として
    /// 実行中インスタンス (または自プロセス) に渡す。標準入力の内容は
    /// single-instance プラグインが転送する argv には載らないため、
    /// 書き出しを起動側で完結させないと転送経路で失われる。
    /// そのため解決側 (lib.rs) は `content` を参照しない。
    WriteFile {
        /// 接続名 (`ServerConfig::name`)。未検証。
        connection: String,
        /// ファイル名。未検証 (拡張子はエンジンのものが補われる)。
        file_name: String,
        /// 書き出す内容 (省略時は既存ファイルをそのまま開く)。
        content: Option<String>,
    },
}

/// 開く対象のクエリファイルを、接続名と (正規化済み) ファイル名で表す。
/// フロントエンドはこの接続を選択してこのファイルを開く。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTarget {
    /// 対象ファイルが属する接続の名前 (`ServerConfig::name`)。
    pub connection: String,
    /// 開くファイル名 (拡張子付き。接続フォルダ内の 1 要素)。
    pub file_name: String,
}

/// ルーティング・パス解決のエラー。フロントへは Display の文字列で伝える
/// (アプリ内メッセージなので英語)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// `queryfolio://` で始まっていない。
    NotQueryfolioUri,
    /// 未知のアクション (`open` 以外)。
    UnknownAction(String),
    /// 開くパスが空。
    EmptyPath,
    /// クエリファイル保存ディレクトリの外を指している。
    OutsideSqlfilesDir,
    /// 保存ディレクトリ直下の「接続フォルダ / ファイル」の形になっていない。
    NotUnderConnectionFolder,
    /// どの接続のフォルダにも一致しないフォルダ名。
    UnknownFolder(String),
    /// 同じフォルダに複数の接続が対応していて、どの接続で開くか一意に決められない。
    AmbiguousFolder(String),
    /// ファイル名が不正 (既知のクエリファイル拡張子でない・ドット始まり等)。
    InvalidFileName(String),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NotQueryfolioUri => {
                write!(f, "Not a {URI_SCHEME}:// URI")
            }
            RouteError::UnknownAction(a) => write!(f, "Unknown action: {a}"),
            RouteError::EmptyPath => write!(f, "The file path is empty"),
            RouteError::OutsideSqlfilesDir => write!(
                f,
                "The path is outside the query files directory"
            ),
            RouteError::NotUnderConnectionFolder => write!(
                f,
                "The path is not a file directly under a connection folder"
            ),
            RouteError::UnknownFolder(folder) => write!(
                f,
                "No connection matches the folder: {folder}"
            ),
            RouteError::AmbiguousFolder(folder) => write!(
                f,
                "Multiple connections map to the folder, cannot decide which to open: {folder}"
            ),
            RouteError::InvalidFileName(name) => {
                write!(f, "Invalid query file name: {name}")
            }
        }
    }
}

impl std::error::Error for RouteError {}

/// `queryfolio://open/<path>` 形式の URI を [`Route`] に解釈する。
///
/// 受け付けるアクションは `open` のみ。`write` は書き込みを伴うため URI からは
/// 受け付けない (モジュールドキュメント参照)。
///
/// アクションは `queryfolio://` の直後、最初の `/` までを取る。残りが開く対象の
/// パス (パーセントエンコードされていればデコードする)。絶対パスが渡ると
/// `queryfolio://open//abs/path.sql` のように `/` が重なるが、`open` を取り出した
/// 残り `/abs/path.sql` がそのままパスになる。
pub fn parse_uri(uri: &str) -> Result<Route, RouteError> {
    let scheme_prefix = format!("{URI_SCHEME}://");
    let rest = uri
        .strip_prefix(&scheme_prefix)
        .ok_or(RouteError::NotQueryfolioUri)?;
    // アクションは最初の `/` まで。`/` が無ければパス無し (= 空パス)。
    let (action, raw_path) = match rest.split_once('/') {
        Some((action, raw)) => (action, raw),
        None => (rest, ""),
    };
    match action {
        "open" => {
            let path = percent_decode(raw_path);
            if path.trim().is_empty() {
                return Err(RouteError::EmptyPath);
            }
            Ok(Route::OpenFile { path })
        }
        other => Err(RouteError::UnknownAction(other.to_string())),
    }
}

/// CLI 引数列から [`Route`] を解釈する。
///
/// 扱うのは次の 2 形式:
///
/// - `open <path>` — 保存済みのクエリファイルをパス指定で開く
/// - `write <connection> <file-name> [content]` — 接続のフォルダにクエリファイルを
///   書き出して開く (`content` 省略時は呼び出し側が標準入力を読む。読めなければ
///   既存ファイルをそのまま開く)
///
/// 引数列にはプログラム名が含まれることがある (single-instance が転送する argv は
/// argv[0] 込み) ため、先頭固定ではなく**最初に現れたサブコマンド語**を起点にする。
///
/// `queryfolio://` URL 引数は deep-link プラグインが処理するため `open` のパスとしては
/// 受け取らない (二重処理防止)。**引数列全体からの除去はしない** — 除去すると
/// `write` の内容が URL だった時 (`write prod a.sql 'queryfolio://open/x'`) にその
/// 内容ごと消えてしまうため。URL 引数はサブコマンド語と一致しないので、起点探索の
/// 邪魔にもならない。
pub fn route_from_cli_args<S: AsRef<str>>(args: &[S]) -> Option<Route> {
    let scheme_prefix = format!("{URI_SCHEME}://");
    let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
    // open / write のどちらか先に現れた方を採用する (両方を別々に探すと、
    // 後ろのサブコマンドの引数に紛れた語を拾ってしまう)。
    let pos = args
        .iter()
        .position(|a| *a == OPEN_SUBCOMMAND || *a == WRITE_SUBCOMMAND)?;
    match args[pos] {
        OPEN_SUBCOMMAND => {
            let path = args.get(pos + 1)?;
            if path.trim().is_empty() || path.starts_with(&scheme_prefix) {
                return None;
            }
            Some(Route::OpenFile {
                path: (*path).to_string(),
            })
        }
        WRITE_SUBCOMMAND => {
            let connection = args.get(pos + 1)?;
            let file_name = args.get(pos + 2)?;
            if connection.trim().is_empty() || file_name.trim().is_empty() {
                return None;
            }
            // 内容は省略可 (省略時は標準入力、それも無ければ書き出さない)。
            // 空文字を明示的に渡した場合は「空で書く」意図として Some("") を保つ。
            Some(Route::WriteFile {
                connection: (*connection).to_string(),
                file_name: (*file_name).to_string(),
                content: args.get(pos + 3).map(|c| (*c).to_string()),
            })
        }
        _ => None,
    }
}

/// 生パスを、保存ディレクトリ配下の接続フォルダにあるクエリファイルとして解決する。
///
/// - `sqlfiles_dir`: クエリファイル保存ディレクトリ。**呼び出し側で絶対パスに
///   しておくこと** (相対だと `cwd` の基準が生パスと食い違う: 生パスは deep link /
///   CLI の起動元 cwd で解決するが、保存ディレクトリはアプリプロセスの cwd で
///   I/O される。両者を混同しないよう base の絶対化は呼び出し側の責務とする)。
/// - `folders`: `(フォルダ名, 接続名)` の対応表 (設定順)。フォルダ名は
///   `ServerConfig::sqlfiles_folder_name()` が返すもの。
/// - `raw_path`: 開く対象の生パス (`~` / 相対パスは `home` / `cwd` で展開)。
/// - `home`: `~` 展開に使うホームディレクトリ (無ければ `~` は展開しない)。
/// - `cwd`: 相対パスの基準ディレクトリ (無ければ相対パスはそのまま)。
///
/// 成功条件: 展開・字句正規化したパスが `sqlfiles_dir/<フォルダ>/<name>.sql` の形
/// (ちょうど 2 階層) で、`<フォルダ>` が `folders` に存在し、`<name>.sql` が
/// 妥当なファイル名であること。`..` によるトラバーサルは字句正規化で潰れ、
/// 保存領域外に出れば `OutsideSqlfilesDir` になる (ファイルシステムには触れない)。
pub fn resolve_open_target(
    sqlfiles_dir: &Path,
    folders: &[(String, String)],
    raw_path: &str,
    home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<OpenTarget, RouteError> {
    let expanded = expand_path(raw_path, home, cwd);
    // base (sqlfiles_dir) は呼び出し側が絶対化済み。生パスだけ cwd で解決する。
    let normalized = lexical_normalize(&expanded);
    let base = lexical_normalize(sqlfiles_dir);

    let relative = normalized
        .strip_prefix(&base)
        .map_err(|_| RouteError::OutsideSqlfilesDir)?;

    // 保存ディレクトリ直下は「接続フォルダ / ファイル」のちょうど 2 要素。
    let components: Vec<&std::ffi::OsStr> = relative
        .components()
        .map(|c| c.as_os_str())
        .collect();
    if components.len() != 2 {
        return Err(RouteError::NotUnderConnectionFolder);
    }
    let folder = components[0].to_string_lossy().into_owned();
    let file_name = components[1].to_string_lossy().into_owned();

    // 同じフォルダに複数の接続が対応している場合 (同一 folder_name や、生成される
    // host/engine/schema/user フォルダが偶然一致) は、どの接続で開くか一意に
    // 決められない。先頭を黙って選ぶと別 DB / 別 readonly ポリシーの接続で開いて
    // しまう恐れがあるため、曖昧としてエラーにする。
    let mut matches = folders.iter().filter(|(f, _)| *f == folder);
    let connection = match (matches.next(), matches.next()) {
        (None, _) => return Err(RouteError::UnknownFolder(folder)),
        (Some(_), Some(_)) => return Err(RouteError::AmbiguousFolder(folder)),
        (Some((_, conn)), None) => conn.clone(),
    };

    validate_sql_file_name(&file_name)?;

    Ok(OpenTarget {
        connection,
        file_name,
    })
}

/// クエリファイルとして開ける拡張子 (エンジン別。engines::EngineCapabilities の
/// file_extension と対応させること)。
const ALLOWED_EXTENSIONS: &[&str] = &["sql", "redis", "es"];

/// クエリファイル名として妥当かを検証する
/// (query_files.rs の validate_component / normalize_file_name と同じ方針: 空・
/// ドット始まり・区切り文字を拒否し、拡張子が既知のクエリファイル拡張子で
/// あることを要求する)。
fn validate_sql_file_name(name: &str) -> Result<(), RouteError> {
    // 前後に空白がある名前は拒否する。query_files.rs の normalize_file_name は
    // 読み込み時に名前を trim するため、空白付きを許すと「検証したパス
    // (verify_within_dir が canonicalize したパス)」と「実際に開くパス (trim 後)」が
    // 食い違い、検証を通した別ファイル (symlink 等) を開けてしまう。ここで拒否して
    // 検証対象と開く対象を必ず同一にする。
    let lower = name.to_ascii_lowercase();
    let invalid = name.is_empty()
        || name != name.trim()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || !ALLOWED_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(&format!(".{ext}")));
    if invalid {
        return Err(RouteError::InvalidFileName(name.to_string()));
    }
    Ok(())
}

/// `~` / 相対パスを展開する (ファイルシステムには触れない字句的展開)。
fn expand_path(raw: &str, home: Option<&Path>, cwd: Option<&Path>) -> PathBuf {
    let raw = raw.trim();
    if let Some(home) = home {
        if raw == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path
    }
}

/// パスの `.` / `..` を字句的に解決する (シンボリックリンクは辿らない)。
/// `..` は 1 つ前の通常要素を取り除く。ルートより上には遡れない。
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 直前の通常要素を取り除く。ルート直下ではこれ以上遡らない。
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// パーセントエンコード (`%XX`) をデコードする。不完全な `%` はそのまま残す。
/// deep link 経由のパスは空白等がエンコードされ得るため、URI パスに使う。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) =
                (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
            {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 16 進 1 桁を数値へ (それ以外は None)。
fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders() -> Vec<(String, String)> {
        vec![
            ("db1_mysql__root".to_string(), "prod".to_string()),
            ("reporting".to_string(), "reporting-conn".to_string()),
        ]
    }

    #[test]
    fn test_parse_uri_open_absolute() {
        // 絶対パスはスキームの後に `/` が重なる形になる
        assert_eq!(
            parse_uri("queryfolio://open//home/u/.config/queryfolio/sqlfiles/reporting/a.sql"),
            Ok(Route::OpenFile {
                path: "/home/u/.config/queryfolio/sqlfiles/reporting/a.sql".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_uri_percent_decode() {
        assert_eq!(
            parse_uri("queryfolio://open//tmp/my%20query.sql"),
            Ok(Route::OpenFile {
                path: "/tmp/my query.sql".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_uri_errors() {
        assert_eq!(parse_uri("http://open/x"), Err(RouteError::NotQueryfolioUri));
        assert_eq!(
            parse_uri("queryfolio://delete/x"),
            Err(RouteError::UnknownAction("delete".to_string()))
        );
        assert_eq!(parse_uri("queryfolio://open"), Err(RouteError::EmptyPath));
        assert_eq!(parse_uri("queryfolio://open/"), Err(RouteError::EmptyPath));
    }

    #[test]
    fn test_route_from_cli_args() {
        assert_eq!(
            route_from_cli_args(&["open", "/tmp/a.sql"]),
            Some(Route::OpenFile {
                path: "/tmp/a.sql".to_string(),
            })
        );
        // queryfolio:// URL 引数は無視する (deep-link が処理する)
        assert_eq!(
            route_from_cli_args(&["queryfolio://open//tmp/a.sql"]),
            None
        );
        // open の後ろに URL が来ても、それをパスとしては受け取らない
        assert_eq!(
            route_from_cli_args(&["open", "queryfolio://open//tmp/a.sql"]),
            None
        );
        // open の後にパスが無ければ None
        assert_eq!(route_from_cli_args(&["open"]), None);
        // 無関係な引数だけなら None
        assert_eq!(route_from_cli_args(&["--flag", "value"]), None);
        // argv[0] (プログラムパス) が混ざっていても拾える
        assert_eq!(
            route_from_cli_args(&["/Applications/QueryFolio.app/queryfolio", "open", "/tmp/a.sql"]),
            Some(Route::OpenFile {
                path: "/tmp/a.sql".to_string(),
            })
        );
    }

    #[test]
    fn test_route_from_cli_args_write() {
        // 内容つき
        assert_eq!(
            route_from_cli_args(&["write", "prod", "report.sql", "select 1"]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "report.sql".to_string(),
                content: Some("select 1".to_string()),
            })
        );
        // 内容省略 (標準入力または「開くだけ」)
        assert_eq!(
            route_from_cli_args(&["write", "prod", "report"]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "report".to_string(),
                content: None,
            })
        );
        // 空文字の内容は「空で書く」意図として保つ (None に潰さない)
        assert_eq!(
            route_from_cli_args(&["write", "prod", "report.sql", ""]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "report.sql".to_string(),
                content: Some(String::new()),
            })
        );
        // 内容が queryfolio:// URL でもそのまま内容として扱う
        // (URL 引数の除去で内容を落とさない)
        assert_eq!(
            route_from_cli_args(&["write", "prod", "a.sql", "queryfolio://open/x"]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "a.sql".to_string(),
                content: Some("queryfolio://open/x".to_string()),
            })
        );
        // 引数が足りない / 空白だけなら None
        assert_eq!(route_from_cli_args(&["write", "prod"]), None);
        assert_eq!(route_from_cli_args(&["write"]), None);
        assert_eq!(route_from_cli_args(&["write", " ", "a.sql"]), None);
        assert_eq!(route_from_cli_args(&["write", "prod", "  "]), None);
    }

    #[test]
    fn test_route_from_cli_args_first_subcommand_wins() {
        // 先に現れたサブコマンドを採用する。write の内容に "open" という語が
        // 入っていても open サブコマンドとして誤解釈しない。
        assert_eq!(
            route_from_cli_args(&["write", "prod", "a.sql", "open /etc/passwd"]),
            Some(Route::WriteFile {
                connection: "prod".to_string(),
                file_name: "a.sql".to_string(),
                content: Some("open /etc/passwd".to_string()),
            })
        );
        assert_eq!(
            route_from_cli_args(&["open", "/tmp/a.sql", "write"]),
            Some(Route::OpenFile {
                path: "/tmp/a.sql".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_uri_rejects_write() {
        // 書き込みアクションは URI からは受け付けない (Web ページから
        // queryfolio:// を開かせられるため)
        assert_eq!(
            parse_uri("queryfolio://write/prod/a.sql"),
            Err(RouteError::UnknownAction("write".to_string()))
        );
    }

    #[test]
    fn test_resolve_open_target_ok() {
        let base = Path::new("/home/u/.config/queryfolio/sqlfiles");
        let target = resolve_open_target(
            base,
            &folders(),
            "/home/u/.config/queryfolio/sqlfiles/reporting/monthly.sql",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            target,
            OpenTarget {
                connection: "reporting-conn".to_string(),
                file_name: "monthly.sql".to_string(),
            }
        );
    }

    #[test]
    fn test_resolve_open_target_tilde_and_relative() {
        let home = Path::new("/home/u");
        let base = Path::new("~/.config/queryfolio/sqlfiles"); // base も展開される
        // base に ~ が入っていても展開して比較する
        let target = resolve_open_target(
            &expand_path("~/.config/queryfolio/sqlfiles", Some(home), None),
            &folders(),
            "~/.config/queryfolio/sqlfiles/reporting/a.sql",
            Some(home),
            None,
        )
        .unwrap();
        assert_eq!(target.connection, "reporting-conn");
        let _ = base;
    }

    #[test]
    fn test_resolve_open_target_relative_raw_path_with_cwd() {
        // base は絶対 (呼び出し側が絶対化する契約)。相対の入力パスは cwd 基準で解決。
        let cwd = Path::new("/work");
        let base = Path::new("/work/queries");
        let target = resolve_open_target(
            base,
            &folders(),
            "queries/reporting/a.sql",
            None,
            Some(cwd),
        )
        .unwrap();
        assert_eq!(target.connection, "reporting-conn");
        assert_eq!(target.file_name, "a.sql");
    }

    #[test]
    fn test_resolve_open_target_traversal_rejected() {
        let base = Path::new("/data/sqlfiles");
        // .. で保存領域の外に出ようとするパスは拒否
        let err = resolve_open_target(
            base,
            &folders(),
            "/data/sqlfiles/reporting/../../../etc/passwd",
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err, RouteError::OutsideSqlfilesDir);
    }

    #[test]
    fn test_resolve_open_target_outside() {
        let base = Path::new("/data/sqlfiles");
        assert_eq!(
            resolve_open_target(base, &folders(), "/etc/passwd", None, None)
                .unwrap_err(),
            RouteError::OutsideSqlfilesDir
        );
    }

    #[test]
    fn test_resolve_open_target_unknown_folder() {
        let base = Path::new("/data/sqlfiles");
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/unknown/a.sql",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::UnknownFolder("unknown".to_string())
        );
    }

    #[test]
    fn test_resolve_open_target_ambiguous_folder() {
        let base = Path::new("/data/sqlfiles");
        // 同じフォルダ名に 2 つの接続が対応する場合は曖昧としてエラー
        let dup = vec![
            ("shared".to_string(), "conn-a".to_string()),
            ("shared".to_string(), "conn-b".to_string()),
        ];
        assert_eq!(
            resolve_open_target(base, &dup, "/data/sqlfiles/shared/a.sql", None, None)
                .unwrap_err(),
            RouteError::AmbiguousFolder("shared".to_string())
        );
    }

    #[test]
    fn test_resolve_open_target_too_deep() {
        let base = Path::new("/data/sqlfiles");
        // 接続フォルダの下にサブディレクトリがある = 2 階層でない
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/reporting/sub/a.sql",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::NotUnderConnectionFolder
        );
        // 保存ディレクトリ直下のファイル (フォルダ無し) も拒否
        assert_eq!(
            resolve_open_target(base, &folders(), "/data/sqlfiles/a.sql", None, None)
                .unwrap_err(),
            RouteError::NotUnderConnectionFolder
        );
    }

    #[test]
    fn test_resolve_open_target_not_sql() {
        let base = Path::new("/data/sqlfiles");
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/reporting/notes.txt",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::InvalidFileName("notes.txt".to_string())
        );
        // ドット始まりの隠しファイルも拒否
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/reporting/.secret.sql",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::InvalidFileName(".secret.sql".to_string())
        );
        // 前後に空白のある名前は拒否 (trim で別ファイルに化けるのを防ぐ)。
        // expand_path がパス全体を trim するため末尾空白は落ち、先頭空白が残る。
        assert_eq!(
            resolve_open_target(
                base,
                &folders(),
                "/data/sqlfiles/reporting/ report.sql",
                None,
                None,
            )
            .unwrap_err(),
            RouteError::InvalidFileName(" report.sql".to_string())
        );
    }

    #[test]
    fn test_percent_decode_incomplete() {
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("%2F"), "/");
    }
}
