import { StreamLanguage } from "@codemirror/language";

/// Elasticsearch (Kibana Console 風) エディタの簡易シンタックスハイライト。
/// - 行頭の HTTP メソッド (GET/POST/PUT/DELETE/HEAD/PATCH) → keyword、
///   同じ行の残り (パス) → string
/// - `#` 始まりの行 → comment (バックエンドの parse_input と同じ規則)
/// - それ以外の行は JSON body として文字列 / プロパティ名 / 数値 /
///   true・false・null / 括弧を色分けする
/// lezer 文法を書くほどの構造は無いため StreamLanguage で実装する。

const METHOD_RE = /^(GET|POST|PUT|DELETE|HEAD|PATCH)(?=\s|$)/i;

interface EsStreamState {
  /// 現在の行でトークンをいくつ読んだか (行頭判定用)
  tokenIndex: number;
  /// 現在の行がメソッド行で、残り (パス) をまだ読んでいない
  inMethodLine: boolean;
}

export const esLanguage = StreamLanguage.define<EsStreamState>({
  name: "es",
  startState: () => ({ tokenIndex: 0, inMethodLine: false }),
  token(stream, state) {
    if (stream.sol()) {
      state.tokenIndex = 0;
      state.inMethodLine = false;
    }
    if (stream.eatSpace()) {
      return null;
    }
    // コメント行 (# 始まり)
    if (state.tokenIndex === 0 && stream.peek() === "#") {
      stream.skipToEnd();
      return "comment";
    }
    // メソッド行: 行頭トークンが HTTP メソッドなら keyword、残りはパス
    if (state.tokenIndex === 0 && stream.match(METHOD_RE)) {
      state.tokenIndex++;
      state.inMethodLine = true;
      return "keyword";
    }
    if (state.inMethodLine) {
      stream.skipToEnd();
      return "string";
    }
    state.tokenIndex++;
    // JSON body
    const ch = stream.peek();
    if (ch === '"') {
      stream.next();
      let escaped = false;
      while (!stream.eol()) {
        const c = stream.next();
        if (escaped) {
          escaped = false;
        } else if (c === "\\") {
          escaped = true;
        } else if (c === '"') {
          break;
        }
      }
      // 直後 (空白を挟んで) に ":" が続くならプロパティ名
      return stream.match(/^\s*:/, false) ? "propertyName" : "string";
    }
    if (stream.match(/^-?\d+(\.\d+)?([eE][+-]?\d+)?/)) {
      return "number";
    }
    if (stream.match(/^(true|false)(?![\w$])/)) {
      return "bool";
    }
    if (stream.match(/^null(?![\w$])/)) {
      return "null";
    }
    if (ch && "{}[]:,".includes(ch)) {
      stream.next();
      return "punctuation";
    }
    stream.next();
    return "name";
  },
  languageData: {
    commentTokens: { line: "#" },
  },
});
