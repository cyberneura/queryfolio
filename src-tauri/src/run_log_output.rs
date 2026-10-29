//! The output file of a Run and Log statement carrying `out=<name>` (CYBERNEURA-DEV-1167).
//!
//! The file holds the **whole** result, so it is serialized here rather than in the frontend:
//! cells have no character limit on the sqlx path, and building a string of every cell on the
//! WebView's main thread freezes it (500 rows x 3 columns x 100 KB is already ~150 MB). The
//! format follows the extension — `.csv` CSV, `.json` a JSON array of objects, anything else TSV —
//! with the same conversion as the frontend's Export (`src/lib/export.ts`): the formula guard on
//! string cells and headers, tabs / newlines in TSV cells turned into spaces, CSV quoting, and
//! `_2`, `_3` suffixes for duplicate JSON keys.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rows to write. Built from a QueryResult, or sent by the frontend when the displayed result is
/// already complete (no need to run the statement again)
#[derive(Debug, Deserialize)]
pub struct RunLogRows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: bool,
}

/// What was written, for the pointer written below the statement
#[derive(Debug, Serialize)]
pub struct RunLogFile {
    pub file_name: String,
    pub row_count: usize,
    /// The result itself was cut (the row cap of the re-fetch, or the backend's truncation)
    pub truncated: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Format {
    Tsv,
    Csv,
    Json,
}

fn format_of(file_name: &str) -> Format {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".csv") {
        Format::Csv
    } else if lower.ends_with(".json") {
        Format::Json
    } else {
        Format::Tsv
    }
}

/// Same as export.ts's cellToString: null is empty, objects / arrays are JSON, the rest as is
fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
        other => other.to_string(),
    }
}

/// Characters a spreadsheet takes for the start of a formula (export.ts's FORMULA_TRIGGER)
fn starts_like_formula(text: &str) -> bool {
    matches!(
        text.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r')
    )
}

/// Only string values get the guard, so a number like -1 is left alone (escapeFormulaInjection)
fn guarded_cell(value: &Value) -> String {
    let text = cell_text(value);
    if value.is_string() && starts_like_formula(&text) {
        format!("'{text}")
    } else {
        text
    }
}

/// Headers are always strings, so they are always checked (escapeHeaderFormula)
fn guarded_header(header: &str) -> String {
    if starts_like_formula(header) {
        format!("'{header}")
    } else {
        header.to_string()
    }
}

/// export.ts's sanitizeTsv: tabs and newlines would break the row / column structure
fn tsv_field(text: &str) -> String {
    text.replace('\t', " ")
        .replace("\r\n", " ")
        .replace('\n', " ")
}

/// export.ts's escapeCsvField
fn csv_field(text: &str) -> String {
    if text.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// export.ts's uniqueColumnKeys: `id, id` becomes `id, id_2` so a later value does not overwrite.
/// A generated key also skips names already taken, so `id, id, id_2` gives `id, id_3, id_2`
/// rather than two `id_2`
fn unique_keys(columns: &[String]) -> Vec<String> {
    let mut taken: std::collections::HashSet<String> = columns.iter().cloned().collect();
    let mut first_seen = std::collections::HashSet::<&str>::new();
    columns
        .iter()
        .map(|column| {
            if first_seen.insert(column.as_str()) {
                return column.clone();
            }
            let mut n = 2;
            loop {
                let key = format!("{column}_{n}");
                if taken.insert(key.clone()) {
                    return key;
                }
                n += 1;
            }
        })
        .collect()
}

/// The file content, ending with a newline
pub fn serialize(rows: &RunLogRows, file_name: &str) -> String {
    let mut out = String::new();
    match format_of(file_name) {
        Format::Tsv | Format::Csv => {
            let csv = format_of(file_name) == Format::Csv;
            let (sep, field): (&str, fn(&str) -> String) = if csv {
                (",", csv_field)
            } else {
                ("\t", tsv_field)
            };
            let header: Vec<String> = rows
                .columns
                .iter()
                .map(|c| field(&guarded_header(c)))
                .collect();
            out.push_str(&header.join(sep));
            out.push('\n');
            for row in &rows.rows {
                let cells: Vec<String> = row.iter().map(|v| field(&guarded_cell(v))).collect();
                out.push_str(&cells.join(sep));
                out.push('\n');
            }
        }
        Format::Json => {
            // One object per line, keys in column order (serde_json's Map would sort them)
            let keys: Vec<String> = unique_keys(&rows.columns)
                .iter()
                .map(|k| Value::String(k.clone()).to_string())
                .collect();
            out.push('[');
            for (i, row) in rows.rows.iter().enumerate() {
                out.push_str(if i == 0 { "\n  {" } else { ",\n  {" });
                for (j, key) in keys.iter().enumerate() {
                    if j > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(key);
                    out.push_str(": ");
                    out.push_str(&row.get(j).unwrap_or(&Value::Null).to_string());
                }
                out.push('}');
            }
            out.push_str(if rows.rows.is_empty() { "]\n" } else { "\n]\n" });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(columns: &[&str], rows: Vec<Vec<Value>>) -> RunLogRows {
        RunLogRows {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows,
            truncated: false,
        }
    }

    #[test]
    fn test_tsv_guards_and_flattens_cells() {
        let r = rows(
            &["id", "=note"],
            vec![
                vec![json!(-1), json!("=SUM(A1)")],
                vec![json!(2), json!("a\tb\nc")],
                vec![Value::Null, json!({"k": [1, 2]})],
            ],
        );
        assert_eq!(
            serialize(&r, "out.tsv"),
            "id\t'=note\n-1\t'=SUM(A1)\n2\ta b c\n\t{\"k\":[1,2]}\n"
        );
        // An unknown extension is TSV too
        assert_eq!(serialize(&r, "out.txt"), serialize(&r, "out.tsv"));
    }

    #[test]
    fn test_csv_quotes_fields() {
        let r = rows(&["a", "b"], vec![vec![json!("x,y"), json!("say \"hi\"\n")]]);
        assert_eq!(
            serialize(&r, "OUT.CSV"),
            "a,b\n\"x,y\",\"say \"\"hi\"\"\n\"\n"
        );
    }

    #[test]
    fn test_json_keeps_column_order_and_dedupes_keys() {
        let r = rows(
            &["z", "a", "z"],
            vec![vec![json!(1), json!("=x"), Value::Null]],
        );
        let text = serialize(&r, "out.json");
        assert_eq!(text, "[\n  {\"z\": 1, \"a\": \"=x\", \"z_2\": null}\n]\n");
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["a"], json!("=x"));
        assert_eq!(serialize(&rows(&["a"], vec![]), "e.json"), "[]\n");
        // A generated key does not collide with a real column
        let r = rows(
            &["id", "id", "id_2"],
            vec![vec![json!(1), json!(2), json!(3)]],
        );
        assert_eq!(
            serialize(&r, "out.json"),
            "[\n  {\"id\": 1, \"id_3\": 2, \"id_2\": 3}\n]\n"
        );
    }
}
