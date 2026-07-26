/// AI の応答 (Markdown) を表示するための最小限の分割ユーティリティ。
/// 完全な Markdown レンダラは持たず、``` フェンスでコードブロックと
/// テキストに分けるだけ (AiAnalysisModal / ChatPane で共用)。

export interface MarkdownSegment {
  type: "text" | "code";
  content: string;
}

/// ``` フェンスでコードブロックとテキストに分割する。
/// split の偶数番目がテキスト、奇数番目がコード (閉じフェンスが無い
/// 末尾の区間もコードとして表示する)。
export function splitMarkdownSegments(text: string): MarkdownSegment[] {
  const result: MarkdownSegment[] = [];
  text.split("```").forEach((part, i) => {
    if (i % 2 === 0) {
      if (part.trim()) {
        result.push({ type: "text", content: part.trim() });
      }
      return;
    }
    // コードブロック先頭行の言語タグ (sql 等) を取り除く
    const newline = part.indexOf("\n");
    const firstLine = newline >= 0 ? part.slice(0, newline).trim() : "";
    const content =
      newline >= 0 && /^[\w-]*$/.test(firstLine)
        ? part.slice(newline + 1)
        : part;
    result.push({ type: "code", content: content.replace(/\s+$/, "") });
  });
  return result;
}
