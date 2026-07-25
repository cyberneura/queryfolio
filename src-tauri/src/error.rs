use serde::{Serialize, Serializer};

/// アプリ全体のエラー型。Tauri コマンドの戻り値としてフロントエンドに
/// 文字列として渡す。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Config error: {0}")]
    Config(String),

    #[error("SSH tunnel error: {0}")]
    SshTunnel(String),

    #[error("Query file error: {0}")]
    QueryFile(String),

    #[error("Query history error: {0}")]
    History(String),

    /// readonly 接続で書き込み系の文が実行されようとした
    #[error("{0}")]
    Readonly(String),

    /// 危険な文 (WHERE 無しの UPDATE / DELETE、DROP / TRUNCATE 等) が
    /// allow_dangerous_statements を有効にしていない接続で実行されようとした
    #[error("{0}")]
    Dangerous(String),

    /// AI 機能のエラー (設定不備・API 呼び出し失敗)
    #[error("AI error: {0}")]
    Ai(String),

    /// EXPLAIN の対象にできない文 (SELECT / WITH 以外) が指定された
    #[error("{0}")]
    Explain(String),

    /// ユーザーのキャンセル要求でクエリが中断された。
    /// フロントエンドはこの文字列 ("Query cancelled") との一致で
    /// エラーではなくキャンセルとして表示を分ける。
    #[error("Query cancelled")]
    Cancelled,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("Redis error: {0}")]
    Redis(String),

    #[error("Elasticsearch error: {0}")]
    Elasticsearch(String),

    #[error("DuckDB error: {0}")]
    DuckDb(String),

    #[error("DynamoDB error: {0}")]
    DynamoDb(String),
}

impl From<redis::RedisError> for AppError {
    fn from(e: redis::RedisError) -> Self {
        AppError::Redis(e.to_string())
    }
}

impl From<duckdb::Error> for AppError {
    fn from(e: duckdb::Error) -> Self {
        AppError::DuckDb(e.to_string())
    }
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
