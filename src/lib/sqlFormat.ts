// SQL formatter. Reproduces the user-specified style (2-space indent, shallow levels, major
// keywords at line start). It has no external dependencies such as @codemirror and is
// implemented with a small hand-written tokenizer (never breaking string literals or comments
// is the top priority).
//
// Design policy:
// - Only SELECT (and SELECTs joined by UNION/INTERSECT/EXCEPT) is formatted.
//   Anything else (INSERT/UPDATE/DELETE/WITH etc.) is returned as is.
// - Input that cannot be parsed, is unsupported, or contains line comments (-- or #) is returned as is.
// - Has a safety net: the formatted result is re-tokenized, and if its token sequence (ignoring
//   whitespace) does not match the input, the original is returned (missing, broken or
//   reordered tokens are detected and formatting is abandoned).

type TokType =
  | "ws"
  | "lineComment"
  | "blockComment"
  | "string"
  | "number"
  | "word"
  | "punct";

interface Token {
  type: TokType;
  text: string;
}

/**
 * Lexical differences per dialect. The only one so far is T-SQL (mssql):
 * - Keep a `[name]` bracketed identifier as one token (whitespace and symbols inside are part of
 *   the identifier; turning `[order-id]` into `[order - id]` would refer to a different object)
 * - Read `#temp` / `##global` temporary table names as identifiers (in other dialects
 *   `#` starts a MySQL-style line comment)
 * - Keep a `N'...'` Unicode string literal, prefix included, as one token
 */
export type SqlDialect = "mssql";

// Set of keywords that are uppercased for comparison and output. Function names (count/sum etc.)
// are intentionally excluded to preserve what the user wrote.
const KEYWORDS = new Set<string>([
  "SELECT",
  "DISTINCT",
  "ALL",
  "FROM",
  "WHERE",
  "GROUP",
  "BY",
  "HAVING",
  "ORDER",
  "LIMIT",
  "OFFSET",
  "UNION",
  "INTERSECT",
  "EXCEPT",
  "JOIN",
  "INNER",
  "LEFT",
  "RIGHT",
  "FULL",
  "OUTER",
  "CROSS",
  "NATURAL",
  "STRAIGHT_JOIN",
  "ON",
  "USING",
  "AS",
  "AND",
  "OR",
  "NOT",
  "IN",
  "IS",
  "NULL",
  "LIKE",
  "ILIKE",
  "BETWEEN",
  "EXISTS",
  "CASE",
  "WHEN",
  "THEN",
  "ELSE",
  "END",
  "ASC",
  "DESC",
  "TRUE",
  "FALSE",
]);

// Multi-character operators (try longer ones first)
const MULTI_PUNCT = [
  "->>",
  "->",
  "<=>",
  "<=",
  ">=",
  "<>",
  "!=",
  "||",
  "::",
  ":=",
  "<<",
  ">>",
];

// Characters allowed in identifiers. In addition to ASCII alphanumerics, _ and $, non-ASCII
// (U+0080 and above, e.g. Japanese aliases) is allowed. The range is checked explicitly via
// charCodeAt so that symbols do not leak into identifiers.
const isWordStart = (c: string): boolean =>
  /[A-Za-z_$]/.test(c) || c.charCodeAt(0) >= 0x80;
const isWordPart = (c: string): boolean =>
  /[A-Za-z0-9_$]/.test(c) || c.charCodeAt(0) >= 0x80;

// Splits the input string into a token sequence. Strings and comments are kept as a single
// token with their contents intact.
function tokenize(sql: string, dialect?: SqlDialect): Token[] {
  const tokens: Token[] = [];
  const n = sql.length;
  const mssql = dialect === "mssql";
  let i = 0;
  while (i < n) {
    const c = sql[i];

    // Whitespace
    if (c === " " || c === "\t" || c === "\r" || c === "\n" || c === "\f") {
      let j = i + 1;
      while (j < n && /\s/.test(sql[j])) j++;
      tokens.push({ type: "ws", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // Line comment (-- ...)
    if (c === "-" && sql[i + 1] === "-") {
      let j = i + 2;
      while (j < n && sql[j] !== "\n") j++;
      tokens.push({ type: "lineComment", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // Bracketed identifier ([name]) - T-SQL. `]]` is an escaped `]`. If it is not closed,
    // take everything to the end as one token (better to return the original than to break it)
    if (mssql && c === "[") {
      let j = i + 1;
      while (j < n) {
        if (sql[j] === "]") {
          if (sql[j + 1] === "]") {
            j += 2;
            continue;
          }
          j += 1;
          break;
        }
        j += 1;
      }
      tokens.push({ type: "string", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // Line comment (# ...) - MySQL. In T-SQL, # starts a temporary table name
    if (c === "#" && !mssql) {
      let j = i + 1;
      while (j < n && sql[j] !== "\n") j++;
      tokens.push({ type: "lineComment", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // Block comment (/* ... */)
    if (c === "/" && sql[i + 1] === "*") {
      let j = i + 2;
      while (j < n && !(sql[j] === "*" && sql[j + 1] === "/")) j++;
      j = Math.min(n, j + 2);
      tokens.push({ type: "blockComment", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // String / quoted identifier ( ' " ` )
    if (c === "'" || c === '"' || c === "`") {
      const quote = c;
      let j = i + 1;
      while (j < n) {
        const d = sql[j];
        // ' and " take backslash escapes into account (MySQL etc.)
        if (d === "\\" && (quote === "'" || quote === '"')) {
          j += 2;
          continue;
        }
        if (d === quote) {
          if (sql[j + 1] === quote) {
            // Escape by doubling the quote
            j += 2;
            continue;
          }
          j += 1;
          break;
        }
        j += 1;
      }
      tokens.push({ type: "string", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // Number (hex / decimal / exponent). Only checked when it starts with a digit or ".digit"
    // (a pre-guard so that we do not slice on every character).
    if (
      (c >= "0" && c <= "9") ||
      (c === "." && sql[i + 1] >= "0" && sql[i + 1] <= "9")
    ) {
      const numMatch =
        /^(0[xX][0-9a-fA-F]+|(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?)/.exec(
          sql.slice(i),
        );
      if (numMatch) {
        tokens.push({ type: "number", text: numMatch[0] });
        i += numMatch[0].length;
        continue;
      }
    }

    // Identifier / keyword (T-SQL #temp / ##global are identifiers too; the second # is
    // also read as part of the name)
    if (isWordStart(c) || (mssql && c === "#")) {
      let j = i + 1;
      while (j < n && (isWordPart(sql[j]) || (mssql && sql[j] === "#"))) j++;
      // A T-SQL Unicode string N'...' is one literal made of the prefix and the string.
      // As separate tokens, whitespace would end up between N and '...', turning it into a different expression
      if (mssql && j === i + 1 && (c === "N" || c === "n") && sql[j] === "'") {
        let k = j + 1;
        while (k < n) {
          if (sql[k] === "'") {
            if (sql[k + 1] === "'") {
              k += 2;
              continue;
            }
            k += 1;
            break;
          }
          k += 1;
        }
        tokens.push({ type: "string", text: sql.slice(i, k) });
        i = k;
        continue;
      }
      tokens.push({ type: "word", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // Multi-character symbols
    let matched = false;
    for (const op of MULTI_PUNCT) {
      if (sql.startsWith(op, i)) {
        tokens.push({ type: "punct", text: op });
        i += op.length;
        matched = true;
        break;
      }
    }
    if (matched) continue;

    // Single-character symbols
    tokens.push({ type: "punct", text: c });
    i += 1;
  }
  return tokens;
}

const isWord = (t: Token | undefined): boolean => !!t && t.type === "word";
const up = (t: Token | undefined): string =>
  t && t.type === "word" ? t.text.toUpperCase() : "";
const wordUpAt = (toks: Token[], i: number): string =>
  isWord(toks[i]) ? toks[i].text.toUpperCase() : "";

// Display text of a word token. Keywords are uppercased; others keep the original.
function displayText(t: Token): string {
  if (t.type === "word" && KEYWORDS.has(t.text.toUpperCase())) {
    return t.text.toUpperCase();
  }
  return t.text;
}

// Decides whether whitespace should go between two tokens.
function needSpace(prev: Token, cur: Token): boolean {
  const p = prev.text;
  const c = cur.text;

  // No space right after (based on the previous token)
  if (p === "(" || p === "[" || p === ".") return false;
  if (p === "::") return false;

  // No space right before (based on the next token)
  if (c === "," || c === ";" || c === ")" || c === "]") return false;
  if (c === "." || c === "::") return false;

  // The ( of a function call sticks to the identifier (count(*) etc.).
  // A ( after a reserved word gets a space (IN (...), VALUES (...) etc.).
  if (c === "(") {
    if (prev.type === "word" && !KEYWORDS.has(p.toUpperCase())) return false;
    return true;
  }

  return true;
}

// Formats the token sequence onto one line (uppercase keywords, adjust whitespace).
function renderInline(tokens: Token[]): string {
  let out = "";
  let prev: Token | null = null;
  for (const t of tokens) {
    if (prev && needSpace(prev, t)) out += " ";
    out += displayText(t);
    prev = t;
  }
  return out;
}

interface ClauseInfo {
  name: string;
  wordCount: number;
}

// If toks[i] is a keyword that starts a new clause, return its info.
function matchClauseStarter(toks: Token[], i: number): ClauseInfo | null {
  if (!isWord(toks[i])) return null;
  const w = toks[i].text.toUpperCase();
  const w2 = wordUpAt(toks, i + 1);
  switch (w) {
    case "SELECT":
      return { name: "SELECT", wordCount: w2 === "DISTINCT" || w2 === "ALL" ? 2 : 1 };
    case "FROM":
      return { name: "FROM", wordCount: 1 };
    case "WHERE":
      return { name: "WHERE", wordCount: 1 };
    case "GROUP":
      return w2 === "BY" ? { name: "GROUP BY", wordCount: 2 } : null;
    case "ORDER":
      return w2 === "BY" ? { name: "ORDER BY", wordCount: 2 } : null;
    case "HAVING":
      return { name: "HAVING", wordCount: 1 };
    case "LIMIT":
      return { name: "LIMIT", wordCount: 1 };
    case "OFFSET":
      return { name: "OFFSET", wordCount: 1 };
    case "UNION":
      return {
        name: "UNION",
        wordCount: w2 === "ALL" || w2 === "DISTINCT" ? 2 : 1,
      };
    case "INTERSECT":
      return {
        name: "INTERSECT",
        wordCount: w2 === "ALL" || w2 === "DISTINCT" ? 2 : 1,
      };
    case "EXCEPT":
      return {
        name: "EXCEPT",
        wordCount: w2 === "ALL" || w2 === "DISTINCT" ? 2 : 1,
      };
    default:
      return null;
  }
}

// Number of tokens in a JOIN keyword phrase (LEFT OUTER JOIN etc.) starting at toks[k].
// Returns 0 if it does not end in JOIN (not a JOIN clause).
function matchJoin(toks: Token[], k: number): number {
  if (wordUpAt(toks, k) === "STRAIGHT_JOIN") return 1;
  const mods = new Set([
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "NATURAL",
  ]);
  let j = k;
  let count = 0;
  while (isWord(toks[j]) && mods.has(toks[j].text.toUpperCase())) {
    j++;
    count++;
  }
  if (wordUpAt(toks, j) === "JOIN") return count + 1;
  return 0;
}

interface Clause {
  name: string;
  headerTokens: Token[];
  body: Token[];
}

// Splits the token sequence into clauses (SELECT / FROM / WHERE ...).
function segment(toks: Token[]): Clause[] | null {
  const clauses: Clause[] = [];
  let i = 0;
  while (i < toks.length) {
    const cl = matchClauseStarter(toks, i);
    if (!cl) return null;
    const headerTokens = toks.slice(i, i + cl.wordCount);
    i += cl.wordCount;
    const body: Token[] = [];
    let depth = 0;
    while (i < toks.length) {
      const t = toks[i];
      if (depth === 0 && t.type === "word" && matchClauseStarter(toks, i)) {
        break;
      }
      if (t.text === "(") depth++;
      else if (t.text === ")") depth--;
      body.push(t);
      i++;
    }
    clauses.push({ name: cl.name, headerTokens, body });
  }
  return clauses;
}

// Split at top-level commas and output each element on its own line with a 2-space indent
// and a trailing comma (for SELECT / GROUP BY / ORDER BY).
function renderCommaList(body: Token[]): string {
  if (body.length === 0) return "";
  const items: Token[][] = [];
  let cur: Token[] = [];
  let depth = 0;
  for (const t of body) {
    if (t.text === "(") depth++;
    else if (t.text === ")") depth--;
    if (depth === 0 && t.text === ",") {
      items.push(cur);
      cur = [];
      continue;
    }
    cur.push(t);
  }
  items.push(cur);
  return items
    .map(
      (it, idx) => "  " + renderInline(it) + (idx < items.length - 1 ? "," : ""),
    )
    .join("\n");
}

// Formats the body of a FROM clause. Table references, JOINs and ONs are each output on
// their own line with a 2-space indent (the levels are not made deeper).
function renderFrom(body: Token[]): string {
  const lines: string[] = [];
  let buf: Token[] = [];
  let depth = 0;
  const flush = () => {
    if (buf.length) {
      lines.push("  " + renderInline(buf));
      buf = [];
    }
  };
  let p = 0;
  while (p < body.length) {
    const t = body[p];
    if (depth === 0) {
      const jn = matchJoin(body, p);
      if (jn > 0) {
        flush();
        for (let q = 0; q < jn; q++) buf.push(body[p + q]);
        p += jn;
        continue;
      }
      const u = t.type === "word" ? t.text.toUpperCase() : "";
      if (u === "ON" || u === "USING") {
        flush();
        buf.push(t);
        p++;
        continue;
      }
      if (t.text === ",") {
        buf.push(t);
        flush();
        p++;
        continue;
      }
    }
    if (t.text === "(") depth++;
    else if (t.text === ")") depth--;
    buf.push(t);
    p++;
  }
  flush();
  return lines.join("\n");
}

// Formats the body of WHERE / HAVING. Breaks the line at top-level AND / OR and outputs
// each condition with a 2-space indent. The AND in BETWEEN ... AND ... and AND/OR
// inside CASE ... END are not split.
function renderCondition(body: Token[]): string {
  const lines: string[] = [];
  let buf: Token[] = [];
  let depth = 0;
  let caseDepth = 0;
  let pendingBetween = 0;
  const flush = () => {
    if (buf.length) {
      lines.push("  " + renderInline(buf));
      buf = [];
    }
  };
  for (const t of body) {
    if (t.text === "(") depth++;
    else if (t.text === ")") depth--;
    if (depth === 0 && t.type === "word") {
      const u = t.text.toUpperCase();
      if (u === "CASE") {
        caseDepth++;
      } else if (u === "END" && caseDepth > 0) {
        caseDepth--;
      } else if (u === "BETWEEN") {
        pendingBetween++;
      } else if ((u === "AND" || u === "OR") && caseDepth === 0) {
        if (u === "AND" && pendingBetween > 0) {
          // This is the AND of BETWEEN ... AND ..., so do not split
          pendingBetween--;
        } else {
          flush();
          buf.push(t);
          continue;
        }
      }
    }
    buf.push(t);
  }
  flush();
  return lines.join("\n");
}

// Formats one clause into a string.
function renderClause(cl: Clause): string {
  const header = renderInline(cl.headerTokens);
  switch (cl.name) {
    case "SELECT":
    case "GROUP BY":
    case "ORDER BY": {
      const b = renderCommaList(cl.body);
      return b ? header + "\n" + b : header;
    }
    case "FROM": {
      const b = renderFrom(cl.body);
      return b ? header + "\n" + b : header;
    }
    case "WHERE":
    case "HAVING": {
      const b = renderCondition(cl.body);
      return b ? header + "\n" + b : header;
    }
    case "LIMIT":
    case "OFFSET": {
      const b = renderInline(cl.body);
      return b ? header + " " + b : header;
    }
    default: {
      // UNION / INTERSECT / EXCEPT usually have an empty body (a SELECT follows right after)
      const b = renderInline(cl.body);
      return b ? header + "\n  " + b : header;
    }
  }
}

// Converts the whitespace-free token sequence into an array of keys for comparison.
// A word ignores case; anything else (string, comment, number, symbol) is compared by
// exact match.
function signature(sql: string, dialect?: SqlDialect): string[] {
  return tokenize(sql, dialect)
    .filter((t) => t.type !== "ws")
    .map((t) => (t.type === "word" ? t.text.toLowerCase() : t.text));
}

function sameTokens(a: string, b: string, dialect?: SqlDialect): boolean {
  const sa = signature(a, dialect);
  const sb = signature(b, dialect);
  if (sa.length !== sb.length) return false;
  for (let i = 0; i < sa.length; i++) {
    if (sa[i] !== sb[i]) return false;
  }
  return true;
}

// The core of formatting. Returns null if it cannot be formatted.
function tryFormat(sql: string, dialect?: SqlDialect): string | null {
  let toks = tokenize(sql, dialect).filter((t) => t.type !== "ws");
  if (toks.length === 0) return null;

  // Do not format when line comments are present, for safety (to avoid code being
  // commented out by a broken layout)
  if (toks.some((t) => t.type === "lineComment")) return null;

  // Leading and trailing block comments are set aside and kept as is before and after
  const preamble: Token[] = [];
  while (toks.length && toks[0].type === "blockComment") {
    preamble.push(toks.shift() as Token);
  }
  const postamble: Token[] = [];
  while (toks.length && toks[toks.length - 1].type === "blockComment") {
    postamble.unshift(toks.pop() as Token);
  }

  // Trailing semicolon
  let semi = false;
  if (toks.length && toks[toks.length - 1].text === ";") {
    semi = true;
    toks.pop();
  }
  if (toks.length === 0) return null;

  // Do not format multiple statements (a top-level ; in the middle)
  {
    let d = 0;
    for (const t of toks) {
      if (t.text === "(") d++;
      else if (t.text === ")") d--;
      else if (t.text === ";" && d === 0) return null;
    }
  }

  // Only SELECT is supported at the start (WITH / INSERT / UPDATE / DELETE are unsupported)
  if (up(toks[0]) !== "SELECT") return null;

  const clauses = segment(toks);
  if (!clauses) return null;

  let out = clauses.map(renderClause).join("\n");
  if (semi) out += ";";

  const pre = preamble.map((t) => t.text).join("\n");
  const post = postamble.map((t) => t.text).join("\n");
  if (pre) out = pre + "\n" + out;
  if (post) out = out + "\n" + post;
  return out;
}

// Formats an SQL string and returns it. Returns the original if it cannot be formatted or might be broken.
// dialect only covers lexical differences (see SqlDialect). When omitted: standard SQL + MySQL style.
export function formatSql(sql: string, dialect?: SqlDialect): string {
  try {
    const formatted = tryFormat(sql, dialect);
    if (formatted === null) return sql;
    // Safety net: if the token sequence has changed, discard the formatting and return the original
    if (!sameTokens(sql, formatted, dialect)) return sql;
    return formatted;
  } catch {
    return sql;
  }
}
