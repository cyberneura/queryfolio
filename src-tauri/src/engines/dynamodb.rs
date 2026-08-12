//! DynamoDB エンジン。
//!
//! PartiQL (SQL 互換サブセット) を ExecuteStatement API で実行する。
//! エディタは通常の SQL (editor_language "sql" / 拡張子 .sql) で、
//! readonly / dangerous ガードは db.rs の SQL 系ロジックをそのまま再利用する
//! (PartiQL は SELECT / INSERT / UPDATE / DELETE のみ。scan_sql の方言は
//! 標準 SQL 相当で足りる。ダブルクォート識別子は scan_sql が文字列として
//! 空白化するが、WHERE 等のキーワードはクォートされないため判定に影響しない)。
//!
//! - 接続: `schema` = AWS リージョン (必須)。`host` / `port` は dynamodb-local
//!   等のエンドポイント上書き (省略時は AWS の標準エンドポイント)。認証は
//!   user / password (静的なアクセスキー) → `aws_profile` → 既定の
//!   credentials chain の順に解決する。
//! - PartiQL に LIMIT 句が無いため auto LIMIT は付与せず、ExecuteStatement の
//!   `limit` パラメータ + NextToken ページネーションで max_rows + 1 件まで
//!   取得して打ち切り、truncated を報告する。
//! - INSERT / UPDATE / DELETE は API から影響行数が取れないため
//!   affected_rows = None + 空結果を返す (`UPDATE ... RETURNING ALL OLD *` の
//!   ように Items が返る文はそのまま表形式にする)。
//! - すべてのリクエストに SDK のタイムアウト (接続 15 秒 / 操作 120 秒) を
//!   掛け、ページネーション全体にも 120 秒の締切を置く (フィルタの強い
//!   SELECT はスキャンの空ページが延々続き得るため)。
//! - キャンセルはクライアント側で future を打ち切る (`CancelTarget::ClientSide`)。
//!   接続プールを持たないため打ち切りで壊れる状態は無い。
//! - HTTPS クライアントは既存依存と同じ ring ベースの rustls を明示する
//!   (SDK 既定の aws-lc はネイティブビルド (cmake / NASM) を CI に増やすため)。

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use aws_sdk_dynamodb::types::{AttributeValue, KeyType};

use crate::config::ServerConfig;
use crate::db::{
    bytes_to_json, dangerous_block_error, dangerous_reason, is_readonly_allowed,
    json_i64, leading_keyword, readonly_block_error, CancelRegistry, CancelTarget,
    Engine, QueryResult, ReadonlyGuard,
};
use crate::error::AppError;
use crate::schema_info::{ColumnInfo, TableInfo};

/// 接続 (TCP) と接続確認 (ListTables) のタイムアウト。
/// タイムアウト無しの確認リクエストは get_pool (DbManager のロック保持中) を
/// 無期限に止めるため必須。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 1 回の API 操作のタイムアウト (SDK の operation timeout)。
/// ページネーション全体の締切にも同じ値を使う。
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// エンドポイント上書き (host 指定) 時に port 省略なら dynamodb-local の既定
/// ポートを使う。
const DEFAULT_LOCAL_PORT: u16 = 8000;

/// TABLES ペインに出すテーブル一覧の件数上限 (非有界の一覧を作らない)。
const MAX_TABLES: usize = 5000;

/// ListTables 1 ページの取得件数 (API 上限は 100)。
const LIST_TABLES_PAGE: i32 = 100;

/// 1 セルに入れるコレクション (L / M / SS / NS / BS) の要素数の共有予算。
/// 階層ごとの独立上限ではなくセル全体で共有する
/// (1,000 × 1,000 のネストで 100 万値を直列化しない)。
const MAX_CELL_ELEMENTS: usize = 1000;

/// 結果テーブルのカラム数上限。DynamoDB はスキーマレスでアイテムごとに
/// 属性集合が違うため、疎なアイテム群の union でカラムが爆発し得る
/// (1,000 行 × 1,000 属性なら 100 万セルを NULL 充填してしまう)。
/// 名前昇順の先頭からこの数で打ち切り、truncated を報告する。
const MAX_COLUMNS: usize = 500;

/// 1 セルに入れる文字列 (S / B) の文字数上限。超過は打ち切り + truncated。
const MAX_TEXT_CHARS: usize = 10_000;

/// ネスト値 (L / M) を JSON 化する再帰の深さ上限 (スタック保護)。
const MAX_NESTING_DEPTH: usize = 32;

/// DynamoDB への接続クライアント。DbPool::DynamoDb として保持される。
/// SDK クライアントは内部に HTTP コネクションプールを持つ。
#[derive(Clone)]
pub struct DynamoClient {
    client: aws_sdk_dynamodb::Client,
}

impl std::fmt::Debug for DynamoClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DynamoClient")
    }
}

/// SDK のエラーをアプリのエラー型へ。DisplayErrorContext でエラーチェーン
/// (サービスエラーの種別・メッセージ) まで展開する。SDK のエラーに認証情報や
/// 署名は含まれないため、そのまま表示してよい。
fn sdk_error(context: &str, e: impl std::error::Error) -> AppError {
    AppError::DynamoDb(format!(
        "{context}: {}",
        aws_sdk_dynamodb::error::DisplayErrorContext(&e)
    ))
}

/// ring ベースの rustls HTTPS クライアントを組む。
/// SDK 既定の aws-lc (aws-lc-sys) はネイティブビルドに cmake / NASM を要し
/// CI (macOS universal / Windows) のビルドリスクになるため、既存依存
/// (reqwest / rustls) と同じ ring を明示する。
/// (SharedHttpClient 型は SDK の config モジュールの re-export を使い、
/// aws-smithy-runtime-api への直接依存を増やさない)
fn build_http_client() -> aws_sdk_dynamodb::config::SharedHttpClient {
    aws_smithy_http_client::Builder::new()
        .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
            aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
        ))
        .build_https()
}

/// 設定から SDK クライアントを組み立てる (接続確認はしない)。
async fn build_client(server: &ServerConfig) -> Result<DynamoClient, AppError> {
    // schema = AWS リージョン (必須)。dynamodb-local でも SigV4 署名に
    // リージョン名が要るため省略は設定エラーにする
    let region = server
        .schema
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Config(
                "For dynamodb, set schema to the AWS region (e.g. ap-northeast-1)"
                    .into(),
            )
        })?;

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()))
        .http_client(build_http_client())
        .timeout_config(
            aws_config::timeout::TimeoutConfig::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .operation_timeout(REQUEST_TIMEOUT)
                .build(),
        );

    // 認証の解決順: user / password (静的なアクセスキー ID / シークレット) →
    // aws_profile (~/.aws のプロファイル) → 既定の credentials chain
    // (環境変数 → default プロファイル → IMDS)。
    // user / password は他エンジンと同じキーで書ける queryfolio 独自拡張
    // (dynamodb-local はダミー値でよい)。
    let user = server.user.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let password = server.password.as_deref().filter(|s| !s.is_empty());
    match (user, password) {
        (Some(access_key), Some(secret_key)) => {
            loader = loader.credentials_provider(
                aws_sdk_dynamodb::config::Credentials::new(
                    access_key,
                    secret_key,
                    None,
                    None,
                    "queryfolio-config",
                ),
            );
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(AppError::Config(
                "For dynamodb, set both user (access key ID) and password \
                 (secret access key), or neither"
                    .into(),
            ));
        }
        (None, None) => {
            if let Some(profile) = server
                .aws_profile
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                // profile_name だけでは既定チェーンの参照先を変えるだけで、
                // 環境変数 (AWS_ACCESS_KEY_ID 等) のプロバイダが先勝ちする。
                // aws_profile 指定時は明示のプロファイルプロバイダを立てて、
                // 「user/password → aws_profile → 既定チェーン」の優先順を
                // 環境に依らず成立させる
                loader = loader.credentials_provider(
                    aws_config::profile::ProfileFileCredentialsProvider::builder()
                        .profile_name(profile)
                        .build(),
                );
            }
        }
    }

    // host / port はエンドポイント上書き (dynamodb-local 用)。
    // tls: true で https (省略時 http。AWS 標準エンドポイントは host を
    // 書かなければ SDK が https で解決する)
    if let Some(host) = server
        .host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let scheme = if server.tls { "https" } else { "http" };
        let port = server.port.unwrap_or(DEFAULT_LOCAL_PORT);
        loader = loader.endpoint_url(format!("{scheme}://{host}:{port}"));
    }

    let sdk_config = loader.load().await;
    Ok(DynamoClient {
        client: aws_sdk_dynamodb::Client::new(&sdk_config),
    })
}

/// 接続を確立して疎通確認 (ListTables limit 1) まで行う。
/// 確認リクエストにもタイムアウトを掛ける: TCP は繋がるのに応答しない相手で
/// get_pool (DbManager のロック保持中) が無期限に停止しないようにする。
pub async fn connect(server: &ServerConfig) -> Result<DynamoClient, AppError> {
    let client = build_client(server).await?;
    let confirm = client.client.list_tables().limit(1).send();
    match tokio::time::timeout(CONNECT_TIMEOUT, confirm).await {
        Ok(Ok(_)) => Ok(client),
        Ok(Err(e)) => {
            // 最小権限の IAM (特定テーブルの ExecuteStatement のみ許可) では
            // ListTables が AccessDenied になる。それは「資格情報と到達性は
            // 正しいが権限が無い」状態なので、接続自体は成功として扱う
            // (TABLES ペインを開いた時に改めて権限エラーとして表示される)。
            // 資格情報不正 (UnrecognizedClient 等) やネットワークエラーは
            // 従来どおり接続エラーにする
            let text = format!("{:?}", e);
            if text.contains("AccessDenied") {
                return Ok(client);
            }
            Err(sdk_error("Failed to connect to DynamoDB", e))
        }
        Err(_) => Err(AppError::DynamoDb(format!(
            "DynamoDB did not respond within {}s",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

/// PartiQL 文を実行して結果を返す (キャンセル対応版)。
/// db::run_query_cancellable から DbPool::DynamoDb の場合に委譲される。
pub async fn run_query_cancellable(
    client: &DynamoClient,
    registry: &CancelRegistry,
    connection_name: &str,
    sql: &str,
    max_rows: usize,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    // psql 風メタコマンド (\...) は非対応 (translate が DynamoDb でエラーを返す)
    crate::meta_commands::translate(Engine::DynamoDb, sql)?;

    if leading_keyword(sql).is_empty() {
        return Err(AppError::Config("The SQL statement is empty".into()));
    }

    // `tables` はテーブル一覧を返す queryfolio 独自の文 (CYBERNEURA-DEV-406)。
    // PartiQL に SHOW TABLES に相当する構文が無く、DynamoDB へ投げても構文エラーに
    // なるため、ここで受けて ListTables に流す。ExecuteStatement を経由しない
    // 純粋な読み取りなので、readonly / dangerous ガードより前に処理してよい
    // (ガードに掛けると SQL の先頭キーワード判定で fetch 文と見なされず、
    // Writable OFF の接続で拒否されてしまう)。
    if is_tables_statement(sql) {
        return list_tables_query(client, registry, connection_name, max_rows).await;
    }

    // readonly / dangerous ガードは SQL 系の共通ロジックを実行前に全文へ適用する。
    // PartiQL は SELECT / INSERT / UPDATE / DELETE のみなので判定はそのまま使える。
    // 複文はガードが 1 文目しか見ないため、ガードが有効なら拒否する
    // (PartiQL 自体も 1 文しか受け付けないが、判定順を SQL 系と揃えておく)
    if (readonly != ReadonlyGuard::Off || !allow_dangerous)
        && crate::db::contains_multiple_statements(sql, Engine::DynamoDb)
    {
        return Err(crate::db::multi_statement_block_error());
    }
    if readonly != ReadonlyGuard::Off && !is_readonly_allowed(sql, Engine::DynamoDb) {
        return Err(readonly_block_error(readonly));
    }
    if !allow_dangerous {
        if let Some(reason) = dangerous_reason(sql, Engine::DynamoDb) {
            return Err(dangerous_block_error(reason));
        }
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let guard = registry.register(
        connection_name,
        CancelTarget::ClientSide {
            notify: notify.clone(),
        },
        cancelled,
    );
    let started = Instant::now();
    // キャンセルは実行の future を打ち切る。biased で実行結果側を先に見る:
    // 結果とキャンセル通知が同時に ready なら完了済みの結果を優先する
    let result = tokio::select! {
        biased;
        result = execute_statement(client, sql, max_rows) => result,
        _ = notify.notified() => Err(AppError::Cancelled),
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);
    // キャンセルが完了と競合した場合は成功結果をそのまま返す (SQL 側と同じ挙動)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let mut result = result?;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

/// ExecuteStatement を実行し、SELECT なら NextToken でページを辿って
/// max_rows + 1 件まで集める (超過分は shape_items が打ち切って truncated)。
async fn execute_statement(
    client: &DynamoClient,
    sql: &str,
    max_rows: usize,
) -> Result<QueryResult, AppError> {
    // limit パラメータとページネーションは読み取り (SELECT) にのみ意味がある。
    // 書き込み文 (INSERT / UPDATE / DELETE) は単一アイテム操作で 1 回で終わる
    let is_select = leading_keyword(sql) == "select";
    let started = Instant::now();
    let deadline_error = || {
        AppError::DynamoDb(format!(
            "The query did not finish within {}s \
             (narrow the statement, e.g. with a key condition)",
            REQUEST_TIMEOUT.as_secs()
        ))
    };
    let mut items: Vec<HashMap<String, AttributeValue>> = Vec::new();
    let mut next_token: Option<String> = None;
    loop {
        let mut req = client.client.execute_statement().statement(sql);
        if is_select {
            // limit は「評価するアイテム数」の上限。必要数 + 1 で truncated を
            // 検知する (i32 へは実用範囲で収まる値に clamp)
            let remaining = max_rows.saturating_add(1).saturating_sub(items.len());
            req = req.limit(remaining.min(i32::MAX as usize) as i32);
        }
        req = req.set_next_token(next_token.take());
        // フィルタの強い SELECT はスキャンの空ページが延々続き得るため、
        // ページネーション全体に締切を置く。各ページのリクエストを残余時間の
        // timeout で包むことで、締切をページ間だけでなくリクエスト実行中にも
        // 効かせる (1 操作の SDK タイムアウトと合算して ~2 倍待たされない)
        let remaining_time = REQUEST_TIMEOUT
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or_else(deadline_error)?;
        let out = tokio::time::timeout(remaining_time, req.send())
            .await
            .map_err(|_| deadline_error())?
            .map_err(|e| sdk_error("ExecuteStatement failed", e))?;
        items.extend(out.items.unwrap_or_default());
        next_token = out.next_token;
        if !is_select || next_token.is_none() || items.len() > max_rows {
            break;
        }
    }
    Ok(shape_items(items, max_rows))
}

/// Items (AttributeValue のマップ列) を表形式へ整形する。
/// columns は全アイテムのキーの union。SDK の Item は HashMap でキー順が
/// 不定のため、表示を確定的にするようソートして並べる。
/// 書き込み文で Items が空の場合は空結果 (affected_rows は API から取れない
/// ため None) になる。
fn shape_items(
    items: Vec<HashMap<String, AttributeValue>>,
    max_rows: usize,
) -> QueryResult {
    let mut truncated = items.len() > max_rows;
    let items = &items[..items.len().min(max_rows)];

    // カラム union は BTreeSet で収集しながら MAX_COLUMNS に抑える:
    // 上限超過のたびに最大要素を落とすことで、全 union を実体化せずに
    // 「名前昇順の先頭 MAX_COLUMNS 個」を O(N log MAX_COLUMNS) で確定させる
    // (スキーマレス由来の疎な属性群でも中間メモリと走査が有界)
    let mut column_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for item in items {
        for key in item.keys() {
            if column_set.contains(key) {
                continue;
            }
            column_set.insert(key.clone());
            if column_set.len() > MAX_COLUMNS {
                column_set.pop_last();
                truncated = true;
            }
        }
    }
    let columns: Vec<String> = column_set.into_iter().collect();

    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let mut row = Vec::with_capacity(columns.len());
        for column in &columns {
            match item.get(column) {
                Some(value) => row.push(attr_to_json_cell(value, &mut truncated)),
                // スキーマレスなのでアイテムに無い属性は NULL 扱い
                None => row.push(serde_json::Value::Null),
            }
        }
        rows.push(row);
    }

    QueryResult {
        row_count: rows.len(),
        columns,
        rows,
        affected_rows: None,
        truncated,
        elapsed_ms: 0,
        applied_limit: None,
        switched_schema: None,
    }
}

/// AttributeValue を JSON へ変換する (セル単位の共有予算付き)。
fn attr_to_json_cell(value: &AttributeValue, truncated: &mut bool) -> serde_json::Value {
    let mut budget = MAX_CELL_ELEMENTS;
    attr_to_json(value, truncated, 0, &mut budget)
}

/// N (数値) を JSON へ。DynamoDB の N は任意精度 (最大 38 桁) のため、
/// JS の安全整数範囲に収まる整数だけ数値にし、それ以外 (小数・巨大整数) は
/// 精度を保つため文字列のまま返す (invoke 境界の丸め対策)。
fn number_to_json(n: &str) -> serde_json::Value {
    if !n.contains(['.', 'e', 'E']) {
        if let Ok(v) = n.parse::<i64>() {
            // json_i64 が安全範囲外を文字列化する
            return json_i64(v);
        }
    }
    serde_json::Value::String(n.to_string())
}

/// 文字列を文字数上限で打ち切る (超えたら truncated を立てて省略記号を付ける)。
fn text_limited(v: &str, truncated: &mut bool) -> serde_json::Value {
    if v.chars().count() <= MAX_TEXT_CHARS {
        return serde_json::Value::String(v.to_string());
    }
    *truncated = true;
    let cut: String = v.chars().take(MAX_TEXT_CHARS).collect();
    serde_json::Value::String(format!("{cut}…"))
}

/// バイナリ (B / BS 要素) を JSON へ (bytes_to_json: UTF-8 なら文字列、
/// そうでなければ base64)。サイズ上限で先頭だけ変換して打ち切る。
fn blob_limited(bytes: &[u8], truncated: &mut bool) -> serde_json::Value {
    if bytes.len() > MAX_TEXT_CHARS {
        *truncated = true;
        match bytes_to_json(bytes[..MAX_TEXT_CHARS].to_vec()) {
            serde_json::Value::String(s) => serde_json::Value::String(format!("{s}…")),
            other => other,
        }
    } else {
        bytes_to_json(bytes.to_vec())
    }
}

fn attr_to_json(
    value: &AttributeValue,
    truncated: &mut bool,
    depth: usize,
    budget: &mut usize,
) -> serde_json::Value {
    // データ由来の任意深度ネスト (DynamoDB は 32 階層まで許す) で
    // スタックを溢れさせない
    if depth >= MAX_NESTING_DEPTH
        && matches!(value, AttributeValue::L(_) | AttributeValue::M(_))
    {
        *truncated = true;
        return serde_json::Value::String("… (nesting too deep, truncated)".into());
    }
    match value {
        AttributeValue::Null(_) => serde_json::Value::Null,
        AttributeValue::Bool(v) => serde_json::Value::Bool(*v),
        AttributeValue::S(v) => text_limited(v, truncated),
        AttributeValue::N(v) => number_to_json(v),
        AttributeValue::B(v) => blob_limited(v.as_ref(), truncated),
        AttributeValue::L(list) => {
            let mut out = Vec::new();
            for item in list {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(attr_to_json(item, truncated, depth + 1, budget));
            }
            serde_json::Value::Array(out)
        }
        AttributeValue::M(map) => {
            // HashMap のキー順は不定なのでソートして確定的に出す
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut obj = serde_json::Map::new();
            for key in keys {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                obj.insert(
                    key.clone(),
                    attr_to_json(&map[key], truncated, depth + 1, budget),
                );
            }
            serde_json::Value::Object(obj)
        }
        AttributeValue::Ss(list) => {
            let mut out = Vec::new();
            for item in list {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(text_limited(item, truncated));
            }
            serde_json::Value::Array(out)
        }
        AttributeValue::Ns(list) => {
            let mut out = Vec::new();
            for item in list {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(number_to_json(item));
            }
            serde_json::Value::Array(out)
        }
        AttributeValue::Bs(list) => {
            let mut out = Vec::new();
            for item in list {
                if *budget == 0 {
                    *truncated = true;
                    break;
                }
                *budget -= 1;
                out.push(blob_limited(item.as_ref(), truncated));
            }
            serde_json::Value::Array(out)
        }
        // AttributeValue は non_exhaustive (将来の型追加に備える)
        other => serde_json::Value::String(format!("<unsupported: {other:?}>")),
    }
}

/// DynamoDB のテーブル名を検証する (英数と `_ - .`、3〜255 文字)。
/// DescribeTable の API パラメータとして送るだけなのでインジェクション面は
/// 無いが、明らかな不正値 (タイプミス・混入) は API を呼ぶ前に弾く。
fn validate_table_name(name: &str) -> Result<&str, AppError> {
    let valid = (3..=255).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !valid {
        return Err(AppError::Config(format!(
            "Invalid table name: {name} \
             (DynamoDB table names use letters, digits, '_', '-', '.')"
        )));
    }
    Ok(name)
}

/// エディタの入力が `tables` (queryfolio 独自の文) か。
///
/// 末尾のセミコロンと前後の空白だけを許し、`tables where ...` のような続きは
/// 受け付けない (PartiQL の文と紛れないようにするため)。大小は区別しない。
pub(crate) fn is_tables_statement(sql: &str) -> bool {
    let trimmed = sql.trim();
    let body = trimmed.strip_suffix(';').unwrap_or(trimmed);
    body.trim_end().eq_ignore_ascii_case("tables")
}

/// `tables` 文の結果 (name / kind の表)。
///
/// キャンセルの扱いは PartiQL 経路と揃える (クライアント側で future を打ち切る)。
async fn list_tables_query(
    client: &DynamoClient,
    registry: &CancelRegistry,
    connection_name: &str,
    max_rows: usize,
) -> Result<QueryResult, AppError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let guard = registry.register(
        connection_name,
        CancelTarget::ClientSide {
            notify: notify.clone(),
        },
        cancelled,
    );
    let started = Instant::now();
    let result = tokio::select! {
        biased;
        // 表示は max_rows 件まで。truncated の判定に 1 件だけ多く取る
        result = fetch_tables_limited(client, max_rows.saturating_add(1)) => result,
        _ = notify.notified() => Err(AppError::Cancelled),
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let (tables, has_more) = result?;

    // 他の結果と同じく max_rows で打ち切り、切ったことを truncated で伝える。
    // ページネーションの締切で途中終了した場合も has_more で拾う
    // (部分的な一覧を完全なものとして扱わない)
    let truncated = has_more || tables.len() > max_rows;
    let rows: Vec<Vec<serde_json::Value>> = tables
        .into_iter()
        .take(max_rows)
        .map(|t| {
            vec![
                serde_json::Value::String(t.name),
                serde_json::Value::String(t.kind),
            ]
        })
        .collect();

    Ok(QueryResult {
        columns: vec!["name".to_string(), "kind".to_string()],
        row_count: rows.len(),
        rows,
        affected_rows: None,
        truncated,
        elapsed_ms: started.elapsed().as_millis() as u64,
        applied_limit: None,
        switched_schema: None,
    })
}

/// テーブル一覧 (スキーマブラウザの TABLES ペイン用)。
/// ListTables をページネーションで辿り、MAX_TABLES 件で打ち切る。
pub async fn fetch_tables(client: &DynamoClient) -> Result<Vec<TableInfo>, AppError> {
    Ok(fetch_tables_limited(client, MAX_TABLES).await?.0)
}

/// テーブル一覧を最大 `limit` 件まで取る。
///
/// `limit` を分けているのは、`tables` 文が `max_rows` までしか表示しないのに
/// MAX_TABLES (5,000) 件ぶんの ListTables を叩くのを避けるため
/// (CYBERNEURA-DEV-406)。ページ単位でしか止められないので、実際の取得数は
/// LIST_TABLES_PAGE の切り上げになる。
/// 返り値の bool は「まだ続きがある」フラグ。`limit` に達した場合と、
/// ページネーションの締切で打ち切った場合の両方で true になる。呼び出し側が
/// 結果を truncated として報告できるようにするため
/// (締切での打ち切りを黙って完全な一覧として扱わない)。
async fn fetch_tables_limited(
    client: &DynamoClient,
    limit: usize,
) -> Result<(Vec<TableInfo>, bool), AppError> {
    let started = Instant::now();
    let mut names: Vec<String> = Vec::new();
    let mut start_name: Option<String> = None;
    let mut has_more = false;
    loop {
        let out = client
            .client
            .list_tables()
            .set_exclusive_start_table_name(start_name.take())
            .limit(LIST_TABLES_PAGE)
            .send()
            .await
            .map_err(|e| sdk_error("ListTables failed", e))?;
        names.extend(out.table_names.unwrap_or_default());
        start_name = out.last_evaluated_table_name;
        if start_name.is_none() {
            break;
        }
        if names.len() >= limit {
            has_more = true;
            break;
        }
        // 1 操作ごとの SDK タイムアウトとは別に、一覧全体にも締切を置く。
        // ここで抜けた場合は続きが残っているので has_more を立てる
        if started.elapsed() > REQUEST_TIMEOUT {
            has_more = true;
            break;
        }
    }
    if names.len() > limit {
        has_more = true;
    }
    names.truncate(limit);
    let tables: Vec<TableInfo> = names
        .into_iter()
        .map(|name| TableInfo {
            qualified_name: name.clone(),
            name,
            schema: None,
            kind: "table".to_string(),
        })
        .collect();
    Ok((tables, has_more))
}

/// ScalarAttributeType (S / N / B) の表示文字列。
fn scalar_type_label(t: &aws_sdk_dynamodb::types::ScalarAttributeType) -> String {
    t.as_str().to_string()
}

/// テーブルの「カラム」一覧 (TABLES ペインの展開用)。
/// DynamoDB はスキーマレスのため、DescribeTable で分かる範囲 = キースキーマ
/// (パーティション / ソートキー) + 属性定義 (キー・インデックス対象の属性のみ)
/// を返す。data_type は S / N / B の表記で、キーには役割を添える。
/// キー属性は必ず存在するため nullable = false、その他は true。
pub async fn fetch_columns(
    client: &DynamoClient,
    table: &str,
) -> Result<Vec<ColumnInfo>, AppError> {
    let (key_schema, attribute_definitions) = describe_table(client, table).await?;

    // 属性名 → 型 (S / N / B) のマップ
    let mut types: HashMap<String, String> = HashMap::new();
    for def in &attribute_definitions {
        types.insert(
            def.attribute_name().to_string(),
            scalar_type_label(def.attribute_type()),
        );
    }

    let mut columns: Vec<ColumnInfo> = Vec::new();
    // キースキーマ (HASH → RANGE の順で返る) を先頭に
    for element in &key_schema {
        let name = element.attribute_name().to_string();
        let base = types.get(&name).cloned().unwrap_or_else(|| "?".to_string());
        let role = match element.key_type() {
            KeyType::Hash => "partition key",
            KeyType::Range => "sort key",
            _ => "key",
        };
        columns.push(ColumnInfo {
            data_type: format!("{base} ({role})"),
            name,
            nullable: false,
        });
    }
    // 残りの属性定義 (GSI / LSI のキー属性)。テーブルのキーは除く
    for def in &attribute_definitions {
        let name = def.attribute_name();
        if columns.iter().any(|c| c.name == name) {
            continue;
        }
        columns.push(ColumnInfo {
            name: name.to_string(),
            data_type: scalar_type_label(def.attribute_type()),
            nullable: true,
        });
    }
    Ok(columns)
}

/// テーブルの主キー (パーティションキー → ソートキーの順)。
pub async fn fetch_primary_keys(
    client: &DynamoClient,
    table: &str,
) -> Result<Vec<String>, AppError> {
    let (key_schema, _) = describe_table(client, table).await?;
    let mut hash: Vec<String> = Vec::new();
    let mut range: Vec<String> = Vec::new();
    for element in &key_schema {
        match element.key_type() {
            KeyType::Range => range.push(element.attribute_name().to_string()),
            _ => hash.push(element.attribute_name().to_string()),
        }
    }
    hash.extend(range);
    Ok(hash)
}

/// DescribeTable を実行してキースキーマと属性定義を返す。
async fn describe_table(
    client: &DynamoClient,
    table: &str,
) -> Result<
    (
        Vec<aws_sdk_dynamodb::types::KeySchemaElement>,
        Vec<aws_sdk_dynamodb::types::AttributeDefinition>,
    ),
    AppError,
> {
    let table = validate_table_name(table)?;
    let out = client
        .client
        .describe_table()
        .table_name(table)
        .send()
        .await
        .map_err(|e| sdk_error("DescribeTable failed", e))?;
    let Some(desc) = out.table else {
        return Err(AppError::Config(format!("Table not found: {table}")));
    };
    Ok((
        desc.key_schema.unwrap_or_default(),
        desc.attribute_definitions.unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tables` は Writable OFF でも実行できる queryfolio 独自の文
    /// (CYBERNEURA-DEV-406)。PartiQL の文と紛れないよう、末尾のセミコロンと
    /// 前後の空白だけを許す。
    #[test]
    fn test_is_tables_statement() {
        for ok in ["tables", "TABLES", " tables ", "tables;", "  Tables ;  "] {
            assert!(is_tables_statement(ok), "should accept: {ok:?}");
        }
        for ng in [
            "table",
            "tables where x = 1",
            "select * from tables",
            // 複文で別の文を紛れ込ませられないこと
            "tables; select 1",
            // セミコロンは 1 個だけ許す
            "tables;;",
            "",
        ] {
            assert!(!is_tables_statement(ng), "should reject: {ng:?}");
        }
    }
    use crate::db::dangerous_statement_reason;

    fn s(v: &str) -> AttributeValue {
        AttributeValue::S(v.to_string())
    }

    fn n(v: &str) -> AttributeValue {
        AttributeValue::N(v.to_string())
    }

    #[test]
    fn test_shape_items_column_cap() {
        // スキーマレス由来のカラム爆発は MAX_COLUMNS で打ち切る
        let mut items = Vec::new();
        for i in 0..3 {
            let mut item = HashMap::new();
            for j in 0..(MAX_COLUMNS + 50) {
                item.insert(format!("attr_{i}_{j:04}"), s("v"));
            }
            items.push(item);
        }
        let result = shape_items(items, 100);
        assert_eq!(result.columns.len(), MAX_COLUMNS);
        assert!(result.truncated);
        assert!(result.rows.iter().all(|r| r.len() == MAX_COLUMNS));
    }

    #[test]
    fn test_readonly_guard_partiql() {
        let f = |sql: &str| is_readonly_allowed(sql, Engine::DynamoDb);
        assert!(f("SELECT * FROM \"users\""));
        assert!(f("SELECT * FROM \"users\" WHERE pk = 'a' AND EXISTS(tags)"));
        // ? プレースホルダを含んでも SELECT は読み取り
        assert!(f("SELECT * FROM \"users\" WHERE pk = ?"));
        assert!(!f("INSERT INTO \"users\" VALUE {'pk': 'a'}"));
        assert!(!f("UPDATE \"users\" SET x = 1 WHERE pk = 'a'"));
        assert!(!f("DELETE FROM \"users\" WHERE pk = 'a'"));
    }

    #[test]
    fn test_dangerous_guard_partiql() {
        let d = |sql: &str| dangerous_reason(sql, Engine::DynamoDb).is_some();
        // WHERE 無しの UPDATE / DELETE は危険
        assert!(d("DELETE FROM \"users\""));
        assert!(d("UPDATE \"users\" SET x = 1"));
        // WHERE ありは通す。ダブルクォート識別子 (PartiQL のクォート形) が
        // scan_sql に文字列として空白化されても、キーワード where は
        // クォートされないため判定に影響しない (チェックリスト 5)
        assert!(!d("DELETE FROM \"users\" WHERE \"pk\" = 'a'"));
        assert!(!d("UPDATE \"users\" SET x = 1 WHERE pk = 'a'"));
        // 識別子としての "where" (クォート済み) は WHERE 句ではない →
        // 危険側 (WHERE 無し扱い) に倒れる
        assert!(d("DELETE FROM \"where\""));
        // INSERT / SELECT は対象外
        assert!(!d("INSERT INTO \"users\" VALUE {'pk': 'a'}"));
        assert!(!d("SELECT * FROM \"users\""));
        // 公開ラッパー (フロントの実行前確認) 経由でも同じ
        assert!(dangerous_statement_reason("dynamodb", "DELETE FROM \"users\"")
            .unwrap()
            .is_some());
        assert!(dangerous_statement_reason(
            "dynamodb",
            "DELETE FROM \"users\" WHERE pk = 'a'"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn test_number_to_json() {
        assert_eq!(number_to_json("42"), serde_json::json!(42));
        assert_eq!(number_to_json("-7"), serde_json::json!(-7));
        // 小数は精度を保つため文字列
        assert_eq!(number_to_json("1.5"), serde_json::json!("1.5"));
        // 2^53 超は文字列 (invoke 境界の丸め対策)
        assert_eq!(
            number_to_json("9007199254740993"),
            serde_json::json!("9007199254740993")
        );
        // i64 を超える任意精度の整数も文字列
        assert_eq!(
            number_to_json("170141183460469231731687303715884105727"),
            serde_json::json!("170141183460469231731687303715884105727")
        );
        // 指数表記は文字列のまま
        assert_eq!(number_to_json("1e10"), serde_json::json!("1e10"));
    }

    #[test]
    fn test_attr_to_json_scalars() {
        let mut truncated = false;
        assert_eq!(
            attr_to_json_cell(&s("hello"), &mut truncated),
            serde_json::json!("hello")
        );
        assert_eq!(
            attr_to_json_cell(&AttributeValue::Bool(true), &mut truncated),
            serde_json::json!(true)
        );
        assert_eq!(
            attr_to_json_cell(&AttributeValue::Null(true), &mut truncated),
            serde_json::Value::Null
        );
        // B: UTF-8 なら文字列、そうでなければ base64
        assert_eq!(
            attr_to_json_cell(
                &AttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new(b"abc".to_vec())),
                &mut truncated
            ),
            serde_json::json!("abc")
        );
        let b = attr_to_json_cell(
            &AttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new(vec![0xC3, 0x28])),
            &mut truncated,
        );
        assert!(b.as_str().unwrap().starts_with("base64:"));
        assert!(!truncated);
    }

    #[test]
    fn test_attr_to_json_collections() {
        let mut truncated = false;
        let list = AttributeValue::L(vec![s("a"), n("1")]);
        assert_eq!(
            attr_to_json_cell(&list, &mut truncated),
            serde_json::json!(["a", 1])
        );
        let mut map = HashMap::new();
        map.insert("b".to_string(), n("2"));
        map.insert("a".to_string(), s("x"));
        let m = AttributeValue::M(map);
        // M のキーはソートされて確定的に出る
        assert_eq!(
            serde_json::to_string(&attr_to_json_cell(&m, &mut truncated)).unwrap(),
            "{\"a\":\"x\",\"b\":2}"
        );
        let ss = AttributeValue::Ss(vec!["x".into(), "y".into()]);
        assert_eq!(
            attr_to_json_cell(&ss, &mut truncated),
            serde_json::json!(["x", "y"])
        );
        let ns = AttributeValue::Ns(vec!["1".into(), "2.5".into()]);
        assert_eq!(
            attr_to_json_cell(&ns, &mut truncated),
            serde_json::json!([1, "2.5"])
        );
        let bs = AttributeValue::Bs(vec![aws_sdk_dynamodb::primitives::Blob::new(
            b"z".to_vec(),
        )]);
        assert_eq!(
            attr_to_json_cell(&bs, &mut truncated),
            serde_json::json!(["z"])
        );
        assert!(!truncated);
    }

    #[test]
    fn test_cell_budget_is_shared_across_nesting() {
        // 600 要素 × 2 リストのネストでもセル全体の予算 (1,000) で打ち切る
        let inner: Vec<AttributeValue> = (0..600).map(|i| n(&i.to_string())).collect();
        let value = AttributeValue::L(vec![
            AttributeValue::L(inner.clone()),
            AttributeValue::L(inner),
        ]);
        let mut truncated = false;
        let v = attr_to_json_cell(&value, &mut truncated);
        assert!(truncated);
        fn count(v: &serde_json::Value) -> usize {
            match v {
                serde_json::Value::Array(items) => {
                    1 + items.iter().map(count).sum::<usize>()
                }
                serde_json::Value::Object(map) => {
                    1 + map.values().map(count).sum::<usize>()
                }
                _ => 1,
            }
        }
        assert!(count(&v) <= MAX_CELL_ELEMENTS + 10);
    }

    #[test]
    fn test_text_and_blob_truncation() {
        let mut truncated = false;
        let long = "x".repeat(MAX_TEXT_CHARS + 5);
        let v = attr_to_json_cell(&s(&long), &mut truncated);
        assert!(truncated);
        let text = v.as_str().unwrap();
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS + 1); // +1 は省略記号
        assert!(text.ends_with('…'));

        let mut truncated = false;
        let v = attr_to_json_cell(
            &AttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new(vec![
                b'a';
                MAX_TEXT_CHARS + 10
            ])),
            &mut truncated,
        );
        assert!(truncated);
        assert!(v.as_str().unwrap().ends_with('…'));
    }

    #[test]
    fn test_nesting_depth_cap() {
        let mut value = n("1");
        for _ in 0..(MAX_NESTING_DEPTH + 10) {
            value = AttributeValue::L(vec![value]);
        }
        let mut truncated = false;
        let v = attr_to_json_cell(&value, &mut truncated);
        assert!(truncated);
        assert!(v.to_string().contains("nesting too deep"));
    }

    #[test]
    fn test_shape_items_columns_union() {
        let mut a = HashMap::new();
        a.insert("pk".to_string(), s("u1"));
        a.insert("name".to_string(), s("alice"));
        let mut b = HashMap::new();
        b.insert("pk".to_string(), s("u2"));
        b.insert("age".to_string(), n("30"));
        let result = shape_items(vec![a, b], 100);
        // union はソート順で確定
        assert_eq!(result.columns, vec!["age", "name", "pk"]);
        assert_eq!(result.row_count, 2);
        // アイテムに無い属性は NULL
        assert_eq!(result.rows[0][0], serde_json::Value::Null); // a に age は無い
        assert_eq!(result.rows[0][1], serde_json::json!("alice"));
        assert_eq!(result.rows[1][1], serde_json::Value::Null); // b に name は無い
        assert_eq!(result.rows[1][0], serde_json::json!(30));
        assert!(!result.truncated);
        assert_eq!(result.affected_rows, None);
    }

    #[test]
    fn test_shape_items_truncation() {
        let items: Vec<HashMap<String, AttributeValue>> = (0..5)
            .map(|i| {
                let mut item = HashMap::new();
                item.insert("pk".to_string(), n(&i.to_string()));
                item
            })
            .collect();
        let result = shape_items(items, 3);
        assert_eq!(result.row_count, 3);
        assert!(result.truncated);

        // 空 Items (書き込み文) は空結果 + affected None
        let result = shape_items(vec![], 100);
        assert_eq!(result.row_count, 0);
        assert!(result.columns.is_empty());
        assert_eq!(result.affected_rows, None);
        assert!(!result.truncated);
    }

    #[test]
    fn test_validate_table_name() {
        assert!(validate_table_name("users").is_ok());
        assert!(validate_table_name("my-table.v2_x").is_ok());
        assert!(validate_table_name("ab").is_err()); // 3 文字未満
        assert!(validate_table_name("bad name").is_err());
        assert!(validate_table_name("tbl;drop").is_err());
        assert!(validate_table_name(&"x".repeat(256)).is_err());
    }

    #[test]
    fn test_meta_commands_are_rejected() {
        let err = crate::meta_commands::translate(Engine::DynamoDb, "\\dt").unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
        let err = crate::meta_commands::translate(Engine::DynamoDb, "\\c other").unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");
        // 通常の SQL はメタコマンドではない
        assert!(crate::meta_commands::translate(Engine::DynamoDb, "SELECT 1")
            .unwrap()
            .is_none());
    }

    // ---- 統合テスト (dynamodb-local) ----
    //
    // `docker run -d --name queryfolio-test-ddb -p 127.0.0.1:8100:8000 \
    //    amazon/dynamodb-local` を起動しておくと実行される。
    // 起動していなければ skip する (CI や docker の無い環境を壊さない)。

    const LOCAL_ENDPOINT: (&str, u16) = ("127.0.0.1", 8100);

    /// dynamodb-local が起動しているか (TCP 接続の可否) を確認する。
    async fn local_available() -> bool {
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::net::TcpStream::connect(LOCAL_ENDPOINT),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
    }

    fn local_server_config(name: &str) -> ServerConfig {
        // dynamodb-local は認証情報を検証しないためダミーの静的キーでよい
        serde_yaml::from_str(&format!(
            "name: {name}\n\
             engine: dynamodb\n\
             schema: us-east-1\n\
             host: {}\n\
             port: {}\n\
             user: dummyAccessKey\n\
             password: dummySecretKey\n",
            LOCAL_ENDPOINT.0, LOCAL_ENDPOINT.1
        ))
        .unwrap()
    }

    /// テスト用テーブルを作る (パーティションキー pk (S) + ソートキー sk (N))。
    async fn create_test_table(client: &DynamoClient, table: &str) {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, BillingMode, KeySchemaElement, ScalarAttributeType,
        };
        client
            .client
            .create_table()
            .table_name(table)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("pk")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("sk")
                    .attribute_type(ScalarAttributeType::N)
                    .build()
                    .unwrap(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("pk")
                    .key_type(KeyType::Hash)
                    .build()
                    .unwrap(),
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("sk")
                    .key_type(KeyType::Range)
                    .build()
                    .unwrap(),
            )
            .billing_mode(BillingMode::PayPerRequest)
            .send()
            .await
            .unwrap();
    }

    async fn drop_test_table(client: &DynamoClient, table: &str) {
        let _ = client.client.delete_table().table_name(table).send().await;
    }

    /// GUI E2E の代替を兼ねる統合テスト。フロントの Tauri コマンドと同じ
    /// db.rs の公開経路 (DbManager::get_pool → run_query_cancellable の委譲)
    /// を dynamodb-local で通しで検証する。
    #[tokio::test]
    async fn test_integration_dynamodb_local() {
        if !local_available().await {
            eprintln!(
                "skipping test_integration_dynamodb_local: \
                 dynamodb-local is not listening on {}:{}",
                LOCAL_ENDPOINT.0, LOCAL_ENDPOINT.1
            );
            return;
        }
        // 並行実行・再実行と衝突しないよう実行ごとに一意なテーブル名を使う
        let table = format!(
            "qf_it_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let server = local_server_config("ddb-it");
        let manager = crate::db::DbManager::default();
        let registry = CancelRegistry::default();

        // 接続 (ListTables の疎通確認込み)
        let pool = manager.get_pool(&server).await.unwrap();
        let crate::db::DbPool::DynamoDb(client) = &pool else {
            panic!("expected a DynamoDb pool");
        };
        create_test_table(client, &table).await;

        let run = |sql: String, max_rows: usize, readonly, dangerous| {
            let pool = pool.clone();
            let registry = &registry;
            async move {
                crate::db::run_query_cancellable(
                    &pool, registry, "ddb-it", &sql, max_rows, Some(500), readonly,
                    dangerous,
                )
                .await
            }
        };

        // (a) INSERT (writable): 影響行数は取れないため None + 空結果
        for i in 0..30 {
            let result = run(
                format!(
                    "INSERT INTO \"{table}\" VALUE {{\
                     'pk': 'user1', 'sk': {i}, 'name': 'row{i}', \
                     'score': 1.5, 'big': 9007199254740993, \
                     'tags': ['a', 'b'], 'meta': {{'lang': 'ja'}}}}"
                ),
                1000,
                ReadonlyGuard::Off,
                false,
            )
            .await
            .unwrap();
            assert_eq!(result.row_count, 0);
            assert_eq!(result.affected_rows, None);
        }

        // (b) SELECT: 表形式 (columns union) と型変換
        let result = run(
            format!("SELECT * FROM \"{table}\" WHERE pk = 'user1' AND sk = 0"),
            1000,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(
            result.columns,
            vec!["big", "meta", "name", "pk", "score", "sk", "tags"]
        );
        let row = &result.rows[0];
        assert_eq!(row[0], serde_json::json!("9007199254740993")); // 2^53 超は文字列
        assert_eq!(row[1], serde_json::json!({"lang": "ja"}));
        assert_eq!(row[2], serde_json::json!("row0"));
        assert_eq!(row[3], serde_json::json!("user1"));
        assert_eq!(row[4], serde_json::json!("1.5")); // 小数は文字列 (任意精度)
        assert_eq!(row[5], serde_json::json!(0));
        assert_eq!(row[6], serde_json::json!(["a", "b"]));

        // (c) max_rows + 1 での打ち切り + truncated (30 行中 10 行)
        let result = run(
            format!("SELECT * FROM \"{table}\" WHERE pk = 'user1'"),
            10,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 10);
        assert!(result.truncated);

        // 全 30 行は truncated 無しで取れる (ページネーションの動作確認)
        let result = run(
            format!("SELECT * FROM \"{table}\" WHERE pk = 'user1'"),
            1000,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 30);
        assert!(!result.truncated);

        // (d) readonly ガード: Writable OFF (Switch) では INSERT を拒否
        let err = run(
            format!("INSERT INTO \"{table}\" VALUE {{'pk': 'x', 'sk': 0}}"),
            1000,
            ReadonlyGuard::Switch,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Readonly(_)), "{err}");
        assert!(err.to_string().contains("Writable"), "{err}");

        // (e) dangerous ガード: WHERE 無し DELETE を拒否
        let err = run(
            format!("DELETE FROM \"{table}\""),
            1000,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Dangerous(_)), "{err}");

        // (f) メタコマンドは拒否
        let err = run("\\dt".to_string(), 1000, ReadonlyGuard::Switch, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");

        // (g) UPDATE / DELETE (WHERE 付き、writable)。
        //     RETURNING ALL OLD * は変更前アイテムが行として返る
        let result = run(
            format!(
                "UPDATE \"{table}\" SET name = 'renamed' \
                 WHERE pk = 'user1' AND sk = 0"
            ),
            1000,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 0);
        let result = run(
            format!(
                "DELETE FROM \"{table}\" WHERE pk = 'user1' AND sk = 1 \
                 RETURNING ALL OLD *"
            ),
            1000,
            ReadonlyGuard::Off,
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.row_count, 1);
        assert!(result.columns.iter().any(|c| c == "name"));

        // (h) schema_info: テーブル一覧・カラム (キースキーマ)・主キー
        let tables = crate::schema_info::fetch_tables(&pool).await.unwrap();
        assert!(tables.iter().any(|t| t.qualified_name == table));
        assert!(tables.iter().all(|t| t.kind == "table"));
        let columns = crate::schema_info::fetch_columns(&pool, &table).await.unwrap();
        let summary: Vec<(&str, &str, bool)> = columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("pk", "S (partition key)", false),
                ("sk", "N (sort key)", false),
            ]
        );
        let keys = crate::schema_info::fetch_primary_keys(&pool, &table)
            .await
            .unwrap();
        assert_eq!(keys, vec!["pk", "sk"]);
        // 存在しないテーブルの DescribeTable はエラー
        assert!(
            crate::schema_info::fetch_columns(&pool, "qf-no-such-table")
                .await
                .is_err()
        );

        // (i) SELECT の応答が list_schemas / run_statements の拒否経路を壊さない
        let schemas = crate::db::list_schemas(&pool, &server).await.unwrap();
        assert!(schemas.is_empty());
        let err = crate::db::run_statements(
            &pool,
            &["UPDATE t SET a = 1 WHERE id = 1".to_string()],
            ReadonlyGuard::Off,
            true,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not supported"), "{err}");

        drop_test_table(client, &table).await;
        manager.disconnect("ddb-it").await;
    }

    /// 接続確認 (ListTables) が到達不能なエンドポイントで速やかに
    /// エラーになる (無期限に待たない)。
    #[tokio::test]
    async fn test_connect_fails_fast_on_unreachable_endpoint() {
        // TCP 接続自体が拒否されるポート (何も listen していない前提の高位ポート)
        let server: ServerConfig = serde_yaml::from_str(
            "name: x\nengine: dynamodb\nschema: us-east-1\n\
             host: 127.0.0.1\nport: 59998\nuser: a\npassword: b\n",
        )
        .unwrap();
        let started = Instant::now();
        let err = connect(&server).await.unwrap_err();
        assert!(matches!(err, AppError::DynamoDb(_)), "{err}");
        // 接続拒否は即時、悪くても CONNECT_TIMEOUT + マージンで返る
        assert!(started.elapsed() < CONNECT_TIMEOUT + std::time::Duration::from_secs(5));
    }

    /// リージョン (schema) 未設定は設定エラー。
    #[tokio::test]
    async fn test_connect_requires_region_and_paired_credentials() {
        let server: ServerConfig =
            serde_yaml::from_str("name: x\nengine: dynamodb\n").unwrap();
        let err = build_client(&server).await.unwrap_err();
        assert!(err.to_string().contains("AWS region"), "{err}");

        // user だけ・password だけは設定エラー (黙って chain に落とさない)
        let server: ServerConfig = serde_yaml::from_str(
            "name: x\nengine: dynamodb\nschema: us-east-1\nuser: only-key\n",
        )
        .unwrap();
        let err = build_client(&server).await.unwrap_err();
        assert!(err.to_string().contains("both user"), "{err}");
    }

    /// クライアント側キャンセル: 応答を返さないエンドポイントで実行中の
    /// クエリをキャンセルすると、タイムアウトを待たず Cancelled で返る。
    #[tokio::test]
    async fn test_cancel_aborts_running_query() {
        // accept するが何も応答しないローカルサーバー
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hold = tokio::spawn(async move {
            let mut sockets = Vec::new();
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    sockets.push(socket); // 接続は保持したまま応答しない
                }
            }
        });

        let server: ServerConfig = serde_yaml::from_str(&format!(
            "name: ddb-cancel\nengine: dynamodb\nschema: us-east-1\n\
             host: 127.0.0.1\nport: {port}\nuser: a\npassword: b\n"
        ))
        .unwrap();
        let client = build_client(&server).await.unwrap();
        let registry = Arc::new(CancelRegistry::default());

        let registry2 = registry.clone();
        let task = tokio::spawn(async move {
            run_query_cancellable(
                &client,
                &registry2,
                "ddb-cancel",
                "SELECT * FROM \"t\"",
                100,
                ReadonlyGuard::Switch,
                false,
            )
            .await
        });
        // 実行が登録されるのを待ってからキャンセルする
        let mut cancelled = false;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if registry.cancel("ddb-cancel").await.unwrap() {
                cancelled = true;
                break;
            }
        }
        assert!(cancelled, "the query was never registered");
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("cancel did not abort the request")
            .unwrap();
        assert!(matches!(result, Err(AppError::Cancelled)), "{result:?}");
        hold.abort();
    }
}
