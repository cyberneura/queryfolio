/// ヘルプ本文 (Markdown) を表示用のブロックへ分解する。
///
/// 完全な Markdown レンダラは持ち込まない。ここで扱うのは自分たちで書いた
/// ヘルプだけなので、実際に使っている記法 (見出し / コードフェンス / 箇条書き /
/// 段落) に絞る。HTML を組み立てず構造だけ返すので、表示側は Svelte の
/// マークアップで描ける (innerHTML を使わずに済む)。

export type HelpBlock =
  | { type: "heading"; level: 1 | 2; text: string }
  | { type: "code"; content: string }
  | { type: "list"; items: string[] }
  | { type: "paragraph"; text: string };

/**
 * ヘルプ本文をブロック列に分解する。
 * @param markdown - ヘルプの Markdown
 * @returns 表示用ブロックの配列
 */
export function parseHelpDoc(markdown: string): HelpBlock[] {
  const blocks: HelpBlock[] = [];
  const lines = markdown.split("\n");

  let paragraph: string[] = [];
  let list: string[] = [];

  const flushParagraph = () => {
    if (paragraph.length > 0) {
      blocks.push({ type: "paragraph", text: paragraph.join(" ").trim() });
      paragraph = [];
    }
  };
  const flushList = () => {
    if (list.length > 0) {
      blocks.push({ type: "list", items: list });
      list = [];
    }
  };
  const flushAll = () => {
    flushParagraph();
    flushList();
  };

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];

    // コードフェンス。閉じフェンスが無いまま終わっても、そこまでを 1 ブロックにする
    if (line.startsWith("```")) {
      flushAll();
      const content: string[] = [];
      i += 1;
      while (i < lines.length && !lines[i].startsWith("```")) {
        content.push(lines[i]);
        i += 1;
      }
      blocks.push({ type: "code", content: content.join("\n").replace(/\s+$/, "") });
      continue;
    }

    const heading = /^(#{1,2})\s+(.*)$/.exec(line);
    if (heading) {
      flushAll();
      blocks.push({
        type: "heading",
        level: heading[1].length as 1 | 2,
        text: heading[2].trim(),
      });
      continue;
    }

    const bullet = /^[-*]\s+(.*)$/.exec(line);
    if (bullet) {
      flushParagraph();
      list.push(bullet[1].trim());
      continue;
    }

    if (line.trim() === "") {
      flushAll();
      continue;
    }

    // 箇条書きの継続行 (インデントされた折り返し) は直前の項目に足す
    if (list.length > 0 && /^\s+\S/.test(line)) {
      list[list.length - 1] = `${list[list.length - 1]} ${line.trim()}`;
      continue;
    }

    flushList();
    paragraph.push(line.trim());
  }

  flushAll();
  return blocks;
}

/// インライン記法のうち、ヘルプで実際に使うのは `code` と **strong** の 2 つ。
/// これも HTML にせず、描画側が繰り返せる断片の列にして返す。
export interface InlineSpan {
  type: "text" | "code" | "strong";
  text: string;
}

/**
 * 段落・箇条書きの 1 行をインライン断片へ分解する。
 * @param text - 対象の 1 行
 * @returns インライン断片の配列
 */
export function parseInline(text: string): InlineSpan[] {
  const spans: InlineSpan[] = [];
  // `code` を先に切る (** の中に ` が来ることは想定しない)
  const pattern = /`([^`]+)`|\*\*([^*]+)\*\*/g;
  let last = 0;
  let match: RegExpExecArray | null;
  while ((match = pattern.exec(text)) !== null) {
    if (match.index > last) {
      spans.push({ type: "text", text: text.slice(last, match.index) });
    }
    spans.push(
      match[1] !== undefined
        ? { type: "code", text: match[1] }
        : { type: "strong", text: match[2] },
    );
    last = match.index + match[0].length;
  }
  if (last < text.length) {
    spans.push({ type: "text", text: text.slice(last) });
  }
  return spans;
}
