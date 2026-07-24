use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use sqlx::Row;

use crate::db::DbPool;
use crate::error::AppError;
use crate::meta_commands::validate_relation_name;

/// テーブル / ビューの情報 (スキーマブラウザのツリーノード用)。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableInfo {
    /// テーブル名 (スキーマ修飾なし)
    pub name: String,
    /// 所属スキーマ名 (PostgreSQL のみ。MySQL / SQLite は None)
    pub schema: Option<String>,
    /// "table" または "view"
    pub kind: String,
    /// SQL に埋め込める修飾名。フロントはこの値を list_columns の
    /// table 引数・エディタへの挿入にそのまま使う
    pub qualified_name: String,
}

/// カラムの情報。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    /// カラム型 (エンジンのカタログ表記のまま)
    pub data_type: String,
    /// NULL 可否
    pub nullable: bool,
}

/// SQL に埋め込める修飾名を作る。PostgreSQL の public スキーマは
/// search_path のデフォルトで解決できるため修飾しない。
fn build_qualified_name(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if s != "public" => format!("{s}.{name}"),
        _ => name.to_string(),
    }
}

/// 接続先のテーブル / ビューの一覧を返す。
/// カタログ照会 SQL は meta_commands の \dt / \dv 相当。
pub async fn fetch_tables(pool: &DbPool) -> Result<Vec<TableInfo>, AppError> {
    match pool {
        DbPool::Postgres(p) => {
            let rows = sqlx::query(
                "SELECT n.nspname AS schema, c.relname AS name, \
                 CASE WHEN c.relkind IN ('v','m') THEN 'view' ELSE 'table' END AS kind \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r','p','v','m') \
                   AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' \
                 ORDER BY 1, 2",
            )
            .fetch_all(p)
            .await?;
            Ok(rows
                .iter()
                .map(|row| {
                    let schema: String = row.try_get(0).unwrap_or_default();
                    let name: String = row.try_get(1).unwrap_or_default();
                    let kind: String = row.try_get(2).unwrap_or_else(|_| "table".into());
                    TableInfo {
                        qualified_name: build_qualified_name(Some(&schema), &name),
                        name,
                        schema: Some(schema),
                        kind,
                    }
                })
                .collect())
        }
        DbPool::MySql(p) => {
            // DATABASE() が NULL (接続先 database 未指定) の場合は空になる
            let rows = sqlx::query(
                "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
            )
            .fetch_all(p)
            .await?;
            Ok(rows
                .iter()
                .map(|row| {
                    let name: String = row.try_get(0).unwrap_or_default();
                    let table_type: String = row.try_get(1).unwrap_or_default();
                    let kind = if table_type.eq_ignore_ascii_case("VIEW") {
                        "view"
                    } else {
                        "table"
                    };
                    TableInfo {
                        qualified_name: name.clone(),
                        name,
                        schema: None,
                        kind: kind.to_string(),
                    }
                })
                .collect())
        }
        DbPool::Sqlite(p) => {
            let rows = sqlx::query(
                "SELECT name, type FROM sqlite_master \
                 WHERE type IN ('table', 'view') \
                   AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
                 ORDER BY type, name",
            )
            .fetch_all(p)
            .await?;
            Ok(rows
                .iter()
                .map(|row| {
                    let name: String = row.try_get(0).unwrap_or_default();
                    let kind: String = row.try_get(1).unwrap_or_else(|_| "table".into());
                    TableInfo {
                        qualified_name: name.clone(),
                        name,
                        schema: None,
                        kind,
                    }
                })
                .collect())
        }
        // テーブルの概念が無いエンジン (capabilities.supports_tables = false
        // でフロントは呼ばないが、直接呼ばれても壊れないよう空を返す)
        DbPool::Redis(_) => Ok(vec![]),
    }
}

/// テーブルのカラム一覧を返す。
/// テーブル名は SQL に埋め込む (PG の regclass / SQLite の PRAGMA) ため、
/// meta_commands と同じ識別子検証を通す (SQL インジェクション対策)。
pub async fn fetch_columns(pool: &DbPool, table: &str) -> Result<Vec<ColumnInfo>, AppError> {
    let table = validate_relation_name(table)?;
    let columns: Vec<ColumnInfo> = match pool {
        DbPool::Postgres(p) => {
            let sql = format!(
                "SELECT a.attname AS name, \
                 pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type, \
                 NOT a.attnotnull AS nullable \
                 FROM pg_catalog.pg_attribute a \
                 WHERE a.attrelid = '{table}'::regclass \
                   AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum"
            );
            sqlx::query(&sql)
                .fetch_all(p)
                .await?
                .iter()
                .map(|row| ColumnInfo {
                    name: row.try_get(0).unwrap_or_default(),
                    data_type: row.try_get(1).unwrap_or_default(),
                    nullable: row.try_get(2).unwrap_or(true),
                })
                .collect()
        }
        DbPool::MySql(p) => {
            // db.table 形式なら TABLE_SCHEMA も絞り込む (バインドなので安全)
            let (schema_part, table_part) = match table.split_once('.') {
                Some((s, t)) => (Some(s.to_string()), t),
                None => (None, table),
            };
            sqlx::query(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE \
                 FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? \
                 ORDER BY ORDINAL_POSITION",
            )
            .bind(schema_part)
            .bind(table_part)
            .fetch_all(p)
            .await?
            .iter()
            .map(|row| ColumnInfo {
                name: row.try_get(0).unwrap_or_default(),
                data_type: row.try_get(1).unwrap_or_default(),
                nullable: row
                    .try_get::<String, _>(2)
                    .map(|v| v.eq_ignore_ascii_case("YES"))
                    .unwrap_or(true),
            })
            .collect()
        }
        DbPool::Sqlite(p) => {
            let sql = format!("PRAGMA table_info(\"{table}\")");
            sqlx::query(&sql)
                .fetch_all(p)
                .await?
                .iter()
                .map(|row| ColumnInfo {
                    // PRAGMA table_info: (cid, name, type, notnull, dflt_value, pk)
                    name: row.try_get(1).unwrap_or_default(),
                    data_type: row.try_get(2).unwrap_or_default(),
                    nullable: row.try_get::<i64, _>(3).map(|v| v == 0).unwrap_or(true),
                })
                .collect()
        }
        DbPool::Redis(_) => {
            return Err(AppError::Config(
                "This engine does not have tables".into(),
            ));
        }
    };
    // MySQL / SQLite は存在しないテーブルでもエラーにならず空が返るため、
    // ここで明示的にエラーにする (PG は regclass 解決で先にエラーになる)
    if columns.is_empty() {
        return Err(AppError::Config(format!("Table not found: {table}")));
    }
    Ok(columns)
}

/// テーブルの主キー (PRIMARY KEY) を構成するカラム名を返す。
/// 結果グリッドのセル編集で UPDATE の WHERE 句を組むために使う。
/// 主キーが無いテーブルでは空を返す (呼び出し側で編集不可にする)。
/// テーブル名は SQL に埋め込む箇所があるため fetch_columns と同じ識別子
/// 検証を通す (SQL インジェクション対策)。
pub async fn fetch_primary_keys(pool: &DbPool, table: &str) -> Result<Vec<String>, AppError> {
    let table = validate_relation_name(table)?;
    let keys: Vec<String> = match pool {
        DbPool::Postgres(p) => {
            // pg_index.indisprimary で主キー索引の対象カラムを引く。
            // 複合主キーの並び順は WHERE の組立に影響しないため attnum 順でよい。
            let sql = format!(
                "SELECT a.attname \
                 FROM pg_catalog.pg_index i \
                 JOIN pg_catalog.pg_attribute a \
                   ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
                 WHERE i.indrelid = '{table}'::regclass AND i.indisprimary \
                 ORDER BY a.attnum"
            );
            sqlx::query(&sql)
                .fetch_all(p)
                .await?
                .iter()
                .map(|row| row.try_get::<String, _>(0).unwrap_or_default())
                .collect()
        }
        DbPool::MySql(p) => {
            // db.table 形式なら TABLE_SCHEMA も絞り込む (バインドなので安全)
            let (schema_part, table_part) = match table.split_once('.') {
                Some((s, t)) => (Some(s.to_string()), t),
                None => (None, table),
            };
            sqlx::query(
                "SELECT COLUMN_NAME FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? \
                   AND COLUMN_KEY = 'PRI' \
                 ORDER BY ORDINAL_POSITION",
            )
            .bind(schema_part)
            .bind(table_part)
            .fetch_all(p)
            .await?
            .iter()
            .map(|row| row.try_get::<String, _>(0).unwrap_or_default())
            .collect()
        }
        DbPool::Sqlite(p) => {
            // PRAGMA table_info: (cid, name, type, notnull, dflt_value, pk)。
            // pk > 0 が主キー構成カラム (値は 1 始まりの並び順)。
            let sql = format!("PRAGMA table_info(\"{table}\")");
            let mut rows: Vec<(i64, String)> = sqlx::query(&sql)
                .fetch_all(p)
                .await?
                .iter()
                .filter_map(|row| {
                    let pk: i64 = row.try_get(5).unwrap_or(0);
                    if pk > 0 {
                        Some((pk, row.try_get::<String, _>(1).unwrap_or_default()))
                    } else {
                        None
                    }
                })
                .collect();
            rows.sort_by_key(|(pk, _)| *pk);
            rows.into_iter().map(|(_, name)| name).collect()
        }
        DbPool::Redis(_) => vec![],
    };
    Ok(keys)
}

/// 全テーブルの全カラムを一括取得し、修飾テーブル名 → カラム一覧の
/// マップを返す。get_schema_map (SQL 補完) のキャッシュ充填用。
pub async fn fetch_all_columns(
    pool: &DbPool,
) -> Result<BTreeMap<String, Vec<ColumnInfo>>, AppError> {
    let mut map: BTreeMap<String, Vec<ColumnInfo>> = BTreeMap::new();
    match pool {
        DbPool::Postgres(p) => {
            let rows = sqlx::query(
                "SELECT n.nspname, c.relname, a.attname, \
                 pg_catalog.format_type(a.atttypid, a.atttypmod), \
                 NOT a.attnotnull \
                 FROM pg_catalog.pg_attribute a \
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r','p','v','m') \
                   AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' \
                   AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY n.nspname, c.relname, a.attnum",
            )
            .fetch_all(p)
            .await?;
            for row in &rows {
                let schema: String = row.try_get(0).unwrap_or_default();
                let name: String = row.try_get(1).unwrap_or_default();
                let key = build_qualified_name(Some(&schema), &name);
                map.entry(key).or_default().push(ColumnInfo {
                    name: row.try_get(2).unwrap_or_default(),
                    data_type: row.try_get(3).unwrap_or_default(),
                    nullable: row.try_get(4).unwrap_or(true),
                });
            }
        }
        DbPool::MySql(p) => {
            let rows = sqlx::query(
                "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE \
                 FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = DATABASE() \
                 ORDER BY TABLE_NAME, ORDINAL_POSITION",
            )
            .fetch_all(p)
            .await?;
            for row in &rows {
                let table: String = row.try_get(0).unwrap_or_default();
                map.entry(table).or_default().push(ColumnInfo {
                    name: row.try_get(1).unwrap_or_default(),
                    data_type: row.try_get(2).unwrap_or_default(),
                    nullable: row
                        .try_get::<String, _>(3)
                        .map(|v| v.eq_ignore_ascii_case("YES"))
                        .unwrap_or(true),
                });
            }
        }
        DbPool::Sqlite(p) => {
            // pragma_table_info をテーブル値関数として結合し 1 クエリで取る
            let rows = sqlx::query(
                "SELECT m.name, p.name, p.type, p.\"notnull\" \
                 FROM sqlite_master m, pragma_table_info(m.name) p \
                 WHERE m.type IN ('table', 'view') \
                   AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
                 ORDER BY m.name, p.cid",
            )
            .fetch_all(p)
            .await?;
            for row in &rows {
                let table: String = row.try_get(0).unwrap_or_default();
                map.entry(table).or_default().push(ColumnInfo {
                    name: row.try_get(1).unwrap_or_default(),
                    data_type: row.try_get(2).unwrap_or_default(),
                    nullable: row.try_get::<i64, _>(3).map(|v| v == 0).unwrap_or(true),
                });
            }
        }
        // テーブル・カラムの概念が無いエンジンは空のまま返す (SQL 補完なし)
        DbPool::Redis(_) => {}
    }
    Ok(map)
}

/// スキーマ情報のキャッシュ。キーは (接続名, アクティブスキーマ名)。
/// スキーマブラウザ (list_tables / list_columns) と SQL 補完
/// (get_schema_map) で共有する。
/// reset_connections で全クリア、set_active_schema で接続単位のクリア。
#[derive(Default)]
pub struct SchemaCache {
    inner: tokio::sync::Mutex<HashMap<(String, String), CachedSchema>>,
}

/// 1 つの (接続, スキーマ) 分のキャッシュ内容。
#[derive(Default)]
struct CachedSchema {
    /// テーブル一覧 (未取得なら None)
    tables: Option<Vec<TableInfo>>,
    /// 修飾テーブル名 → カラム一覧 (ツリー展開の遅延ロードで貯まる)
    columns: HashMap<String, Vec<ColumnInfo>>,
    /// columns が全テーブル分そろっているか (fetch_all_columns 済みか)
    columns_complete: bool,
}

impl SchemaCache {
    fn key(connection: &str, schema: &str) -> (String, String) {
        (connection.to_string(), schema.to_string())
    }

    pub async fn get_tables(&self, connection: &str, schema: &str) -> Option<Vec<TableInfo>> {
        self.inner
            .lock()
            .await
            .get(&Self::key(connection, schema))?
            .tables
            .clone()
    }

    pub async fn put_tables(&self, connection: &str, schema: &str, tables: &[TableInfo]) {
        let mut inner = self.inner.lock().await;
        inner
            .entry(Self::key(connection, schema))
            .or_default()
            .tables = Some(tables.to_vec());
    }

    pub async fn get_columns(
        &self,
        connection: &str,
        schema: &str,
        table: &str,
    ) -> Option<Vec<ColumnInfo>> {
        self.inner
            .lock()
            .await
            .get(&Self::key(connection, schema))?
            .columns
            .get(table)
            .cloned()
    }

    pub async fn put_columns(
        &self,
        connection: &str,
        schema: &str,
        table: &str,
        columns: &[ColumnInfo],
    ) {
        let mut inner = self.inner.lock().await;
        inner
            .entry(Self::key(connection, schema))
            .or_default()
            .columns
            .insert(table.to_string(), columns.to_vec());
    }

    /// 全テーブル分のカラムを一括登録する (get_schema_map の充填)。
    pub async fn put_all_columns(
        &self,
        connection: &str,
        schema: &str,
        columns: BTreeMap<String, Vec<ColumnInfo>>,
    ) {
        let mut inner = self.inner.lock().await;
        let entry = inner.entry(Self::key(connection, schema)).or_default();
        entry.columns = columns.into_iter().collect();
        entry.columns_complete = true;
    }

    /// テーブル名 → カラム名リストのマップを返す。
    /// 全テーブル分のカラムがキャッシュ済みの場合のみ Some を返す
    /// (部分キャッシュを補完に使うと存在するカラムを提示できないため)。
    pub async fn get_schema_map(
        &self,
        connection: &str,
        schema: &str,
    ) -> Option<BTreeMap<String, Vec<String>>> {
        let inner = self.inner.lock().await;
        let cached = inner.get(&Self::key(connection, schema))?;
        if !cached.columns_complete {
            return None;
        }
        Some(
            cached
                .columns
                .iter()
                .map(|(table, columns)| {
                    (
                        table.clone(),
                        columns.iter().map(|c| c.name.clone()).collect(),
                    )
                })
                .collect(),
        )
    }

    /// (接続, スキーマ) 単位でキャッシュを破棄する (リロードボタン用)。
    pub async fn invalidate_schema(&self, connection: &str, schema: &str) {
        self.inner
            .lock()
            .await
            .remove(&Self::key(connection, schema));
    }

    /// 接続単位でキャッシュを破棄する (アクティブスキーマ切替時)。
    pub async fn invalidate_connection(&self, connection: &str) {
        self.inner
            .lock()
            .await
            .retain(|(conn, _), _| conn != connection);
    }

    /// 全キャッシュを破棄する (設定リロード時)。
    pub async fn clear(&self) {
        self.inner.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    /// テスト用の SQLite 実プールを作り、テーブル・ビューを準備する。
    async fn test_pool() -> DbPool {
        // :memory: はコネクションごとに別 DB になるため、プールを 1 接続に固定する
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(":memory:")
                    .in_memory(true),
            )
            .await
            .unwrap();
        let pool = DbPool::Sqlite(pool);
        for sql in [
            "CREATE TABLE users (id INTEGER NOT NULL, name TEXT, score REAL NOT NULL)",
            // AUTOINCREMENT で内部テーブル sqlite_sequence を作らせる
            "CREATE TABLE orders (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER)",
            "CREATE VIEW user_names AS SELECT name FROM users",
        ] {
            crate::db::run_query(&pool, sql, 10, None, false, false)
                .await
                .unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn test_fetch_tables_sqlite() {
        let pool = test_pool().await;
        let tables = fetch_tables(&pool).await.unwrap();
        let names: Vec<(&str, &str)> = tables
            .iter()
            .map(|t| (t.qualified_name.as_str(), t.kind.as_str()))
            .collect();
        // type, name 順。sqlite_sequence (内部テーブル) は含まれない
        assert_eq!(
            names,
            vec![
                ("orders", "table"),
                ("users", "table"),
                ("user_names", "view"),
            ]
        );
        assert!(tables.iter().all(|t| t.schema.is_none()));
        assert!(tables.iter().all(|t| t.name == t.qualified_name));
    }

    #[tokio::test]
    async fn test_fetch_columns_sqlite() {
        let pool = test_pool().await;
        let columns = fetch_columns(&pool, "users").await.unwrap();
        let summary: Vec<(&str, &str, bool)> = columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("id", "INTEGER", false),
                ("name", "TEXT", true),
                ("score", "REAL", false),
            ]
        );

        // ビューのカラムも取得できる
        let columns = fetch_columns(&pool, "user_names").await.unwrap();
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "name");

        // 存在しないテーブルはエラー
        let err = fetch_columns(&pool, "missing_table").await.unwrap_err();
        assert!(err.to_string().contains("Table not found"));
    }

    #[tokio::test]
    async fn test_fetch_columns_rejects_invalid_table_name() {
        let pool = test_pool().await;
        // SQL インジェクションにつながるテーブル名は SQL 実行前に拒否される
        for name in [
            "users\"); DROP TABLE users; --",
            "users'; --",
            "a\"b",
            "a.b.c",
            "1users",
            "",
        ] {
            let err = fetch_columns(&pool, name).await.unwrap_err();
            assert!(
                err.to_string().contains("Invalid table name"),
                "expected rejection for {name:?}, got: {err}"
            );
        }
        // 検証済みの正当な名前は通る (テーブルは無傷)
        assert!(fetch_columns(&pool, "users").await.is_ok());
    }

    #[tokio::test]
    async fn test_fetch_all_columns_sqlite() {
        let pool = test_pool().await;
        let map = fetch_all_columns(&pool).await.unwrap();
        let keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["orders", "user_names", "users"]);
        let users: Vec<&str> = map["users"].iter().map(|c| c.name.as_str()).collect();
        assert_eq!(users, vec!["id", "name", "score"]);
        assert_eq!(map["orders"].len(), 2);
    }

    #[tokio::test]
    async fn test_schema_cache() {
        let cache = SchemaCache::default();
        let table = TableInfo {
            name: "users".into(),
            schema: None,
            kind: "table".into(),
            qualified_name: "users".into(),
        };
        let column = ColumnInfo {
            name: "id".into(),
            data_type: "INTEGER".into(),
            nullable: false,
        };

        // 空のキャッシュは None
        assert!(cache.get_tables("conn1", "db1").await.is_none());
        assert!(cache.get_columns("conn1", "db1", "users").await.is_none());
        assert!(cache.get_schema_map("conn1", "db1").await.is_none());

        cache.put_tables("conn1", "db1", &[table.clone()]).await;
        cache
            .put_columns("conn1", "db1", "users", &[column.clone()])
            .await;
        assert_eq!(
            cache.get_tables("conn1", "db1").await,
            Some(vec![table.clone()])
        );
        assert_eq!(
            cache.get_columns("conn1", "db1", "users").await,
            Some(vec![column.clone()])
        );
        // スキーマ・接続が違えばヒットしない
        assert!(cache.get_tables("conn1", "db2").await.is_none());
        assert!(cache.get_tables("conn2", "db1").await.is_none());

        // 部分キャッシュ (columns_complete でない) では schema_map は返らない
        assert!(cache.get_schema_map("conn1", "db1").await.is_none());
        let mut all = BTreeMap::new();
        all.insert("users".to_string(), vec![column.clone()]);
        cache.put_all_columns("conn1", "db1", all).await;
        let map = cache.get_schema_map("conn1", "db1").await.unwrap();
        assert_eq!(map["users"], vec!["id".to_string()]);

        // スキーマ単位の破棄
        cache.put_tables("conn1", "db2", &[table.clone()]).await;
        cache.invalidate_schema("conn1", "db1").await;
        assert!(cache.get_tables("conn1", "db1").await.is_none());
        assert!(cache.get_tables("conn1", "db2").await.is_some());

        // 接続単位の破棄 (他の接続は残る)
        cache.put_tables("conn2", "db1", &[table.clone()]).await;
        cache.invalidate_connection("conn1").await;
        assert!(cache.get_tables("conn1", "db2").await.is_none());
        assert!(cache.get_tables("conn2", "db1").await.is_some());

        // 全破棄
        cache.clear().await;
        assert!(cache.get_tables("conn2", "db1").await.is_none());
    }
}
