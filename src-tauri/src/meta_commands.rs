use crate::db::Engine;
use crate::error::AppError;

/// The result of interpreting a meta command.
#[derive(Debug, PartialEq, Eq)]
pub enum MetaCommand {
    /// Converted into a catalog query SQL (executed as is)
    Sql(String),
    /// `\c <schema>` — switches the active schema (database).
    /// It changes connection state rather than running SQL, so lib.rs handles it before execution.
    Connect(String),
}

/// Interprets psql-style meta commands (\l, \dt, etc.) and `USE <database>`.
///
/// Most of them are translated into read-only catalog query SQL. Only `\c <schema>` and
/// `USE <database>` return MetaCommand::Connect, which represents switching the active schema
/// rather than SQL.
/// Other stateful commands such as \i (run a file) are out of scope.
/// Returns None if the input is neither a meta command nor `USE`, and an error for an
/// unsupported meta command.
pub fn translate(engine: Engine, input: &str) -> Result<Option<MetaCommand>, AppError> {
    if let Some(command) = translate_use(engine, input)? {
        return Ok(Some(command));
    }
    let trimmed = input.trim();
    if !trimmed.starts_with('\\') {
        return Ok(None);
    }
    // psql-style meta commands are for SQL engines only
    // (DynamoDB uses PartiQL, which is SQL-like but has no catalog query SQL, so it is excluded)
    if matches!(
        engine,
        Engine::Redis | Engine::Elasticsearch | Engine::DynamoDb
    ) {
        return Err(AppError::Config(
            "Meta commands (\\...) are not supported for this engine".into(),
        ));
    }
    // Ignore a trailing semicolon so that it still works when added out of SQL habit
    let trimmed = trimmed.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    // A bracketed SQL Server database name (`\c [Sales Data]`) may contain whitespace, so read
    // it as a single identifier before splitting on whitespace
    if engine == Engine::MsSql {
        if let Some(connect) = translate_bracketed_connect(trimmed)? {
            return Ok(Some(connect));
        }
    }
    let mut parts = trimmed.split_whitespace();
    let command = parts.next().unwrap_or("");
    let arg = parts.next();

    // \c is handled first, common to all engines (it changes connection state instead of converting to SQL)
    if matches!(command, "\\c" | "\\connect") {
        // arg has been consumed, so what remains are tokens after the database name
        let extra: Vec<&str> = parts.collect();
        return Ok(Some(MetaCommand::Connect(parse_connect_arg(
            engine, command, arg, &extra,
        )?)));
    }

    let sql = match engine {
        Engine::Postgres => postgres_meta(command, arg)?,
        Engine::MySql => mysql_meta(command, arg)?,
        Engine::Sqlite => sqlite_meta(command, arg)?,
        Engine::DuckDb => duckdb_meta(command, arg)?,
        Engine::MsSql => mssql_meta(command, arg)?,
        // rejected by the early return at the top
        Engine::Redis | Engine::Elasticsearch | Engine::DynamoDb => unreachable!(),
    };
    Ok(Some(MetaCommand::Sql(sql)))
}

/// Interprets `USE <database>` as switching the active schema, the same as `\c <database>`.
/// Returns None for out-of-scope input (it is executed as regular SQL).
///
/// Sending `USE` to the DB as is does not switch: MySQL's `USE` is a per-session change and
/// does not affect the next query, which lands on a different connection in the pool.
/// In addition, `USE` is not a fetch-type statement, so the readonly guard also rejects it.
/// Making it the same MetaCommand::Connect as `\c` re-creates the pool, which solves both
/// (the switch itself is not a write, so it is fine even on a readonly connection).
///
/// PostgreSQL has no `USE` statement (running it as is is a syntax error), but it is often
/// typed out of MySQL habit, so it is accepted as a switch too. sqlite / duckdb do not support
/// `\c` itself (the schema is the DB file path), so they are excluded.
/// DuckDB's `USE` works natively, so it is executed as is.
/// SQL Server's `USE` is also a per-session change, so it is treated as a switch, like MySQL
/// (there is only one connection, but the schema override must go through to keep the Database
/// field and the TABLES pane in sync).
fn translate_use(engine: Engine, input: &str) -> Result<Option<MetaCommand>, AppError> {
    if !matches!(engine, Engine::MySql | Engine::Postgres | Engine::MsSql) {
        return Ok(None);
    }
    // Keyword detection matches leading_keyword (leading comments are skipped)
    let rest = crate::db::strip_leading_comments(input);
    let keyword_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if !rest[..keyword_end].eq_ignore_ascii_case("use") {
        return Ok(None);
    }
    // Look only at the body, with the trailing semicolon and comment (`USE mydb; -- switch`) removed.
    // Use scan_sql's body_end so we do not rewrite each dialect's comment rules.
    // body_end is a byte position relative to input, so convert the keyword end to input-relative too
    // (the keyword itself is code, so body_end lies after it; if not, somehow, it is treated as
    // having no argument and falls on the safe side of "do not switch")
    let arg_start = input.len() - rest.len() + keyword_end;
    let body_end = crate::db::scan_sql(input, engine).body_end;
    let after = input.get(arg_start..body_end).unwrap_or("");
    // A multi-statement input like `USE db; SELECT 1` would switch and silently drop the second
    // statement, so reject it (body_end already removes the trailing semicolon, so a `;` left
    // here is evidence of a second statement)
    if after.contains(';') {
        return Err(AppError::Config(
            "USE cannot be combined with another statement \
             (run one statement at a time)"
                .into(),
        ));
    }
    // SQL Server's `USE [Sales Data]`: whitespace inside the brackets is not a separator
    if engine == Engine::MsSql && after.trim_start().starts_with('[') {
        return Ok(Some(MetaCommand::Connect(bracketed_database_name(
            after.trim_start(),
            "USE",
        )?)));
    }
    let mut parts = after.split_whitespace();
    let Some(name) = parts.next() else {
        return Err(AppError::Config(
            "USE requires a database name (usage: USE <database>)".into(),
        ));
    };
    if parts.next().is_some() {
        return Err(AppError::Config(
            "USE takes only a database name (usage: USE <database>)".into(),
        ));
    }
    let name = unquote_database_name(engine, name);
    Ok(Some(MetaCommand::Connect(
        validate_database_name(name)?.to_string(),
    )))
}

/// SQL Server's `\c [name]` / `\connect [name]`: the inside of the brackets may contain
/// whitespace and dots, so read it as an identifier before splitting on whitespace. Returns None
/// if it does not start with a bracket (handled by the normal path).
fn translate_bracketed_connect(trimmed: &str) -> Result<Option<MetaCommand>, AppError> {
    let Some((command, rest)) = trimmed.split_once(char::is_whitespace) else {
        return Ok(None);
    };
    if !matches!(command, "\\c" | "\\connect") {
        return Ok(None);
    }
    let rest = rest.trim_start();
    if !rest.starts_with('[') {
        return Ok(None);
    }
    Ok(Some(MetaCommand::Connect(bracketed_database_name(
        rest, command,
    )?)))
}

/// Reads and validates a bracketed database name (SQL Server). If anything remains after the
/// closing bracket, it is rejected as an extra argument. The contents are not embedded in SQL;
/// they are only passed to the connection option (tiberius's `database`), so the identifier
/// shape does not matter — we only check that it is non-empty, contains no control characters,
/// and is within SQL Server's identifier length (128).
fn bracketed_database_name(input: &str, command: &str) -> Result<String, AppError> {
    let Some((name, tail)) = crate::engines::mssql::parse_bracketed(input) else {
        return Err(AppError::Config(format!(
            "Invalid database name: {input} (a bracketed name must be closed with ])"
        )));
    };
    if !tail.trim().is_empty() {
        return Err(AppError::Config(format!(
            "{command} takes only a database name (usage: {command} <database>)"
        )));
    }
    if name.is_empty() || name.chars().count() > 128 || name.chars().any(char::is_control) {
        return Err(AppError::Config(format!("Invalid database name: [{name}]")));
    }
    Ok(name)
}

/// Strips the identifier quotes from a `USE` argument. The quote characters are `` ` `` for MySQL,
/// `"` for PostgreSQL, and `[...]` (`"` also allowed) for SQL Server. This is so that the
/// dialect-correct spelling such as `USE \`my-db\`` is accepted as is. The stripped contents are
/// validated by validate_database_name (names that use escapes inside quotes are not supported).
fn unquote_database_name(engine: Engine, name: &str) -> &str {
    if engine == Engine::MsSql {
        if let Some(inner) = name.strip_prefix('[').and_then(|n| n.strip_suffix(']')) {
            return inner;
        }
    }
    let quote = if engine == Engine::MySql { '`' } else { '"' };
    name.strip_prefix(quote)
        .and_then(|inner| inner.strip_suffix(quote))
        .unwrap_or(name)
}

/// Validates the argument of `\c <schema>`.
///
/// For sqlite / duckdb the schema is the DB file path and switching would open a different DB
/// file, so they are excluded (splitting connections in the config file is clearer).
fn parse_connect_arg(
    engine: Engine,
    command: &str,
    arg: Option<&str>,
    extra: &[&str],
) -> Result<String, AppError> {
    if matches!(engine, Engine::Sqlite | Engine::DuckDb) {
        let label = if engine == Engine::Sqlite {
            "SQLite"
        } else {
            "DuckDB"
        };
        return Err(AppError::Config(format!(
            "{command} is not supported for {label} \
             (the schema is a database file path; define another connection instead)"
        )));
    }
    let Some(name) = arg else {
        return Err(AppError::Config(format!(
            "{command} requires a database name (usage: {command} <database>)"
        )));
    };
    // psql's \c can take user / host / port after the database, but only the database can be
    // switched here. Silently ignoring them would make people think they connected as a different
    // user, so extra arguments are an error
    if !extra.is_empty() {
        return Err(AppError::Config(format!(
            "{command} takes only a database name (usage: {command} <database>). \
             Connecting as another user or host is not supported; \
             define another connection in the config instead"
        )));
    }
    Ok(validate_database_name(name)?.to_string())
}

/// Validates the database name used as the argument of `\c`.
///
/// It is a value passed to the connection options and is not embedded in SQL, but only a valid
/// identifier shape is accepted so that a typo does not break the pool.
/// Unlike validate_relation_name, which allows the schema.table form, dots are not allowed.
fn validate_database_name(name: &str) -> Result<&str, AppError> {
    let mut chars = name.chars();
    let valid = matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '-');
    if !valid {
        return Err(AppError::Config(format!(
            "Invalid database name: {name} (only simple identifiers are supported)"
        )));
    }
    Ok(name)
}

/// Validates a table name argument. It is embedded in SQL, so only characters safe as an identifier are allowed.
/// Quoted identifiers (with spaces or symbols) are not supported.
/// Used for \d and also for validating table names in the schema browser (schema_info).
pub(crate) fn validate_relation_name(name: &str) -> Result<&str, AppError> {
    let parts: Vec<&str> = name.split('.').collect();
    let valid = !parts.is_empty()
        && parts.len() <= 2
        && parts.iter().all(|part| {
            let mut chars = part.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        });
    if !valid {
        return Err(AppError::Config(format!(
            "Invalid table name: {name} \
             (only simple identifiers like schema.table are supported)"
        )));
    }
    Ok(name)
}

fn unsupported(command: &str, supported: &str) -> AppError {
    AppError::Config(format!(
        "Unsupported meta command: {command} (supported: {supported})"
    ))
}

fn postgres_meta(command: &str, arg: Option<&str>) -> Result<String, AppError> {
    let sql = match (command, arg) {
        ("\\l" | "\\list", _) => "SELECT d.datname AS name, \
             pg_catalog.pg_get_userbyid(d.datdba) AS owner, \
             pg_catalog.pg_encoding_to_char(d.encoding) AS encoding, \
             d.datcollate AS collate, d.datctype AS ctype \
             FROM pg_catalog.pg_database d \
             WHERE d.datistemplate = false ORDER BY 1"
            .to_string(),
        ("\\dt", _) => relation_list_sql("('r','p')"),
        ("\\dv", _) => relation_list_sql("('v','m')"),
        ("\\d", None) => relation_list_sql("('r','p','v','m','S')"),
        ("\\d", Some(name)) => {
            let name = validate_relation_name(name)?;
            format!(
                "SELECT a.attname AS column, \
                 pg_catalog.format_type(a.atttypid, a.atttypmod) AS type, \
                 CASE WHEN a.attnotnull THEN 'not null' ELSE '' END AS nullable, \
                 pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS default \
                 FROM pg_catalog.pg_attribute a \
                 LEFT JOIN pg_catalog.pg_attrdef d \
                   ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
                 WHERE a.attrelid = '{name}'::regclass \
                   AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum"
            )
        }
        ("\\dn", _) => "SELECT n.nspname AS name, \
             pg_catalog.pg_get_userbyid(n.nspowner) AS owner \
             FROM pg_catalog.pg_namespace n \
             WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' \
             ORDER BY 1"
            .to_string(),
        ("\\du", _) => "SELECT r.rolname AS role_name, r.rolsuper AS superuser, \
             r.rolcreaterole AS create_role, r.rolcreatedb AS create_db, \
             r.rolcanlogin AS can_login \
             FROM pg_catalog.pg_roles r ORDER BY 1"
            .to_string(),
        _ => {
            return Err(unsupported(
                command,
                "\\l \\list \\dt \\dv \\dn \\du \\d [table] \\c <database>",
            ));
        }
    };
    Ok(sql)
}

/// Postgres relation listing SQL (narrowed by a set of relkind).
fn relation_list_sql(relkinds: &str) -> String {
    format!(
        "SELECT n.nspname AS schema, c.relname AS name, \
         CASE c.relkind WHEN 'r' THEN 'table' WHEN 'p' THEN 'partitioned table' \
           WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' \
           WHEN 'S' THEN 'sequence' ELSE c.relkind::text END AS type, \
         pg_catalog.pg_get_userbyid(c.relowner) AS owner \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN {relkinds} \
           AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' \
         ORDER BY 1, 2"
    )
}

fn mysql_meta(command: &str, arg: Option<&str>) -> Result<String, AppError> {
    let sql = match (command, arg) {
        ("\\l" | "\\list", _) => "SHOW DATABASES".to_string(),
        // SHOW TABLES includes views too, so \dt narrows to base tables
        ("\\dt", _) => "SHOW FULL TABLES WHERE Table_type = 'BASE TABLE'".to_string(),
        ("\\d", None) => "SHOW TABLES".to_string(),
        ("\\dv", _) => "SHOW FULL TABLES WHERE Table_type = 'VIEW'".to_string(),
        ("\\d", Some(name)) => {
            let name = validate_relation_name(name)?;
            // Quote schema.table as `schema`.`table`
            let quoted = name
                .split('.')
                .map(|part| format!("`{part}`"))
                .collect::<Vec<_>>()
                .join(".");
            format!("DESCRIBE {quoted}")
        }
        ("\\du", _) => {
            "SELECT User AS user, Host AS host FROM mysql.user ORDER BY 1, 2".to_string()
        }
        _ => {
            return Err(unsupported(
                command,
                "\\l \\list \\dt \\dv \\du \\d [table] \\c <database>",
            ));
        }
    };
    Ok(sql)
}

fn sqlite_meta(command: &str, arg: Option<&str>) -> Result<String, AppError> {
    let sql = match (command, arg) {
        ("\\l" | "\\list", _) => "PRAGMA database_list".to_string(),
        ("\\dt", _) => "SELECT name, type FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name"
            .to_string(),
        ("\\dv", _) => "SELECT name, type FROM sqlite_master \
             WHERE type = 'view' ORDER BY name"
            .to_string(),
        ("\\d", None) => "SELECT name, type FROM sqlite_master \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             ORDER BY type, name"
            .to_string(),
        ("\\d", Some(name)) => {
            let name = validate_relation_name(name)?;
            format!("PRAGMA table_info(\"{name}\")")
        }
        _ => {
            return Err(unsupported(command, "\\l \\list \\dt \\dv \\d [table]"));
        }
    };
    Ok(sql)
}

/// DuckDB has both information_schema and PRAGMA (sqlite-compatible).
/// Listings use information_schema and column definitions use PRAGMA table_info.
fn duckdb_meta(command: &str, arg: Option<&str>) -> Result<String, AppError> {
    let sql = match (command, arg) {
        ("\\l" | "\\list", _) => "PRAGMA database_list".to_string(),
        ("\\dt", _) => "SELECT table_schema AS schema, table_name AS name, \
             'table' AS type FROM information_schema.tables \
             WHERE table_type = 'BASE TABLE' ORDER BY 1, 2"
            .to_string(),
        ("\\dv", _) => "SELECT table_schema AS schema, table_name AS name, \
             'view' AS type FROM information_schema.tables \
             WHERE table_type = 'VIEW' ORDER BY 1, 2"
            .to_string(),
        ("\\dn", _) => "SELECT schema_name AS name \
             FROM information_schema.schemata ORDER BY 1"
            .to_string(),
        ("\\d", None) => "SELECT table_schema AS schema, table_name AS name, \
             lower(table_type) AS type FROM information_schema.tables \
             ORDER BY 1, 2"
            .to_string(),
        ("\\d", Some(name)) => {
            let name = validate_relation_name(name)?;
            format!("PRAGMA table_info('{name}')")
        }
        _ => {
            return Err(unsupported(
                command,
                "\\l \\list \\dt \\dv \\dn \\d [table]",
            ));
        }
    };
    Ok(sql)
}

/// SQL Server has both INFORMATION_SCHEMA (ANSI) and the sys.* catalogs.
/// Listings use INFORMATION_SCHEMA; database / schema / principal listings use sys.*.
/// The name for \d <table> is only an identifier that has passed validate_relation_name,
/// so it can be embedded as a string literal (no quotes or `;` are included).
fn mssql_meta(command: &str, arg: Option<&str>) -> Result<String, AppError> {
    let sql = match (command, arg) {
        ("\\l" | "\\list", _) => "SELECT name, database_id, create_date, state_desc \
             FROM sys.databases ORDER BY name"
            .to_string(),
        ("\\dt", _) => "SELECT TABLE_SCHEMA AS [schema], TABLE_NAME AS name, \
             'table' AS type FROM INFORMATION_SCHEMA.TABLES \
             WHERE TABLE_TYPE = 'BASE TABLE' ORDER BY 1, 2"
            .to_string(),
        ("\\dv", _) => "SELECT TABLE_SCHEMA AS [schema], TABLE_NAME AS name, \
             'view' AS type FROM INFORMATION_SCHEMA.TABLES \
             WHERE TABLE_TYPE = 'VIEW' ORDER BY 1, 2"
            .to_string(),
        ("\\dn", _) => "SELECT name FROM sys.schemas ORDER BY name".to_string(),
        ("\\d", None) => "SELECT TABLE_SCHEMA AS [schema], TABLE_NAME AS name, \
             LOWER(TABLE_TYPE) AS type FROM INFORMATION_SCHEMA.TABLES ORDER BY 1, 2"
            .to_string(),
        ("\\d", Some(name)) => {
            let name = validate_relation_name(name)?;
            let (schema, table) = crate::engines::mssql::split_qualified(name);
            format!(
                "SELECT COLUMN_NAME AS [column], DATA_TYPE AS [type], \
                 CHARACTER_MAXIMUM_LENGTH AS [length], NUMERIC_PRECISION AS [precision], \
                 NUMERIC_SCALE AS [scale], IS_NULLABLE AS [nullable], \
                 COLUMN_DEFAULT AS [default] \
                 FROM INFORMATION_SCHEMA.COLUMNS \
                 WHERE TABLE_SCHEMA = '{schema}' AND TABLE_NAME = '{table}' \
                 ORDER BY ORDINAL_POSITION"
            )
        }
        ("\\du", _) => "SELECT name, type_desc, create_date \
             FROM sys.database_principals \
             WHERE type IN ('S', 'U', 'G') AND name NOT LIKE '##%' ORDER BY name"
            .to_string(),
        _ => {
            return Err(unsupported(
                command,
                "\\l \\list \\dt \\dv \\dn \\du \\d [table] \\c <database>",
            ));
        }
    };
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// For verifying meta commands that translate to SQL. Extracts the resulting SQL.
    fn sql_of(engine: Engine, input: &str) -> String {
        match translate(engine, input).unwrap().unwrap() {
            MetaCommand::Sql(sql) => sql,
            other => panic!("expected SQL, got {other:?}"),
        }
    }

    #[test]
    fn test_trailing_semicolon_is_ignored() {
        let sql = sql_of(Engine::Postgres, "\\dt;");
        assert!(sql.contains("pg_catalog.pg_class"));
        let sql = sql_of(Engine::Postgres, "\\d users;");
        assert!(sql.contains("'users'::regclass"));
        let sql = sql_of(Engine::MySql, "\\l ;;");
        assert_eq!(sql, "SHOW DATABASES");
    }

    #[test]
    fn test_non_meta_returns_none() {
        assert!(translate(Engine::Postgres, "SELECT 1").unwrap().is_none());
        assert!(translate(Engine::MySql, "  SHOW TABLES").unwrap().is_none());
    }

    #[test]
    fn test_postgres_meta() {
        let sql = sql_of(Engine::Postgres, "\\l");
        assert!(sql.contains("pg_database"));
        let sql = sql_of(Engine::Postgres, "\\list");
        assert!(sql.contains("pg_database"));
        let sql = sql_of(Engine::Postgres, "\\dt");
        assert!(sql.contains("('r','p')"));
        let sql = sql_of(Engine::Postgres, "\\d users");
        assert!(sql.contains("'users'::regclass"));
        let sql = sql_of(Engine::Postgres, "\\d public.users");
        assert!(sql.contains("'public.users'::regclass"));
        let sql = sql_of(Engine::Postgres, "\\du");
        assert!(sql.contains("pg_roles"));
        let sql = sql_of(Engine::Postgres, "\\dn");
        assert!(sql.contains("pg_namespace"));
    }

    #[test]
    fn test_mysql_meta() {
        assert_eq!(
            sql_of(Engine::MySql, "\\l"),
            "SHOW DATABASES"
        );
        assert_eq!(
            sql_of(Engine::MySql, "\\dt"),
            "SHOW FULL TABLES WHERE Table_type = 'BASE TABLE'"
        );
        assert_eq!(
            sql_of(Engine::MySql, "\\d"),
            "SHOW TABLES"
        );
        assert_eq!(
            sql_of(Engine::MySql, "\\d users"),
            "DESCRIBE `users`"
        );
        assert_eq!(
            sql_of(Engine::MySql, "\\d mydb.users"),
            "DESCRIBE `mydb`.`users`"
        );
    }

    #[test]
    fn test_sqlite_meta() {
        let sql = sql_of(Engine::Sqlite, "\\dt");
        assert!(sql.contains("sqlite_master"));
        // With an ESCAPE clause so that _ is not treated as a LIKE wildcard
        assert!(sql.contains("ESCAPE"));
        assert_eq!(
            sql_of(Engine::Sqlite, "\\d users"),
            "PRAGMA table_info(\"users\")"
        );
    }

    #[test]
    fn test_duckdb_meta() {
        assert_eq!(sql_of(Engine::DuckDb, "\\l"), "PRAGMA database_list");
        let sql = sql_of(Engine::DuckDb, "\\dt");
        assert!(sql.contains("information_schema.tables"));
        assert!(sql.contains("BASE TABLE"));
        let sql = sql_of(Engine::DuckDb, "\\dv");
        assert!(sql.contains("'VIEW'"));
        let sql = sql_of(Engine::DuckDb, "\\dn");
        assert!(sql.contains("schemata"));
        assert_eq!(
            sql_of(Engine::DuckDb, "\\d users"),
            "PRAGMA table_info('users')"
        );
        // Reject arguments that could lead to injection
        assert!(translate(Engine::DuckDb, "\\d users'; DROP TABLE x; --").is_err());
        // \c is a DB file path, so reject it
        let err = translate(Engine::DuckDb, "\\c other").unwrap_err();
        assert!(err.to_string().contains("not supported for DuckDB"));
        // Unsupported command
        assert!(translate(Engine::DuckDb, "\\du").is_err());
    }

    #[test]
    fn test_mssql_meta() {
        let sql = sql_of(Engine::MsSql, "\\l");
        assert!(sql.contains("sys.databases"));
        let sql = sql_of(Engine::MsSql, "\\dt");
        assert!(sql.contains("INFORMATION_SCHEMA.TABLES"));
        assert!(sql.contains("BASE TABLE"));
        let sql = sql_of(Engine::MsSql, "\\dv");
        assert!(sql.contains("'VIEW'"));
        assert!(sql_of(Engine::MsSql, "\\dn").contains("sys.schemas"));
        assert!(sql_of(Engine::MsSql, "\\du").contains("sys.database_principals"));
        // Unqualified names are narrowed to dbo; schema.table is narrowed to that schema
        let sql = sql_of(Engine::MsSql, "\\d users");
        assert!(
            sql.contains("TABLE_SCHEMA = 'dbo' AND TABLE_NAME = 'users'"),
            "{sql}"
        );
        // Wrap all column aliases in square brackets (PRECISION etc. are T-SQL reserved words)
        for alias in [
            "[column]",
            "[type]",
            "[length]",
            "[precision]",
            "[scale]",
            "[nullable]",
            "[default]",
        ] {
            assert!(sql.contains(&format!(" AS {alias}")), "{alias}: {sql}");
        }
        let sql = sql_of(Engine::MsSql, "\\d sales.orders");
        assert!(
            sql.contains("TABLE_SCHEMA = 'sales' AND TABLE_NAME = 'orders'"),
            "{sql}"
        );
        // Reject arguments that could lead to injection
        assert!(translate(Engine::MsSql, "\\d users'; DROP TABLE x; --").is_err());
        assert!(translate(Engine::MsSql, "\\d [users]").is_err());
        // \c and USE switch the database. Bracket quotes are stripped too
        assert!(matches!(
            translate(Engine::MsSql, "\\c reporting").unwrap(),
            Some(MetaCommand::Connect(ref db)) if db == "reporting"
        ));
        assert!(matches!(
            translate(Engine::MsSql, "USE [reporting];").unwrap(),
            Some(MetaCommand::Connect(ref db)) if db == "reporting"
        ));
        assert!(translate(Engine::MsSql, "USE db; SELECT 1").is_err());
        // Whitespace and dots inside brackets are not separators (a Codex review finding).
        // `]]` goes back to `]`
        for input in [
            "USE [Sales Data]",
            "\\c [Sales Data]",
            "\\connect  [Sales Data] ;",
        ] {
            assert!(
                matches!(
                    translate(Engine::MsSql, input).unwrap(),
                    Some(MetaCommand::Connect(ref db)) if db == "Sales Data"
                ),
                "{input}"
            );
        }
        assert!(matches!(
            translate(Engine::MsSql, "USE [a.b]]c]").unwrap(),
            Some(MetaCommand::Connect(ref db)) if db == "a.b]c"
        ));
        // Unclosed, extra arguments, and empty are rejected
        assert!(translate(Engine::MsSql, "USE [unclosed").is_err());
        assert!(translate(Engine::MsSql, "USE [a] extra").is_err());
        assert!(translate(Engine::MsSql, "\\c [a] [b]").is_err());
        assert!(translate(Engine::MsSql, "USE []").is_err());
        // Brackets for other engines are invalid as an identifier, as before
        assert!(translate(Engine::MySql, "USE [x]").is_err());
    }

    #[test]
    fn test_injection_is_rejected() {
        // Arguments that could lead to SQL injection are rejected
        assert!(translate(Engine::Postgres, "\\d users'; DROP TABLE x; --").is_err());
        assert!(translate(Engine::Postgres, "\\d users'||x").is_err());
        assert!(translate(Engine::MySql, "\\d `users`").is_err());
        assert!(translate(Engine::Sqlite, "\\d a\"b").is_err());
        assert!(translate(Engine::Postgres, "\\d a.b.c").is_err());
    }

    #[test]
    fn test_unsupported_command_is_error() {
        let err = translate(Engine::Postgres, "\\x").unwrap_err();
        assert!(err.to_string().contains("Unsupported meta command"));
        assert!(translate(Engine::MySql, "\\dn").is_err());
        assert!(translate(Engine::Sqlite, "\\du").is_err());
    }

    #[test]
    fn test_connect_meta() {
        // Both \c and \connect are interpreted as a schema switch
        assert_eq!(
            translate(Engine::Postgres, "\\c otherdb").unwrap().unwrap(),
            MetaCommand::Connect("otherdb".to_string())
        );
        assert_eq!(
            translate(Engine::MySql, "\\connect other_db;")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("other_db".to_string())
        );
        // A database name starting with a digit (which can exist in MySQL) is accepted too
        assert_eq!(
            translate(Engine::MySql, "\\c 2024_logs").unwrap().unwrap(),
            MetaCommand::Connect("2024_logs".to_string())
        );
    }

    #[test]
    fn test_connect_without_argument_is_error() {
        let err = translate(Engine::Postgres, "\\c").unwrap_err();
        assert!(err.to_string().contains("requires a database name"));
    }

    #[test]
    fn test_connect_rejects_extra_arguments() {
        // psql's `\c <db> <user>` form. We cannot switch users, so reject it
        // rather than silently switching only the database
        let err = translate(Engine::Postgres, "\\c proddb readonly_user").unwrap_err();
        assert!(err.to_string().contains("takes only a database name"));
        assert!(translate(Engine::MySql, "\\c proddb host 3306;").is_err());
    }

    #[test]
    fn test_connect_rejects_unsafe_names() {
        // It is a value passed to the connection options, so it is not SQL injection, but
        // reject anything unnatural as an identifier as a typo
        assert!(translate(Engine::Postgres, "\\c a;b").is_err());
        assert!(translate(Engine::Postgres, "\\c my.db").is_err());
        assert!(translate(Engine::MySql, "\\c `db`").is_err());
    }

    #[test]
    fn test_use_switches_database() {
        // USE is handled as the same active schema switch as \c
        // (a per-session USE would not affect the next connection in the pool)
        assert_eq!(
            translate(Engine::MySql, "USE chatbot_backend;")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("chatbot_backend".to_string())
        );
        // Mixed lower / upper case, surrounding whitespace, and newlines are the same
        assert_eq!(
            translate(Engine::MySql, "  use  2024_logs  \n")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("2024_logs".to_string())
        );
        // PostgreSQL has no USE statement, but it is typed out of MySQL habit, so accept it
        assert_eq!(
            translate(Engine::Postgres, "Use otherdb").unwrap().unwrap(),
            MetaCommand::Connect("otherdb".to_string())
        );
        // Leading comments are skipped, as in leading_keyword
        assert_eq!(
            translate(Engine::MySql, "-- switch\nUSE mydb")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("mydb".to_string())
        );
        // Trailing comments are outside the body, so ignore them too (scan_sql's body_end)
        assert_eq!(
            translate(Engine::MySql, "USE mydb; -- switch to backend")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("mydb".to_string())
        );
        assert_eq!(
            translate(Engine::Postgres, "USE mydb /* switch */")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("mydb".to_string())
        );
    }

    #[test]
    fn test_use_accepts_quoted_database_name() {
        // Dialect quoting (MySQL's `db`) is accepted as is
        assert_eq!(
            translate(Engine::MySql, "USE `my-db`").unwrap().unwrap(),
            MetaCommand::Connect("my-db".to_string())
        );
        assert_eq!(
            translate(Engine::Postgres, "USE \"mydb\"")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("mydb".to_string())
        );
        // Quotes from another dialect are not stripped, so it becomes invalid as an identifier
        assert!(translate(Engine::MySql, "USE \"mydb\"").is_err());
        assert!(translate(Engine::Postgres, "USE `mydb`").is_err());
    }

    #[test]
    fn test_use_argument_errors() {
        let err = translate(Engine::MySql, "USE").unwrap_err();
        assert!(err.to_string().contains("requires a database name"));
        assert!(translate(Engine::MySql, "USE ;").is_err());
        // A multi-statement input would switch and drop the second statement, so reject it
        let err = translate(Engine::MySql, "USE mydb; DELETE FROM t").unwrap_err();
        assert!(err.to_string().contains("one statement at a time"));
        assert!(translate(Engine::MySql, "USE mydb;DELETE FROM t").is_err());
        assert!(translate(Engine::MySql, "USE a;b").is_err());
        // Extra arguments after the database name are also rejected
        let err = translate(Engine::MySql, "USE mydb other").unwrap_err();
        assert!(err.to_string().contains("takes only a database name"));
        // MySQL executable comments are executed by the server, so do not treat them as comments
        // (we must not silently switch only and discard the contents)
        assert!(translate(Engine::MySql, "USE mydb /*! DROP TABLE t */").is_err());
        // Reject names that are unnatural as an identifier
        assert!(translate(Engine::MySql, "USE my.db").is_err());
    }

    #[test]
    fn test_use_is_not_intercepted_for_other_engines() {
        // sqlite / duckdb do not support \c itself. DuckDB's USE works natively,
        // so let it run as regular SQL (return None)
        assert!(translate(Engine::Sqlite, "USE mydb").unwrap().is_none());
        assert!(translate(Engine::DuckDb, "USE mydb").unwrap().is_none());
        assert!(translate(Engine::Redis, "USE mydb").unwrap().is_none());
        // Do not misread another identifier that starts with USE as a switch
        assert!(translate(Engine::MySql, "USER_TABLE").unwrap().is_none());
        assert!(translate(Engine::MySql, "SELECT * FROM t USE INDEX (i)")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_connect_is_rejected_for_sqlite() {
        // The sqlite schema is a DB file path, so it is not a switch target
        let err = translate(Engine::Sqlite, "\\c other").unwrap_err();
        assert!(err.to_string().contains("not supported for SQLite"));
    }
}
