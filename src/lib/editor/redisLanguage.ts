import { StreamLanguage } from "@codemirror/language";

/// Simple syntax highlighting for the Redis command editor.
/// Assumes 1 line = 1 command; colors the leading command name (keyword if known),
/// subcommands, strings, numbers and comments (#).
/// The grammar is too simple to justify a lezer grammar, so this uses StreamLanguage.

/// Known command names (uppercase). This is only for highlighting, so it need not be exhaustive
/// (unknown commands can still be executed; independent of the backend's readonly check).
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

/// Commands whose second word, a subcommand (CONFIG GET / CLIENT LIST etc.), after the leading
/// command is also treated as a keyword.
const SUBCOMMAND_PARENTS = new Set([
  "CONFIG", "CLIENT", "OBJECT", "MEMORY", "XINFO", "COMMAND", "SLOWLOG",
  "DEBUG",
]);

interface RedisStreamState {
  /// How many tokens have been read on the current line (to detect the line start)
  tokenIndex: number;
  /// Whether the leading command takes a subcommand
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
    // Comment line (starts with #). The backend's parse_input skips them by the same rule
    if (state.tokenIndex === 0 && stream.peek() === "#") {
      stream.skipToEnd();
      return "comment";
    }
    // String ("..." / '...'). If not closed within the line, it runs to the end of the line
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
    // Normal token (up to whitespace)
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
