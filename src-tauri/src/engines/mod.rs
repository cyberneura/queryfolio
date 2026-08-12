//! エンジンごとの差分を差し替え可能にするプラガブル層。
//!
//! - `EngineCapabilities`: エンジンの能力宣言 (エディタ言語・クエリファイル
//!   拡張子・スキーマ/テーブル閲覧・Explain 等の対応可否)。Rust 側を単一の
//!   真実とし、`ConnectionInfo` に載せてフロントへ渡す。フロントはエンジン名
//!   ではなく capability で UI を出し分ける。
//! - `engines::redis` などエンジン別モジュール: sqlx を使わないエンジンの
//!   接続・実行・ガードの実装。`db.rs` の enum match は各モジュールへの
//!   1 行委譲に留め、エンジン追加時は「モジュールを足す + capability を
//!   宣言する + enum に variant を足す」だけで済むようにする。

pub mod duckdb;
pub mod dynamodb;
pub mod elasticsearch;
pub mod redis;

use serde::Serialize;

use crate::db::Engine;

/// エンジンの能力宣言。フロントエンドの UI 出し分けの単一の真実。
/// 新しいエンジンを追加する時はここに能力を宣言する。
#[derive(Debug, Clone, Serialize)]
pub struct EngineCapabilities {
    /// エディタのシンタックスハイライト言語 ("sql" | "redis")
    pub editor_language: &'static str,
    /// クエリファイルの拡張子 (ドット無し)
    pub file_extension: &'static str,
    /// スキーマ (database) の一覧・切替に対応するか
    pub supports_schemas: bool,
    /// スキーマブラウザ (TABLES ペイン) に対応するか
    pub supports_tables: bool,
    /// EXPLAIN (実行計画) に対応するか
    pub supports_explain: bool,
    /// エディタの Format (整形) に対応するか
    pub supports_format: bool,
    /// 結果グリッドのセル編集 (UPDATE 生成) に対応するか
    pub supports_editable_cells: bool,
    /// AI 機能 (SQL 生成 / 解説) に対応するか
    pub supports_ai: bool,
}

/// SQL 系エンジン (mysql / postgres / sqlite) の共通 capability。
const SQL_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    editor_language: "sql",
    file_extension: "sql",
    supports_schemas: true,
    supports_tables: true,
    supports_explain: true,
    supports_format: true,
    supports_editable_cells: true,
    supports_ai: true,
};

const REDIS_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    editor_language: "redis",
    file_extension: "redis",
    // Redis の「スキーマ」は database 番号 (CYBERNEURA-DEV-408)。
    // SELECT で切り替える概念が接続にあるので、一覧と切替を提供する
    supports_schemas: true,
    supports_tables: false,
    supports_explain: false,
    supports_format: false,
    supports_editable_cells: false,
    supports_ai: false,
};

/// Elasticsearch は Kibana Console 風のリクエストブロックをエディタで扱い、
/// TABLES ペインにはインデックス一覧 + mapping のフィールドを出す。
const ELASTICSEARCH_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    editor_language: "es",
    file_extension: "es",
    supports_schemas: false,
    supports_tables: true,
    supports_explain: false,
    supports_format: false,
    supports_editable_cells: false,
    supports_ai: false,
};

/// DuckDB は SQL エンジンだが、セル編集の適用経路 (run_statements) が
/// sqlx 前提のため supports_editable_cells のみ false にする。
const DUCKDB_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    supports_editable_cells: false,
    ..SQL_CAPABILITIES
};

/// DynamoDB は PartiQL (SQL 互換サブセット) をエディタで扱う。
/// schema はリージョン (一覧・切替の対象外)、EXPLAIN は存在しない。
/// セル編集 (run_statements) と AI (SQL 方言前提) も対象外。
/// TABLES ペインにはテーブル一覧 + キースキーマ・属性定義を出す。
const DYNAMODB_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    supports_schemas: false,
    supports_explain: false,
    supports_editable_cells: false,
    supports_ai: false,
    ..SQL_CAPABILITIES
};

pub fn capabilities(engine: Engine) -> EngineCapabilities {
    match engine {
        Engine::MySql | Engine::Postgres | Engine::Sqlite => SQL_CAPABILITIES.clone(),
        Engine::Redis => REDIS_CAPABILITIES.clone(),
        Engine::Elasticsearch => ELASTICSEARCH_CAPABILITIES.clone(),
        Engine::DuckDb => DUCKDB_CAPABILITIES.clone(),
        Engine::DynamoDb => DYNAMODB_CAPABILITIES.clone(),
    }
}

/// 設定の engine 文字列から capability を解決する。
/// 未知のエンジンは SQL 相当を返す (設定エラー自体は接続時に
/// `db::parse_engine` が返すため、ここでは一覧表示を壊さない)。
pub fn capabilities_for_name(engine: &str) -> EngineCapabilities {
    match crate::db::parse_engine(engine) {
        Ok(engine) => capabilities(engine),
        Err(_) => SQL_CAPABILITIES.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capabilities_for_name() {
        let sql = capabilities_for_name("mysql");
        assert_eq!(sql.editor_language, "sql");
        assert_eq!(sql.file_extension, "sql");
        assert!(sql.supports_tables);

        let redis = capabilities_for_name("redis");
        assert_eq!(redis.editor_language, "redis");
        assert_eq!(redis.file_extension, "redis");
        // Redis の「スキーマ」は database 番号。Database 欄で切り替えられる
        // (CYBERNEURA-DEV-408)
        assert!(redis.supports_schemas);
        // エイリアスも同じ capability
        assert!(capabilities_for_name("valkey").supports_schemas);
        assert!(!redis.supports_tables);
        assert!(!redis.supports_explain);
        assert!(!redis.supports_format);
        assert!(!redis.supports_editable_cells);
        assert!(!redis.supports_ai);

        let es = capabilities_for_name("elasticsearch");
        assert_eq!(es.editor_language, "es");
        assert_eq!(es.file_extension, "es");
        assert!(!es.supports_schemas);
        assert!(es.supports_tables);
        assert!(!es.supports_explain);
        assert!(!es.supports_format);
        assert!(!es.supports_editable_cells);
        assert!(!es.supports_ai);
        // エイリアスも同じ capability
        assert_eq!(capabilities_for_name("es").editor_language, "es");
        assert_eq!(capabilities_for_name("opensearch").editor_language, "es");

        // DuckDB は SQL 系だがセル編集のみ非対応
        let duckdb = capabilities_for_name("duckdb");
        assert_eq!(duckdb.editor_language, "sql");
        assert_eq!(duckdb.file_extension, "sql");
        assert!(duckdb.supports_schemas);
        assert!(duckdb.supports_tables);
        assert!(duckdb.supports_explain);
        assert!(duckdb.supports_format);
        assert!(!duckdb.supports_editable_cells);
        assert!(duckdb.supports_ai);

        // DynamoDB は SQL エディタだが schema / EXPLAIN / セル編集 / AI 非対応
        let dynamodb = capabilities_for_name("dynamodb");
        assert_eq!(dynamodb.editor_language, "sql");
        assert_eq!(dynamodb.file_extension, "sql");
        assert!(!dynamodb.supports_schemas);
        assert!(dynamodb.supports_tables);
        assert!(!dynamodb.supports_explain);
        assert!(dynamodb.supports_format);
        assert!(!dynamodb.supports_editable_cells);
        assert!(!dynamodb.supports_ai);

        // 未知のエンジンは SQL 相当 (エラーは接続時に出す)
        let unknown = capabilities_for_name("oracle");
        assert_eq!(unknown.editor_language, "sql");
    }

    #[test]
    fn test_capabilities_serialize_snake_case() {
        let json = serde_json::to_value(capabilities_for_name("redis")).unwrap();
        assert_eq!(json["editor_language"], "redis");
        assert_eq!(json["supports_editable_cells"], false);
    }
}
