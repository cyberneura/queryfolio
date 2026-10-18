/// Line-based 3-way merge (equivalent to diff3). When a query file is changed externally,
/// this compares the local unsaved edits (local) with the external changes (remote) against
/// their common ancestor (base). Changes on different lines are merged automatically, and
/// conflict=true is returned only when both sides changed the same line differently.
///
/// It is a pure function (independent of Tauri), so it can be verified in isolation. SQL files
/// are small, so a naive O(n*m) DP is enough for the LCS.

export interface Merge3Result {
  /// The merged text. When conflict=true it may be a half-baked result that merely picks one
  /// side, so callers must not use it.
  merged: string;
  /// local and remote changed the same region differently, so it could not be merged
  /// automatically.
  conflict: boolean;
}

/// The LCS is O(n*m), so files with more lines than this are not merged and are treated as a
/// conflict (to avoid long main-thread stalls and memory pressure from a huge DP array).
/// Normal query files are unlikely to exceed this; if one does, it is left to manual
/// resolution.
const MAX_MERGE_LINES = 20000;

/// The DP table allocates (n+1)*(m+1) cells, so cap the area (base×side) as well as the line
/// count. MAX_MERGE_LINES alone would allow about 400 million cells when both base and side
/// have 20000 lines, which could freeze or crash the UI thread at allocation time.
/// Combinations above 4,000,000 cells (e.g. 2000×2000) give up on merging and are treated as
/// a conflict.
const MAX_MERGE_CELLS = 4_000_000;

/// Split text into an array of lines. Uses split("\n") so the split is reversible via
/// join("\n") ("a\nb" -> ["a","b"], "a\nb\n" -> ["a","b",""]).
function splitLines(text: string): string[] {
  return text.split("\n");
}

/// Return the index pairs (in increasing order) that belong to the longest common
/// subsequence of base and other.
function lcsPairs(base: string[], other: string[]): Array<[number, number]> {
  const n = base.length;
  const m = other.length;
  // dp[i][j] = LCS length of base[i:] and other[j:]
  const dp: number[][] = Array.from({ length: n + 1 }, () =>
    new Array<number>(m + 1).fill(0),
  );
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] =
        base[i] === other[j]
          ? dp[i + 1][j + 1] + 1
          : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const pairs: Array<[number, number]> = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (base[i] === other[j]) {
      pairs.push([i, j]);
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      i++;
    } else {
      j++;
    }
  }
  return pairs;
}

interface DiffRegion {
  /// Range changed on the base side: [oStart, oStart+oLen)
  oStart: number;
  oLen: number;
  /// Corresponding range on the other side: [tStart, tStart+tLen)
  tStart: number;
  tLen: number;
}

/// Return the regions changed from base to other (diff blocks sandwiched between common
/// parts).
function diffRegions(base: string[], other: string[]): DiffRegion[] {
  const pairs = lcsPairs(base, other);
  const regions: DiffRegion[] = [];
  let oi = 0;
  let ti = 0;
  for (const [bi, tj] of pairs) {
    if (bi > oi || tj > ti) {
      regions.push({ oStart: oi, oLen: bi - oi, tStart: ti, tLen: tj - ti });
    }
    oi = bi + 1;
    ti = tj + 1;
  }
  if (base.length > oi || other.length > ti) {
    regions.push({
      oStart: oi,
      oLen: base.length - oi,
      tStart: ti,
      tLen: other.length - ti,
    });
  }
  return regions;
}

interface Hunk {
  oStart: number;
  oLen: number;
  /// 0 = local(A), 2 = remote(B) (follows the diff3 convention)
  side: 0 | 2;
  sideStart: number;
  sideLen: number;
}

/// 3-way merge local and remote using base as the common ancestor.
export function merge3(
  baseText: string,
  localText: string,
  remoteText: string,
): Merge3Result {
  const base = splitLines(baseText);
  const local = splitLines(localText);
  const remote = splitLines(remoteText);

  // Give up on automatic merging for huge files and treat them as a conflict (the caller
  // warns). In addition to the line cap, limit the DP table area (base×local / base×remote)
  // so allocating a huge 2D array does not freeze the main thread.
  if (
    base.length > MAX_MERGE_LINES ||
    local.length > MAX_MERGE_LINES ||
    remote.length > MAX_MERGE_LINES ||
    base.length * local.length > MAX_MERGE_CELLS ||
    base.length * remote.length > MAX_MERGE_CELLS
  ) {
    return { merged: localText, conflict: true };
  }

  // Collect both sides' changed regions relative to base as hunks, ordered by base position.
  const hunks: Hunk[] = [];
  for (const r of diffRegions(base, local)) {
    hunks.push({
      oStart: r.oStart,
      oLen: r.oLen,
      side: 0,
      sideStart: r.tStart,
      sideLen: r.tLen,
    });
  }
  for (const r of diffRegions(base, remote)) {
    hunks.push({
      oStart: r.oStart,
      oLen: r.oLen,
      side: 2,
      sideStart: r.tStart,
      sideLen: r.tLen,
    });
  }
  hunks.sort((x, y) => x.oStart - y.oStart || x.side - y.side);

  const out: string[] = [];
  let conflict = false;
  let cursor = 0; // next base position not yet output

  let k = 0;
  while (k < hunks.length) {
    // Group hunks that overlap in base coordinates into a single region. Hunks that merely
    // touch at an endpoint (changes to adjacent, separate lines) are not treated as
    // overlapping and form separate regions (decided with <). However, insertions at the
    // same point (oLen=0 with the same oStart) are not caught by <, so hunks with the same
    // start position are also included in the same region (to detect the case where both
    // sides insert different content at the same place as a conflict).
    const regionStart = hunks[k].oStart;
    let regionEnd = hunks[k].oStart + hunks[k].oLen;
    const group: Hunk[] = [hunks[k]];
    k++;
    while (
      k < hunks.length &&
      (hunks[k].oStart < regionEnd || hunks[k].oStart === regionStart)
    ) {
      regionEnd = Math.max(regionEnd, hunks[k].oStart + hunks[k].oLen);
      group.push(hunks[k]);
      k++;
    }

    // Output the unchanged base before the region as is.
    if (regionStart > cursor) {
      for (let i = cursor; i < regionStart; i++) out.push(base[i]);
    }

    // Reconstruct each side's content for the region [regionStart, regionEnd). If the side
    // has no hunk, it is the same as base. If it has hunks, convert to side coordinates using
    // the property that the matching lines before and after a changed block correspond 1:1
    // with base.
    const sideContent = (side: 0 | 2, src: string[]): string[] => {
      const parts = group.filter((h) => h.side === side);
      if (parts.length === 0) {
        return base.slice(regionStart, regionEnd);
      }
      let oMin = Infinity;
      let oMax = -Infinity;
      let sMin = Infinity;
      let sMax = -Infinity;
      for (const h of parts) {
        oMin = Math.min(oMin, h.oStart);
        oMax = Math.max(oMax, h.oStart + h.oLen);
        sMin = Math.min(sMin, h.sideStart);
        sMax = Math.max(sMax, h.sideStart + h.sideLen);
      }
      const lead = oMin - regionStart; // number of matching lines from the region start to the change start
      const trail = regionEnd - oMax; // number of matching lines from the change end to the region end
      return src.slice(sMin - lead, sMax + trail);
    };

    const hasA = group.some((h) => h.side === 0);
    const hasB = group.some((h) => h.side === 2);
    const aContent = sideContent(0, local);
    const bContent = sideContent(2, remote);

    if (hasA && hasB) {
      if (aContent.join("\n") === bContent.join("\n")) {
        // Both sides made the same change -> either one will do
        for (const line of aContent) out.push(line);
      } else {
        // Same region changed differently -> conflict. The caller must not use merged.
        conflict = true;
        for (const line of aContent) out.push(line);
      }
    } else if (hasA) {
      for (const line of aContent) out.push(line);
    } else {
      for (const line of bContent) out.push(line);
    }
    cursor = regionEnd;
  }

  // Output the remaining unchanged base.
  for (let i = cursor; i < base.length; i++) out.push(base[i]);

  return { merged: out.join("\n"), conflict };
}
