/// Split the help body (Markdown) into blocks for display.
///
/// We do not bring in a full Markdown renderer. This only handles help text we wrote
/// ourselves, so it is limited to the syntax actually used (headings / code fences / bullet
/// lists / paragraphs). It returns only structure without building HTML, so the view side
/// can render it with Svelte markup (no need for innerHTML).

export type HelpBlock =
  | { type: "heading"; level: 1 | 2; text: string }
  | { type: "code"; content: string }
  | { type: "list"; items: string[] }
  | { type: "paragraph"; text: string };

/**
 * Split the help body into a list of blocks.
 * @param markdown - The help Markdown
 * @returns Array of blocks for display
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

    // Code fence. Even if the closing fence is missing, treat everything up to the end as one block
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

    // A bullet continuation line (an indented wrapped line) is appended to the previous item
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

/// Of the inline syntax, the help only uses `code` and **strong**.
/// This also returns a sequence of fragments the renderer can iterate over, not HTML.
export interface InlineSpan {
  type: "text" | "code" | "strong";
  text: string;
}

/**
 * Split one line of a paragraph / bullet into inline fragments.
 * @param text - The line to process
 * @returns Array of inline fragments
 */
export function parseInline(text: string): InlineSpan[] {
  const spans: InlineSpan[] = [];
  // Split out `code` first (we do not expect a ` inside **)
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
