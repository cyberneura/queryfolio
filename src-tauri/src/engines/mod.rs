//! A pluggable layer that makes per-engine differences swappable.
//!
//! - `EngineCapabilities`: declares what each engine supports (editor language, query file
//!   extension, schema/table browsing, Explain, etc.). The Rust side is the single source of
//!   truth; it is attached to `ConnectionInfo` and passed to the frontend. The frontend shows
//!   or hides UI by capability, not by engine name.
//! - Per-engine modules such as `engines::redis`: connection, execution and guard
//!   implementations for engines that do not use sqlx. The enum match in `db.rs` is kept to a
//!   one-line delegation to each module, so adding an engine only takes "add a module +
//!   declare its capabilities + add a variant to the enum".

pub mod duckdb;
pub mod dynamodb;
pub mod elasticsearch;
pub mod mssql;
pub mod redis;

use serde::Serialize;

use crate::db::Engine;

/// Declares what an engine supports. The single source of truth for how the frontend shows or
/// hides UI. When adding a new engine, declare its capabilities here.
#[derive(Debug, Clone, Serialize)]
pub struct EngineCapabilities {
    /// Syntax highlighting language of the editor ("sql" | "redis")
    pub editor_language: &'static str,
    /// Query file extension (without the dot)
    pub file_extension: &'static str,
    /// Whether listing and switching schemas (databases) is supported
    pub supports_schemas: bool,
    /// Whether the schema browser (TABLES pane) is supported
    pub supports_tables: bool,
    /// Whether EXPLAIN (execution plan) is supported
    pub supports_explain: bool,
    /// Whether the editor's Format (pretty-print) is supported
    pub supports_format: bool,
    /// Whether cell editing in the result grid (UPDATE generation) is supported
    pub supports_editable_cells: bool,
    /// Whether AI features (SQL generation / explanation) are supported
    pub supports_ai: bool,
}

/// Common capabilities of the SQL engines (mysql / postgres / sqlite).
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
    // A Redis "schema" is the database number (CYBERNEURA-DEV-408).
    // A connection has the concept of switching with SELECT, so listing and switching are provided
    supports_schemas: true,
    supports_tables: false,
    supports_explain: false,
    supports_format: false,
    supports_editable_cells: false,
    supports_ai: false,
};

/// Elasticsearch works with Kibana Console-style request blocks in the editor, and the
/// TABLES pane shows the index list plus the mapping fields.
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

/// DuckDB is a SQL engine, but the path that applies cell edits (run_statements) assumes
/// sqlx, so only supports_editable_cells is set to false.
const DUCKDB_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    supports_editable_cells: false,
    ..SQL_CAPABILITIES
};

/// DynamoDB works with PartiQL (a SQL-compatible subset) in the editor.
/// The schema is the region (not subject to listing or switching), and EXPLAIN does not exist.
/// Cell editing (run_statements) and AI (which assumes a SQL dialect) are also unsupported.
/// The TABLES pane shows the table list plus key schema and attribute definitions.
const DYNAMODB_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    supports_schemas: false,
    supports_explain: false,
    supports_editable_cells: false,
    supports_ai: false,
    ..SQL_CAPABILITIES
};

/// SQL Server is a SQL engine, but the path that applies cell edits (run_statements) assumes
/// sqlx, so only supports_editable_cells is set to false (same as DuckDB).
/// EXPLAIN is implemented by engines/mssql.rs as a queryfolio pseudo-statement
/// (SET SHOWPLAN_ALL).
/// Format is usable because the frontend's sqlFormat.ts knows the mssql dialect
/// (bracket identifiers, `#temp`); SqlEditor passes the dialect based on the engine.
const MSSQL_CAPABILITIES: EngineCapabilities = EngineCapabilities {
    supports_editable_cells: false,
    ..SQL_CAPABILITIES
};

pub fn capabilities(engine: Engine) -> EngineCapabilities {
    match engine {
        Engine::MySql | Engine::Postgres | Engine::Sqlite => SQL_CAPABILITIES.clone(),
        Engine::MsSql => MSSQL_CAPABILITIES.clone(),
        Engine::Redis => REDIS_CAPABILITIES.clone(),
        Engine::Elasticsearch => ELASTICSEARCH_CAPABILITIES.clone(),
        Engine::DuckDb => DUCKDB_CAPABILITIES.clone(),
        Engine::DynamoDb => DYNAMODB_CAPABILITIES.clone(),
    }
}

/// Resolves capabilities from the engine string in the config.
/// An unknown engine gets SQL-equivalent capabilities (the config error itself is returned by
/// `db::parse_engine` at connect time, so this does not break the list display).
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
        // A Redis "schema" is the database number. It can be switched in the Database field
        // (CYBERNEURA-DEV-408)
        assert!(redis.supports_schemas);
        // Aliases get the same capabilities
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
        // Aliases get the same capabilities
        assert_eq!(capabilities_for_name("es").editor_language, "es");
        assert_eq!(capabilities_for_name("opensearch").editor_language, "es");

        // DuckDB is SQL-based but only cell editing is unsupported
        let duckdb = capabilities_for_name("duckdb");
        assert_eq!(duckdb.editor_language, "sql");
        assert_eq!(duckdb.file_extension, "sql");
        assert!(duckdb.supports_schemas);
        assert!(duckdb.supports_tables);
        assert!(duckdb.supports_explain);
        assert!(duckdb.supports_format);
        assert!(!duckdb.supports_editable_cells);
        assert!(duckdb.supports_ai);

        // DynamoDB uses the SQL editor but does not support schema / EXPLAIN / cell editing / AI
        let dynamodb = capabilities_for_name("dynamodb");
        assert_eq!(dynamodb.editor_language, "sql");
        assert_eq!(dynamodb.file_extension, "sql");
        assert!(!dynamodb.supports_schemas);
        assert!(dynamodb.supports_tables);
        assert!(!dynamodb.supports_explain);
        assert!(dynamodb.supports_format);
        assert!(!dynamodb.supports_editable_cells);
        assert!(!dynamodb.supports_ai);

        // SQL Server is SQL-based but only cell editing is unsupported. Aliases get the same capabilities
        let mssql = capabilities_for_name("mssql");
        assert_eq!(mssql.editor_language, "sql");
        assert_eq!(mssql.file_extension, "sql");
        assert!(mssql.supports_schemas);
        assert!(mssql.supports_tables);
        assert!(mssql.supports_explain);
        assert!(mssql.supports_format);
        assert!(!mssql.supports_editable_cells);
        assert!(mssql.supports_ai);
        assert!(!capabilities_for_name("sqlserver").supports_editable_cells);

        // An unknown engine gets SQL-equivalent capabilities (the error is raised at connect time)
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
