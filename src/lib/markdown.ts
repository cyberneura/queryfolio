/// Minimal splitting utility for displaying AI responses (Markdown).
/// It has no full Markdown renderer; it only splits the text into code blocks and
/// text by ``` fences (shared by AiAnalysisModal / ChatPane).

export interface MarkdownSegment {
  type: "text" | "code";
  content: string;
}

/// Splits into code blocks and text by ``` fences.
/// Even-indexed parts of the split are text and odd-indexed ones are code (a trailing
/// section with no closing fence is also shown as code).
export function splitMarkdownSegments(text: string): MarkdownSegment[] {
  const result: MarkdownSegment[] = [];
  text.split("```").forEach((part, i) => {
    if (i % 2 === 0) {
      if (part.trim()) {
        result.push({ type: "text", content: part.trim() });
      }
      return;
    }
    // Strip the language tag (sql, etc.) from the first line of the code block
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
