// Adapted from Beckwell (Cyberneura), MIT, via Astragal. See docs/agent-chat.md.
import DOMPurify from 'dompurify';
import { Marked, Renderer } from 'marked';
import { COPY_BUTTON_HTML } from './copy';

// Produces a form usable in both attribute values and text. Character references (&amp; etc.) are not converted twice
function escapeHtml(text: string): string {
  return text.replace(/&(?!(#\d{1,7}|#[Xx][a-fA-F0-9]{1,6}|\w+);)/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

// Like a terminal, show a single newline as a line break (breaks)
const marked = new Marked({ gfm: true, breaks: true });

const DETAILS_OPEN = /^ {0,3}<details\b[^>\n]*>/i;
const LINE_DETAILS_OPEN = /^<details\b[^>\n]*>/i;
const DETAILS_CLOSE = /<\/details\s*>/gi;
const FENCE_OPEN = /^ {0,3}(`{3,}|~{3,})/;
const FENCE_CLOSE = /^ {0,3}(`{3,}|~{3,})[ \t]*$/;
// Blockquote (>) and list markers. The body lexer strips these before reading the content, so a <details> or fence after them is also treated as line start
const BLOCK_PREFIX = /^(?:[ \t]*(?:>|(?:[-*+]|\d{1,9}[.)])(?=[ \t])))*[ \t]*/;

// Turns code spans into spaces of the same length (so that a matched position can be used as-is as a position in the original line).
// A code span is closed by a backtick run of the same length. A run of a different length is part of the content.
// If the head of the opening run is escaped with \, that one character stays literal (a \ on the closing side is inside the code, so it has no effect)
function maskCodeSpans(line: string): string {
  const runs = [...line.matchAll(/`+/g)];
  let masked = line;
  for (let i = 0; i < runs.length; i++) {
    const escaped = (/\\*$/.exec(line.slice(0, runs[i].index))?.[0].length ?? 0) % 2 === 1;
    const length = runs[i][0].length - (escaped ? 1 : 0);
    if (!length) continue;
    const j = runs.findIndex((run, k) => k > i && run[0].length === length);
    if (j === -1) continue;
    const from = runs[i].index + (escaped ? 1 : 0);
    const to = runs[j].index + length;
    masked = masked.slice(0, from) + ' '.repeat(to - from) + masked.slice(to);
    i = j;
  }
  return masked;
}

// Each nesting level rescans its content, so the depth is limited. <details> beyond it stay as text
const MAX_DETAILS_DEPTH = 8;
let detailsDepth = 0;

// From a <details> at line start to the matching </details>. If it is not closed (while streaming), to the end.
// Tags inside body code (fences, code spans) are not counted. Only opening tags at a line start that forms a block are counted
function findDetailsBlock(src: string): { raw: string; body: string } | null {
  const open = DETAILS_OPEN.exec(src);
  if (!open) return null;
  let depth = 0;
  let fence = '';
  for (let start = 0; start < src.length; ) {
    const newline = src.indexOf('\n', start);
    const end = newline === -1 ? src.length : newline;
    const line = src.slice(start, end);
    const prefix = BLOCK_PREFIX.exec(line)?.[0].length ?? 0;
    const content = line.slice(prefix);
    if (fence) {
      const marker = FENCE_CLOSE.exec(content)?.[1];
      if (marker && marker[0] === fence[0] && marker.length >= fence.length) fence = '';
    } else {
      fence = FENCE_OPEN.exec(content)?.[1] ?? '';
      if (!fence) {
        const text = maskCodeSpans(line);
        const lineOpen = LINE_DETAILS_OPEN.exec(text.slice(prefix));
        if (lineOpen) depth++;
        DETAILS_CLOSE.lastIndex = lineOpen ? prefix + lineOpen[0].length : 0;
        for (let m = DETAILS_CLOSE.exec(text); m; m = DETAILS_CLOSE.exec(text)) {
          if (--depth > 0) continue;
          const close = start + m.index;
          return { raw: src.slice(0, close + m[0].length), body: src.slice(open[0].length, close) };
        }
      }
    }
    start = end + 1;
  }
  return { raw: src, body: src.slice(open[0].length) };
}

// <details> / <summary> are not collapsed; they become divs with a frame and a heading. Shown as text like other HTML tags,
// they would be hard to read when the agent writes supplementary notes in <details>. The content is interpreted as Markdown
marked.use({
  extensions: [
    {
      name: 'details',
      level: 'block',
      // marked passes the paragraph from its second character on, so the leading position is not a line start. Only positions right after a newline are candidates
      start(src) {
        const index = /\n {0,3}<details\b/i.exec(src)?.index;
        return index === undefined ? undefined : index + 1;
      },
      tokenizer(src) {
        if (detailsDepth >= MAX_DETAILS_DEPTH) return undefined;
        const block = findDetailsBlock(src);
        if (!block) return undefined;
        const summary = /^\s*<summary\b[^>]*>([\s\S]*?)<\/summary>/i.exec(block.body);
        const body = summary ? block.body.slice(summary[0].length) : block.body;
        const token = { type: 'details', raw: block.raw, summaryTokens: [], tokens: [] };
        if (summary) this.lexer.inline(summary[1].trim(), token.summaryTokens);
        detailsDepth++;
        try {
          // Drop only the leading blank lines. trim would also remove the first line's indentation (an indented code block)
          this.lexer.blockTokens(body.replace(/^\s*\n/, '').trimEnd(), token.tokens);
        } finally {
          detailsDepth--;
        }
        return token;
      },
      renderer(token) {
        const summary = token.summaryTokens.length
          ? `<div class="summary">${this.parser.parseInline(token.summaryTokens)}</div>`
          : '';
        return `<div class="details">${summary}${this.parser.parse(token.tokens ?? [])}</div>\n`;
      },
      childTokens: ['summaryTokens', 'tokens'],
    },
  ],
  renderer: {
    // HTML tags in the body are shown as text. When the agent explains how to write HTML, it is unreadable if the tags disappear
    html({ text }) {
      return escapeHtml(text);
    },
    // marked treats the text after an opening <pre> / <script> / <style> tag as already-escaped HTML and emits it as-is
    text(token) {
      if (token.type === 'text' && token.escaped && !token.tokens) return escapeHtml(token.text);
      return false;
    },
    // Code blocks get a copy button that appears at the top right on hover (ChatTab handles the click)
    code(token) {
      return `<div class="code-block">${Renderer.prototype.code.call(this, token)}${COPY_BUTTON_HTML}</div>\n`;
    },
    // Images are not loaded; they become links. This avoids requests going to external servers merely by displaying
    image({ href, text, tokens }) {
      const label = tokens ? this.parser.parseInline(tokens, this.parser.textRenderer) : text;
      return `<a href="${escapeHtml(href)}">${escapeHtml(label || href)}</a>`;
    },
  },
});

// Allow only the tags marked emits. The WebView can call Tauri commands, so no element or attribute that can run scripts is kept
const SANITIZE_OPTIONS = {
  ALLOWED_TAGS: [
    'p', 'br', 'hr', 'strong', 'em', 'del', 'code', 'pre', 'blockquote',
    'ul', 'ol', 'li', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'a',
    'table', 'thead', 'tbody', 'tr', 'th', 'td', 'input', 'div', 'button',
  ],
  ALLOWED_ATTR: ['href', 'title', 'align', 'start', 'class', 'type', 'checked', 'disabled', 'aria-label'],
  ALLOW_DATA_ATTR: false,
};

// The attribute that holds the link target. Left as href, the WebView could navigate to that page by routes other than a click (the context menu, etc.).
// The destination page could call the app's commands, so it is not kept in a form the WebView can follow
export const LINK_ATTRIBUTE = 'data-href';

DOMPurify.addHook('afterSanitizeAttributes', (node) => {
  if (node.tagName !== 'A') return;
  const href = node.getAttribute('href');
  if (href === null) return;
  node.removeAttribute('href');
  node.setAttribute(LINK_ATTRIBUTE, href);
  // Lets the user check the destination before opening
  if (!node.hasAttribute('title')) node.setAttribute('title', href);
});

// Converts the agent's or the user's Markdown into HTML that can be inserted into the screen
export function renderMarkdown(text: string): string {
  const html = marked.parse(text, { async: false });
  return DOMPurify.sanitize(html, SANITIZE_OPTIONS);
}

// Whether the link may be opened in an external app. Opening a relative path or the like as-is would navigate the WebView away from the app's screen
export function isExternalLink(href: string): boolean {
  return /^(https?:|mailto:)/i.test(href);
}
