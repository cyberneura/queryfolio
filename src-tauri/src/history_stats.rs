//! `queryfolio --stats`: weekly usage numbers built from the local query history.
//!
//! The README's goal section measures Queryfolio by two things only the developer's own
//! machine knows: whether it keeps being used, and whether the safety guards keep stopping
//! statements. Both are counted here from `~/.config/queryfolio/history/*.jsonl`, and nothing
//! is sent anywhere.
//!
//! **Only counts are printed.** No SQL text, no connection names, no hosts -- the output is
//! meant to be pasted into a task or chat as it is. Connections are reduced to a number per
//! engine. Keep to this when adding columns (a unit test checks it).
//!
//! The functions here are pure (no filesystem, no clock) and pinned by unit tests; `lib.rs`
//! reads the files and the config and passes them in.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Duration, NaiveDate};

use crate::db::{parse_engine, Engine};
use crate::history::{HistoryEntry, BLOCKED_DANGEROUS, BLOCKED_READONLY};

/// Number of weeks shown when `--weeks` is not given.
pub const DEFAULT_WEEKS: u32 = 8;
/// Upper bound of `--weeks` (10 years; the history is capped per connection anyway).
pub const MAX_WEEKS: u32 = 520;

/// Engine shown for history files whose connection is no longer in the config.
const UNKNOWN_ENGINE: &str = "unknown";

/// Numbers for one period (a week, or the whole range for the total row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStats {
    /// First day of the period (the Monday of the week).
    pub start: NaiveDate,
    /// All executions, including failures and guard stops.
    pub runs: usize,
    /// Failed executions other than guard stops (syntax errors, connection errors, ...).
    pub failed: usize,
    /// Statements stopped by the read-only guard.
    pub blocked_readonly: usize,
    /// Statements stopped by the dangerous-statement guard.
    pub blocked_dangerous: usize,
    /// Days with at least one execution.
    pub days: usize,
    /// Number of connections used, per engine.
    pub engines: BTreeMap<String, usize>,
}

impl PeriodStats {
    /// Number of distinct connections used in the period.
    pub fn connections(&self) -> usize {
        self.engines.values().sum()
    }
}

/// Accumulates one period. Days and connections are sets so that each counts once.
struct Accumulator {
    start: NaiveDate,
    runs: usize,
    failed: usize,
    blocked_readonly: usize,
    blocked_dangerous: usize,
    days: BTreeSet<NaiveDate>,
    connections: BTreeSet<String>,
}

impl Accumulator {
    fn new(start: NaiveDate) -> Self {
        Self {
            start,
            runs: 0,
            failed: 0,
            blocked_readonly: 0,
            blocked_dangerous: 0,
            days: BTreeSet::new(),
            connections: BTreeSet::new(),
        }
    }

    fn add(&mut self, connection: &str, date: NaiveDate, entry: &HistoryEntry) {
        self.runs += 1;
        match entry.blocked.as_deref() {
            Some(BLOCKED_READONLY) => self.blocked_readonly += 1,
            Some(BLOCKED_DANGEROUS) => self.blocked_dangerous += 1,
            // An unknown value (written by a newer version) is still a failure
            _ if !entry.success => self.failed += 1,
            _ => {}
        }
        self.days.insert(date);
        self.connections.insert(connection.to_string());
    }

    fn finish(self, engines: &HashMap<String, String>) -> PeriodStats {
        let mut per_engine = BTreeMap::new();
        for connection in &self.connections {
            let engine = engines
                .get(connection)
                .map_or(UNKNOWN_ENGINE, |engine| engine_label(engine));
            *per_engine.entry(engine.to_string()).or_insert(0) += 1;
        }
        PeriodStats {
            start: self.start,
            runs: self.runs,
            failed: self.failed,
            blocked_readonly: self.blocked_readonly,
            blocked_dangerous: self.blocked_dangerous,
            days: self.days.len(),
            engines: per_engine,
        }
    }
}

/// The canonical name of an engine as written in the config (`postgresql` -> `postgres`).
/// Anything that is not a known engine becomes `unknown`: the value is free text from the
/// user's config, and only fixed names may reach the output.
fn engine_label(engine: &str) -> &'static str {
    match parse_engine(engine) {
        Ok(Engine::MySql) => "mysql",
        Ok(Engine::Postgres) => "postgres",
        Ok(Engine::Sqlite) => "sqlite",
        Ok(Engine::DuckDb) => "duckdb",
        Ok(Engine::MsSql) => "mssql",
        Ok(Engine::Redis) => "redis",
        Ok(Engine::Elasticsearch) => "elasticsearch",
        Ok(Engine::DynamoDb) => "dynamodb",
        Err(_) => UNKNOWN_ENGINE,
    }
}

/// The Monday of the week that contains `date`.
fn week_start(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

/// Counts the last `weeks` weeks (Monday to Sunday) up to the week containing `today`.
///
/// - `entries`: `(connection name, entry)` pairs from [`crate::history::read_all_history`]
/// - `engines`: connection name -> engine, from the config. Connections missing from it are
///   counted as `unknown`.
///
/// The date of an entry is the date in the offset it was recorded with (the local time of
/// the machine at that moment), not converted to today's offset. Entries whose time cannot
/// be parsed, and entries outside the range (older weeks, or a future date from a clock
/// that was wrong), are left out.
///
/// Returns the weeks oldest first (weeks without any execution are included with zeros),
/// and the total over the whole range.
pub fn weekly_stats(
    entries: &[(String, HistoryEntry)],
    engines: &HashMap<String, String>,
    today: NaiveDate,
    weeks: u32,
) -> (Vec<PeriodStats>, PeriodStats) {
    let weeks = weeks.clamp(1, MAX_WEEKS);
    let current = week_start(today);
    let first = current - Duration::weeks(i64::from(weeks - 1));

    let mut accumulators: Vec<Accumulator> = (0..weeks)
        .map(|i| Accumulator::new(first + Duration::weeks(i64::from(i))))
        .collect();
    let mut total = Accumulator::new(first);

    for (connection, entry) in entries {
        let Ok(time) = DateTime::parse_from_rfc3339(&entry.time) else {
            continue;
        };
        let date = time.date_naive();
        let start = week_start(date);
        if start < first || date > today {
            continue;
        }
        let index = ((start - first).num_days() / 7) as usize;
        accumulators[index].add(connection, date, entry);
        total.add(connection, date, entry);
    }

    let weeks = accumulators
        .into_iter()
        .map(|a| a.finish(engines))
        .collect();
    (weeks, total.finish(engines))
}

/// The CONNECTIONS cell: the count, and the count per engine when any were used.
fn connections_cell(stats: &PeriodStats) -> String {
    if stats.engines.is_empty() {
        return "0".to_string();
    }
    let breakdown: Vec<String> = stats
        .engines
        .iter()
        .map(|(engine, count)| format!("{engine} {count}"))
        .collect();
    format!("{} ({})", stats.connections(), breakdown.join(", "))
}

const COLUMNS: [&str; 7] = [
    "WEEK OF",
    "RUNS",
    "FAILED",
    "READONLY STOPS",
    "DANGEROUS STOPS",
    "DAYS",
    "CONNECTIONS",
];

fn row(label: String, stats: &PeriodStats) -> [String; 7] {
    [
        label,
        stats.runs.to_string(),
        stats.failed.to_string(),
        stats.blocked_readonly.to_string(),
        stats.blocked_dangerous.to_string(),
        stats.days.to_string(),
        connections_cell(stats),
    ]
}

/// Builds the body of `queryfolio --stats` from the result of [`weekly_stats`].
pub fn format_stats(weeks: &[PeriodStats], total: &PeriodStats) -> String {
    let mut rows: Vec<[String; 7]> = weeks
        .iter()
        .rev() // newest week first
        .map(|w| row(w.start.format("%Y-%m-%d").to_string(), w))
        .collect();
    rows.push(row("TOTAL".to_string(), total));

    let header = COLUMNS.map(str::to_string);
    let widths: Vec<usize> = (0..COLUMNS.len())
        .map(|i| {
            rows.iter()
                .chain(std::iter::once(&header))
                .map(|r| r[i].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = format!(
        "Query history of the last {} week(s), Monday to Sunday.\n\n",
        weeks.len()
    );
    for r in std::iter::once(&header).chain(rows.iter()) {
        let cells: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let pad = " ".repeat(widths[i].saturating_sub(cell.chars().count()));
                // The label and the last column are left-aligned, the numbers right-aligned
                if i == 0 || i + 1 == r.len() {
                    format!("{cell}{pad}")
                } else {
                    format!("{pad}{cell}")
                }
            })
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out.push_str(
        "\nRUNS counts every execution. FAILED excludes the statements stopped by a guard.\n\
         DAYS and CONNECTIONS count the days and connections with at least one execution.\n\
         Guard stops are recorded from Queryfolio 0.12.0 on; before that they are in FAILED.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(time: &str, success: bool, blocked: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            time: time.into(),
            sql: "SELECT secret_column FROM payroll".into(),
            schema: Some("hr".into()),
            row_count: None,
            elapsed_ms: 1,
            success,
            blocked: blocked.map(str::to_string),
        }
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn engines() -> HashMap<String, String> {
        HashMap::from([
            ("prod-db".to_string(), "postgres".to_string()),
            ("local".to_string(), "sqlite".to_string()),
        ])
    }

    #[test]
    fn counts_per_week() {
        // 2026-10-21 is a Wednesday; its week starts on Monday 2026-10-19
        let entries = vec![
            (
                "prod-db".into(),
                entry("2026-10-19T09:00:00+09:00", true, None),
            ),
            (
                "prod-db".into(),
                entry("2026-10-19T10:00:00+09:00", false, None),
            ),
            (
                "prod-db".into(),
                entry("2026-10-20T10:00:00+09:00", false, Some("readonly")),
            ),
            (
                "local".into(),
                entry("2026-10-21T10:00:00+09:00", false, Some("dangerous")),
            ),
            // Previous week
            (
                "local".into(),
                entry("2026-10-18T23:59:00+09:00", true, None),
            ),
            // Too old for 2 weeks, and a broken time: both left out
            (
                "local".into(),
                entry("2026-10-01T10:00:00+09:00", true, None),
            ),
            ("local".into(), entry("not a time", true, None)),
        ];
        let (weeks, total) = weekly_stats(&entries, &engines(), date("2026-10-21"), 2);
        assert_eq!(weeks.len(), 2);

        assert_eq!(weeks[0].start, date("2026-10-12"));
        assert_eq!(weeks[0].runs, 1);
        assert_eq!(weeks[0].days, 1);

        let current = &weeks[1];
        assert_eq!(current.start, date("2026-10-19"));
        assert_eq!(current.runs, 4);
        assert_eq!(current.failed, 1); // guard stops are not in FAILED
        assert_eq!(current.blocked_readonly, 1);
        assert_eq!(current.blocked_dangerous, 1);
        assert_eq!(current.days, 3);
        assert_eq!(current.connections(), 2);
        assert_eq!(current.engines.get("postgres"), Some(&1));
        assert_eq!(current.engines.get("sqlite"), Some(&1));

        assert_eq!(total.runs, 5);
        assert_eq!(total.days, 4);
        assert_eq!(total.connections(), 2);
    }

    #[test]
    fn date_is_taken_in_the_recorded_offset() {
        // Monday 00:30 in +09:00 is Sunday in UTC; it belongs to the week of that Monday
        let entries = vec![(
            "local".into(),
            entry("2026-10-19T00:30:00+09:00", true, None),
        )];
        let (weeks, _) = weekly_stats(&entries, &engines(), date("2026-10-19"), 2);
        assert_eq!(weeks[1].runs, 1);
        assert_eq!(weeks[0].runs, 0);
    }

    #[test]
    fn future_entries_and_unknown_connections() {
        let entries = vec![
            (
                "gone".into(),
                entry("2026-10-20T10:00:00+09:00", true, None),
            ),
            (
                "local".into(),
                entry("2027-01-01T10:00:00+09:00", true, None),
            ),
            // Later in the current week than today
            (
                "local".into(),
                entry("2026-10-23T10:00:00+09:00", true, None),
            ),
        ];
        let (weeks, total) = weekly_stats(&entries, &engines(), date("2026-10-21"), 1);
        assert_eq!(weeks[0].runs, 1);
        assert_eq!(weeks[0].engines.get("unknown"), Some(&1));
        assert_eq!(total.runs, 1);
    }

    #[test]
    fn engines_are_normalized_and_free_text_is_dropped() {
        let engines = HashMap::from([
            ("a".to_string(), "PostgreSQL".to_string()),
            ("b".to_string(), "db.internal.example.com".to_string()),
        ]);
        let entries = vec![
            ("a".into(), entry("2026-10-20T10:00:00+09:00", true, None)),
            ("b".into(), entry("2026-10-20T10:00:00+09:00", true, None)),
        ];
        let (weeks, total) = weekly_stats(&entries, &engines, date("2026-10-21"), 1);
        assert_eq!(weeks[0].engines.get("postgres"), Some(&1));
        assert_eq!(weeks[0].engines.get("unknown"), Some(&1));
        let text = format_stats(&weeks, &total);
        assert!(!text.contains("example.com"), "{text}");
    }

    #[test]
    fn weeks_are_clamped() {
        let (weeks, _) = weekly_stats(&[], &engines(), date("2026-10-21"), 0);
        assert_eq!(weeks.len(), 1);
        let (weeks, _) = weekly_stats(&[], &engines(), date("2026-10-21"), 10_000);
        assert_eq!(weeks.len(), MAX_WEEKS as usize);
    }

    #[test]
    fn output_has_no_sql_connection_names_or_hosts() {
        let entries = vec![
            (
                "prod-db".into(),
                entry("2026-10-19T09:00:00+09:00", true, None),
            ),
            (
                "local".into(),
                entry("2026-10-20T09:00:00+09:00", false, Some("readonly")),
            ),
        ];
        let (weeks, total) = weekly_stats(&entries, &engines(), date("2026-10-21"), 2);
        let text = format_stats(&weeks, &total);
        assert!(!text.contains("secret_column"), "{text}");
        assert!(!text.contains("payroll"), "{text}");
        assert!(!text.contains("prod-db"), "{text}");
        assert!(!text.contains("local "), "{text}");
        assert!(!text.contains("hr"), "{text}");
        // The numbers are there, newest week first
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[2].starts_with("WEEK OF"), "{text}");
        assert!(lines[3].starts_with("2026-10-19"), "{text}");
        assert!(lines[3].contains("2 (postgres 1, sqlite 1)"), "{text}");
        assert!(lines[4].starts_with("2026-10-12"), "{text}");
        assert!(lines[5].starts_with("TOTAL"), "{text}");
    }
}
