import type { QueryResult } from "$lib/api";
import { toTsvCapped } from "$lib/export";

/// Marker placed at the start of a written-back log block (U+1F5D2 + variation selector).
/// Existing blocks are detected with U+1F5D2 without the selector, so they are still found even if the selector is removed by hand
export const RUN_LOG_RESULT_MARKER = "\u{1F5D2}\u{FE0F}";

/// A result with **more than** this many rows shows a confirmation dialog before being written back.
/// If it is exactly this many rows, writing everything is the same amount, so no question is asked.
///
/// The same value is also the number of rows when "write only part" is chosen (the dialog choices are
/// "write all / write only this many rows / do not write", so with a value different from the threshold
/// we could not explain how many rows it would be)
export const RUN_LOG_CONFIRM_ROWS = 200;

/// The choice in the confirmation dialog before writing back a result with many rows.
/// `limited` writes only [`RUN_LOG_CONFIRM_ROWS`] rows
export type RunLogChoice = "all" | "limited" | "cancel";

/// Upper limit on the total number of characters of the TSV to write back.
/// **The row-count confirmation dialog alone is not enough** — cells have no character limit, so even a few
/// rows with a long TEXT / JSON column can reach hundreds of MB, and inserting into CodeMirror and
/// auto-saving freeze the app. The excess is cut off and a note to that effect is written in the body
const MAX_BODY_CHARS = 200_000;

/// Upper limit on the number of characters per cell of the TSV to write back.
/// The total limit alone cannot prevent a single huge row (number of columns x long TEXT)
const MAX_CELL_CHARS = 2_000;

/// Maximum length of the log label (the part of the marker line after 📝).
/// If the whole one-line comment became the heading it would be hard to read, so it is truncated
const MAX_LABEL_CHARS = 200;

/// The execution target and the information of the 📝 marker attached to it.
/// Created by the editor (SqlEditor) at execution time and used for the write-back after the result returns
export interface RunTarget {
  /// The SQL to execute (the text of [from, to) in the editor)
  sql: string;
  from: number;
  to: number;
  /// The label of the 📝 marker if there is one (null if there is no marker; an empty string if the label is omitted)
  logLabel: string | null;
}

/// Whether this is a line comment (`--`) line
const isLineComment = (line: string): boolean => line.trimStart().startsWith("--");

/// If the line comment is a 📝 (U+1F4DD) marker line, returns its label (the part after 📝).
/// Returns null if it is not a marker line. The marker must be placed immediately after the comment symbol
/// (a 📝 appearing in the middle of the comment body is not regarded as a marker)
const markerLabel = (line: string): string | null => {
  const match = line.match(/^\s*--+\s*\u{1F4DD}\u{FE0F}?\s*(.*)$/u);
  if (!match) {
    return null;
  }
  return match[1].trim().slice(0, MAX_LABEL_CHARS);
};

/// Returns the (0-based) number of the line containing position from
const lineIndexOf = (lines: string[], offset: number): number => {
  let start = 0;
  for (let i = 0; i < lines.length; i++) {
    // +1 is for the newline at the end of the line. The end-of-line position (just before the newline) belongs to the same line
    const end = start + lines[i].length;
    if (offset <= end) {
      return i;
    }
    start = end + 1;
  }
  return lines.length - 1;
};

/// Looks for the `-- 📝 <label>` marker attached to the execution target [from, to)
/// in the editor body doc. Returns null if there is none.
///
/// Only line comments **consecutive** right before the execution target are examined (if there is a blank
/// line or another statement in between, it is a marker attached to another statement and is out of scope).
/// In addition, the execution target may itself start with a comment line (when lang-sql includes the
/// preceding comment in the Statement), so comment lines continuing at the start of the range are treated
/// as the same sequence (here, blank lines in between still count as the same sequence. See below for the reason).
///
/// If there are multiple marker lines, the one closest to the SQL is adopted.
export const findRunLogLabel = (
  doc: string,
  from: number,
  to: number,
): string | null => {
  const lines = doc.split("\n");
  const startLine = lineIndexOf(lines, from);
  // Skip the comment lines included at the start of the execution range to get the start line of the SQL body.
  //
  // The reason we **also skip blank lines** here is that, inside the execution range, a blank line
  // is not a break in the sequence. lang-sql does not treat a `--` line with no content as a LineComment,
  // so everything from there to the SQL becomes a single Statement
  // (writing an explanatory comment -> blank line -> `-- 📝 label` -> SQL is perfectly plausible).
  // Stopping at a blank line would never reach the marker line (CYBERNEURA-DEV-516).
  //
  // Stop at lastLine to keep the skipping within the execution range. If it goes
  // outside the range, it would pick up the marker attached to the next statement as its own.
  //
  // to is the (exclusive) end of the range, so the line to look up is to - 1. If to were passed as is,
  // a to that falls exactly at the start of a line would include a line outside the range
  const lastLine = to > from ? lineIndexOf(lines, to - 1) : startLine;
  let end = startLine;
  while (
    end <= lastLine &&
    (isLineComment(lines[end]) || lines[end].trim() === "")
  ) {
    end++;
  }
  // Go back through the comment lines that continue right before it.
  // This is outside the execution range, so blank lines are treated as a break in the sequence as before
  let begin = startLine;
  while (begin > 0 && isLineComment(lines[begin - 1])) {
    begin--;
  }
  for (let i = end - 1; i >= begin; i--) {
    const label = markerLabel(lines[i]);
    if (label !== null) {
      return label;
    }
  }
  return null;
};

/// Sanitize text to be pasted as a SQL block comment.
///
/// If `*/` were left, the comment would close there and the following data lines would be executed
/// as SQL as is. `/*` cannot be left either — PostgreSQL block comments
/// **nest**, so an unclosed `/*` keeps the comment from ending and swallows
/// the whole rest of the file.
const escapeBlockComment = (text: string): string =>
  text.replace(/\/\*/g, "/ *").replace(/\*\//g, "* /");

/// Execution time to put in the log block heading (local time, YYYY-MM-DD HH:MM:SS)
export const formatRunLogTimestamp = (date: Date): string => {
  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
    ` ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  );
};

/// Turns the result into the log body (TSV).
/// A statement that returns no rows (INSERT etc.) gets its affected row count written. If the output is not complete,
/// a note is left for each reason (so that a later reader does not mistake it for the full result).
/// They can occur at the same time, so they are listed as independent notes.
///
/// If `maxRows` is passed, only that many rows are written and `(limited to N rows)` is added
/// (when "write only part" is chosen in the confirmation dialog).
export const runLogBody = (result: QueryResult, maxRows?: number): string => {
  if (result.columns.length === 0) {
    return result.affected_rows === null
      ? "(no rows)"
      : `(${result.affected_rows} rows affected)`;
  }
  // When limiting the row count, replace the result itself (toTsvCapped only looks at the character limit)
  const capped =
    maxRows !== undefined && result.rows.length > maxRows
      ? { ...result, rows: result.rows.slice(0, maxRows) }
      : null;
  const { text, truncated } = toTsvCapped(
    capped ?? result,
    MAX_BODY_CHARS,
    MAX_CELL_CHARS,
  );
  const lines = [text];
  if (capped !== null) {
    lines.push(`(limited to ${capped.rows.length} rows)`);
  }
  // **Merely having an auto LIMIT attached does not mean it was "suppressed".**
  // If LIMIT 500 was added but only 5 rows came back, nothing was dropped, so writing it would give
  // the wrong warning that "there is more". Write it only when the row count has reached the limit
  // (when it is exactly the limit we cannot know whether there is more, so we err on the side of writing it).
  // It is a different fact from the "write only part" note, so make the wording distinguishable too
  if (result.applied_limit !== null && result.rows.length >= result.applied_limit) {
    lines.push(`(the query was limited to ${result.applied_limit} rows)`);
  }
  if (result.truncated) {
    lines.push("(the result itself was truncated)");
  }
  if (truncated) {
    lines.push("(this log was truncated — see the result table for the full output)");
  }
  return lines.join("\n");
};

/// Builds the log block (a SQL block comment)
export const formatRunLogBlock = (
  label: string,
  timestamp: string,
  body: string,
): string => {
  const heading = label
    ? `${RUN_LOG_RESULT_MARKER} ${label} ${timestamp}`
    : `${RUN_LOG_RESULT_MARKER} ${timestamp}`;
  return `/* ${escapeBlockComment(heading)}\n${escapeBlockComment(body)}\n*/`;
};

/// Start of the log block (`/* 🗒️`). The variation selector is optional
const RUN_LOG_BLOCK_OPEN = /^\/\*\s*\u{1F5D2}/u;

/// Returns the position after skipping, among the whitespace continuing from pos, **only completely blank lines**
/// (just after the last newline crossed. pos itself if there is no blank line).
///
/// If whitespace were skipped as is, the indentation of the next line would enter the replacement range,
/// and the indentation of an unrelated line would vanish on every write-back.
const skipBlankLines = (doc: string, pos: number): number => {
  let lineStart = pos;
  for (let i = pos; i < doc.length && /\s/.test(doc[i]); i++) {
    if (doc[i] === "\n") {
      lineStart = i + 1;
    }
  }
  return lineStart;
};

/// A change to write back to the editor (replacement range and text to insert)
export interface RunLogWrite {
  from: number;
  to: number;
  insert: string;
}

/// Builds the change that writes a log block right after the execution target.
///
/// If there is an existing log block right after it, replace it (same as runandlog; however many times
/// it is run, only one block remains). If there is none, insert. Blank lines before and after are normalized
/// to one line, so writing the same result yields the same text.
///
/// If the existing block is not closed with `*/`, returns null (adding text before a broken
/// comment only increases nesting, so we notify instead of writing)
export const runLogWrite = (
  doc: string,
  statementTo: number,
  block: string,
): RunLogWrite | null => {
  // Trailing whitespace and `;` of the statement are treated as part of the statement, and we write after them
  // (so that, when the Statement range does not include `;`, the block is not sandwiched
  // before the semicolon)
  let anchor = statementTo;
  while (anchor < doc.length && /[ \t;]/.test(doc[anchor])) {
    anchor++;
  }
  // Whether what is past the following whitespace is an existing log block (found even if indented)
  let contentStart = anchor;
  while (contentStart < doc.length && /\s/.test(doc[contentStart])) {
    contentStart++;
  }
  // Even when there is no existing block, the blank lines in between are included in the replacement range
  // (normalized to the same number of blank lines every time so blank lines do not increase with repeated write-backs)
  let to = skipBlankLines(doc, anchor);
  if (RUN_LOG_BLOCK_OPEN.test(doc.slice(contentStart, contentStart + 16))) {
    // `*/` in the body has been neutralized by escapeBlockComment, so
    // the first `*/` found is the end of this block
    const close = doc.indexOf("*/", contentStart);
    if (close < 0) {
      return null;
    }
    to = skipBlankLines(doc, close + 2);
  }
  return { from: anchor, to, insert: `\n\n${block}\n\n` };
};

/// Result of the write-back. stale = the target range has shifted (do not write),
/// unmarked = the 📝 marker has disappeared (do not write),
/// broken = the existing log block is not closed with `*/` (do not write),
/// conflicted = the destination tab is in conflict with an external change (do not write)
export type RunLogOutcome =
  | "written"
  | "stale"
  | "unmarked"
  | "broken"
  | "conflicted";

/// Builds the change that writes a log right after the execution target target in the body doc.
/// If it cannot be written, returns the reason (RunLogOutcome).
///
/// If an edit or file switch happened while the query was running, the range of target
/// points to a different place, so write only when the text of the range matches the SQL that was executed.
///
/// **The label is re-read from the body at write-back time.** Even if the SQL body is unchanged,
/// the marker line alone can be edited, and a same-length rewrite (`Step 1` ->
/// `Step 2`) passes straight through the range check. If the marker itself has been deleted,
/// it is a cancellation of the write-back, so write nothing and return unmarked.
///
/// Used from both the editor (the displayed tab) and the body (store) of inactive
/// tabs.
export const planRunLogWrite = (
  doc: string,
  target: RunTarget,
  buildBlock: (label: string) => string,
): RunLogWrite | "stale" | "unmarked" | "broken" => {
  if (
    target.from < 0 ||
    target.to < target.from ||
    target.to > doc.length ||
    doc.slice(target.from, target.to) !== target.sql
  ) {
    return "stale";
  }
  const label = findRunLogLabel(doc, target.from, target.to);
  if (label === null) {
    return "unmarked";
  }
  return runLogWrite(doc, target.to, buildBlock(label)) ?? "broken";
};
