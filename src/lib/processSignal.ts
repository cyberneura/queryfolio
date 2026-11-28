// Cancel query / Kill connection from the PROCESS LIST pane (CYBERNEURA-DEV-1399).
//
// Pure helpers for ProcessListPane.svelte (tests/process-signal.test.mts).

import type { ProcessAction, ProcessInfo } from "./api";

function fieldValue(p: ProcessInfo, name: string): string | null {
  const f = p.fields.find((x) => x.name.toLowerCase() === name.toLowerCase());
  return f ? f.value : null;
}

/**
 * Whether `current` (fetched again right before sending) is still the session and statement the
 * user confirmed in the dialog (`confirmed`, a snapshot of the row). The server only takes a
 * session id, so without this check a statement that started while the dialog was open would be
 * the one stopped.
 *
 * - The session: same id, and on PostgreSQL the same `backend_start` (a pid can be reused by a new
 *   backend; MySQL thread ids are not reused while the server runs).
 * - For Cancel query also the statement: same text, and on PostgreSQL the same `query_start`
 *   (the same text run again is a different statement). On MySQL there is no start time, so the
 *   elapsed seconds must not have gone down (a re-run restarts Time from 0).
 *
 * Returns null when it is the same target, otherwise the reason to show.
 */
export function targetChangedReason(
  confirmed: ProcessInfo,
  current: ProcessInfo | undefined,
  action: ProcessAction,
): string | null {
  if (!current) {
    return `#${confirmed.id} is no longer listed (it has ended)`;
  }
  if (fieldValue(confirmed, "backend_start") !== fieldValue(current, "backend_start")) {
    return `#${confirmed.id} is now a different session`;
  }
  if (action === "kill_connection") {
    return null;
  }
  const changed =
    (confirmed.info ?? null) !== (current.info ?? null) ||
    fieldValue(confirmed, "query_start") !== fieldValue(current, "query_start") ||
    (confirmed.time_secs !== null &&
      current.time_secs !== null &&
      current.time_secs < confirmed.time_secs);
  return changed
    ? `#${confirmed.id} is now running a different statement than the one you confirmed`
    : null;
}

/**
 * MySQL statements whose interruption can leave a table unusable until it is repaired (the KILL
 * documentation: REPAIR TABLE / OPTIMIZE TABLE on MyISAM; ALTER TABLE on a non-transactional table
 * is also left half done). Used to add a warning to the confirmation.
 */
export function mysqlKillMayDamageTable(statement: string | null): boolean {
  if (!statement) return false;
  const body = statement.replace(/^(?:\s+|--[^\n]*\n|#[^\n]*\n|\/\*[\s\S]*?\*\/)*/, "");
  return /^(?:repair|optimize|alter)\s+/i.test(body);
}
