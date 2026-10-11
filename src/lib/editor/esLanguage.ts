import { StreamLanguage } from "@codemirror/language";

/// Simple syntax highlighting for the Elasticsearch (Kibana Console style) editor.
/// - An HTTP method at the start of a line (GET/POST/PUT/DELETE/HEAD/PATCH) -> keyword,
///   and the rest of the same line (the path) -> string
/// - A line starting with `#` -> comment (same rule as the backend's parse_input)
/// - Any other line is treated as a JSON body, coloring strings / property names / numbers /
///   true, false, null / brackets
/// There is not enough structure to justify writing a lezer grammar, so this is implemented with StreamLanguage.

const METHOD_RE = /^(GET|POST|PUT|DELETE|HEAD|PATCH)(?=\s|$)/i;

interface EsStreamState {
  /// How many tokens have been read on the current line (for detecting the line start)
  tokenIndex: number;
  /// The current line is a method line and the rest (the path) has not been read yet
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
    // Comment line (starts with #)
    if (state.tokenIndex === 0 && stream.peek() === "#") {
      stream.skipToEnd();
      return "comment";
    }
    // Method line: if the first token is an HTTP method it is a keyword, and the rest is the path
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
      // If ":" follows immediately (possibly after whitespace), it is a property name
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
