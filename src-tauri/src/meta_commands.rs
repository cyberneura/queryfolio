use crate::db::Engine;
use crate::error::AppError;

/// メタコマンドの解釈結果。
#[derive(Debug, PartialEq, Eq)]
pub enum MetaCommand {
    /// カタログ照会 SQL に変換できたもの (そのまま実行する)
    Sql(String),
    /// `\c <schema>` — アクティブスキーマ (database) の切り替え。
    /// SQL の実行ではなく接続状態の変更なので、実行前に lib.rs が処理する。
    Connect(String),
}

/// psql 風メタコマンド (\l, \dt など) と `USE <database>` を解釈する。
///
/// 大半は読み取り系のカタログ照会 SQL に変換する。`\c <schema>` と
/// `USE <database>` だけは SQL ではなくアクティブスキーマの切り替えを表す
/// MetaCommand::Connect を返す。
/// \i (ファイル実行) のようなその他の状態を持つコマンドは対象外。
/// 入力がメタコマンドでも `USE` でもなければ None、未対応のメタコマンドは
/// エラーを返す。
pub fn translate(engine: Engine, input: &str) -> Result<Option<MetaCommand>, AppError> {
    if let Some(command) = translate_use(engine, input)? {
        return Ok(Some(command));
    }
    let trimmed = input.trim();
    if !trimmed.starts_with('\\') {
        return Ok(None);
    }
    // psql 風メタコマンドは SQL 系エンジン専用
    // (DynamoDB は PartiQL = SQL 風だがカタログ照会 SQL を持たないため対象外)
    if matches!(
        engine,
        Engine::Redis | Engine::Elasticsearch | Engine::DynamoDb
    ) {
        return Err(AppError::Config(
            "Meta commands (\\...) are not supported for this engine".into(),
        ));
    }
    // SQL の癖で末尾に ; を付けても動くよう、末尾のセミコロンは無視する
    let trimmed = trimmed.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    let mut parts = trimmed.split_whitespace();
    let command = parts.next().unwrap_or("");
    let arg = parts.next();

    // \c はエンジン共通で先に処理する (SQL に変換せず接続状態を変える)
    if matches!(command, "\\c" | "\\connect") {
        // arg は消費済みなので、残りは database 名より後ろのトークン
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
        // 冒頭の早期 return で弾いている
        Engine::Redis | Engine::Elasticsearch | Engine::DynamoDb => unreachable!(),
    };
    Ok(Some(MetaCommand::Sql(sql)))
}

/// `USE <database>` を `\c <database>` と同じアクティブスキーマの切り替えとして
/// 解釈する。対象外の入力には None を返す (通常の SQL として実行される)。
///
/// `USE` をそのまま DB へ投げても切り替わらない: MySQL の `USE` はセッション
/// 単位の変更で、プールの別のコネクションに当たる次のクエリには効かない。
/// 加えて `USE` は fetch 系の文でないため readonly ガードにも弾かれる。
/// `\c` と同じ MetaCommand::Connect にすればプールを張り直すのでどちらも解決する
/// (切り替え自体は書き込みではないので readonly 接続でも許してよい)。
///
/// PostgreSQL に `USE` 文は無い (素で実行すれば構文エラー) が、MySQL の癖で
/// 打たれることが多いため同じく切り替えとして受け付ける。sqlite / duckdb は
/// `\c` 自体が非対応 (schema が DB ファイルパス) なので対象にしない。
/// DuckDB の `USE` はネイティブに動くため、そのまま実行させる。
fn translate_use(engine: Engine, input: &str) -> Result<Option<MetaCommand>, AppError> {
    if !matches!(engine, Engine::MySql | Engine::Postgres) {
        return Ok(None);
    }
    // 先頭キーワードの判定は leading_keyword と揃える (先頭のコメントは読み飛ばす)
    let rest = crate::db::strip_leading_comments(input);
    let keyword_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if !rest[..keyword_end].eq_ignore_ascii_case("use") {
        return Ok(None);
    }
    // 末尾のセミコロン・コメント (`USE mydb; -- switch`) を落とした本体だけを見る。
    // 方言ごとのコメント規則を書き直さないよう scan_sql の body_end を使う。
    // body_end は input 基準の byte 位置なので、キーワードの終端も input 基準へ直す
    // (キーワード自身は code なので body_end はその後ろにあるが、万一そうでなければ
    //  引数無しとして扱われ「切り替えない」側に倒れる)
    let arg_start = input.len() - rest.len() + keyword_end;
    let body_end = crate::db::scan_sql(input, engine).body_end;
    let after = input.get(arg_start..body_end).unwrap_or("");
    // `USE db; SELECT 1` のような複文は、切り替えだけ行って 2 文目を黙って
    // 捨てることになるため拒否する (末尾のセミコロンは body_end が落としているので、
    // ここに残る `;` は 2 文目がある証拠)
    if after.contains(';') {
        return Err(AppError::Config(
            "USE cannot be combined with another statement \
             (run one statement at a time)"
                .into(),
        ));
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

/// `USE` の引数に付いた識別子クォートを外す。MySQL は `` ` ``、PostgreSQL は
/// `"` がクォート文字。`USE \`my-db\`` のように方言として正しい書き方を
/// そのまま受け付けるため。外した中身は validate_database_name で検証する
/// (クォート内のエスケープを使う名前は非対応)。
fn unquote_database_name(engine: Engine, name: &str) -> &str {
    let quote = if engine == Engine::MySql { '`' } else { '"' };
    name.strip_prefix(quote)
        .and_then(|inner| inner.strip_suffix(quote))
        .unwrap_or(name)
}

/// `\c <schema>` の引数を検証する。
///
/// sqlite / duckdb は schema が DB ファイルパスで、切り替えは別の DB ファイルを
/// 開くことになるため対象外にする (設定ファイルで接続を分ける方が明快)。
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
    // psql の \c は database の後ろに user / host / port を取れるが、
    // ここで切り替えられるのは database だけ。黙って無視すると別のユーザーで
    // 繋がったと誤解させるため、余分な引数はエラーにする
    if !extra.is_empty() {
        return Err(AppError::Config(format!(
            "{command} takes only a database name (usage: {command} <database>). \
             Connecting as another user or host is not supported; \
             define another connection in the config instead"
        )));
    }
    Ok(validate_database_name(name)?.to_string())
}

/// `\c` の引数として使う database 名を検証する。
///
/// 接続オプションに渡す値で SQL には埋め込まないが、タイプミスで
/// プールを壊さないよう識別子として妥当な形だけ受け付ける。
/// schema.table 形式を許す validate_relation_name と違いドットは許可しない。
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

/// テーブル名引数を検証する。SQL に埋め込むため、識別子として安全な文字のみ許可する。
/// クォート付き識別子 (スペースや記号入り) は非対応。
/// \d のほか、スキーマブラウザ (schema_info) のテーブル名検証にも使う。
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

/// Postgres のリレーション一覧 SQL (relkind の集合で絞り込む)。
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
        // SHOW TABLES はビューも含むため、\dt はベーステーブルに絞る
        ("\\dt", _) => "SHOW FULL TABLES WHERE Table_type = 'BASE TABLE'".to_string(),
        ("\\d", None) => "SHOW TABLES".to_string(),
        ("\\dv", _) => "SHOW FULL TABLES WHERE Table_type = 'VIEW'".to_string(),
        ("\\d", Some(name)) => {
            let name = validate_relation_name(name)?;
            // schema.table を `schema`.`table` にクォートする
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

/// DuckDB は information_schema と PRAGMA (sqlite 互換) の両方を持つ。
/// 一覧系は information_schema、カラム定義は PRAGMA table_info を使う。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// SQL に変換されるメタコマンドの検証用。変換結果の SQL を取り出す。
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
        // _ が LIKE ワイルドカード扱いされないよう ESCAPE 句付き
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
        // インジェクションにつながる引数は拒否
        assert!(translate(Engine::DuckDb, "\\d users'; DROP TABLE x; --").is_err());
        // \c は DB ファイルパスなので拒否
        let err = translate(Engine::DuckDb, "\\c other").unwrap_err();
        assert!(err.to_string().contains("not supported for DuckDB"));
        // 未対応コマンド
        assert!(translate(Engine::DuckDb, "\\du").is_err());
    }

    #[test]
    fn test_injection_is_rejected() {
        // SQL インジェクションにつながる引数は拒否される
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
        // \c / \connect はどちらもスキーマ切替として解釈される
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
        // 先頭が数字の database 名 (MySQL では実在しうる) も受け付ける
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
        // psql の `\c <db> <user>` 形式。ユーザー切替はできないので、
        // 黙って database だけ切り替えず拒否する
        let err = translate(Engine::Postgres, "\\c proddb readonly_user").unwrap_err();
        assert!(err.to_string().contains("takes only a database name"));
        assert!(translate(Engine::MySql, "\\c proddb host 3306;").is_err());
    }

    #[test]
    fn test_connect_rejects_unsafe_names() {
        // 接続オプションに渡す値なので SQL インジェクションにはならないが、
        // 識別子として不自然なものはタイプミスとして弾く
        assert!(translate(Engine::Postgres, "\\c a;b").is_err());
        assert!(translate(Engine::Postgres, "\\c my.db").is_err());
        assert!(translate(Engine::MySql, "\\c `db`").is_err());
    }

    #[test]
    fn test_use_switches_database() {
        // USE は \c と同じアクティブスキーマの切り替えとして扱う
        // (セッション単位の USE ではプールの次のコネクションに効かないため)
        assert_eq!(
            translate(Engine::MySql, "USE chatbot_backend;")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("chatbot_backend".to_string())
        );
        // 小文字・大文字混在・前後の空白・改行も同じ
        assert_eq!(
            translate(Engine::MySql, "  use  2024_logs  \n")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("2024_logs".to_string())
        );
        // PostgreSQL に USE 文は無いが、MySQL の癖で打たれるので受け付ける
        assert_eq!(
            translate(Engine::Postgres, "Use otherdb").unwrap().unwrap(),
            MetaCommand::Connect("otherdb".to_string())
        );
        // 先頭のコメントは leading_keyword と同じく読み飛ばす
        assert_eq!(
            translate(Engine::MySql, "-- switch\nUSE mydb")
                .unwrap()
                .unwrap(),
            MetaCommand::Connect("mydb".to_string())
        );
        // 末尾のコメントも本体の外なので無視する (scan_sql の body_end)
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
        // 方言のクォート付き (MySQL の `db`) はそのまま受け付ける
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
        // 方言違いのクォートは外さないので、識別子として不正になる
        assert!(translate(Engine::MySql, "USE \"mydb\"").is_err());
        assert!(translate(Engine::Postgres, "USE `mydb`").is_err());
    }

    #[test]
    fn test_use_argument_errors() {
        let err = translate(Engine::MySql, "USE").unwrap_err();
        assert!(err.to_string().contains("requires a database name"));
        assert!(translate(Engine::MySql, "USE ;").is_err());
        // 複文は切り替えだけ行って 2 文目を捨てることになるので拒否する
        let err = translate(Engine::MySql, "USE mydb; DELETE FROM t").unwrap_err();
        assert!(err.to_string().contains("one statement at a time"));
        assert!(translate(Engine::MySql, "USE mydb;DELETE FROM t").is_err());
        assert!(translate(Engine::MySql, "USE a;b").is_err());
        // database 名より後ろに余分な引数があるものも拒否する
        let err = translate(Engine::MySql, "USE mydb other").unwrap_err();
        assert!(err.to_string().contains("takes only a database name"));
        // MySQL の実行コメントはサーバーが実行するのでコメント扱いにしない
        // (黙って切り替えだけ行い中身を捨ててはいけない)
        assert!(translate(Engine::MySql, "USE mydb /*! DROP TABLE t */").is_err());
        // 識別子として不自然な名前は弾く
        assert!(translate(Engine::MySql, "USE my.db").is_err());
    }

    #[test]
    fn test_use_is_not_intercepted_for_other_engines() {
        // sqlite / duckdb は \c 自体が非対応。duckdb の USE はネイティブに
        // 動くため、通常の SQL として実行させる (None を返す)
        assert!(translate(Engine::Sqlite, "USE mydb").unwrap().is_none());
        assert!(translate(Engine::DuckDb, "USE mydb").unwrap().is_none());
        assert!(translate(Engine::Redis, "USE mydb").unwrap().is_none());
        // USE で始まる別の識別子を切り替えと誤読しない
        assert!(translate(Engine::MySql, "USER_TABLE").unwrap().is_none());
        assert!(translate(Engine::MySql, "SELECT * FROM t USE INDEX (i)")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_connect_is_rejected_for_sqlite() {
        // sqlite の schema は DB ファイルパスなので切替対象にしない
        let err = translate(Engine::Sqlite, "\\c other").unwrap_err();
        assert!(err.to_string().contains("not supported for SQLite"));
    }
}
