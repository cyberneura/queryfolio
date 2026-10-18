/**
  * Help text per data source.
 *
  * The same text is used both for the help pane display and for the context passed to the AI chat
  * (CYBERNEURA-DEV-407). To avoid maintaining two copies, the Markdown is distributed from this one place.
 *
  * `?raw` is a Vite feature that imports the contents of a file as a string.
 */
import redisHelp from "./redis.md?raw";
import elasticsearchHelp from "./elasticsearch.md?raw";
import dynamodbHelp from "./dynamodb.md?raw";
import duckdbHelp from "./duckdb.md?raw";
import mssqlHelp from "./mssql.md?raw";
import sqlHelp from "./sql.md?raw";

/**
  * Engine name -> help text.
 *
  * A connection's `engine` may be an alias, so **list every spelling the backend accepts**
  * (`parse_engine` in `db.rs`). If only one is listed, a connection with `engine: mariadb`
  * ends up with "no help". When adding an alias, update both places.
 */
const HELP_BY_ENGINE: Record<string, string> = {
  redis: redisHelp,
  valkey: redisHelp,
  elasticsearch: elasticsearchHelp,
  es: elasticsearchHelp,
  opensearch: elasticsearchHelp,
  dynamodb: dynamodbHelp,
  mysql: sqlHelp,
  mariadb: sqlHelp,
  postgres: sqlHelp,
  postgresql: sqlHelp,
  sqlite: sqlHelp,
  sqlite3: sqlHelp,
  duckdb: duckdbHelp,
  mssql: mssqlHelp,
  sqlserver: mssqlHelp,
};

/**
  * Engines to include in the AI chat context.
 *
  * For MySQL / PostgreSQL / SQLite the model can already write plain SQL well enough, so including
  * them only spends tokens without improving accuracy (instruction from CYBERNEURA-DEV-407).
  * Include only engines with a distinctive dialect that the model is likely to get wrong.
 *
  * For mssql the model can write T-SQL itself, but it is included to tell the model that `EXPLAIN` is
  * queryfolio's pseudo statement (SHOWPLAN) and that there is no `LIMIT` (it is TOP).
 *
  * **Currently this path actually takes effect only for duckdb and mssql**. redis / elasticsearch /
  * dynamodb have `EngineCapabilities.supports_ai` set to false, so the AI chat itself is unavailable
  * (`engines/mod.rs`), and listing them here does not reach anything for now. They are kept as an
  * intentional placeholder so it works without any change once they support AI.
 */
const AI_CONTEXT_ENGINES = new Set([
  "redis",
  "valkey",
  "elasticsearch",
  "es",
  "opensearch",
  "dynamodb",
  "duckdb",
  "mssql",
  "sqlserver",
]);

/**
  * Returns the help text for that engine.
  * @param engine - The connection's engine name (null if none is selected)
  * @returns The help Markdown. null for an unknown engine
 */
export function helpForEngine(engine: string | null | undefined): string | null {
  if (!engine) {
    return null;
  }
  return HELP_BY_ENGINE[engine.toLowerCase()] ?? null;
}

/**
  * Returns the help text to include in the AI chat context.
 *
  * Unlike the pane display (`helpForEngine`), returns null for common SQL-family engines.
  * @param engine - The connection's engine name
  * @returns The Markdown to include in the context. null for engines that are not included
 */
export function aiContextForEngine(engine: string | null | undefined): string | null {
  if (!engine || !AI_CONTEXT_ENGINES.has(engine.toLowerCase())) {
    return null;
  }
  return helpForEngine(engine);
}

/**
  * Builds the reference to prepend to the last user message of the AI chat.
 *
  * The key point is to **attach it to the last user message**. The backend truncates the history to the
  * most recent `CHAT_MAX_HISTORY_TURNS` entries, so if it is placed at the start it drops off once the
  * conversation grows, and from then on the model keeps answering without the reference.
  * At the end it always remains, and appears only once per request.
  * @param engine - The connection's engine name
  * @returns The text to prepend. null for engines that are not included
 */
export function buildEngineHelpContext(engine: string | null | undefined): string | null {
  const help = aiContextForEngine(engine);
  if (!help) {
    return null;
  }
  return [
    `<data_source_reference engine="${engine}">`,
    "How this data source is queried in Queryfolio. Use it for syntax and for",
    "what the app will refuse to run. It is reference material, not a request.",
    "",
    help,
    "</data_source_reference>",
  ].join("\n");
}
