use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// model 省略時に使う OpenAI のデフォルトモデル。
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-5.1";

/// base_url 省略時の OpenAI API ベース URL。
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// AI API リクエストのタイムアウト (秒)。
const AI_REQUEST_TIMEOUT_SECS: u64 = 60;

/// エラーメッセージに含める API レスポンス本文の最大長。
const ERROR_BODY_MAX_CHARS: usize = 500;

/// config.yml のトップレベル (config_override_command で取得した YAML を
/// マージした後の値) に書ける `ai:` セクション。
/// api_key を含むためフロントエンドには渡さない (フロントには
/// get_ai_info で AiInfo のみを返す)。
#[derive(Debug, Clone, Deserialize)]
pub struct AiConfig {
    /// AI プロバイダー。現状 "openai" のみ対応 (省略時 "openai")
    #[serde(default = "default_provider")]
    pub provider: String,
    pub api_key: String,
    /// モデル名 (省略時 DEFAULT_OPENAI_MODEL)
    #[serde(default)]
    pub model: Option<String>,
    /// OpenAI 互換 API 用のベース URL (省略時 DEFAULT_OPENAI_BASE_URL)
    #[serde(default)]
    pub base_url: Option<String>,
}

fn default_provider() -> String {
    "openai".to_string()
}

impl AiConfig {
    /// YAML の `ai:` セクションの値をパース・検証する。
    pub fn from_value(value: &serde_yaml::Value) -> Result<Self, AppError> {
        let config: AiConfig = serde_yaml::from_value(value.clone())
            .map_err(|e| AppError::Ai(format!("Failed to parse the 'ai' section: {e}")))?;
        if config.provider != "openai" {
            return Err(AppError::Ai(format!(
                "Unsupported AI provider '{}' (only 'openai' is supported)",
                config.provider
            )));
        }
        if config.api_key.trim().is_empty() {
            return Err(AppError::Ai(
                "The 'ai' section has an empty api_key".into(),
            ));
        }
        Ok(config)
    }

    /// 使用するモデル名 (省略時はデフォルトモデル)。
    pub fn model(&self) -> &str {
        self.model.as_deref().unwrap_or(DEFAULT_OPENAI_MODEL)
    }

    /// API のベース URL (省略時は OpenAI 公式。末尾スラッシュは除去)。
    fn base_url(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or(DEFAULT_OPENAI_BASE_URL)
            .trim_end_matches('/')
    }
}

/// マージ済み設定のトップレベル `ai:` セクションから AI 設定を解決する。
/// ローカルと取得 YAML の優先順位は設定マージ (AppConfig::load_merged) が
/// 決めるため、ここでは渡された値を検証するだけ。未設定なら None。
pub fn resolve_ai_config(ai: Option<&serde_yaml::Value>) -> Result<Option<AiConfig>, AppError> {
    match ai {
        Some(value) => Ok(Some(AiConfig::from_value(value)?)),
        None => Ok(None),
    }
}

/// フロントエンドに渡す AI 設定の情報。api_key は含めない。
#[derive(Debug, Serialize)]
pub struct AiInfo {
    pub configured: bool,
    pub model: String,
}

/// エンジン名を SQL 方言の表示名に変換する (プロンプト用)。
fn dialect_name(engine: &str) -> String {
    match engine.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => "PostgreSQL".to_string(),
        "mysql" | "mariadb" => "MySQL".to_string(),
        "sqlite" | "sqlite3" => "SQLite".to_string(),
        "duckdb" => "DuckDB".to_string(),
        other => other.to_string(),
    }
}

/// SQL 生成用の system prompt を組み立てる。
/// LLM に送るのはスキーマ情報 (テーブル名・カラム名) と方言・アクティブ
/// スキーマ名のみ。クエリの結果データや接続情報 (ホスト・認証情報) は
/// 絶対に含めない。
pub fn build_sql_system_prompt(
    engine: &str,
    active_schema: Option<&str>,
    schema_map: &BTreeMap<String, Vec<String>>,
) -> String {
    let dialect = dialect_name(engine);
    let mut prompt = format!(
        "You are a SQL assistant for a {dialect} database. \
         Write a single SQL statement in the {dialect} dialect that fulfills \
         the user's request, using only the tables and columns listed below.\n\
         Return ONLY the SQL statement, no markdown fences, no explanation.\n"
    );
    push_schema_section(&mut prompt, active_schema, schema_map);
    prompt
}

/// system prompt にアクティブスキーマ名とテーブル・カラム一覧を追記する
/// (SQL 生成とエラー修正で共通)。
fn push_schema_section(
    prompt: &mut String,
    active_schema: Option<&str>,
    schema_map: &BTreeMap<String, Vec<String>>,
) {
    if let Some(schema) = active_schema.filter(|s| !s.trim().is_empty()) {
        prompt.push_str(&format!("The active schema (database) is '{schema}'.\n"));
    }
    prompt.push_str("\nTables and columns:\n");
    if schema_map.is_empty() {
        prompt.push_str("(no tables found)\n");
    }
    for (table, columns) in schema_map {
        prompt.push_str(&format!("- {table} ({})\n", columns.join(", ")));
    }
}

/// SQL エラー修正用の system prompt を組み立てる。
/// LLM に送るのは失敗した SQL・DB のエラーメッセージ・スキーマ情報
/// (テーブル名・カラム名)・方言・アクティブスキーマ名のみ。
/// クエリの結果データや接続情報 (ホスト・認証情報) は絶対に含めない。
pub fn build_fix_sql_system_prompt(
    engine: &str,
    active_schema: Option<&str>,
    schema_map: &BTreeMap<String, Vec<String>>,
) -> String {
    let dialect = dialect_name(engine);
    let mut prompt = format!(
        "You are a SQL assistant for a {dialect} database. \
         The user will provide a SQL statement that failed and the error \
         message returned by the database. Fix the SQL statement so that it \
         runs in the {dialect} dialect, using only the tables and columns \
         listed below while preserving the intent of the original statement.\n\
         Return ONLY the corrected SQL statement, no markdown fences, \
         no explanation.\n"
    );
    push_schema_section(&mut prompt, active_schema, schema_map);
    prompt
}

/// SQL エラー修正用の user prompt を組み立てる
/// (失敗した SQL と DB のエラーメッセージ)。
pub fn build_fix_sql_user_prompt(sql: &str, error_message: &str) -> String {
    format!(
        "The following SQL statement failed:\n\n{}\n\n\
         The database returned this error:\n\n{}",
        sql.trim(),
        error_message.trim()
    )
}

/// LLM の応答が ```sql フェンス付きで返ってきた場合に中身を取り出す。
/// フェンスが無ければ前後の空白だけ除去して返す。
pub fn strip_sql_fences(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        // 先頭行の言語タグ (sql 等) を読み飛ばす
        let body = match rest.split_once('\n') {
            Some((_lang, body)) => body,
            None => rest,
        };
        let body = body.strip_suffix("```").unwrap_or(body);
        return body.trim().to_string();
    }
    trimmed.to_string()
}

/// エラーメッセージ用にレスポンス本文を切り詰める。
fn truncate_for_error(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= ERROR_BODY_MAX_CHARS {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(ERROR_BODY_MAX_CHARS).collect();
    format!("{truncated}...")
}

/// OpenAI Chat Completions API を呼び、アシスタント応答のテキストを返す。
/// AI 機能 (SQL 生成 / エラー修正 / EXPLAIN 解説 等) の共通基盤。
/// 呼び出し側は目的別の Tauri コマンドとしてプロンプトを組み立てること
/// (フロントから任意プロンプトを送れる汎用コマンドは作らない)。
pub async fn chat_complete(
    config: &AiConfig,
    system: &str,
    user: &str,
) -> Result<String, AppError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(AI_REQUEST_TIMEOUT_SECS))
        .build()
        .map_err(|e| AppError::Ai(format!("Failed to build the HTTP client: {e}")))?;
    let url = format!("{}/chat/completions", config.base_url());
    let body = serde_json::json!({
        "model": config.model(),
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
    });

    let response = client
        .post(&url)
        .bearer_auth(&config.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Ai(format!("The AI API request failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| AppError::Ai(format!("Failed to read the AI API response: {e}")))?;
    if !status.is_success() {
        return Err(AppError::Ai(format!(
            "The AI API returned an error (HTTP {}): {}",
            status.as_u16(),
            truncate_for_error(&text)
        )));
    }

    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Ai(format!("Failed to parse the AI API response: {e}")))?;
    let content = json
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .ok_or_else(|| AppError::Ai("The AI API response has no message content".into()))?;
    Ok(content.to_string())
}

/// EXPLAIN 解説用の system prompt を組み立てる。
/// LLM に送るのはスキーマ情報 (テーブル名・カラム名)・方言・アクティブ
/// スキーマ名のみ (SQL と実行計画は user message 側)。実行計画はクエリの
/// 結果データではなくプランナー出力なので送ってよい。接続情報 (ホスト・
/// 認証情報) は絶対に含めない。
pub fn build_explain_system_prompt(
    engine: &str,
    active_schema: Option<&str>,
    schema_map: &BTreeMap<String, Vec<String>>,
) -> String {
    let dialect = dialect_name(engine);
    let mut prompt = format!(
        "You are a {dialect} query performance expert. The user provides a \
         SQL statement and its execution plan ({dialect} EXPLAIN output).\n\
         Respond in Markdown with the following sections:\n\
         1. **Bottlenecks** — identify the dominant costs in the plan \
         (full scans, row estimate mismatches, expensive joins, sorts, etc.). \
         If the plan is already efficient, say so.\n\
         2. **Index suggestions** — concrete CREATE INDEX statements with a \
         short rationale, using only the tables and columns listed below. \
         If no index would help, say so.\n\
         3. **Query rewrite** — a rewritten query only if it would improve \
         the plan.\n\
         Be specific and concise. Use fenced code blocks for SQL.\n"
    );
    if let Some(schema) = active_schema.filter(|s| !s.trim().is_empty()) {
        prompt.push_str(&format!("The active schema (database) is '{schema}'.\n"));
    }
    prompt.push_str("\nTables and columns:\n");
    if schema_map.is_empty() {
        prompt.push_str("(no tables found)\n");
    }
    for (table, columns) in schema_map {
        prompt.push_str(&format!("- {table} ({})\n", columns.join(", ")));
    }
    prompt
}

/// EXPLAIN 解説用の user message (SQL + 実行計画テキスト) を組み立てる。
pub fn build_explain_user_message(sql: &str, plan_text: &str) -> String {
    format!(
        "SQL:\n```sql\n{}\n```\n\nExecution plan:\n```\n{}\n```",
        sql.trim(),
        plan_text.trim()
    )
}

/// 選択 SQL の解説用の system prompt を組み立てる。
/// LLM に送るのはスキーマ情報 (テーブル名・カラム名)・方言・アクティブ
/// スキーマ名のみ (SQL は user message 側)。クエリの結果データや接続情報
/// (ホスト・認証情報) は絶対に含めない。
pub fn build_explain_sql_system_prompt(
    engine: &str,
    active_schema: Option<&str>,
    schema_map: &BTreeMap<String, Vec<String>>,
) -> String {
    let dialect = dialect_name(engine);
    let mut prompt = format!(
        "You are a SQL assistant for a {dialect} database. The user provides \
         a SQL statement. Explain in plain language what it does, for a \
         reader who did not write it.\n\
         Respond in Markdown with the following sections:\n\
         1. **Summary** — one or two sentences describing what the statement \
         returns or changes.\n\
         2. **Step by step** — walk through each clause (FROM / JOINs, \
         WHERE, GROUP BY, window functions, subqueries / CTEs, \
         ORDER BY / LIMIT, etc.) and explain its role in this statement.\n\
         3. **Caveats** — pitfalls to be aware of (NULL handling, implicit \
         type conversions, row duplication from joins, missing filters, \
         performance concerns). If there are none, say so.\n\
         Be specific and concise. Use fenced code blocks for SQL fragments. \
         Use the tables and columns listed below as reference when they \
         appear in the statement.\n"
    );
    push_schema_section(&mut prompt, active_schema, schema_map);
    prompt
}

/// 選択 SQL の解説用の user message (SQL のみ) を組み立てる。
pub fn build_explain_sql_user_message(sql: &str) -> String {
    format!("SQL:\n```sql\n{}\n```", sql.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(text: &str) -> serde_yaml::Value {
        serde_yaml::from_str(text).unwrap()
    }

    #[test]
    fn test_ai_config_from_value_full() {
        let config = AiConfig::from_value(&yaml(
            "provider: openai\napi_key: sk-test\nmodel: gpt-5.2\nbase_url: https://example.com/v1",
        ))
        .unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.api_key, "sk-test");
        assert_eq!(config.model(), "gpt-5.2");
        assert_eq!(config.base_url(), "https://example.com/v1");
    }

    #[test]
    fn test_ai_config_defaults() {
        // provider / model / base_url は省略できる
        let config = AiConfig::from_value(&yaml("api_key: sk-test")).unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model(), DEFAULT_OPENAI_MODEL);
        assert_eq!(config.base_url(), DEFAULT_OPENAI_BASE_URL);
    }

    #[test]
    fn test_ai_config_base_url_trailing_slash() {
        let config =
            AiConfig::from_value(&yaml("api_key: sk-test\nbase_url: https://example.com/v1/"))
                .unwrap();
        assert_eq!(config.base_url(), "https://example.com/v1");
    }

    #[test]
    fn test_ai_config_unknown_provider_is_error() {
        let err = AiConfig::from_value(&yaml("provider: anthropic\napi_key: sk-test"))
            .unwrap_err();
        assert!(err.to_string().contains("Unsupported AI provider"));
    }

    #[test]
    fn test_ai_config_missing_api_key_is_error() {
        assert!(AiConfig::from_value(&yaml("provider: openai")).is_err());
        let err = AiConfig::from_value(&yaml("provider: openai\napi_key: \"  \"")).unwrap_err();
        assert!(err.to_string().contains("empty api_key"));
    }

    #[test]
    fn test_resolve_ai_config_some() {
        // 優先順位 (ローカル config.yml vs 取得 YAML) は設定マージ側の責務に
        // なったため、ここは渡された ai セクションを解釈できるかだけを見る
        let ai = yaml("api_key: sk-test\nmodel: test-model");
        let config = resolve_ai_config(Some(&ai)).unwrap().unwrap();
        assert_eq!(config.api_key, "sk-test");
        assert_eq!(config.model(), "test-model");
    }

    #[test]
    fn test_resolve_ai_config_none() {
        assert!(resolve_ai_config(None).unwrap().is_none());
    }

    #[test]
    fn test_resolve_ai_config_invalid_is_error() {
        // 不正な provider は黙って無視せずエラーにする (誤設定で動き続けない)
        let ai = yaml("provider: unknown\napi_key: sk-test");
        assert!(resolve_ai_config(Some(&ai)).is_err());
    }

    #[test]
    fn test_strip_sql_fences() {
        // ```sql フェンス付き
        assert_eq!(
            strip_sql_fences("```sql\nSELECT * FROM users;\n```"),
            "SELECT * FROM users;"
        );
        // 言語タグ無しのフェンス
        assert_eq!(strip_sql_fences("```\nSELECT 1;\n```"), "SELECT 1;");
        // 1 行フェンス
        assert_eq!(strip_sql_fences("```SELECT 1```"), "SELECT 1");
        // フェンス無しは前後の空白のみ除去
        assert_eq!(strip_sql_fences("  SELECT 1;\n"), "SELECT 1;");
        // 閉じフェンスが無い場合も先頭フェンスは剥がす
        assert_eq!(strip_sql_fences("```sql\nSELECT 1;"), "SELECT 1;");
        // 複数行の SQL は中の改行を保持する
        assert_eq!(
            strip_sql_fences("```sql\nSELECT a\nFROM t;\n```"),
            "SELECT a\nFROM t;"
        );
    }

    #[test]
    fn test_build_sql_system_prompt() {
        let mut schema_map = BTreeMap::new();
        schema_map.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );
        schema_map.insert("orders".to_string(), vec!["id".to_string()]);
        let prompt = build_sql_system_prompt("postgres", Some("app_db"), &schema_map);
        assert!(prompt.contains("PostgreSQL"));
        assert!(prompt.contains("'app_db'"));
        assert!(prompt.contains("- users (id, name)"));
        assert!(prompt.contains("- orders (id)"));
        assert!(prompt.contains("Return ONLY the SQL statement"));
    }

    #[test]
    fn test_build_sql_system_prompt_no_schema() {
        // アクティブスキーマ無し・テーブル無しでも壊れないこと
        let prompt = build_sql_system_prompt("sqlite", None, &BTreeMap::new());
        assert!(prompt.contains("SQLite"));
        assert!(prompt.contains("(no tables found)"));
        assert!(!prompt.contains("active schema"));
        // 空文字のスキーマ名は含めない
        let prompt = build_sql_system_prompt("mysql", Some(""), &BTreeMap::new());
        assert!(prompt.contains("MySQL"));
        assert!(!prompt.contains("active schema"));
    }

    #[test]
    fn test_build_fix_sql_system_prompt() {
        let mut schema_map = BTreeMap::new();
        schema_map.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );
        let prompt = build_fix_sql_system_prompt("mysql", Some("app_db"), &schema_map);
        assert!(prompt.contains("MySQL"));
        assert!(prompt.contains("'app_db'"));
        assert!(prompt.contains("- users (id, name)"));
        assert!(prompt.contains("Return ONLY the corrected SQL statement"));
    }

    #[test]
    fn test_build_fix_sql_system_prompt_no_schema() {
        // アクティブスキーマ無し・テーブル無しでも壊れないこと
        let prompt = build_fix_sql_system_prompt("sqlite", None, &BTreeMap::new());
        assert!(prompt.contains("SQLite"));
        assert!(prompt.contains("(no tables found)"));
        assert!(!prompt.contains("active schema"));
    }

    #[test]
    fn test_build_fix_sql_user_prompt() {
        let prompt = build_fix_sql_user_prompt(
            "SELECT * FROM userz;\n",
            "  ERROR 1146: Table 'app.userz' doesn't exist ",
        );
        assert!(prompt.contains("The following SQL statement failed:\n\nSELECT * FROM userz;"));
        assert!(prompt.contains(
            "The database returned this error:\n\nERROR 1146: Table 'app.userz' doesn't exist"
        ));
        // 前後の空白は除去される
        assert!(!prompt.ends_with(' '));
    }

    #[test]
    fn test_truncate_for_error() {
        assert_eq!(truncate_for_error(" short "), "short");
        let long = "x".repeat(ERROR_BODY_MAX_CHARS + 100);
        let truncated = truncate_for_error(&long);
        assert!(truncated.chars().count() == ERROR_BODY_MAX_CHARS + 3);
        assert!(truncated.ends_with("..."));
    }
    #[test]
    fn test_build_explain_system_prompt() {
        let mut schema_map = BTreeMap::new();
        schema_map.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );
        let prompt = build_explain_system_prompt("postgres", Some("app_db"), &schema_map);
        assert!(prompt.contains("PostgreSQL"));
        assert!(prompt.contains("'app_db'"));
        assert!(prompt.contains("- users (id, name)"));
        assert!(prompt.contains("Bottlenecks"));
        assert!(prompt.contains("Index suggestions"));
        assert!(prompt.contains("Query rewrite"));
        // アクティブスキーマ無し・テーブル無しでも壊れないこと
        let prompt = build_explain_system_prompt("sqlite", None, &BTreeMap::new());
        assert!(prompt.contains("SQLite"));
        assert!(prompt.contains("(no tables found)"));
        assert!(!prompt.contains("active schema"));
    }

    #[test]
    fn test_build_explain_sql_system_prompt() {
        let mut schema_map = BTreeMap::new();
        schema_map.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );
        let prompt = build_explain_sql_system_prompt("postgres", Some("app_db"), &schema_map);
        assert!(prompt.contains("PostgreSQL"));
        assert!(prompt.contains("'app_db'"));
        assert!(prompt.contains("- users (id, name)"));
        assert!(prompt.contains("Summary"));
        assert!(prompt.contains("Step by step"));
        assert!(prompt.contains("Caveats"));
        assert!(prompt.contains("Respond in Markdown"));
    }

    #[test]
    fn test_build_explain_sql_system_prompt_no_schema() {
        // アクティブスキーマ無し・テーブル無しでも壊れないこと
        let prompt = build_explain_sql_system_prompt("sqlite", None, &BTreeMap::new());
        assert!(prompt.contains("SQLite"));
        assert!(prompt.contains("(no tables found)"));
        assert!(!prompt.contains("active schema"));
        // 空文字のスキーマ名は含めない
        let prompt = build_explain_sql_system_prompt("mysql", Some(""), &BTreeMap::new());
        assert!(prompt.contains("MySQL"));
        assert!(!prompt.contains("active schema"));
    }

    #[test]
    fn test_build_explain_sql_user_message() {
        // SQL は前後の空白を除去してフェンスに入れる
        let message = build_explain_sql_user_message("  SELECT * FROM users\n");
        assert_eq!(message, "SQL:\n```sql\nSELECT * FROM users\n```");
    }

    #[test]
    fn test_build_explain_user_message() {
        let message = build_explain_user_message(
            "EXPLAIN QUERY PLAN\nSELECT * FROM t\n",
            "id\tparent\tdetail\n2\t0\tSCAN t\n",
        );
        assert!(message.contains("SQL:\n```sql\nEXPLAIN QUERY PLAN\nSELECT * FROM t\n```"));
        assert!(message.contains("Execution plan:\n```\nid\tparent\tdetail\n2\t0\tSCAN t\n```"));
    }

}
