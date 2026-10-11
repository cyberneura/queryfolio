// Line-by-line bulk replacement. Replaces %%% in the template with each input line and outputs one line per input line.
// Blank lines and lines starting with # or // are skipped (compatible with t.ytyng.com/line-replace).
// Main use: bulk-converting a list of IDs from SHOW FULL PROCESSLIST into `KILL %%%;`, etc.
export const PLACEHOLDER = "%%%";

export const generateLineReplace = (
  lines: string,
  template: string,
): string => {
  const out: string[] = [];
  for (const raw of lines.split(/\r?\n/)) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#") || line.startsWith("//")) {
      continue;
    }
    // Replace every occurrence of %%%. split/join is used so regex special characters are not a concern
    out.push(template.split(PLACEHOLDER).join(line));
  }
  return out.join("\n");
};

// Number of output lines (for the preview "N lines" display). 0 for empty input
export const countLineReplaceResults = (
  lines: string,
  template: string,
): number => {
  const result = generateLineReplace(lines, template);
  return result === "" ? 0 : result.split("\n").length;
};
