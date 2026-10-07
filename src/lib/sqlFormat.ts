// SQL 整形器。ユーザー指定のスタイル (2sp インデント・浅い段・主要
// キーワード行頭) を再現する。@codemirror などの外部依存は持たず、
// 自前の軽量トークナイザで実装する (文字列リテラル・コメントを壊さない
// ことが最優先)。
//
// 設計方針:
// - SELECT (および UNION/INTERSECT/EXCEPT で連結された SELECT) のみを
//   整形する。それ以外 (INSERT/UPDATE/DELETE/WITH 等) は原文のまま返す。
// - パース不能・未対応・行コメント (-- や #) を含む場合は原文のまま返す。
// - 整形結果を再トークナイズし、入力と (空白を除いた) トークン列が一致
//   しない場合は原文を返す安全ネットを持つ (トークンの欠落・破壊・
//   並び替えを検出して整形をあきらめる)。

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
 * 方言ごとの字句の違い。今あるのは T-SQL (mssql) だけ:
 * - `[name]` の角括弧識別子を 1 トークンとして保つ (中の空白・記号は識別子の
 *   一部で、`[order-id]` を `[order - id]` にすると別のオブジェクトになる)
 * - `#temp` / `##global` の一時テーブル名を識別子として読む (他の方言では
 *   `#` は MySQL 風の行コメントの始まり)
 * - `N'...'` の Unicode 文字列定数を接頭辞ごと 1 トークンとして保つ
 */
export type SqlDialect = "mssql";

// 大文字化して比較・出力するキーワード集合。関数名 (count/sum 等) は
// ユーザーの記述を保つため意図的に含めない。
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

// 複数文字の演算子 (長いものを先に試す)
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

// 識別子に使える文字。ASCII の英数字・_・$ に加え、非 ASCII
// (U+0080 以上、日本語エイリアス等) を許可する。記号類が識別子に
// 混入しないよう、範囲は charCodeAt で明示的に判定する。
const isWordStart = (c: string): boolean =>
  /[A-Za-z_$]/.test(c) || c.charCodeAt(0) >= 0x80;
const isWordPart = (c: string): boolean =>
  /[A-Za-z0-9_$]/.test(c) || c.charCodeAt(0) >= 0x80;

// 入力文字列をトークン列へ分解する。文字列・コメントは 1 トークンとして
// 中身をそのまま保持する。
function tokenize(sql: string, dialect?: SqlDialect): Token[] {
  const tokens: Token[] = [];
  const n = sql.length;
  const mssql = dialect === "mssql";
  let i = 0;
  while (i < n) {
    const c = sql[i];

    // 空白
    if (c === " " || c === "\t" || c === "\r" || c === "\n" || c === "\f") {
      let j = i + 1;
      while (j < n && /\s/.test(sql[j])) j++;
      tokens.push({ type: "ws", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // 行コメント (-- ...)
    if (c === "-" && sql[i + 1] === "-") {
      let j = i + 2;
      while (j < n && sql[j] !== "\n") j++;
      tokens.push({ type: "lineComment", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // 角括弧識別子 ([name]) — T-SQL。`]]` は `]` のエスケープ。閉じていない
    // 場合は末尾まで 1 トークンにする (壊すより原文のまま返す側に倒す)
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

    // 行コメント (# ...) — MySQL。T-SQL では # は一時テーブル名の先頭
    if (c === "#" && !mssql) {
      let j = i + 1;
      while (j < n && sql[j] !== "\n") j++;
      tokens.push({ type: "lineComment", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // ブロックコメント (/* ... */)
    if (c === "/" && sql[i + 1] === "*") {
      let j = i + 2;
      while (j < n && !(sql[j] === "*" && sql[j + 1] === "/")) j++;
      j = Math.min(n, j + 2);
      tokens.push({ type: "blockComment", text: sql.slice(i, j) });
      i = j;
      continue;
    }

    // 文字列 / クォート付き識別子 ( ' " ` )
    if (c === "'" || c === '"' || c === "`") {
      const quote = c;
      let j = i + 1;
      while (j < n) {
        const d = sql[j];
        // ' と " はバックスラッシュエスケープを考慮 (MySQL 等)
        if (d === "\\" && (quote === "'" || quote === '"')) {
          j += 2;
          continue;
        }
        if (d === quote) {
          if (sql[j + 1] === quote) {
            // クォートの二重化によるエスケープ
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

    // 数値 (16進 / 小数 / 指数表記)。先頭が数字か「.数字」のときだけ判定
    // する (全文字で slice しないようにするための前置ガード)。
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

    // 識別子・キーワード (T-SQL の #temp / ##global も識別子。2 つ目の # も
    // 名前の一部として読む)
    if (isWordStart(c) || (mssql && c === "#")) {
      let j = i + 1;
      while (j < n && (isWordPart(sql[j]) || (mssql && sql[j] === "#"))) j++;
      // T-SQL の Unicode 文字列 N'...' は接頭辞と文字列で 1 つの定数。
      // 別トークンにすると N と '...' の間に空白が入り、別の式に変わる
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

    // 複数文字の記号
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

    // 単一文字の記号
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

// word トークンの表示テキスト。キーワードは大文字化、それ以外は原文維持。
function displayText(t: Token): string {
  if (t.type === "word" && KEYWORDS.has(t.text.toUpperCase())) {
    return t.text.toUpperCase();
  }
  return t.text;
}

// 2 つのトークン間に空白を入れるべきか判定する。
function needSpace(prev: Token, cur: Token): boolean {
  const p = prev.text;
  const c = cur.text;

  // 直後に空白を入れない (前トークン基準)
  if (p === "(" || p === "[" || p === ".") return false;
  if (p === "::") return false;

  // 直前に空白を入れない (後トークン基準)
  if (c === "," || c === ";" || c === ")" || c === "]") return false;
  if (c === "." || c === "::") return false;

  // 関数呼び出しの ( は識別子に密着させる (count(*) 等)。
  // 予約語の後の ( は空白を空ける (IN (...), VALUES (...) 等)。
  if (c === "(") {
    if (prev.type === "word" && !KEYWORDS.has(p.toUpperCase())) return false;
    return true;
  }

  return true;
}

// トークン列を 1 行に整形して返す (キーワード大文字化・空白調整)。
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

// toks[i] が新しい句を開始するキーワードなら情報を返す。
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

// toks[k] から始まる JOIN キーワード句 (LEFT OUTER JOIN 等) のトークン数を
// 返す。JOIN で終わらない場合は 0 (JOIN 句ではない)。
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

// トークン列を句 (SELECT / FROM / WHERE ...) 単位に分割する。
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

// トップレベルのカンマで区切り、各要素を 2sp インデントで 1 行ずつ、
// 行末カンマで出力する (SELECT / GROUP BY / ORDER BY 用)。
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

// FROM 句の本体を整形する。テーブル参照・JOIN・ON をそれぞれ 2sp
// インデントで 1 行ずつ出力する (段は深くしない)。
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

// WHERE / HAVING の本体を整形する。トップレベルの AND / OR で改行し、
// 各条件を 2sp インデントで出力する。BETWEEN ... AND ... の AND や
// CASE ... END 内の AND/OR は分割しない。
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
          // BETWEEN ... AND ... の AND なので分割しない
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

// 1 つの句を整形して文字列にする。
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
      // UNION / INTERSECT / EXCEPT は通常 body が空 (直後に SELECT が続く)
      const b = renderInline(cl.body);
      return b ? header + "\n  " + b : header;
    }
  }
}

// 空白を除いたトークン列を比較用のキー配列に変換する。
// word は大文字小文字を無視し、それ以外 (文字列・コメント・数値・記号) は
// 完全一致で比較する。
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

// 整形の本体。整形できない場合は null を返す。
function tryFormat(sql: string, dialect?: SqlDialect): string | null {
  let toks = tokenize(sql, dialect).filter((t) => t.type !== "ws");
  if (toks.length === 0) return null;

  // 行コメントを含む場合は安全のため整形しない (レイアウト崩しでコードを
  // コメントアウトしてしまう事故を避ける)
  if (toks.some((t) => t.type === "lineComment")) return null;

  // 先頭・末尾のブロックコメントは前後にそのまま退避する
  const preamble: Token[] = [];
  while (toks.length && toks[0].type === "blockComment") {
    preamble.push(toks.shift() as Token);
  }
  const postamble: Token[] = [];
  while (toks.length && toks[toks.length - 1].type === "blockComment") {
    postamble.unshift(toks.pop() as Token);
  }

  // 末尾のセミコロン
  let semi = false;
  if (toks.length && toks[toks.length - 1].text === ";") {
    semi = true;
    toks.pop();
  }
  if (toks.length === 0) return null;

  // 複数ステートメント (トップレベルの ; が途中にある) は整形しない
  {
    let d = 0;
    for (const t of toks) {
      if (t.text === "(") d++;
      else if (t.text === ")") d--;
      else if (t.text === ";" && d === 0) return null;
    }
  }

  // 先頭は SELECT のみ対応する (WITH / INSERT / UPDATE / DELETE は非対応)
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

// SQL 文字列を整形して返す。整形できない・壊す恐れがある場合は原文を返す。
// dialect は字句の違いだけ (SqlDialect 参照)。省略時は標準 SQL + MySQL 風。
export function formatSql(sql: string, dialect?: SqlDialect): string {
  try {
    const formatted = tryFormat(sql, dialect);
    if (formatted === null) return sql;
    // 安全ネット: トークン列が変化していたら整形を破棄して原文を返す
    if (!sameTokens(sql, formatted, dialect)) return sql;
    return formatted;
  } catch {
    return sql;
  }
}
