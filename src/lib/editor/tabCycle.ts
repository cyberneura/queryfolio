/**
 * エディタタブを履歴 (MRU) 順に巡回するための純粋なロジック。
 *
 * 状態そのものは `stores/app.svelte.ts` が持つ。ここに切り出しているのは、
 * 順序の計算だけを Svelte / Tauri 抜きで検証できるようにするため。
 */

/** タブを MRU の先頭へ繰り上げた新しい順序を返す (先頭 = 直近にアクティブ)。 */
export function touchMru(mru: readonly number[], id: number): number[] {
  return [id, ...mru.filter((t) => t !== id)];
}

/** 閉じたタブを MRU から取り除く。 */
export function forgetMru(mru: readonly number[], id: number): number[] {
  return mru.filter((t) => t !== id);
}

/**
 * 巡回の対象順を組み立てる。
 *
 * MRU に載っているものを履歴順に並べ、まだ一度もアクティブになっていないタブを
 * 表示順で後ろに足す。MRU 側は実在するタブだけに絞る (閉じたタブの ID が
 * 残っていても巡回対象にしない)。
 *
 * @param mru - MRU 順のタブ ID
 * @param tabIds - 現在開いているタブ ID (表示順)
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
 * 巡回位置を direction 方向へ 1 つ進める (端は反対側へ回り込む)。
 *
 * @param index - 現在位置
 * @param direction - 1 = 次へ (Ctrl+Tab) / -1 = 前へ (Ctrl+Shift+Tab)
 * @param length - 巡回対象の数
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
