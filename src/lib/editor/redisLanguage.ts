import { StreamLanguage } from "@codemirror/language";

/// Redis コマンドエディタの簡易シンタックスハイライト。
/// 1 行 = 1 コマンドの前提で、行頭のコマンド名 (既知なら keyword) と
/// サブコマンド・文字列・数値・コメント (#) を色分けする。
/// lezer 文法を書くほどの構造は無いため StreamLanguage で実装する。

/// 既知のコマンド名 (大文字)。ハイライト用なので網羅でなくてよい
/// (未知のコマンドも実行自体はできる。バックエンドの readonly 判定とは独立)。
const REDIS_COMMANDS = new Set([
  // string / generic
  "GET", "SET", "SETNX", "SETEX", "PSETEX", "MGET", "MSET", "MSETNX", "APPEND",
  "STRLEN", "GETRANGE", "SETRANGE", "SUBSTR", "GETSET", "GETDEL", "GETEX",
  "INCR", "INCRBY", "INCRBYFLOAT", "DECR", "DECRBY",
  "DEL", "UNLINK", "EXISTS", "TYPE", "RENAME", "RENAMENX", "COPY", "TOUCH",
  "EXPIRE", "PEXPIRE", "EXPIREAT", "PEXPIREAT", "PERSIST", "TTL", "PTTL",
  "EXPIRETIME", "PEXPIRETIME", "KEYS", "SCAN", "RANDOMKEY", "DBSIZE", "DUMP",
  "RESTORE", "OBJECT", "MEMORY", "SORT", "SORT_RO",
  // hash
  "HGET", "HSET", "HSETNX", "HMGET", "HMSET", "HGETALL", "HDEL", "HKEYS",
  "HVALS", "HLEN", "HEXISTS", "HSTRLEN", "HRANDFIELD", "HSCAN", "HINCRBY",
  "HINCRBYFLOAT",
  // list
  "LPUSH", "RPUSH", "LPUSHX", "RPUSHX", "LPOP", "RPOP", "LRANGE", "LLEN",
  "LINDEX", "LSET", "LINSERT", "LREM", "LTRIM", "LPOS", "LMOVE", "RPOPLPUSH",
  "BLPOP", "BRPOP", "BLMOVE",
  // set
  "SADD", "SREM", "SMEMBERS", "SCARD", "SISMEMBER", "SMISMEMBER", "SPOP",
  "SRANDMEMBER", "SMOVE", "SSCAN", "SINTER", "SUNION", "SDIFF", "SINTERSTORE",
  "SUNIONSTORE", "SDIFFSTORE", "SINTERCARD",
  // sorted set
  "ZADD", "ZREM", "ZRANGE", "ZRANGEBYSCORE", "ZRANGEBYLEX", "ZREVRANGE",
  "ZREVRANGEBYSCORE", "ZCARD", "ZCOUNT", "ZSCORE", "ZMSCORE", "ZRANK",
  "ZREVRANK", "ZINCRBY", "ZSCAN", "ZPOPMIN", "ZPOPMAX", "ZRANDMEMBER",
  "ZLEXCOUNT", "ZRANGESTORE", "ZREMRANGEBYSCORE", "ZREMRANGEBYRANK",
  "ZREMRANGEBYLEX",
  // stream
  "XADD", "XRANGE", "XREVRANGE", "XLEN", "XREAD", "XINFO", "XDEL", "XTRIM",
  // bitmap / hyperloglog / geo
  "SETBIT", "GETBIT", "BITCOUNT", "BITPOS", "BITOP", "BITFIELD", "BITFIELD_RO",
  "PFADD", "PFCOUNT", "PFMERGE",
  "GEOADD", "GEOPOS", "GEODIST", "GEOSEARCH", "GEOHASH",
  // transaction / script
  "MULTI", "EXEC", "DISCARD", "WATCH", "UNWATCH", "EVAL", "EVALSHA",
  // server
  "INFO", "PING", "ECHO", "TIME", "LASTSAVE", "COMMAND", "CONFIG", "CLIENT",
  "SELECT", "FLUSHDB", "FLUSHALL", "SHUTDOWN", "DEBUG", "SLOWLOG", "MONITOR",
  "SUBSCRIBE", "UNSUBSCRIBE", "PSUBSCRIBE", "PUNSUBSCRIBE", "PUBLISH",
  "LOLWUT", "WAIT",
]);

/// 行頭コマンドに続く 2 語目のサブコマンド (CONFIG GET / CLIENT LIST 等) も
/// キーワード扱いにするコマンド。
const SUBCOMMAND_PARENTS = new Set([
  "CONFIG", "CLIENT", "OBJECT", "MEMORY", "XINFO", "COMMAND", "SLOWLOG",
  "DEBUG",
]);

interface RedisStreamState {
  /// 現在の行でトークンをいくつ読んだか (行頭判定用)
  tokenIndex: number;
  /// 行頭コマンドがサブコマンドを取るものだったか
  expectSubcommand: boolean;
}

export const redisLanguage = StreamLanguage.define<RedisStreamState>({
  name: "redis",
  startState: () => ({ tokenIndex: 0, expectSubcommand: false }),
  token(stream, state) {
    if (stream.sol()) {
      state.tokenIndex = 0;
      state.expectSubcommand = false;
    }
    if (stream.eatSpace()) {
      return null;
    }
    // コメント行 (# 始まり)。バックエンドの parse_input も同じ規則でスキップする
    if (state.tokenIndex === 0 && stream.peek() === "#") {
      stream.skipToEnd();
      return "comment";
    }
    // 文字列 ("..." / '...')。行内で閉じない場合は行末まで文字列扱い
    const quote = stream.peek();
    if (quote === '"' || quote === "'") {
      stream.next();
      let escaped = false;
      while (!stream.eol()) {
        const ch = stream.next();
        if (escaped) {
          escaped = false;
        } else if (ch === "\\") {
          escaped = true;
        } else if (ch === quote) {
          break;
        }
      }
      state.tokenIndex++;
      return "string";
    }
    // 通常トークン (空白まで)
    let word = "";
    while (!stream.eol() && !/\s/.test(stream.peek() ?? " ")) {
      word += stream.next();
    }
    const index = state.tokenIndex++;
    if (index === 0) {
      const upper = word.toUpperCase();
      state.expectSubcommand = SUBCOMMAND_PARENTS.has(upper);
      return REDIS_COMMANDS.has(upper) ? "keyword" : "name";
    }
    if (index === 1 && state.expectSubcommand && /^[A-Za-z_-]+$/.test(word)) {
      return "keyword";
    }
    if (/^-?\d+(\.\d+)?$/.test(word)) {
      return "number";
    }
    return "name";
  },
  languageData: {
    commentTokens: { line: "#" },
  },
});
