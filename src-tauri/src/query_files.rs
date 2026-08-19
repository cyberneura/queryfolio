use std::cmp::Ordering;
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
pub(crate) fn normalize_file_name(name: &str, ext: &str) -> Result<String, AppError> {
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

/// 数字の並びを数値として扱う比較 (自然順)。
///
/// 素の辞書順だと桁数の違う連番が作成順とズレる: 同一分に複数作った時の連番は
/// ゼロ埋めされないため (FilesPane の defaultFileName)、"-9" と "-10" では
/// '1' < '9' により降順で "-9" が先に来る = 古い方が上に並んでしまう。
/// 数字の並びをまとめて数値として比較することでこれを避ける。
///
/// 値が同じ数字列 ("02" と "2") は、比較を決定的にするため桁数の少ない方を先にする。
fn natural_cmp(a: &str, b: &str) -> Ordering {
    /// 先頭から続く ASCII 数字を消費して返す。
    fn take_digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
        let mut out = String::new();
        while let Some(c) = it.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            out.push(*c);
            it.next();
        }
        out
    }

    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            // 前方一致する短い方を小さいとみなす (辞書順と同じ)
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let da = take_digits(&mut ai);
                    let db = take_digits(&mut bi);
                    // 先頭ゼロを除けば「桁数 → 辞書順」で数値の大小になる
                    // (u64 へのパースだと極端に長い数字列で溢れる)
                    let ta = da.trim_start_matches('0');
                    let tb = db.trim_start_matches('0');
                    let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    let ord = da.len().cmp(&db.len());
                    if ord != Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = x.cmp(&y);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
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
    // 降順 (新しいものが先頭)。比較は**拡張子を除いた部分**を自然順で行う。
    //
    // 拡張子を付けたまま比べると、同じ分に作られた連番ファイル
    // ("20260804-1200.sql" と "20260804-1200-2.sql") の順序が逆になる:
    // '.' (0x2E) > '-' (0x2D) なので、降順では古い無印の方が先に来てしまう。
    // 拡張子を落とせば "20260804-1200-2" > "20260804-1200" (前方一致する
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
        natural_cmp(b_stem, a_stem).then_with(|| b.cmp(a))
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

/// 保存領域のディレクトリを作る。**Unix では自分が作った階層だけ**を 0700 にする。
///
/// クエリファイルには WHERE の実値・テーブル名・DDL が入り、同じ内容を保存する
/// 実行履歴 (history.rs) は 0700 / 0600 で明示的に絞ってある。作成時のモードを
/// 指定しないと umask 既定 (通常 0755 / 0644) になり、同一ホストの他ユーザーから
/// 読めてしまう (CYBERNEURA-DEV-510)。Windows ではモードの概念が無く、ACL は
/// 従来どおり親ディレクトリからの継承に任せる。
///
/// `create_dir_all` + `set_permissions` にしないのは 2 つの理由から:
/// - `create_dir_all` は途中の階層 (保存ルート等) も作るが、モードを設定できるのは
///   最終ディレクトリだけで、間の階層が umask 既定のまま残る
/// - 「存在するか」を見てから作ると、その間に他プロセスが作ったディレクトリまで
///   締め直してしまう (相手がより厳しく作っていた場合は逆に緩める)
///
/// 代わりに階層ごとに `create_dir` を使う。「無ければ作る」が 1 回のシステムコールで
/// 完結し、既にあれば `AlreadyExists` が返るので、**自分が作った時だけ**モードを
/// 設定できる。既存ディレクトリのモードは変えない (利用者が意図的に緩めた設定を
/// 後から締め直さない。書き込み時に既存のパーミッションを引き継ぐ write_file_atomic と
/// 同じ考え方)。
fn ensure_dir_700(dir: &Path) -> Result<(), AppError> {
    // 作る必要がある階層を、深い方から順に集める。
    let mut missing = Vec::new();
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.as_os_str().is_empty() || path.is_dir() {
            break;
        }
        missing.push(path);
        current = path.parent();
    }

    // 浅い方から作る。
    for path in missing.iter().rev() {
        match fs::create_dir(path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
                }
            }
            // 競合して他プロセスが先に作った。モードは相手のものを尊重する。
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// 新規作成するための共通オプション (`O_EXCL`)。**Unix ではモードを 600 にする。**
///
/// `create_new` なので既存は潰さない。history.rs の `open_options_600` と同じ意図で、
/// 新規作成分のモードだけを絞る。Windows ではモード指定が効かず、ACL は従来どおり。
fn create_new_options_600() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// 同一ディレクトリの一時ファイルへ書いてから rename で置き換える。
///
/// `fs::write` は宛先を truncate してから書き足すため、書いている途中の状態が
/// ディスク上に見える。CLI (`queryfolio write`) は**アプリ本体とは別プロセス**なので、
/// 同じファイルへの同時書き込みや、実行中インスタンスの外部変更ウォッチャによる
/// 読み取りが書き込みと重なりうる。その窓では中途半端な内容が読まれたり、
/// 2 つの書き手の内容が混ざった壊れたクエリが残ったりする。
/// rename は同一ファイルシステム内では atomic なので、読み手には「書き込み前」か
/// 「書き込み後」のどちらかしか見えない (書き手同士も最後の 1 つが丸ごと勝つ)。
///
/// 一時ファイルは**必ず同じディレクトリに置く** (別ファイルシステムだと rename が
/// EXDEV で失敗する)。名前はドット始まり + `.tmp` 拡張子なので、一覧・検索
/// (list_query_file_names) には出ない。
///
/// Windows の `fs::rename` も `MOVEFILE_REPLACE_EXISTING` 相当で既存を置き換える
/// (`move_query_file` が予約済みの空ファイルを rename で潰しているのと同じ前提)。
fn write_file_atomic(path: &Path, content: &str) -> Result<(), AppError> {
    /// 一時ファイル名が埋まっていた時に作り直す回数の上限。
    /// 名前は pid + 連番なので通常は 1 回目で成功する (残骸やリンクがある時だけ進む)。
    const MAX_TMP_FILE_ATTEMPTS: usize = 8;

    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    /// 同一プロセス内で同じファイルへ同時に書いた時に一時ファイル名が衝突しないようにする
    /// (プロセス間は pid で分かれる)。
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .ok_or_else(|| AppError::QueryFile(format!("Invalid file path: {}", path.display())))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AppError::QueryFile(format!("Invalid file name: {}", path.display())))?;
    // 一時ファイルは `create_new` (`O_EXCL`) で作る。`File::create` はシンボリック
    // リンクを辿って truncate するため、クラッシュで残った同名ファイルや、
    // 事前に置かれたリンクがあると保存領域の外を壊しうる。名前が使われていたら
    // 連番を進めて作り直す (同一プロセス内の同時書き込みや残骸との衝突)。
    let mut tmp_path = PathBuf::new();
    let mut file = None;
    for _ in 0..MAX_TMP_FILE_ATTEMPTS {
        let candidate = parent.join(format!(
            ".{file_name}.{}-{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        match create_new_options_600().open(&candidate) {
            Ok(f) => {
                tmp_path = candidate;
                file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let Some(mut file) = file else {
        return Err(AppError::QueryFile(format!(
            "Could not create a temporary file to write: {}",
            path.display()
        )));
    };

    let written = (|| -> std::io::Result<()> {
        // 既存ファイルのパーミッションを引き継ぐ (rename は中身ごと属性を差し替えるため、
        // 引き継がないと利用者が 600 に絞ったクエリファイルが umask 既定に戻ってしまう)。
        #[cfg(unix)]
        if let Ok(meta) = fs::metadata(path) {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(meta.permissions().mode()));
        }
        file.write_all(content.as_bytes())?;
        // rename の前に内容をディスクへ落とす (先に rename が耐久化されると、
        // クラッシュ時に「名前はあるが中身が空」のファイルが残りうる)。
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
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
        ensure_dir_700(parent)?;
    }
    write_file_atomic(&path, content)?;
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
        ensure_dir_700(parent)?;
    }
    write_file_atomic(&path, content)?;
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
        ensure_dir_700(parent)?;
    }
    // `create_new` (`O_EXCL`) にすることで、上の存在確認と作成の間に他プロセスが
    // 作ったファイルを潰さない (`fs::write` は既存を truncate する)。
    create_new_options_600().open(&path)?;
    Ok(normalized)
}

/// `file` をシンボリックリンク解決込みで canonicalize し、`base` (これも
/// canonicalize したもの) の配下に留まることを確かめる。保存領域外の実体を指す
/// リンクを弾く多重防御。対象は既存ファイルのはずなので、canonicalize
/// できない (存在しない等) 場合は拒否する。
pub fn verify_within_dir(base: &Path, file: &Path) -> Result<(), AppError> {
    let canonical_base = base.canonicalize().map_err(|e| {
        AppError::QueryFile(format!("Cannot resolve the query files directory: {e}"))
    })?;
    let canonical_file = file
        .canonicalize()
        .map_err(|e| AppError::QueryFile(format!("Cannot open the file: {e}")))?;
    if !canonical_file.starts_with(&canonical_base) {
        return Err(AppError::QueryFile(
            "The file resolves outside the query files directory".into(),
        ));
    }
    Ok(())
}

/// クエリファイルが無ければ空で作る (あれば内容はそのまま)。
/// 正規化されたファイル名を返す。
///
/// `create_query_file` との違いは**既存を上書きも失敗もしない**こと。
/// CLI (`queryfolio write <connection> <file-name>`) で内容を省略した時に使う:
/// 「まだ無ければ作って開く / あればそのまま開く」が期待される挙動で、
/// 既存の内容を空で潰してはいけない。
///
/// 作成は `create_new` (`O_EXCL`) で行い、存在確認と作成の間に他プロセスが
/// 同名ファイルを作った場合も既存扱いにする (中身を消さない)。
/// ただし既存が通常ファイルでない場合はエラーにする (下記参照)。
pub fn ensure_query_file(
    sqlfiles_dir: &Path,
    connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let normalized = normalize_file_name(file_name, ext)?;
    let path = file_path(sqlfiles_dir, connection, &normalized, ext)?;
    if let Some(parent) = path.parent() {
        ensure_dir_700(parent)?;
    }
    match create_new_options_600().open(&path) {
        Ok(_) => Ok(normalized),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // `O_EXCL` は「通常ファイルが既にある」時だけでなく、ディレクトリ・
            // 壊れたシンボリックリンク・FIFO 等が同名で存在する時も AlreadyExists を返す。
            // そのまま成功として返すと、CLI (`queryfolio write`) は書き出しに成功したつもりで
            // 起動を続け、実際の失敗はフロントの読み込み時まで遅れる = 呼び出したエージェントは
            // 終了ステータスから失敗を判別できない。開けるクエリファイルが本当にあるかを
            // ここで確かめる (metadata はリンクを辿るので、壊れたリンクは Err になる)。
            let meta = fs::metadata(&path).map_err(|e| {
                AppError::QueryFile(format!(
                    "The existing entry cannot be opened as a query file: {} ({e})",
                    path.display()
                ))
            })?;
            if !meta.is_file() {
                return Err(AppError::QueryFile(format!(
                    "The path already exists and is not a regular file: {}",
                    path.display()
                )));
            }
            // `metadata` はリンクを辿るため、**保存領域の外にある通常ファイル**を
            // 指すシンボリックリンクは上の is_file() を通ってしまう。実行中インスタンス
            // 側は同じ対象を verify_within_dir で拒否するので、ここで通すと
            // 「CLI は終了ステータス 0 なのにファイルは開かれない」になる。
            // 開く側と同じ判定をここでも行い、成功の意味を揃える。
            verify_within_dir(sqlfiles_dir, &path)?;
            Ok(normalized)
        }
        Err(e) => Err(e.into()),
    }
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

/// クエリファイルを別の接続のフォルダへ移動し、正規化されたファイル名を返す。
/// 移動元と移動先が同じフォルダを指す場合は no-op で名前を返す
/// (別々の接続でも folder_name が同じなら同じフォルダになりうる)。
///
/// 拡張子はエンジンごとに違うので、呼び出し側 (lib.rs) が移動元と移動先で
/// 同じであることを確認してから呼ぶ。ここでは 1 つの ext として扱う。
pub fn move_query_file(
    sqlfiles_dir: &Path,
    from_connection: &str,
    to_connection: &str,
    file_name: &str,
    ext: &str,
) -> Result<String, AppError> {
    let normalized = normalize_file_name(file_name, ext)?;
    let from_dir = connection_dir(sqlfiles_dir, from_connection)?;
    let to_dir = connection_dir(sqlfiles_dir, to_connection)?;

    // 存在確認は同一フォルダの判定より先に行う。後にすると、存在しない
    // ファイルの移動が「成功」として返ってしまう。
    let from_path = from_dir.join(&normalized);
    if !from_path.exists() {
        return Err(AppError::QueryFile(format!(
            "File not found: {}",
            from_path.display()
        )));
    }
    if from_dir == to_dir {
        return Ok(normalized);
    }

    ensure_dir_700(&to_dir)?;

    // 大文字小文字だけが違う同名ファイルを先に弾く (case-insensitive FS の実挙動
    // と揃え、rename_query_file の判定とも一致させる)。列挙に失敗したら
    // 「衝突なし」とはみなさずエラーにする (見落としたまま移動しない)。
    let lower = normalized.to_ascii_lowercase();
    for entry in fs::read_dir(&to_dir)? {
        let Ok(name) = entry?.file_name().into_string() else {
            continue;
        };
        if name.to_ascii_lowercase() == lower {
            return Err(AppError::QueryFile(format!(
                "A file with the same name already exists at the destination: {normalized}"
            )));
        }
    }

    // **移動先の名前を atomic に予約してから rename する。**
    // Unix の rename は移動先が存在しても黙って置き換えるため、上の存在確認と
    // rename の間に同名ファイルが作られると (並行した移動・外部からの作成)
    // そのファイルを失う。O_EXCL の作成なら「無ければ作る」が atomic なので、
    // 予約に成功した = その名前は自分のものだと確定できる。
    let to_path = to_dir.join(&normalized);
    match create_new_options_600().open(&to_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(AppError::QueryFile(format!(
                "A file with the same name already exists at the destination: {normalized}"
            )));
        }
        Err(e) => return Err(e.into()),
    }

    // 移動元・移動先とも sqlfiles_dir の直下なので同一ファイルシステムになり、
    // rename が使える (EXDEV でのコピー + 削除のフォールバックは不要)。
    // ここで置き換えられるのは自分が予約した空ファイルだけ。
    if let Err(e) = fs::rename(&from_path, &to_path) {
        // 予約した空ファイルを残さない (残すと次の移動が衝突で失敗し続ける)
        let _ = fs::remove_file(&to_path);
        return Err(e.into());
    }
    Ok(normalized)
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
    fn test_ensure_query_file_creates_and_keeps_existing() {
        let dir = test_dir().join("ensure");
        let connection = "conn";

        // 無ければ空で作る (拡張子も補う)
        assert_eq!(
            ensure_query_file(&dir, connection, "report", "sql").unwrap(),
            "report.sql"
        );
        assert_eq!(read_query_file(&dir, connection, "report", "sql").unwrap(), "");

        // 既存の内容は消さない (create_query_file と違いエラーにもしない)
        write_query_file(&dir, connection, "report.sql", "SELECT 1;", "sql").unwrap();
        assert_eq!(
            ensure_query_file(&dir, connection, "report.sql", "sql").unwrap(),
            "report.sql"
        );
        assert_eq!(
            read_query_file(&dir, connection, "report", "sql").unwrap(),
            "SELECT 1;"
        );

        // 不正な名前は作らずエラー
        assert!(ensure_query_file(&dir, connection, "../evil", "sql").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    /// 同名でディレクトリ / 壊れたシンボリックリンクが居座っている時は、
    /// 「既にあるので OK」ではなくエラーにする (開けるクエリファイルが無いため)。
    #[test]
    fn test_ensure_query_file_rejects_non_file() {
        let dir = test_dir().join("ensure-non-file");
        let connection = "conn";
        let conn_dir = connection_dir(&dir, connection).unwrap();
        fs::create_dir_all(&conn_dir).unwrap();

        // ディレクトリ
        fs::create_dir(conn_dir.join("dir-entry.sql")).unwrap();
        assert!(ensure_query_file(&dir, connection, "dir-entry", "sql").is_err());

        // 壊れたシンボリックリンク (リンク先が無い)
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                conn_dir.join("missing-target.sql"),
                conn_dir.join("dangling.sql"),
            )
            .unwrap();
            assert!(ensure_query_file(&dir, connection, "dangling", "sql").is_err());
        }

        // 保存領域の外にある**実在する通常ファイル**を指すリンク。
        // metadata はリンクを辿るので is_file() は通ってしまうが、実行中
        // インスタンス側は verify_within_dir で拒否する。ここで成功にすると
        // 「CLI は 0 で終わったのにファイルは開かれない」になる。
        #[cfg(unix)]
        {
            let outside = test_dir().join("ensure-non-file-outside");
            fs::create_dir_all(&outside).unwrap();
            let target = outside.join("real.sql");
            fs::write(&target, "SELECT 1;").unwrap();
            std::os::unix::fs::symlink(&target, conn_dir.join("escaping.sql")).unwrap();
            assert!(ensure_query_file(&dir, connection, "escaping", "sql").is_err());

            // 保存領域の**中**を指すリンクは、開く側 (verify_within_dir) が
            // 受け入れるのでこちらも受け入れる (判定を食い違わせない)。
            let inside = conn_dir.join("inside.sql");
            fs::write(&inside, "SELECT 1;").unwrap();
            std::os::unix::fs::symlink(&inside, conn_dir.join("linked.sql")).unwrap();
            assert!(ensure_query_file(&dir, connection, "linked", "sql").is_ok());

            let _ = fs::remove_dir_all(&outside);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    /// 新規作成分のパーミッションを 0600 / ディレクトリを 0700 に絞る
    /// (CYBERNEURA-DEV-510)。同じ内容を保存する history.rs と水準を揃える。
    #[cfg(unix)]
    #[test]
    fn test_new_query_files_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir().join("permissions");
        let connection = "conn";

        let mode_of = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

        // create_query_file (空ファイルの新規作成)。保存ルートごと未作成の状態から始める
        assert!(!dir.exists());
        create_query_file(&dir, connection, "created", "sql").unwrap();
        let conn_dir = connection_dir(&dir, connection).unwrap();
        assert_eq!(
            mode_of(&dir),
            0o700,
            "保存ルートも 0700 (途中の階層を残さない)"
        );
        assert_eq!(mode_of(&conn_dir), 0o700, "接続ディレクトリは 0700");
        assert_eq!(mode_of(&conn_dir.join("created.sql")), 0o600);

        // ensure_query_file (CLI が「無ければ作る」に使う経路)
        ensure_query_file(&dir, connection, "ensured", "sql").unwrap();
        assert_eq!(mode_of(&conn_dir.join("ensured.sql")), 0o600);

        // write_query_file (一時ファイル + rename。新規作成なので引き継ぐ元が無い)
        write_query_file(&dir, connection, "written", "SELECT 1;", "sql").unwrap();
        assert_eq!(mode_of(&conn_dir.join("written.sql")), 0o600);

        // 既存ディレクトリのモードは変えない (利用者が緩めた設定を勝手に締め直さない)
        fs::set_permissions(&conn_dir, fs::Permissions::from_mode(0o755)).unwrap();
        write_query_file(&dir, connection, "second", "SELECT 2;", "sql").unwrap();
        assert_eq!(
            mode_of(&conn_dir),
            0o755,
            "既存ディレクトリのパーミッションは変更しないこと"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 書き込みは一時ファイル + rename で行い、宛先を途中の状態で晒さない。
    /// 一時ファイルを残さないこと・既存のパーミッションを引き継ぐことも確認する。
    #[test]
    fn test_write_query_file_is_atomic() {
        let dir = test_dir().join("atomic");
        let connection = "conn";

        write_query_file(&dir, connection, "report", "SELECT 1;", "sql").unwrap();
        let conn_dir = connection_dir(&dir, connection).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = conn_dir.join("report.sql");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            write_query_file(&dir, connection, "report", "SELECT 2;", "sql").unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "既存ファイルのパーミッションを引き継ぐこと"
            );
        }

        assert_eq!(
            read_query_file(&dir, connection, "report", "sql").unwrap(),
            if cfg!(unix) { "SELECT 2;" } else { "SELECT 1;" }
        );

        // 一時ファイルが残っていない (残ると一覧・検索には出ないがゴミになる)
        let leftovers: Vec<String> = fs::read_dir(&conn_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "一時ファイルが残っている: {leftovers:?}"
        );

        let _ = fs::remove_dir_all(&dir);
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
    fn test_natural_cmp() {
        // 数字列は数値として比較する (桁数が違っても作成順どおり)
        assert_eq!(natural_cmp("a-9", "a-10"), Ordering::Less);
        assert_eq!(natural_cmp("a-10", "a-9"), Ordering::Greater);
        assert_eq!(natural_cmp("a-100", "a-99"), Ordering::Greater);
        // 先頭ゼロがあっても値で比較する。値が同じなら桁数の少ない方を先に
        assert_eq!(natural_cmp("a-02", "a-9"), Ordering::Less);
        assert_eq!(natural_cmp("a-2", "a-02"), Ordering::Less);
        // 数字以外は通常の文字比較。前方一致する短い方が小さい
        assert_eq!(natural_cmp("20260102-1200", "20260102-1200-2"), Ordering::Less);
        assert_eq!(natural_cmp("report", "report"), Ordering::Equal);
        assert_eq!(natural_cmp("apple", "banana"), Ordering::Less);
        // 日付部分も数値として比較される (桁数が同じなので辞書順と一致する)
        assert_eq!(natural_cmp("20260102-1200", "20260315-1830"), Ordering::Less);
    }

    #[test]
    fn test_list_query_files_sorts_newest_first() {
        let dir = test_dir().join("order");
        let connection = "order-conn";

        // 既定のファイル名は YYYYMMDD-HHMM なので、名前の降順 = 新しい順になる。
        // 作成順と無関係に並ぶことを見るため、わざと時系列とズラして作る。
        // 末尾の -2 / -10 は同一分内に複数作った時の連番 (FilesPane)。
        // 拡張子を含めて比較すると無印 (最も古い) が連番より前に来てしまい、
        // 辞書順で比較すると -2 が -10 より前に来てしまう。その両方の退行を見る。
        create_query_file(&dir, connection, "20260101-0900", "sql").unwrap();
        create_query_file(&dir, connection, "20260315-1830", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200-2", "sql").unwrap();
        create_query_file(&dir, connection, "20260102-1200-10", "sql").unwrap();

        let expected = vec![
            "20260315-1830.sql",
            "20260102-1200-10.sql",
            "20260102-1200-2.sql",
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
    fn test_move_query_file() {
        let dir = test_dir().join("move");
        let from = "from-conn";
        let to = "to-conn";

        create_query_file(&dir, from, "report", "sql").unwrap();
        write_query_file(&dir, from, "report", "SELECT 1;", "sql").unwrap();

        // 移動先フォルダがまだ無くても作られる。内容は保持される
        assert_eq!(
            move_query_file(&dir, from, to, "report", "sql").unwrap(),
            "report.sql"
        );
        assert!(list_query_files(&dir, from, "sql").unwrap().is_empty());
        assert_eq!(list_query_files(&dir, to, "sql").unwrap(), vec!["report.sql"]);
        assert_eq!(
            read_query_file(&dir, to, "report", "sql").unwrap(),
            "SELECT 1;"
        );

        // 移動元に無いファイルはエラー (同一フォルダ指定でも成功にしない)
        assert!(move_query_file(&dir, from, to, "report", "sql").is_err());
        assert!(move_query_file(&dir, from, from, "report", "sql").is_err());

        // 移動先に同名があればエラー (移動元は残る)
        create_query_file(&dir, from, "report", "sql").unwrap();
        assert!(move_query_file(&dir, from, to, "report", "sql").is_err());
        assert_eq!(
            list_query_files(&dir, from, "sql").unwrap(),
            vec!["report.sql"]
        );
        // 大文字小文字違いも同名扱い (移動元と移動先は別フォルダなので、
        // case-insensitive な FS でも両方を作れる)
        create_query_file(&dir, from, "Sales", "sql").unwrap();
        create_query_file(&dir, to, "sales", "sql").unwrap();
        assert!(move_query_file(&dir, from, to, "Sales", "sql").is_err());

        // 同じフォルダへの移動は no-op (別接続でも folder_name が同じことがある)
        assert_eq!(
            move_query_file(&dir, from, from, "report", "sql").unwrap(),
            "report.sql"
        );
        assert_eq!(
            read_query_file(&dir, from, "report", "sql").unwrap(),
            ""
        );

        // パストラバーサルは拒否
        assert!(move_query_file(&dir, from, "../evil", "report", "sql").is_err());
        assert!(move_query_file(&dir, "../evil", to, "report", "sql").is_err());
        assert!(move_query_file(&dir, from, to, "../evil", "sql").is_err());

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
