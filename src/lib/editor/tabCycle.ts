/**
 * Pure logic for cycling editor tabs in history (MRU) order.
 *
 * The state itself lives in `stores/app.svelte.ts`. It is split out here so that
 * the ordering computation can be tested without Svelte / Tauri.
 */

/** Returns a new order with the tab moved to the head of the MRU (head = most recently active). */
export function touchMru(mru: readonly number[], id: number): number[] {
  return [id, ...mru.filter((t) => t !== id)];
}

/** Removes a closed tab from the MRU. */
export function forgetMru(mru: readonly number[], id: number): number[] {
  return mru.filter((t) => t !== id);
}

/**
 * Builds the order of tabs to cycle through.
 *
 * Lists the tabs in the MRU in history order, then appends tabs that have never been
 * active, in display order. The MRU side is restricted to tabs that exist (IDs of
 * closed tabs that linger are not cycled through).
 *
 * @param mru - tab IDs in MRU order
 * @param tabIds - currently open tab IDs (display order)
 */
export function buildCycleOrder(
  mru: readonly number[],
  tabIds: readonly number[],
): number[] {
  const open = new Set(tabIds);
  const known = new Set(mru);
  return [
    ...mru.filter((id) => open.has(id)),
    ...tabIds.filter((id) => !known.has(id)),
  ];
}

/**
 * Advances the cycle position by one in the given direction (wraps around at the ends).
 *
 * @param index - current position
 * @param direction - 1 = next (Ctrl+Tab) / -1 = previous (Ctrl+Shift+Tab)
 * @param length - number of tabs to cycle through
 */
export function stepCycleIndex(
  index: number,
  direction: 1 | -1,
  length: number,
): number {
  if (length <= 0) {
    return 0;
  }
  return (index + direction + length) % length;
}
