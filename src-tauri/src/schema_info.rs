use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use sqlx::Row;

use crate::db::DbPool;
use crate::error::AppError;
use crate::meta_commands::validate_relation_name;

/// Table / view info (for tree nodes in the schema browser).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableInfo {
    /// Table name (without schema qualification)
    pub name: String,
    /// Owning schema name (PostgreSQL only; None for MySQL / SQLite)
    pub schema: Option<String>,
    /// "table" or "view"
    pub kind: String,
    /// Qualified name that can be embedded in SQL. The frontend uses this value as-is for the
    /// `table` argument of list_columns and for insertion into the editor
    pub qualified_name: String,
}

/// Column info.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    /// Column type (as written in the engine's catalog)
    pub data_type: String,
    /// Whether the column is nullable
    pub nullable: bool,
}

/// Builds a qualified name that can be embedded in SQL. PostgreSQL's public schema is not
/// qualified because it resolves via the default search_path.
fn build_qualified_name(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) if s != "public" => format!("{s}.{name}"),
        _ => name.to_string(),
    }
}

/// Returns the list of tables / views on the connection.
/// The catalog queries are equivalent to \dt / \dv in meta_commands.
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
            // Empty when DATABASE() is NULL (no target database specified)
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
        // Engines without a table concept (the frontend does not call this when
        // capabilities.supports_tables = false, but return empty so a direct call does not break)
        DbPool::Redis(_) => Ok(vec![]),
        // Elasticsearch returns the index list as "tables"
        DbPool::Elasticsearch(client) => {
            crate::engines::elasticsearch::fetch_indices(client).await
        }
        DbPool::DuckDb(handle) => crate::engines::duckdb::fetch_tables(handle).await,
        // DynamoDB returns the table list (ListTables)
        DbPool::DynamoDb(client) => crate::engines::dynamodb::fetch_tables(client).await,
        DbPool::MsSql(handle) => crate::engines::mssql::fetch_tables(handle).await,
    }
}

/// Returns the list of columns of a table.
/// The table name is embedded in SQL (PG regclass / SQLite PRAGMA), so it goes through the
/// same identifier validation as meta_commands (SQL injection protection).
pub async fn fetch_columns(pool: &DbPool, table: &str) -> Result<Vec<ColumnInfo>, AppError> {
    // Elasticsearch index names (hyphens, dots, etc.) do not fit the SQL identifier rules
    // (validate_relation_name), so use the module's own validation
    // (a character set safe as a URL path) and return the mapping fields
    if let DbPool::Elasticsearch(client) = pool {
        return crate::engines::elasticsearch::fetch_index_columns(client, table).await;
    }
    // DynamoDB table names (dots and hyphens allowed) also do not fit the SQL identifier rules,
    // so DescribeTable with the module's own validation (DynamoDB naming rules)
    if let DbPool::DynamoDb(client) = pool {
        return crate::engines::dynamodb::fetch_columns(client, table).await;
    }
    // SQL Server table names may contain spaces or symbols (`Order Details`), so they do not go
    // through the SQL identifier rules (validate_relation_name). The module queries
    // INFORMATION_SCHEMA with @P1 / @P2 bindings and never embeds the name in SQL
    if let DbPool::MsSql(handle) = pool {
        return crate::engines::mssql::fetch_columns(handle, table).await;
    }
    let table = validate_relation_name(table)?;
    // DuckDB queries information_schema with the table name bound
    // (the identifier validation uses the same rules as the SQL engines)
    if let DbPool::DuckDb(handle) = pool {
        return crate::engines::duckdb::fetch_columns(handle, table).await;
    }
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
            // For the db.table form, also narrow by TABLE_SCHEMA (safe because it is bound)
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
        // Already handled by the early return at the top
        DbPool::Elasticsearch(_) | DbPool::DuckDb(_) | DbPool::DynamoDb(_) | DbPool::MsSql(_) => {
            unreachable!()
        }
    };
    // MySQL / SQLite return empty rather than an error for a nonexistent table,
    // so raise an explicit error here (PG errors earlier on regclass resolution)
    if columns.is_empty() {
        return Err(AppError::Config(format!("Table not found: {table}")));
    }
    Ok(columns)
}

/// Returns the column names that make up the table's primary key (PRIMARY KEY).
/// Used to build the WHERE clause of UPDATE for cell edits in the result grid.
/// Returns empty for tables without a primary key (the caller makes them non-editable).
/// The table name is embedded in SQL in some places, so it goes through the same identifier
/// validation as fetch_columns (SQL injection protection).
pub async fn fetch_primary_keys(pool: &DbPool, table: &str) -> Result<Vec<String>, AppError> {
    // Elasticsearch has no concept of a primary key (cell editing is also unsupported).
    // Index names do not fit the SQL identifier validation, so return before validating
    if matches!(pool, DbPool::Elasticsearch(_)) {
        return Ok(vec![]);
    }
    // DynamoDB key schema (PK / SK). Table names do not fit the SQL identifier validation,
    // so delegate to the module (validated by DynamoDB naming rules) before validation
    if let DbPool::DynamoDb(client) = pool {
        return crate::engines::dynamodb::fetch_primary_keys(client, table).await;
    }
    // SQL Server queries with bindings, so it skips identifier validation (same as fetch_columns)
    if let DbPool::MsSql(handle) = pool {
        return crate::engines::mssql::fetch_primary_keys(handle, table).await;
    }
    let table = validate_relation_name(table)?;
    if let DbPool::DuckDb(handle) = pool {
        return crate::engines::duckdb::fetch_primary_keys(handle, table).await;
    }
    let keys: Vec<String> = match pool {
        DbPool::Postgres(p) => {
            // Look up the columns of the primary key index via pg_index.indisprimary.
            // The order of a composite primary key does not affect building the WHERE, so attnum order is fine.
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
            // For the db.table form, also narrow by TABLE_SCHEMA (safe because it is bound)
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
            // PRAGMA table_info: (cid, name, type, notnull, dflt_value, pk).
            // pk > 0 marks the primary key columns (the value is the 1-based order).
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
        // Already handled by the early return at the top
        DbPool::Elasticsearch(_) | DbPool::DuckDb(_) | DbPool::DynamoDb(_) | DbPool::MsSql(_) => {
            unreachable!()
        }
    };
    Ok(keys)
}

/// Fetches all columns of all tables at once and returns a map of qualified table name ->
/// column list. Used to fill the cache for get_schema_map (SQL completion).
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
            // Join pragma_table_info as a table-valued function to fetch everything in one query
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
        // Engines without a table / column concept, and engines that do not use SQL completion
        // (Elasticsearch / DynamoDB; DynamoDB is schemaless, so the full set of columns
        // cannot be determined) are returned empty
        DbPool::Redis(_) | DbPool::Elasticsearch(_) | DbPool::DynamoDb(_) => {}
        DbPool::MsSql(handle) => {
            return crate::engines::mssql::fetch_all_columns(handle).await;
        }
        DbPool::DuckDb(handle) => {
            return crate::engines::duckdb::fetch_all_columns(handle).await;
        }
    }
    Ok(map)
}

/// Cache of schema info. The key is (connection name, active schema name).
/// Shared by the schema browser (list_tables / list_columns) and SQL completion
/// (get_schema_map).
/// Cleared entirely by reset_connections, and per connection by set_active_schema.
#[derive(Default)]
pub struct SchemaCache {
    inner: tokio::sync::Mutex<HashMap<(String, String), CachedSchema>>,
}

/// Cache contents for one (connection, schema).
#[derive(Default)]
struct CachedSchema {
    /// Table list (None if not yet fetched)
    tables: Option<Vec<TableInfo>>,
    /// Qualified table name -> column list (accumulated by lazy loading on tree expansion)
    columns: HashMap<String, Vec<ColumnInfo>>,
    /// Whether columns covers all tables (whether fetch_all_columns has been done)
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

    /// Registers the columns of all tables at once (fills the cache for get_schema_map).
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

    /// Returns a map of table name -> list of column names.
    /// Returns Some only when the columns of all tables are cached
    /// (using a partial cache for completion would fail to suggest existing columns).
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

    /// Discards the cache per (connection, schema) (for the reload button).
    pub async fn invalidate_schema(&self, connection: &str, schema: &str) {
        self.inner
            .lock()
            .await
            .remove(&Self::key(connection, schema));
    }

    /// Discards the cache per connection (when switching the active schema).
    pub async fn invalidate_connection(&self, connection: &str) {
        self.inner
            .lock()
            .await
            .retain(|(conn, _), _| conn != connection);
    }

    /// Discards the whole cache (when reloading settings).
    pub async fn clear(&self) {
        self.inner.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    /// Creates a real SQLite pool for tests and prepares tables and views.
    async fn test_pool() -> DbPool {
        // :memory: is a separate DB per connection, so pin the pool to a single connection
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
            // Let AUTOINCREMENT create the internal sqlite_sequence table
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
        // Ordered by type, name. sqlite_sequence (an internal table) is not included
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

        // Columns of a view can also be fetched
        let columns = fetch_columns(&pool, "user_names").await.unwrap();
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "name");

        // A nonexistent table is an error
        let err = fetch_columns(&pool, "missing_table").await.unwrap_err();
        assert!(err.to_string().contains("Table not found"));
    }

    #[tokio::test]
    async fn test_fetch_columns_rejects_invalid_table_name() {
        let pool = test_pool().await;
        // Table names that could lead to SQL injection are rejected before SQL execution
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
        // A valid, validated name passes (the table is left intact)
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

        // An empty cache is None
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
        // No hit if the schema or connection differs
        assert!(cache.get_tables("conn1", "db2").await.is_none());
        assert!(cache.get_tables("conn2", "db1").await.is_none());

        // A partial cache (columns_complete not set) does not return a schema_map
        assert!(cache.get_schema_map("conn1", "db1").await.is_none());
        let mut all = BTreeMap::new();
        all.insert("users".to_string(), vec![column.clone()]);
        cache.put_all_columns("conn1", "db1", all).await;
        let map = cache.get_schema_map("conn1", "db1").await.unwrap();
        assert_eq!(map["users"], vec!["id".to_string()]);

        // Discard per schema
        cache.put_tables("conn1", "db2", &[table.clone()]).await;
        cache.invalidate_schema("conn1", "db1").await;
        assert!(cache.get_tables("conn1", "db1").await.is_none());
        assert!(cache.get_tables("conn1", "db2").await.is_some());

        // Discard per connection (other connections remain)
        cache.put_tables("conn2", "db1", &[table.clone()]).await;
        cache.invalidate_connection("conn1").await;
        assert!(cache.get_tables("conn1", "db2").await.is_none());
        assert!(cache.get_tables("conn2", "db1").await.is_some());

        // Discard everything
        cache.clear().await;
        assert!(cache.get_tables("conn2", "db1").await.is_none());
    }
}
