/**
 * Candidates of the Cmd+K search modal (SearchModal.svelte): connections, query files of the
 * selected connection, and the tables already loaded in the TABLES pane
 * (CYBERNEURA-DEV-1208). Pure functions so the ordering and the "no tables unless loaded"
 * rule can be tested without the UI.
 */

import type { ConnectionInfo, FileSearchHit, TableInfo } from "./api";
// The .ts extension is needed because tests/*.test.mts load this module with plain Node
// (--experimental-strip-types resolves ESM imports by exact file name)
import { filterTerms, matchesFilter } from "./paneFilter.ts";

/// The table list the TABLES pane fetched last, with the connection and database it belongs to.
/// Published by TablesPane after `list_tables`; the search never fetches tables itself
/// (that would open the connection / SSH tunnel just to search).
export interface LoadedTables {
  connection: string;
  /// The active database at load time (null when the connection has none)
  database: string | null;
  tables: TableInfo[];
}

export type SearchItem =
  | { kind: "connection"; name: string; description: string | null }
  | { kind: "file"; hit: FileSearchHit }
  | { kind: "table"; table: TableInfo };

/// Connection filtering. Partial match on name and description (case-insensitive). An empty
/// query shows all entries so the modal can be used to switch connections (jump).
export function matchConnections(
  connections: readonly ConnectionInfo[],
  query: string,
): ConnectionInfo[] {
  const q = query.trim().toLowerCase();
  if (!q) {
    return [...connections];
  }
  return connections.filter(
    (c) =>
      c.name.toLowerCase().includes(q) ||
      (c.description ?? "").toLowerCase().includes(q),
  );
}

/// Tables matching the query, taken only from the loaded list of the **selected connection and
/// its current database**. Nothing when the query is empty (tables are not a jump target like
/// connections), when nothing is loaded, or when the loaded list belongs to another
/// connection / database (it would insert a name that does not exist there).
/// Terms are whitespace-separated like the pane filters (every term must match).
export function matchLoadedTables(
  loaded: LoadedTables | null,
  selectedConnection: string | null,
  activeSchema: string | null,
  query: string,
): TableInfo[] {
  const terms = filterTerms(query);
  if (
    terms.length === 0 ||
    !loaded ||
    !selectedConnection ||
    loaded.connection !== selectedConnection ||
    loaded.database !== (activeSchema || null)
  ) {
    return [];
  }
  return loaded.tables.filter((t) =>
    matchesFilter(terms, [t.qualified_name, t.name, t.schema]),
  );
}

/// The flat candidate list for keyboard navigation: connections, then files, then tables.
/// The display grouping and the indices in the modal follow this order.
export function buildSearchItems(
  connections: readonly ConnectionInfo[],
  fileHits: readonly FileSearchHit[],
  tables: readonly TableInfo[],
): SearchItem[] {
  return [
    ...connections.map((c) => ({
      kind: "connection" as const,
      name: c.name,
      description: c.description,
    })),
    ...fileHits.map((hit) => ({ kind: "file" as const, hit })),
    ...tables.map((table) => ({ kind: "table" as const, table })),
  ];
}
