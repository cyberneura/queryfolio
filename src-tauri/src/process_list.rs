//! Server process list (the PROCESS LIST pane, CYBERNEURA-DEV-1144).
//!
//! - MySQL: `SHOW FULL PROCESSLIST`
//! - PostgreSQL: `pg_stat_activity`
//!
//! Fetched on a pooled connection that is not registered with `QueryRuns`, so it works while a
//! query is running on the same connection (the running query holds one of the pool's
//! connections; this takes another). The rows are normalized into `ProcessInfo` and sorted so
//! that the busiest (non-idle, longest running) come first.

use serde::Serialize;
use sqlx::{Column, Executor, Row};

use crate::db::{mysql_value_to_json, pg_value_to_json, DbPool};
use crate::error::AppError;

/// One column of the raw row, for the detail view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcessField {
    pub name: String,
    pub value: Option<String>,
}

/// One server process / session, normalized across engines.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcessInfo {
    /// Process id (MySQL Id / PostgreSQL pid). A string so that BIGINT UNSIGNED ids survive
    /// the trip to JavaScript
    pub id: String,
    pub user: Option<String>,
    pub host: Option<String>,
    pub db: Option<String>,
    /// MySQL Command (Query / Sleep / ...) / PostgreSQL state (active / idle / ...), or the
    /// backend_type for PostgreSQL background processes, which have no state
    pub command: Option<String>,
    /// Seconds in the current state (MySQL Time / PostgreSQL now() - query_start or state_change)
    pub time_secs: Option<i64>,
    /// MySQL State / PostgreSQL wait event ("Type: Event")
    pub state: Option<String>,
    /// The statement being run (MySQL Info / PostgreSQL query)
    pub info: Option<String>,
    /// Whether the process is not using the server (MySQL Sleep / Daemon / Binlog Dump,
    /// PostgreSQL idle and background processes). The pane hides these by default
    pub idle: bool,
    /// All columns as returned by the server, for the detail view
    pub fields: Vec<ProcessField>,
}

/// Returns the process list of the connection's server, busiest first.
/// The connection used to fetch the list itself is left out.
pub async fn fetch_processes(pool: &DbPool) -> Result<Vec<ProcessInfo>, AppError> {
    let mut processes = match pool {
        DbPool::MySql(p) => fetch_mysql(p).await?,
        DbPool::Postgres(p) => fetch_postgres(p).await?,
        _ => {
            return Err(AppError::Config(
                "The process list is only available for MySQL and PostgreSQL connections".into(),
            ))
        }
    };
    sort_processes(&mut processes);
    Ok(processes)
}

async fn fetch_mysql(pool: &sqlx::MySqlPool) -> Result<Vec<ProcessInfo>, AppError> {
    // Both statements on the same connection, so that its own row can be left out
    let mut conn = pool.acquire().await?;
    // CONNECTION_ID() is BIGINT UNSIGNED; fall back to i64 as db.rs does
    let row = sqlx::query("SELECT CONNECTION_ID()")
        .fetch_one(&mut *conn)
        .await?;
    let own_id: u64 = match row.try_get::<u64, _>(0) {
        Ok(id) => id,
        Err(_) => row.try_get::<i64, _>(0)? as u64,
    };
    // SHOW PROCESSLIST is not supported by the prepared statement protocol on every server
    // version, so send it as a plain text query. Called through Executor (rather than
    // RawSql::fetch_all) because the latter's bounds make the Tauri command's future fail the
    // `Send` check ("implementation of `Executor` is not general enough")
    let rows = (&mut *conn)
        .fetch_all(sqlx::raw_sql("SHOW FULL PROCESSLIST"))
        .await?;
    let own_id = own_id.to_string();
    Ok(rows
        .iter()
        .map(|row| {
            let fields = (0..row.len())
                .map(|i| ProcessField {
                    name: row.column(i).name().to_string(),
                    value: json_to_text(mysql_value_to_json(row, i)),
                })
                .collect();
            mysql_process(fields)
        })
        .filter(|p| p.id != own_id)
        .collect())
}

/// Builds a ProcessInfo from the columns of a SHOW FULL PROCESSLIST row
/// (Id, User, Host, db, Command, Time, State, Info; MariaDB adds Progress).
fn mysql_process(fields: Vec<ProcessField>) -> ProcessInfo {
    let get = |name: &str| field(&fields, name);
    let command = get("Command");
    let idle = command.as_deref().is_some_and(mysql_command_is_idle);
    ProcessInfo {
        id: get("Id").unwrap_or_default(),
        user: get("User"),
        host: get("Host"),
        db: get("db"),
        time_secs: get("Time").and_then(|t| t.trim().parse().ok()),
        state: get("State").filter(|s| !s.is_empty()),
        info: get("Info"),
        command,
        idle,
        fields,
    }
}

/// Commands that do not use the server: idle connections (Sleep), the event scheduler and
/// other daemons (Daemon), and replication senders waiting for binlog events (Binlog Dump)
fn mysql_command_is_idle(command: &str) -> bool {
    ["Sleep", "Daemon", "Binlog Dump", "Binlog Dump GTID"]
        .iter()
        .any(|c| command.eq_ignore_ascii_case(c))
}

/// Seconds in the current state: since the statement started for active sessions, since the
/// last state change otherwise. Casts keep the column types the same on every version
/// (EXTRACT returns numeric from PG 14, double precision before)
const PG_ACTIVITY_SQL: &str = "SELECT pid::bigint AS pid, \
     usename::text AS usename, \
     CASE WHEN client_addr IS NULL THEN NULL \
          ELSE host(client_addr) || ':' || client_port::text END AS client, \
     datname::text AS datname, \
     state, \
     backend_type, \
     CAST(EXTRACT(EPOCH FROM clock_timestamp() - COALESCE( \
          CASE WHEN state = 'active' THEN query_start ELSE state_change END, \
          backend_start)) AS bigint) AS seconds, \
     CASE WHEN wait_event IS NULL THEN NULL \
          ELSE wait_event_type || ': ' || wait_event END AS wait_event, \
     query, \
     application_name, \
     backend_start::text AS backend_start, \
     xact_start::text AS xact_start, \
     query_start::text AS query_start, \
     state_change::text AS state_change \
     FROM pg_stat_activity \
     WHERE pid <> pg_backend_pid()";

async fn fetch_postgres(pool: &sqlx::PgPool) -> Result<Vec<ProcessInfo>, AppError> {
    let rows = sqlx::query(PG_ACTIVITY_SQL).fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|row| {
            let fields = (0..row.len())
                .map(|i| ProcessField {
                    name: row.column(i).name().to_string(),
                    value: json_to_text(pg_value_to_json(row, i)),
                })
                .collect();
            pg_process(fields)
        })
        .collect())
}

/// Builds a ProcessInfo from a PG_ACTIVITY_SQL row.
fn pg_process(fields: Vec<ProcessField>) -> ProcessInfo {
    let get = |name: &str| field(&fields, name);
    let state = get("state");
    let backend_type = get("backend_type");
    // Background processes (autovacuum, walwriter, ...) have no state and are not serving a
    // client. A client backend with no state is another user's session that this role may
    // not inspect (query reads "<insufficient privilege>"): its state is unknown, not idle,
    // so it stays visible. "idle in transaction" stays visible too: it holds locks and a snapshot
    let idle = match state.as_deref() {
        Some(s) => s == "idle",
        None => backend_type.as_deref() != Some("client backend"),
    };
    ProcessInfo {
        id: get("pid").unwrap_or_default(),
        user: get("usename"),
        host: get("client"),
        db: get("datname"),
        command: state.or(backend_type),
        time_secs: get("seconds").and_then(|t| t.trim().parse().ok()),
        state: get("wait_event"),
        info: get("query").filter(|q| !q.is_empty()),
        idle,
        fields,
    }
}

/// Looks up a column by name, ignoring case (the case of SHOW PROCESSLIST's column names
/// differs between MySQL and MariaDB versions)
fn field(fields: &[ProcessField], name: &str) -> Option<String> {
    fields
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case(name))
        .and_then(|f| f.value.clone())
}

fn json_to_text(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    }
}

/// Busiest first: processes using the server before idle ones, then the longest running,
/// then by id so the order is stable between refreshes.
fn sort_processes(processes: &mut [ProcessInfo]) {
    processes.sort_by(|a, b| {
        a.idle
            .cmp(&b.idle)
            .then_with(|| b.time_secs.unwrap_or(-1).cmp(&a.time_secs.unwrap_or(-1)))
            .then_with(|| match (a.id.parse::<u64>(), b.id.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                _ => a.id.cmp(&b.id),
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, Option<&str>)]) -> Vec<ProcessField> {
        pairs
            .iter()
            .map(|(name, value)| ProcessField {
                name: name.to_string(),
                value: value.map(str::to_string),
            })
            .collect()
    }

    fn mysql_row(id: &str, command: &str, time: &str, info: Option<&str>) -> ProcessInfo {
        mysql_process(fields(&[
            ("Id", Some(id)),
            ("User", Some("app")),
            ("Host", Some("10.0.0.1:5000")),
            ("db", None),
            ("Command", Some(command)),
            ("Time", Some(time)),
            ("State", Some("")),
            ("Info", info),
        ]))
    }

    #[test]
    fn test_mysql_process_normalizes_columns() {
        let p = mysql_row("42", "Query", "17", Some("SELECT SLEEP(100)"));
        assert_eq!(p.id, "42");
        assert_eq!(p.user.as_deref(), Some("app"));
        assert_eq!(p.host.as_deref(), Some("10.0.0.1:5000"));
        assert_eq!(p.db, None);
        assert_eq!(p.command.as_deref(), Some("Query"));
        assert_eq!(p.time_secs, Some(17));
        // An empty State means "nothing to report"
        assert_eq!(p.state, None);
        assert_eq!(p.info.as_deref(), Some("SELECT SLEEP(100)"));
        assert!(!p.idle);
        assert_eq!(p.fields.len(), 8);
    }

    #[test]
    fn test_mysql_column_lookup_ignores_case() {
        // Some MariaDB versions report "ID" / "DB"
        let p = mysql_process(fields(&[
            ("ID", Some("7")),
            ("DB", Some("shop")),
            ("COMMAND", Some("Sleep")),
            ("TIME", Some("3")),
        ]));
        assert_eq!(p.id, "7");
        assert_eq!(p.db.as_deref(), Some("shop"));
        assert_eq!(p.time_secs, Some(3));
        assert!(p.idle);
    }

    #[test]
    fn test_mysql_idle_commands() {
        for command in [
            "Sleep",
            "Daemon",
            "Binlog Dump",
            "Binlog Dump GTID",
            "sleep",
        ] {
            assert!(mysql_command_is_idle(command), "{command}");
        }
        for command in ["Query", "Execute", "Connect", "Killed"] {
            assert!(!mysql_command_is_idle(command), "{command}");
        }
    }

    #[test]
    fn test_pg_process_normalizes_columns() {
        let p = pg_process(fields(&[
            ("pid", Some("1234")),
            ("usename", Some("app")),
            ("client", Some("10.0.0.2:51000")),
            ("datname", Some("shop")),
            ("state", Some("active")),
            ("backend_type", Some("client backend")),
            ("seconds", Some("95")),
            ("wait_event", Some("Lock: transactionid")),
            ("query", Some("UPDATE t SET x = 1")),
        ]));
        assert_eq!(p.id, "1234");
        assert_eq!(p.host.as_deref(), Some("10.0.0.2:51000"));
        assert_eq!(p.command.as_deref(), Some("active"));
        assert_eq!(p.time_secs, Some(95));
        assert_eq!(p.state.as_deref(), Some("Lock: transactionid"));
        assert_eq!(p.info.as_deref(), Some("UPDATE t SET x = 1"));
        assert!(!p.idle);
    }

    #[test]
    fn test_pg_idle_classification() {
        let with_state = |state: Option<&str>| {
            pg_process(fields(&[
                ("pid", Some("1")),
                ("state", state),
                ("backend_type", Some("autovacuum launcher")),
            ]))
        };
        assert!(with_state(Some("idle")).idle);
        // Background processes have no state; the backend type stands in for the command
        let background = with_state(None);
        assert!(background.idle);
        assert_eq!(background.command.as_deref(), Some("autovacuum launcher"));
        // Another user's client session hidden from this role: unknown, so not hidden
        let restricted = pg_process(fields(&[
            ("pid", Some("2")),
            ("state", None),
            ("backend_type", Some("client backend")),
            ("query", Some("<insufficient privilege>")),
        ]));
        assert!(!restricted.idle);
        assert_eq!(restricted.command.as_deref(), Some("client backend"));
        // Holding a transaction open is worth seeing
        assert!(!with_state(Some("idle in transaction")).idle);
        assert!(!with_state(Some("active")).idle);
    }

    #[test]
    fn test_sort_busiest_first() {
        let mut processes = vec![
            mysql_row("1", "Sleep", "5000", None),
            mysql_row("2", "Query", "3", Some("SELECT 1")),
            mysql_row("10", "Query", "120", Some("SELECT big")),
            mysql_row("3", "Query", "120", Some("SELECT big too")),
            mysql_row("4", "Sleep", "10", None),
        ];
        sort_processes(&mut processes);
        let ids: Vec<&str> = processes.iter().map(|p| p.id.as_str()).collect();
        // Active before idle, longest first, ties by numeric id
        assert_eq!(ids, ["3", "10", "2", "1", "4"]);
    }

    #[test]
    fn test_json_to_text() {
        assert_eq!(json_to_text(serde_json::Value::Null), None);
        assert_eq!(json_to_text(serde_json::json!("x")), Some("x".into()));
        assert_eq!(json_to_text(serde_json::json!(12)), Some("12".into()));
    }

    /// Runs against a real PostgreSQL when QUERYFOLIO_TEST_PG_URL is set (skipped otherwise).
    /// A session held in pg_sleep must show up as active, and the fetching session must not
    #[tokio::test]
    async fn test_fetch_processes_against_postgres() {
        let Ok(url) = std::env::var("QUERYFOLIO_TEST_PG_URL") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let sleeper = pool.clone();
        let sleeping = tokio::spawn(async move {
            sqlx::query("SELECT pg_sleep(3), 'qf-process-list-probe'")
                .execute(&sleeper)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let processes = fetch_processes(&DbPool::Postgres(pool.clone()))
            .await
            .unwrap();
        let probe = processes
            .iter()
            .find(|p| {
                p.info
                    .as_deref()
                    .is_some_and(|q| q.contains("qf-process-list-probe"))
            })
            .expect("the sleeping session should be listed");
        assert!(!probe.idle);
        assert_eq!(probe.command.as_deref(), Some("active"));
        assert!(probe.time_secs.is_some());
        assert!(probe.fields.iter().any(|f| f.name == "application_name"));
        assert!(!processes
            .iter()
            .any(|p| p.info.as_deref() == Some(PG_ACTIVITY_SQL)));
        // Active sessions come before idle ones
        let first_idle = processes.iter().position(|p| p.idle);
        let last_active = processes.iter().rposition(|p| !p.idle);
        if let (Some(i), Some(a)) = (first_idle, last_active) {
            assert!(a < i);
        }
        sleeping.await.unwrap().unwrap();
    }

    /// Runs against a real MySQL when QUERYFOLIO_TEST_MYSQL_URL is set (skipped otherwise).
    #[tokio::test]
    async fn test_fetch_processes_against_mysql() {
        let Ok(url) = std::env::var("QUERYFOLIO_TEST_MYSQL_URL") else {
            return;
        };
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let sleeper = pool.clone();
        let sleeping = tokio::spawn(async move {
            sqlx::query("SELECT SLEEP(3), 'qf-process-list-probe'")
                .execute(&sleeper)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let processes = fetch_processes(&DbPool::MySql(pool.clone())).await.unwrap();
        let probe = processes
            .iter()
            .find(|p| {
                p.info
                    .as_deref()
                    .is_some_and(|q| q.contains("qf-process-list-probe"))
            })
            .expect("the sleeping session should be listed");
        assert!(!probe.idle);
        assert!(probe.time_secs.is_some());
        assert!(!processes
            .iter()
            .any(|p| p.info.as_deref() == Some("SHOW FULL PROCESSLIST")));
        sleeping.await.unwrap().unwrap();
    }
}
