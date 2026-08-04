use std::fs;
use std::path::{Path, PathBuf};

use crate::error::AppError;

/// パス要素として安全な名前かを検証して返す。
/// パストラバーサルや不可視ファイルを防ぐ。
/// (history.rs でも接続名の検証に使う)
pub(crate) fn validate_component(name: &str) -> Result<&str, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::QueryFile("The name is empty".into()));
    }
    if name.starts_with('.') {
        return Err(AppError::QueryFile(format!(
            "Names starting with a dot are not allowed: {name}"
        )));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(AppError::QueryFile(format!(
            "The name contains invalid characters: {name}"
        )));
    }
    Ok(name)
}

/// クエリファイル名を正規化する (接続エンジンの拡張子を保証する)。
/// ext は "sql" / "redis" などドット無しの拡張子 (engines::EngineCapabilities
/// の file_extension)。
fn normalize_file_name(name: &str, ext: &str) -> Result<String, AppError> {
    let name = validate_component(name)?;
    let suffix = format!(".{}", ext.to_ascii_lowercase());
    if name.to_ascii_lowercase().ends_with(&suffix) {
        Ok(name.to_string())
    } else {
        Ok(format!("{name}{suffix}"))
    }
}

/// 接続名に対応するクエリファイル保存ディレクトリを返す。
pub(crate) fn connection_dir(
    sqlfiles_dir: &Path,
    connection: &str,
) -> Result<PathBuf, AppError> {
    let connection = validate_component(connection)?;
    Ok(sqlfiles_dir.join(connection))
}

fn file_path(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<PathBuf, AppError> {
    let file_name = normalize_file_name(file_name, ext)?;
    Ok(connection_dir(sqlfiles_dir, connection)?.join(file_name))
}

/// ディレクトリ直下の、拡張子が ext のファイル名を**降順**で返す。存在しなければ空。
/// (list_query_files と search_query_files で列挙条件を共有し、
///  隠しファイル/拡張子判定/ソートが片方だけズレるのを防ぐ)
///
/// dot 始まりの隠しファイルは除外する。validate_component が dot 始まりの名前を
/// 拒否する (= CRUD で開けない) のと一貫させ、手動配置された隠しファイルの中身が
/// 検索プレビューから漏れないようにする。
///
/// 降順にしているのは「新しいファイルを一覧の上に出す」ため。既定のファイル名は
/// `YYYYMMDD-HHMM` 形式で、名前順がそのまま時系列になるよう作られている
/// (FilesPane の defaultFileName)。したがって名前の降順 = 新しい順になる。
/// 更新日時ではなく名前を基準にするのは、保存のたびに並びが入れ替わって
/// 作業中に一覧が跳ねるのを避けるため。
fn list_query_file_names(dir: &Path, ext: &str) -> Result<Vec<String>, AppError> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let suffix = format!(".{}", ext.to_ascii_lowercase());
    let mut names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .filter(|name| name.to_ascii_lowercase().ends_with(&suffix))
        .collect();
    // 降順 (新しいものが先頭)。比較は**拡張子を除いた部分**で行う。
    //
    // 拡張子を付けたまま比べると、同じ分に作られた連番ファイル
    // ("20260804-1200.sql" と "20260804-1200-02.sql") の順序が逆になる:
    // '.' (0x2E) > '-' (0x2D) なので、降順では古い無印の方が先に来てしまう。
    // 拡張子を落とせば "20260804-1200-02" > "20260804-1200" (前方一致する
    // 短い方が小さい) となり、作成順どおり新しいものが先頭に来る。
    //
    // 全要素が suffix で終わることは上の filter が保証しており、suffix は
    // ASCII なので、末尾 suffix.len() バイトを落とす位置は必ず文字境界になる。
    let ext_len = suffix.len();
    names.sort_unstable_by(|a, b| {
        let a_stem = &a[..a.len() - ext_len];
        let b_stem = &b[..b.len() - ext_len];
        // stem が同じになるのは拡張子の大文字小文字だけが違う場合。
        // 並びを決定的にするため、その時は名前全体で決める。
        b_stem.cmp(a_stem).then_with(|| b.cmp(a))
    });
    Ok(names)
}

/// 接続のクエリファイル一覧を返す (名前降順 = 新しいものが先頭)。
pub fn list_query_files(
    sqlfiles_dir: &Path,
    connection: &str,
    ext: &str,
) -> Result<Vec<String>, AppError> {
    list_query_file_names(&connection_dir(sqlfiles_dir, connection)?, ext)
}

/// クエリファイル検索の 1 ヒット。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct FileSearchHit {
    /// ヒットしたファイル名 (拡張子付き)
    pub file_name: String,
    /// ファイル名が query に一致したか
    pub name_match: bool,
    /// 中身が一致した最初の行 (プレビュー用。名前のみ一致なら None)
    pub content_preview: Option<String>,
}

/// プレビュー行の最大文字数 (これを超えたら末尾を省略記号にする)。
const PREVIEW_MAX_CHARS: usize = 120;

/// 検索結果の最大件数。名前降順 (新しい順) で先頭からこの数で打ち切る
/// (モーダルの一覧を短く保ち、多数ファイル環境での読み取りコストも抑える)。
const MAX_SEARCH_HITS: usize = 50;

/// プレビュー行を前後の空白除去 + 長さ制限で整形する。
fn truncate_preview(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= PREVIEW_MAX_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(PREVIEW_MAX_CHARS).collect();
    format!("{cut}…")
}

/// 接続のクエリファイルをファイル名・中身で検索する。
/// 大文字小文字を区別しない部分一致。中身は最初に一致した行をプレビューとして返す。
/// 名前降順 (新しい順) で、名前一致または中身一致したファイルのみ返す。
/// (rg/grep のような外部プロセスは使わない。クエリファイルは少数のため
///  純 Rust で読み取る方が堅牢で、外部依存・インジェクション面も持たない)
pub fn search_query_files(
    sqlfiles_dir: &Path,
    connection: &str,
    query: &str,
    ext: &str,
) -> Result<Vec<FileSearchHit>, AppError> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(vec![]);
    }
    let dir = connection_dir(sqlfiles_dir, connection)?;
    let names = list_query_file_names(&dir, ext)?;

    let mut hits = Vec::new();
    for name in names {
        let name_match = name.to_lowercase().contains(&needle);
        // 中身検索。読めないファイル (バイナリ等) はスキップし、名前一致だけで拾う
        let content_preview = fs::read_to_string(dir.join(&name))
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .find(|line| line.to_lowercase().contains(&needle))
                    .map(truncate_preview)
            });
        if name_match || content_preview.is_some() {
            hits.push(FileSearchHit {
                file_name: name,
                name_match,
                content_preview,
            });
            // 名前降順 (新しい順) で先頭から上限まで。以降のファイルは読まずに打ち切る
            if hits.len() >= MAX_SEARCH_HITS {
                break;
            }
        }
    }
    Ok(hits)
}

/// クエリファイルの絶対パスを文字列で返す (「Copy full path」用)。
/// パストラバーサル対策のため名前を検証・正規化してから組み立てる。
/// 一覧に出ているファイルからのみ呼ばれるため存在チェックはしない。
/// sqlfiles_dir が相対パスで設定されている場合、組み立てた path も相対になる。
/// 「Copy full path」の名の通り常に絶対パスを返すため、相対のときは
/// カレントディレクトリ基準で絶対化する (std::path::absolute は存在チェック
/// もシンボリックリンク解決も伴わない字句的な絶対化)。
pub fn query_file_path(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    let path = std::path::absolute(&path)?;
    Ok(path.to_string_lossy().into_owned())
}

pub fn read_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    if !path.exists() {
        return Err(AppError::QueryFile(format!(
            "File not found: {}",
            path.display()
        )));
    }
    Ok(fs::read_to_string(&path)?)
}

pub fn write_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    content: &str,
    ext: &str,
) -> Result<(), AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content)?;
    Ok(())
}

/// 楽観的排他つきの書き込み。呼び出し側が把握している base (expected_base) と、
/// 書き込み直前に読んだディスクの現在内容が一致する時だけ書き込む。
/// - 書き込めたら Ok(true)。
/// - ファイルが存在するのに expected_base と食い違う (= アプリ外で変更された) 場合は
///   書き込まず Ok(false) を返す (呼び出し側でマージ/衝突処理へ回すため)。
/// - ファイルが存在しない場合は expected_base に関わらず (再) 作成して Ok(true)
///   (外部で削除されたケースで手元の編集を確実に残す)。
///
/// 検査と書き込みを同一のバックエンド呼び出し内で隣接して行うことで、フロントとの
/// 非同期往復ぶんの TOCTOU 窓を無くす (完全な OS レベル atomic ではないが、
/// read→write の間隔を隣接システムコールまで詰める)。
pub fn write_query_file_if_unchanged(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    content: &str,
    expected_base: &str,
    ext: &str,
) -> Result<bool, AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    match fs::read_to_string(&path) {
        Ok(current) => {
            if current != expected_base {
                // アプリ外で変更されている。上書きしない。
                return Ok(false);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 外部で削除された等。下で再作成する。
        }
        Err(e) => return Err(e.into()),
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, content)?;
    Ok(true)
}

/// 空のクエリファイルを新規作成し、正規化されたファイル名を返す。
pub fn create_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let normalized = normalize_file_name(file_name, ext)?;
    let path = file_path(sqlfiles_dir, connection, &normalized, ext)?;
    if path.exists() {
        return Err(AppError::QueryFile(format!(
            "A file with the same name already exists: {normalized}"
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, "")?;
    Ok(normalized)
}

pub fn delete_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<(), AppError> {
    let path = file_path(sqlfiles_dir, connection, file_name, ext)?;
    if !path.exists() {
        return Err(AppError::QueryFile(format!(
            "File not found: {}",
            path.display()
        )));
    }
    fs::remove_file(&path)?;
    Ok(())
}

/// クエリファイルをリネームし、正規化された新しいファイル名を返す。
/// 新旧が同名 (正規化後) なら no-op で新名を返す。
pub fn rename_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    old_name: &str,
    new_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let old_normalized = normalize_file_name(old_name, ext)?;
    let new_normalized = normalize_file_name(new_name, ext)?;
    if old_normalized == new_normalized {
        return Ok(new_normalized);
    }
    let old_path = file_path(sqlfiles_dir, connection, &old_normalized, ext)?;
    if !old_path.exists() {
        return Err(AppError::QueryFile(format!(
            "File not found: {}",
            old_path.display()
        )));
    }
    // 衝突判定は case-insensitive で行う (case-insensitive FS の実挙動と揃え、
    // フロントの判定とも一致させる)。リネーム対象自身 (old) は除外するので、
    // 大文字小文字だけを変える改名 (Test.sql -> test.sql) は許可される。
    let new_lower = new_normalized.to_ascii_lowercase();
    let dir = connection_dir(sqlfiles_dir, connection)?;
    if dir.exists() {
        for entry in fs::read_dir(&dir)?.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name != old_normalized && name.to_ascii_lowercase() == new_lower {
                return Err(AppError::QueryFile(format!(
                    "A file with the same name already exists: {new_normalized}"
                )));
            }
        }
    }
    let new_path = file_path(sqlfiles_dir, connection, &new_normalized, ext)?;
    fs::rename(&old_path, &new_path)?;
    Ok(new_normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "queryfolio-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn test_validate_component() {
        assert!(validate_component("normal-name").is_ok());
        assert!(validate_component("").is_err());
        assert!(validate_component("   ").is_err());
        assert!(validate_component("..").is_err());
        assert!(validate_component(".hidden").is_err());
        assert!(validate_component("a/b").is_err());
        assert!(validate_component("a\\b").is_err());
        assert!(validate_component("../../etc/passwd").is_err());
    }

    #[test]
    fn test_normalize_file_name() {
        assert_eq!(normalize_file_name("query", "sql").unwrap(), "query.sql");
        assert_eq!(normalize_file_name("query.sql", "sql").unwrap(), "query.sql");
        assert_eq!(normalize_file_name("query.SQL", "sql").unwrap(), "query.SQL");
        assert!(normalize_file_name("../evil", "sql").is_err());
        // エンジン別拡張子 (redis)
        assert_eq!(normalize_file_name("keys", "redis").unwrap(), "keys.redis");
        assert_eq!(
            normalize_file_name("keys.redis", "redis").unwrap(),
            "keys.redis"
        );
        // 別エンジンの拡張子は付け直す (keys.sql は redis 接続では別名)
        assert_eq!(
            normalize_file_name("keys.sql", "redis").unwrap(),
            "keys.sql.redis"
        );
    }

    #[test]
    fn test_list_query_files_filters_by_extension() {
        let dir = test_dir().join("ext");
        let connection = "redis-conn";

        create_query_file(&dir, connection, "commands", "redis").unwrap();
        // 手動配置された別拡張子のファイルは一覧に出ない
        fs::write(
            connection_dir(&dir, connection).unwrap().join("other.sql"),
            "SELECT 1;",
        )
        .unwrap();

        assert_eq!(
            list_query_files(&dir, connection, "redis").unwrap(),
            vec!["commands.redis"]
        );
        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            vec!["other.sql"]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_query_files_sorts_newest_first() {
        let dir = test_dir().join("order");
        let connection = "order-conn";

        // 既定のファイル名は YYYYMMDD-HHMM なので、名前の降順 = 新しい順になる。
        // 作成順と無関係に並ぶことを見るため、わざと時系列とズラして作る。
        // 末尾の -02 / -10 は同一分内に複数作った時の連番 (FilesPane)。
        // 拡張子を含めて比較すると無印が連番より前に来てしまうため、その退行も見る。
        create_query_file(&dir, connection, "20260101-0900", "sql").unwrap();
        create_query_file(&dir, connection, "20260315-1830", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200-02", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200-10", "sql").unwrap();

        let expected = vec![
            "20260315-1830.sql",
            "20260102-1200-10.sql",
            "20260102-1200-02.sql",
            "20260102-1200.sql",
            "20260101-0900.sql",
        ];
        assert_eq!(list_query_files(&dir, connection, "sql").unwrap(), expected);

        // 検索結果も同じ並び (列挙を list_query_file_names で共有しているため)
        let hits = search_query_files(&dir, connection, "2026", "sql").unwrap();
        assert_eq!(
            hits.iter().map(|h| h.file_name.as_str()).collect::<Vec<_>>(),
            expected
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_query_file_crud() {
        let dir = test_dir();
        let connection = "test-conn";

        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            Vec::<String>::new()
        );

        let name = create_query_file(&dir, connection, "my query", "sql").unwrap();
        assert_eq!(name, "my query.sql");

        // 同名の再作成はエラー
        assert!(create_query_file(&dir, connection, "my query", "sql").is_err());

        write_query_file(&dir, connection, &name, "SELECT 1;", "sql").unwrap();
        assert_eq!(
            read_query_file(&dir, connection, &name, "sql").unwrap(),
            "SELECT 1;"
        );

        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            vec!["my query.sql"]
        );

        delete_query_file(&dir, connection, &name, "sql").unwrap();
        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            Vec::<String>::new()
        );

        // 後始末
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_query_file_if_unchanged() {
        let dir = test_dir().join("cas");
        let connection = "test-conn";
        let name = "q.sql";

        // ファイルが無ければ expected_base に関わらず作成する (外部削除からの復帰)。
        assert_eq!(
            write_query_file_if_unchanged(&dir, connection, name, "V1", "", "sql").unwrap(),
            true
        );
        assert_eq!(read_query_file(&dir, connection, name, "sql").unwrap(), "V1");

        // base が現在のディスク内容と一致すれば書き込む。
        assert_eq!(
            write_query_file_if_unchanged(&dir, connection, name, "V2", "V1", "sql").unwrap(),
            true
        );
        assert_eq!(read_query_file(&dir, connection, name, "sql").unwrap(), "V2");

        // アプリ外で "EXTERNAL" に変更されたのに、こちらの base が古い ("V2") 場合は
        // 書き込まず false を返す (外部変更を黙って上書きしない)。
        write_query_file(&dir, connection, name, "EXTERNAL", "sql").unwrap();
        assert_eq!(
            write_query_file_if_unchanged(&dir, connection, name, "MINE", "V2", "sql").unwrap(),
            false
        );
        assert_eq!(read_query_file(&dir, connection, name, "sql").unwrap(), "EXTERNAL");

        // base を現在値に合わせれば再び書ける。
        assert_eq!(
            write_query_file_if_unchanged(&dir, connection, name, "MINE", "EXTERNAL", "sql")
                .unwrap(),
            true
        );
        assert_eq!(read_query_file(&dir, connection, name, "sql").unwrap(), "MINE");

        // 後始末
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_query_file_path() {
        let dir = test_dir().join("fullpath");
        let connection = "test-conn";

        // .sql 補完・接続フォルダ・ディレクトリが連結された絶対パスが返る
        let path = query_file_path(&dir, connection, "report", "sql").unwrap();
        let expected = dir
            .join(connection)
            .join("report.sql")
            .to_string_lossy()
            .into_owned();
        assert_eq!(path, expected);

        // 既に .sql 付きの名前は二重付与しない
        let path = query_file_path(&dir, connection, "report.sql", "sql").unwrap();
        assert_eq!(path, expected);

        // パストラバーサルは拒否
        assert!(query_file_path(&dir, connection, "../evil", "sql").is_err());
        assert!(query_file_path(&dir, connection, "a/b", "sql").is_err());
    }

    #[test]
    fn test_rename_query_file() {
        let dir = test_dir().join("rename");
        let connection = "test-conn";

        create_query_file(&dir, connection, "old", "sql").unwrap();
        write_query_file(&dir, connection, "old", "SELECT 1;", "sql").unwrap();

        // リネーム成功 (内容は保持される)
        let renamed = rename_query_file(&dir, connection, "old", "new", "sql").unwrap();
        assert_eq!(renamed, "new.sql");
        assert_eq!(
            list_query_files(&dir, connection, "sql").unwrap(),
            vec!["new.sql"]
        );
        assert_eq!(
            read_query_file(&dir, connection, "new", "sql").unwrap(),
            "SELECT 1;"
        );

        // 既存名への変更は拒否
        create_query_file(&dir, connection, "other", "sql").unwrap();
        assert!(rename_query_file(&dir, connection, "new", "other", "sql").is_err());

        // 同名 (正規化後) への変更は no-op
        assert_eq!(
            rename_query_file(&dir, connection, "new", "new.sql", "sql").unwrap(),
            "new.sql"
        );

        // 存在しないファイルのリネームはエラー
        assert!(rename_query_file(&dir, connection, "missing", "x", "sql").is_err());

        // 不正な新名は拒否 (パストラバーサル)
        assert!(rename_query_file(&dir, connection, "new", "../evil", "sql").is_err());
        assert!(rename_query_file(&dir, connection, "new", "a/b", "sql").is_err());

        // 大文字小文字違いの別ファイルへの改名は拒否 (case-insensitive 判定)
        assert!(rename_query_file(&dir, connection, "new", "OTHER", "sql").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rename_query_file_case_only() {
        let dir = test_dir().join("rename-case");
        let connection = "test-conn";

        create_query_file(&dir, connection, "Report", "sql").unwrap();
        write_query_file(&dir, connection, "Report", "SELECT 2;", "sql").unwrap();

        // 自分自身の大文字小文字だけを変える改名は許可される
        let renamed =
            rename_query_file(&dir, connection, "Report", "report", "sql").unwrap();
        assert_eq!(renamed, "report.sql");
        assert_eq!(
            read_query_file(&dir, connection, "report", "sql").unwrap(),
            "SELECT 2;"
        );
        // case-insensitive FS では 1 ファイルのまま、case-sensitive FS でも
        // 旧名は残らない (rename 済み)
        let files = list_query_files(&dir, connection, "sql").unwrap();
        assert!(files.iter().any(|f| f.eq_ignore_ascii_case("report.sql")));
        assert!(!files.contains(&"Report.sql".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_search_query_files() {
        let dir = test_dir().join("search");
        let connection = "test-conn";

        create_query_file(&dir, connection, "users report", "sql").unwrap();
        write_query_file(
            &dir,
            connection,
            "users report",
            "SELECT * FROM users WHERE active = 1;",
            "sql",
        )
        .unwrap();
        create_query_file(&dir, connection, "orders", "sql").unwrap();
        write_query_file(
            &dir,
            connection,
            "orders",
            "SELECT id, total FROM orders;",
            "sql",
        )
        .unwrap();

        // 空クエリは空
        assert!(search_query_files(&dir, connection, "  ", "sql").unwrap().is_empty());

        // ファイル名一致 (大文字小文字を区別しない)
        let hits = search_query_files(&dir, connection, "USERS", "sql").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_name, "users report.sql");
        assert!(hits[0].name_match);
        // "users" は中身にもあるのでプレビューが付く
        assert!(hits[0].content_preview.as_deref().unwrap().contains("users"));

        // 中身のみ一致 (ファイル名は "orders" だが中身に total がある)
        let hits = search_query_files(&dir, connection, "total", "sql").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_name, "orders.sql");
        assert!(!hits[0].name_match);
        assert_eq!(
            hits[0].content_preview.as_deref(),
            Some("SELECT id, total FROM orders;")
        );

        // どちらにも無い語は 0 件
        assert!(search_query_files(&dir, connection, "zzz", "sql").unwrap().is_empty());

        // 手動配置された隠し .sql は検索対象外 (中身プレビューを漏らさない)。
        // validate_component が dot 始まりを拒否するため create 経由では作れないので
        // 直接ファイルを書き込んで再現する
        fs::write(
            connection_dir(&dir, connection).unwrap().join(".secret.sql"),
            "SELECT secret_total FROM vault;",
        )
        .unwrap();
        assert!(search_query_files(&dir, connection, "secret", "sql")
            .unwrap()
            .is_empty());
        assert!(!list_query_files(&dir, connection, "sql")
            .unwrap()
            .iter()
            .any(|f| f.starts_with('.')));

        // 存在しない接続ディレクトリは 0 件
        assert!(search_query_files(&dir, "no-such-conn", "users", "sql")
            .unwrap()
            .is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_truncate_preview() {
        assert_eq!(truncate_preview("  SELECT 1  "), "SELECT 1");
        let long = "x".repeat(200);
        let out = truncate_preview(&long);
        assert_eq!(out.chars().count(), PREVIEW_MAX_CHARS + 1); // +1 は省略記号
        assert!(out.ends_with('…'));
    }
}
