/// What Queryfolio tells a coding agent with each message (CYBERNEURA-DEV-1354): which
/// connection the user is on, which query file is open, and the statement at the cursor, so
/// that "fix this query" needs no explanation. Pure functions; tested with node.
///
/// What is sent: the connection name, the engine, the active database, file paths, the SQL at
/// the cursor, and (first message only) table and column names. Never sent: passwords or
/// query results. Host and user names are not put into the prompt as such, but the default
/// query folder name is built from them (it is part of the file path), and `_queryfolio.md`
/// in that folder, which the agent can read, lists them.

export interface AgentEditorContext {
  /// Connection name as shown in the app
  connection: string;
  /// Engine name of the connection (mysql / postgres / ...)
  engine: string;
  /// Active database / schema (null when the connection has none selected)
  database: string | null;
  /// Absolute path of the query file open in the editor (null when no file is open)
  filePath: string | null;
  /// The statement at the cursor, or the selection (empty when there is none)
  statement: string;
  /// The editor holds changes that could not be saved, so the file on disk is behind it
  unsaved?: boolean;
}

/// Longest statement quoted in the context. The file itself is on disk for the agent to read
export const MAX_STATEMENT_CHARS = 4_000;

/// Budget of the table / column list in the first message of a conversation
export const MAX_SCHEMA_CHARS = 8_000;

const clip = (text: string, max: number): string =>
  text.length > max ? `${text.slice(0, max)}\n… (truncated)` : text;

/// A code fence longer than any backtick run in `text`, so that the text cannot close it
const fenceFor = (text: string): string => {
  let longest = 0;
  for (const run of text.match(/`+/g) ?? []) {
    longest = Math.max(longest, run.length);
  }
  return "`".repeat(Math.max(3, longest + 1));
};

/// `table(col, col)` lines for as many tables as fit the budget; the rest are counted
export const schemaSummary = (
  map: Record<string, string[]> | null,
  maxChars = MAX_SCHEMA_CHARS,
): string => {
  if (!map) {
    return "";
  }
  const tables = Object.keys(map);
  if (tables.length === 0) {
    return "";
  }
  const lines: string[] = [];
  let used = 0;
  let listed = 0;
  for (const table of tables) {
    const line = `${table}(${map[table].join(", ")})`;
    if (used + line.length + 1 > maxChars) {
      break;
    }
    lines.push(line);
    used += line.length + 1;
    listed += 1;
  }
  if (listed < tables.length) {
    lines.push(`… and ${tables.length - listed} more tables`);
  }
  return lines.join("\n");
};

const contextLines = (context: AgentEditorContext): string[] => {
  const lines = [
    `Connection: ${context.connection} (${context.engine})` +
      (context.database ? `, database: ${context.database}` : ""),
  ];
  lines.push(
    context.filePath
      ? `Query file open in the editor: ${context.filePath}`
      : "No query file is open in the editor.",
  );
  if (context.filePath && context.unsaved) {
    lines.push(
      "Warning: the editor has changes that could not be saved; the file on disk is older.",
    );
  }
  const statement = context.statement.trim();
  if (statement) {
    const clipped = clip(statement, MAX_STATEMENT_CHARS);
    const fence = fenceFor(clipped);
    lines.push("Statement at the cursor:", `${fence}sql`, clipped, fence);
  }
  return lines;
};

/// The text sent to the chat agent for one user message. `schema` (the output of
/// schemaSummary) is passed for the first message of a conversation only.
export const chatPrompt = (
  request: string,
  context: AgentEditorContext | null,
  schema = "",
): string => {
  if (!context) {
    return request;
  }
  const lines = [
    "<queryfolio-context>",
    "The user is working in Queryfolio, a SQL client. Edits to the open query file appear in",
    "its editor (already saved to disk; the user runs the SQL there, you cannot run it).",
    ...contextLines(context),
  ];
  if (schema) {
    lines.push("Tables (name(columns)):", schema);
  }
  lines.push("</queryfolio-context>", "", request);
  return lines.join("\n");
};

/// The prompt of a one-shot run from the inline AI field above the editor: the agent edits the
/// open query file as instructed, and its short final message is shown to the user.
export const oncePrompt = (
  instruction: string,
  context: AgentEditorContext,
  schema = "",
): string => {
  const lines = [
    "You are asked from the SQL editor of Queryfolio, a SQL client, to change the query file",
    "that is open there. Edit that file directly as instructed below (the editor reloads it).",
    "Do not run anything. When done, reply with one or two short sentences saying what you",
    "changed; that reply is shown to the user in a notification.",
    "",
    ...contextLines(context),
  ];
  if (schema) {
    lines.push("Tables (name(columns)):", schema);
  }
  lines.push("", "Instruction:", instruction.trim());
  return lines.join("\n");
};
