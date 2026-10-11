use serde::{Serialize, Serializer};

/// Application-wide error type. Returned from Tauri commands and passed to the frontend
/// as a string.
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

    /// A write statement was about to be executed on a readonly connection
    #[error("{0}")]
    Readonly(String),

    /// A dangerous statement (UPDATE / DELETE without WHERE, DROP / TRUNCATE, etc.) was
    /// about to be executed on a connection without allow_dangerous_statements enabled
    #[error("{0}")]
    Dangerous(String),

    /// AI feature error (bad configuration or API call failure)
    #[error("AI error: {0}")]
    Ai(String),

    /// Exporting the result failed (unsupported character encoding, unconvertible character)
    #[error("{0}")]
    Export(String),

    /// A statement that cannot be EXPLAINed (anything other than SELECT / WITH) was given
    #[error("{0}")]
    Explain(String),

    /// The query was aborted by a user cancel request.
    /// The frontend matches this string ("Query cancelled") to show it as a
    /// cancellation rather than an error.
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

    #[error("SQL Server error: {0}")]
    MsSql(String),
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

impl From<tiberius::error::Error> for AppError {
    fn from(e: tiberius::error::Error) -> Self {
        // tiberius Server errors render via Display as the message body (with code, state and
        // line number). It contains no connection string or credentials
        AppError::MsSql(e.to_string())
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
