/// Statements that only make sense together with the statements after them on the same database
/// session. A batch runs each statement through run_query, which takes a connection from the pool
/// per call, so `BEGIN; UPDATE ...; ROLLBACK;` would commit the UPDATE on another connection, and
/// a `SET` or a temporary table would not be seen by the next statement.
const SESSION_KEYWORDS = new Set([
  "begin",
  "start",
  "commit",
  "end",
  "rollback",
  "abort",
  "savepoint",
  "release",
  "set",
  "reset",
  "lock",
  "unlock",
  "prepare",
  "execute",
  "deallocate",
  "declare",
  "fetch",
  "close",
  "listen",
  "unlisten",
  "discard",
]);

const TEMPORARY_TABLE = /^create\s+(?:(?:global|local)\s+)?temp(?:orary)?\b/i;

/// SQL Server temporary tables (`#name`) created by CREATE TABLE or SELECT ... INTO
const MSSQL_TEMPORARY_TABLE = /\b(?:table|into)\s+#/i;

/// Strip leading whitespace, line comments and block comments
const stripLeadingComments = (sql: string): string => {
  let rest = sql;
  for (;;) {
    rest = rest.trimStart();
    if (rest.startsWith("--")) {
      const newline = rest.indexOf("\n");
      rest = newline < 0 ? "" : rest.slice(newline + 1);
    } else if (rest.startsWith("/*")) {
      const end = rest.indexOf("*/");
      rest = end < 0 ? "" : rest.slice(end + 2);
    } else {
      return rest;
    }
  }
};

/// Return the reason a statement must not run as part of a batch (null when it can), shown as
/// the leading keyword or "temporary table"
export const sessionStatementReason = (statement: string): string | null => {
  const body = stripLeadingComments(statement);
  const keyword = body.match(/^[A-Za-z_]+/)?.[0].toLowerCase() ?? "";
  if (SESSION_KEYWORDS.has(keyword)) {
    return keyword.toUpperCase();
  }
  if (TEMPORARY_TABLE.test(body) || MSSQL_TEMPORARY_TABLE.test(body)) {
    return "temporary table";
  }
  return null;
};
