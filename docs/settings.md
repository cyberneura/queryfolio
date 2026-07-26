# Settings

QueryFolio is configured through a single YAML file. This page explains where that
file lives and every key it accepts.

For a ready-to-copy starting point, see [`config.example.yaml`](../config.example.yaml)
in the repository root.

## Table of contents

- [File location](#file-location)
- [Editing the config](#editing-the-config)
- [Connections (`servers`)](#connections-servers)
  - [Common keys](#common-keys)
  - [Engines](#engines)
  - [Safety guards (`readonly`, `allow_dangerous_statements`)](#safety-guards)
  - [SSH tunnels (`ssh_tunnel`)](#ssh-tunnels-ssh_tunnel)
- [Connection groups](#connection-groups)
- [Connection templates](#connection-templates)
- [Overriding config from an external source (`config_override_command`)](#overriding-config-from-an-external-source-config_override_command)
- [AI features (`ai`)](#ai-features-ai)
- [Auto `LIMIT` (`default_limit`)](#auto-limit-default_limit)
- [Query file storage (`sqlfiles_dir`, `folder_name`)](#query-file-storage-sqlfiles_dir-folder_name)
- [Environment variable override (`QUERYFOLIO_CONFIG_YAML`)](#environment-variable-override-queryfolio_config_yaml)
- [Full example](#full-example)

## File location

The config file lives at:

```
~/.config/queryfolio/config.yml
```

`config.yaml` (the `.yaml` spelling) is also accepted; `config.yml` takes
precedence when both exist. On first launch, if neither file is present,
QueryFolio creates a starter `config.yml` for you.

Because the file can contain plaintext passwords and other secrets, QueryFolio
writes it with `0600` permissions (owner read/write only) on macOS/Linux. If an
existing file has looser permissions, they are tightened to `0600` on load and on
save. (On Windows, the platform's default file permissions apply.)

## Editing the config

You can edit the file with any text editor, or from inside the app:

- Menu bar **QueryFolio → Edit config.yml** opens a built-in editor (with YAML
  syntax highlighting and lint errors shown inline).
- Saving from the in-app editor validates that the content still parses as a YAML
  mapping, writes it atomically, and reloads all connections.

After editing the file directly, reload connections (reopen the app or use the
menu) to pick up the changes.

## Connections (`servers`)

`servers` is a **list** of connection definitions. Each entry has the same
shape as `sql-agent-mcp-server` (where the top-level key is `sql_servers`).
Writing a mapping here (instead of a list) is an error.

> **Renamed:** this key used to be `sql_servers`, and `server_templates` used to
> be `sql_server_templates`. The old names are rejected with an error, so rename
> them in your config file.

```yaml
servers:
  - name: dev-postgres
    description: "Development PostgreSQL"
    engine: postgres
    host: localhost
    port: 5432
    schema: development_db
    user: dev_user
    password: your_password_here
```

### Common keys

| Key | Required | Description |
|-----|----------|-------------|
| `name` | yes | Display name shown in the connections list. |
| `engine` | yes | `postgres` (aliases: `postgresql`), `mysql` (aliases: `mariadb`), `sqlite` (aliases: `sqlite3`), `duckdb`, `redis` (aliases: `valkey`), `elasticsearch` (aliases: `es`, `opensearch`), or `dynamodb`. |
| `description` | no | Free-text note shown in the UI. |
| `host` | no | Database host. Defaults to `localhost` when omitted. Not needed for SQLite / DuckDB. For DynamoDB it is an **endpoint override** (dynamodb-local); omit it to use the standard AWS endpoint. When using an SSH tunnel, this is the DB host **as seen from the SSH endpoint** (often `localhost`). |
| `port` | no | Database port. Defaults per engine when omitted: `5432` (PostgreSQL) / `3306` (MySQL) / `6379` (Redis) / `9200` (Elasticsearch) / `8000` (DynamoDB endpoint override). |
| `schema` | depends | The database / schema to connect to. For SQLite / DuckDB, this is the **path to the database file** (queryfolio extension; `~` is expanded; if `schema` is omitted, `host` is used as the file path instead). For DynamoDB, this is the **AWS region** (required, e.g. `ap-northeast-1`). |
| `user` | no | Database user. For DynamoDB, a static **access key ID** (paired with `password` as the secret access key). |
| `password` | no | Database password. |
| `tls` | no | Use `https` for HTTP-based engines (Elasticsearch, and the DynamoDB endpoint override; queryfolio extension). Default `false`. Ignored by SQL engines. |
| `aws_profile` | no | DynamoDB only (queryfolio extension): the AWS profile name (`~/.aws/config` / `credentials`) used for credentials. Ignored when `user` / `password` are set. SSO-based profiles (`sso_session` etc.) are not supported yet — use static keys or the default credential chain. |
| `readonly` | no | See [Safety guards](#safety-guards). Default `false`. |
| `allow_dangerous_statements` | no | See [Safety guards](#safety-guards). Default `false`. |
| `folder_name` | no | Override the query-file folder name. See [Query file storage](#query-file-storage-sqlfiles_dir-folder_name). |
| `ssh_tunnel` | no | Connect through an SSH tunnel. See [SSH tunnels](#ssh-tunnels-ssh_tunnel). |
| `template` | no | Inherit keys from a named template. See [Connection templates](#connection-templates). |

### Engines

- **PostgreSQL** — `engine: postgres`. Standard host / port / schema / user /
  password.
- **MySQL / MariaDB** — `engine: mysql`. Standard host / port / schema / user /
  password.
- **SQLite** — `engine: sqlite`. Put the **file path** in `schema` (queryfolio
  extension; if `schema` is omitted, `host` is used as the file path instead).
  `port` / `user` / `password` are not used.

  ```yaml
  - name: local-sqlite
    engine: sqlite
    schema: ~/data/example.sqlite3
  ```

- **DuckDB** — `engine: duckdb` (queryfolio extension). A SQL engine, so the
  SQL editor, meta commands (`\l` `\dt` `\dv` `\dn` `\d [table]`), Explain,
  Format, auto LIMIT, AI features, and the TABLES pane all work as for the
  other SQL engines. Like SQLite, put the **file path** in `schema` (`~` is
  expanded; if `schema` is omitted, `host` is used as the file path instead).
  The file must already exist — QueryFolio does not create a new database
  file. `port` / `user` / `password` / `ssh_tunnel` are not used. Cell editing
  in the results grid is not available for DuckDB.

  ```yaml
  - name: local-duckdb
    engine: duckdb
    schema: ~/data/example.duckdb
  ```

- **Redis** — `engine: redis` (alias: `valkey`; queryfolio extension). The
  editor runs **one command per line** (`GET my-key`, `MGET a b c`, ...). Run
  the line under the cursor with Cmd+Enter, or select multiple lines to run
  them in order. Lines starting with `#` are comments. Query files use the
  `.redis` extension. `schema` is the **database number** (default `0`).
  `user` / `password` are used for ACL / AUTH. SSH tunnels work the same way
  as for SQL engines. While the **Writable** switch is off, only known
  read-only commands (GET / MGET / HGETALL / SCAN / ZRANGE, ...) are allowed;
  `FLUSHALL` / `FLUSHDB` / `SHUTDOWN` / `DEBUG` additionally require
  `allow_dangerous_statements: true`. Schema browsing (TABLES), Explain,
  Format, cell editing, and AI features are not available for Redis.

  ```yaml
  - name: local-redis
    engine: redis
    host: localhost
    port: 6379
    schema: "0"
  ```

- **Elasticsearch** — `engine: elasticsearch` (aliases: `es`, `opensearch`;
  queryfolio extension). The editor works like the Kibana Console: write
  **request blocks** — a method line (`GET /books/_search`) optionally followed
  by a JSON body — and run the block under the cursor with Cmd+Enter, or select
  multiple blocks to run them in order (results are shown as a
  request / status / result table). Lines starting with `#` are comments. An
  NDJSON body (multiple JSON objects, for `_bulk`) is sent as-is. Query files
  use the `.es` extension. `user` / `password` are used for HTTP Basic auth,
  and `tls: true` switches to `https`. SSH tunnels work the same way as for
  SQL engines. The TABLES pane lists indices (system indices starting with `.`
  are hidden) and expands them into the flattened mapping fields. While the
  **Writable** switch is off, only `GET` / `HEAD` and a whitelist of search
  POST endpoints (`_search`, `_msearch`, `_count`, `_analyze`, `_mget`,
  `_field_caps`, `_validate`, `_explain`, `_termvectors`, `_pit`, `_sql`,
  `_render`) are allowed; deleting an index (`DELETE /<index>`) and
  `_delete_by_query` additionally require `allow_dangerous_statements: true`.
  Schemas, Explain, Format, cell editing, and AI features are not available
  for Elasticsearch.

  ```yaml
  - name: local-elasticsearch
    engine: elasticsearch
    host: localhost
    port: 9200
    # tls: true
    # user: elastic
    # password: your_es_password
  ```

- **DynamoDB** — `engine: dynamodb` (queryfolio extension). The editor runs
  **PartiQL** (DynamoDB's SQL-compatible language: `SELECT` / `INSERT` /
  `UPDATE` / `DELETE`) through the ExecuteStatement API, so the regular SQL
  editor, Format, and the `.sql` query-file extension are used. PartiQL has no
  `LIMIT` clause; QueryFolio bounds the result with the API's page limit and
  reports truncation instead (no auto LIMIT is appended). `schema` is the
  **AWS region** (required). Credentials are resolved in this order:
  1. `user` / `password` as a static access key ID / secret access key,
  2. `aws_profile` (queryfolio extension) naming a profile in `~/.aws`
     (static-key profiles only; SSO-based profiles are not supported yet),
  3. the default AWS credentials chain (environment variables → `default`
     profile → IMDS).
  `host` / `port` override the endpoint for
  [dynamodb-local](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/DynamoDBLocal.html)
  (`tls: true` for https; dynamodb-local does not verify credentials, so
  alphanumeric dummy values are fine). The TABLES pane lists tables and
  expands them into the key schema (partition / sort key) and the indexed
  attribute definitions — DynamoDB is schemaless, so non-key attributes are
  not listed. While the **Writable** switch is off only `SELECT` runs, and
  `UPDATE` / `DELETE` without a `WHERE` clause require
  `allow_dangerous_statements: true`. Meta commands, schemas, Explain, cell
  editing, AI features, and SSH tunnels are not available for DynamoDB.

  ```yaml
  - name: aws-dynamodb
    engine: dynamodb
    schema: ap-northeast-1
    # aws_profile: my-profile

  - name: local-dynamodb
    engine: dynamodb
    schema: us-east-1
    host: 127.0.0.1
    port: 8000
    user: dummyAccessKey
    password: dummySecretKey
  ```

### Safety guards

Two per-connection flags help prevent accidents. They are independent of the
toolbar **Writable** switch (which is off by default each session and only lets
side-effect-free statements run until you turn it on).

- **`readonly: true`** — rejects write statements (INSERT / UPDATE / DELETE /
  DDL, CTE-wrapped DML, `SELECT INTO`, `EXPLAIN ANALYZE` of a DML, an assignment
  `PRAGMA` like `PRAGMA journal_mode = WAL`, etc.). The check is keyword-based:
  statements whose leading keyword is `SELECT` / `WITH` / `SHOW` / `DESCRIBE` /
  `DESC` / `EXPLAIN` / `VALUES` / `TABLE` / `CALL` / (non-assignment) `PRAGMA`,
  plus meta commands, are allowed. This is a guard, not a sandbox — it does **not** stop
  every side effect: a `CALL` to a stored procedure that writes, a SELECT that
  calls a side-effecting function (e.g. `nextval`), or a parenthesized settings
  `PRAGMA` are not blocked. A `readonly` connection shows a lock in the UI and
  cannot be unlocked with the Writable switch.

  ```yaml
  - name: production-replica
    engine: mysql
    host: replica.example.com
    schema: production_db
    user: replica_user
    password: replica_password
    readonly: true
  ```

- **`allow_dangerous_statements: true`** — by default (`false`), statements that
  can destroy a lot of data at once — `UPDATE` / `DELETE` with no `WHERE`, and
  `DROP` / `TRUNCATE` — are rejected. Set this to `true` to allow them; the app
  still shows a confirmation dialog before running such a statement. `readonly`
  is evaluated first, so a `readonly` connection never reaches this guard.

  The `WHERE` check is a simple word scan (after stripping literals and
  comments), so it is deliberately conservative: an `UPDATE` / `DELETE` whose
  only `WHERE` is inside a sub-query or an unrelated CTE is treated as "has a
  `WHERE`" and allowed through. It reliably catches the typical `WHERE`-less
  form; it is not a full parser.

  ```yaml
  - name: dev-db
    engine: postgres
    host: localhost
    schema: dev
    user: dev_user
    password: dev_password
    allow_dangerous_statements: true
  ```

### SSH tunnels (`ssh_tunnel`)

Add an `ssh_tunnel` block to connect through an SSH local port forward. There are
two modes.

**1. Built-in tunnel (libssh2).** Give it the SSH host / user and one of:
password, private key, or (by default) the SSH agent.

```yaml
- name: remote-db-with-ssh
  engine: postgres
  host: localhost      # DB host as seen from the SSH endpoint
  port: 5432
  schema: remote_db
  user: remote_user
  password: remote_password
  ssh_tunnel:
    host: ssh.example.com
    port: 22
    user: ssh_user
    # Pick ONE auth method:
    # password: ssh_password
    private_key_path: ~/.ssh/id_rsa
    # private_key_passphrase: key_passphrase
    # When no password/private_key_path is set, the SSH agent is used.
    # identity_agent: ~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock
```

`ssh_tunnel` keys (built-in mode):

| Key | Description |
|-----|-------------|
| `host` | SSH host to connect to. |
| `port` | SSH port (default `22`). |
| `user` | SSH user. |
| `password` | SSH password (optional). |
| `private_key_path` | Path to a private key (optional; `~` expanded). |
| `private_key_passphrase` | Passphrase for the private key (optional). |
| `identity_agent` | ssh-agent socket to use (queryfolio extension, like OpenSSH's `IdentityAgent`). Use `none` to disable the agent. When omitted, it is resolved from `~/.ssh/config` (`IdentityAgent`) and then `$SSH_AUTH_SOCK`. Useful when the app is launched from Finder/Dock and does not inherit the right socket (e.g. the 1Password SSH agent). |

**2. Delegate to the system `ssh` client (`ssh_config`).** Set `ssh_config` to a
`Host` alias from your `~/.ssh/config`. QueryFolio then runs the system `ssh`
client (`ssh -N -L`) instead of libssh2, so **ProxyJump / multi-hop tunnels** and
full `HostName` / `User` / `Port` resolution are handled by OpenSSH.

```yaml
- name: remote-db-via-ssh-config
  engine: postgres
  host: localhost      # DB host as seen from the final SSH host
  port: 5432
  schema: remote_db
  user: remote_user
  password: remote_password
  ssh_tunnel:
    ssh_config: pop-three-ec2-staging
```

With a `~/.ssh/config` like:

```
Host pop-three-ec2-staging
    HostName 172.21.122.39
    User torico
    ProxyJump pop-three-bastion
```

naming the alias is enough. In this mode the built-in-mode keys (`host` / `user`
/ `password` / `private_key_*` / `identity_agent`) are **ignored** — authentication
and host-key checking are done entirely by OpenSSH (`BatchMode=yes`, so an unknown
host key or a passphrase prompt fails instead of blocking; agent auth still
works).

## Connection groups

Wrap servers in a `group_name` + nested `servers` entry to show them under a
group heading in the connections list (queryfolio extension). Grouped and plain
(ungrouped) servers can be mixed; the display order follows the config order.
Groups cannot be nested (a group inside a group is an error), and a group entry
may only contain `group_name` and `servers`.

```yaml
servers:
  - group_name: production
    servers:
      - name: prod-main
        engine: mysql
        host: prod.example.com
        # ...
      - name: prod-replica
        engine: mysql
        host: replica.example.com
        # ...
  - name: ungrouped-db          # plain servers can be mixed in
    engine: sqlite
    schema: ~/data/example.sqlite3
```

Templates (see below) still work inside a group.

## Connection templates

Define reusable defaults under `server_templates`, then reference one with
`template: <name>` on a server. Keys set on the server override the same key in
the template (shallow merge).

```yaml
server_templates:
  - name: my-awesome-sql-host
    engine: mysql
    host: db.example.com
    port: 3306
    user: shared_user
    password: shared_password

servers:
  - name: reporting
    template: my-awesome-sql-host
    schema: reporting_db      # host/port/user/password inherited
```

## Overriding config from an external source (`config_override_command`)

`config_override_command` runs a command whose **stdout is YAML**, and merges
that YAML over your file. This lets you keep secrets (API keys, passwords, whole
connection lists) in a secrets manager like 1Password instead of in plaintext.

```yaml
config_override_command: op read "op://development/queryfolio/config-yaml"
```

Merge rules (`config.rs > merge_mapping`):

- **Mappings are merged recursively** — e.g. you can override just `ai.api_key`
  and keep the local `ai.model`.
- **Scalars and lists (including `servers`) are replaced wholesale** — lists
  are not merged element-by-element, because there is no reliable element
  identity.
- **Any key** can be overridden this way, not just `servers`.

Notes:

- A `config_override_command` inside the fetched YAML is **not** followed
  recursively; the key is dropped after merging.
- The command runs **without a shell** (arguments are split with shlex, so pipes
  and redirects do not work). The minimal GUI `PATH` is supplemented with
  `/opt/homebrew/bin` and `/usr/local/bin`. It has a 60-second timeout.
- The merged config is cached once per session (the getter can take a few seconds
  plus Touch ID), and cleared on reload.
- If the key exists but is not a non-empty string, that is an error (QueryFolio
  will not silently fall back to the local-only config).
- Menu bar **QueryFolio → View override config yaml (Copy only)** appears when
  this key is set. It runs the command every time and shows the fetched YAML for
  inspection/copying. You can edit the text in the modal (handy for reformatting
  before copying it into 1Password), but those edits stay in memory only — there
  is no Save, so the changes are never written back.

## AI features (`ai`)

Configure natural-language → SQL generation and related AI helpers. The `ai`
section can live at the top level of the local config **or** in the YAML fetched
by `config_override_command`. When both exist, the fetched YAML wins (so the API
key can stay in 1Password). Because `ai` is a mapping, it is merged recursively —
putting just `api_key` in the fetched YAML keeps your local `model`.

```yaml
ai:
  provider: openai   # only "openai" is supported for now
  api_key: sk-your-api-key
  model: gpt-5.6-luna                   # optional (default: gpt-5.6-luna)
  base_url: https://api.openai.com/v1   # optional (for OpenAI-compatible APIs)
```

| Key | Required | Description |
|-----|----------|-------------|
| `provider` | no | Currently only `openai` (default). An unknown value is an error. |
| `api_key` | yes | The API key. It is not exposed through the app's AI status (which reports only whether AI is `configured` and the `model`), though the in-app config editor naturally shows the whole file, including this key. |
| `model` | no | Model name (default `gpt-5.6-luna`). |
| `base_url` | no | Base URL for OpenAI-compatible APIs (default `https://api.openai.com/v1`). |

What is sent to the AI: the schema (table / column names), the engine dialect,
your statements, your natural-language instructions, query plans (for EXPLAIN
analysis), and — for the "Fix with AI" helper — the **database error message** of
the failed statement. Query result rows are never sent. Note that a database error
message can itself embed data values (for example, a unique-constraint violation
often includes the conflicting key value), so it is not strictly free of row data.
Generated SQL is inserted into the editor, never auto-executed.

### AI chat pane

The toolbar's **Chat** button opens a chat pane on the right. Unlike the helpers
above, the assistant there can **run SQL itself** (a `run_sql` tool) to answer
questions about your data, so the rows it reads are sent back to the model as
tool results. Everything else it receives is the same: schema, dialect, active
schema name, and your messages — never connection details.

Those queries are **always read-only**, regardless of the Writable switch: the
same guard that backs read-only mode rejects writes, `allow_dangerous_statements`
is never applied, results are capped at 50 rows, and the assistant may call the
tool at most 6 times per reply. Every query it ran is listed above the answer,
so you can check what it looked at. The conversation is cleared when you switch
connections (the schema in the prompt changes) and is not persisted to disk.

## Auto `LIMIT` (`default_limit`)

`default_limit` appends `LIMIT n` to `SELECT` statements that do not already have
one, to avoid accidentally fetching huge result sets.

```yaml
default_limit: 500   # default: 500; set 0 to disable
```

Sub-query `LIMIT`s, `FOR UPDATE`, and similar cases are skipped conservatively.

## Query file storage (`sqlfiles_dir`, `folder_name`)

QueryFolio auto-saves per-connection query files as
`<sqlfiles_dir>/<folder>/<name>.sql`.

```yaml
sqlfiles_dir: ~/queries   # default: ~/.config/queryfolio/sqlfiles
```

The `<folder>` name is chosen per connection:

- If the server sets `folder_name`, that is used.
- Otherwise it is built as `<host>_<engine>_<schema>_<user>` (the connection
  `name` is **not** used; path separators are sanitized).

For example, the `dev-postgres` connection above resolves to
`localhost_postgres_development_db_dev_user`. Set `folder_name` to pin the folder
so it does not move when you change `host`/`schema`/etc.:

```yaml
- name: dev-postgres
  folder_name: dev-pg
  engine: postgres
  # ...
```

> If you rename a connection's folder (or change the fields it is derived from),
> existing query files stay in the old folder.

Each folder also gets an auto-generated `_queryfolio.md` describing the connection
(non-secret info only). It is not a `.sql` file, so it never shows up in the
file list or search.

## Environment variable override (`QUERYFOLIO_CONFIG_YAML`)

Setting the `QUERYFOLIO_CONFIG_YAML` environment variable replaces the **entire**
config file with the variable's contents. This is a development / testing hook
(GUI apps launched from Finder/Dock do not inherit shell environment variables,
so this mainly applies when launching from a terminal). While it is set, the
in-app config editor has no file to write and returns an error.

## Full example

See [`config.example.yaml`](../config.example.yaml) for a complete, annotated
example covering every key above.
