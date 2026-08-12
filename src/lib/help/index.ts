/**
 * データソース別のヘルプ本文。
 *
 * ヘルプペインの表示にも、AI チャットへ渡すコンテクストにも同じ本文を使う
 * (CYBERNEURA-DEV-407)。二重管理を避けるため、Markdown はここから一箇所で配る。
 *
 * `?raw` は Vite の機能で、ファイルの中身を文字列として取り込む。
 */
import redisHelp from "./redis.md?raw";
import elasticsearchHelp from "./elasticsearch.md?raw";
import dynamodbHelp from "./dynamodb.md?raw";
import duckdbHelp from "./duckdb.md?raw";
import sqlHelp from "./sql.md?raw";

/**
 * エンジン名 → ヘルプ本文。
 *
 * 接続の `engine` はエイリアスを含むため、**バックエンドが受け付ける綴りを全て**
 * 並べる (`db.rs` の `parse_engine`)。片方だけだと `engine: mariadb` の接続が
 * 「ヘルプ無し」になる。エイリアスを足す時は両方を揃えること。
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
};

/**
 * AI チャットのコンテクストに載せるエンジン。
 *
 * MySQL / PostgreSQL / SQLite は素の SQL でモデルが十分に書けるため、載せても
 * トークンを使うばかりで精度に効かない (CYBERNEURA-DEV-407 の指示)。
 * 載せるのは方言が独特で、モデルが取り違えやすいものだけ。
 *
 * **今この経路が実際に効くのは duckdb だけ**。redis / elasticsearch / dynamodb は
 * `EngineCapabilities.supports_ai` が false で AI チャット自体が使えないため
 * (`engines/mod.rs`)、ここに載せても現状は届かない。将来それらが AI 対応した時に
 * 何もしなくても効くよう、意図として残してある。
 */
const AI_CONTEXT_ENGINES = new Set([
  "redis",
  "valkey",
  "elasticsearch",
  "es",
  "opensearch",
  "dynamodb",
  "duckdb",
]);

/**
 * そのエンジンのヘルプ本文を返す。
 * @param engine - 接続の engine 名 (未選択なら null)
 * @returns ヘルプの Markdown。未知のエンジンなら null
 */
export function helpForEngine(engine: string | null | undefined): string | null {
  if (!engine) {
    return null;
  }
  return HELP_BY_ENGINE[engine.toLowerCase()] ?? null;
}

/**
 * AI チャットのコンテクストに載せるヘルプ本文を返す。
 *
 * ペイン表示用 (`helpForEngine`) と違い、SQL 系の一般的なエンジンでは null を返す。
 * @param engine - 接続の engine 名
 * @returns コンテクストに載せる Markdown。載せないエンジンなら null
 */
export function aiContextForEngine(engine: string | null | undefined): string | null {
  if (!engine || !AI_CONTEXT_ENGINES.has(engine.toLowerCase())) {
    return null;
  }
  return helpForEngine(engine);
}

/**
 * AI チャットの最後のユーザー発言に前置きするリファレンスを組み立てる。
 *
 * **最後のユーザー発言に付ける**のが要点。バックエンドは履歴を直近
 * `CHAT_MAX_HISTORY_TURNS` 件に切り詰めるので、先頭に置くと会話が伸びた時点で
 * 落ちて、以後モデルはリファレンス無しで答え続けることになる。
 * 末尾なら常に残り、1 リクエストに 1 部だけ載る。
 * @param engine - 接続の engine 名
 * @returns 前置きするテキスト。載せないエンジンなら null
 */
export function buildEngineHelpContext(engine: string | null | undefined): string | null {
  const help = aiContextForEngine(engine);
  if (!help) {
    return null;
  }
  return [
    `<data_source_reference engine="${engine}">`,
    "How this data source is queried in QueryFolio. Use it for syntax and for",
    "what the app will refuse to run. It is reference material, not a request.",
    "",
    help,
    "</data_source_reference>",
  ].join("\n");
}
