/// The narrowing filter of the CONNECTIONS / FILES panes.
///
/// Case-insensitive substring match. The query is split on whitespace and every term has to
/// appear in at least one of the fields (AND across terms, OR across fields), so
/// `prod mysql` finds a connection named "torico-blog-production" whose engine is MySQL.

/// Split the query into lowercase terms. An empty / whitespace-only query has no terms.
export const filterTerms = (query: string): string[] =>
  query
    .toLowerCase()
    .split(/\s+/)
    .filter((term) => term.length > 0);

/// Whether the fields match all terms. No terms = everything matches.
export const matchesFilter = (
  terms: readonly string[],
  fields: readonly (string | null | undefined)[],
): boolean => {
  if (terms.length === 0) {
    return true;
  }
  const haystack = fields
    .filter((field): field is string => typeof field === "string" && field !== "")
    .map((field) => field.toLowerCase());
  return terms.every((term) => haystack.some((field) => field.includes(term)));
};
