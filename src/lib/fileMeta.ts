// Formatters for the modified time and size shown on each FILES pane row (CYBERNEURA-DEV-774).
// Pure functions that do not depend on Svelte / Tauri.

const pad = (n: number) => String(n).padStart(2, "0");

/// Local time as `YYYY-mm-dd HH:MM`
export const formatModifiedAt = (ms: number): string => {
  const date = new Date(ms);
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
    ` ${pad(date.getHours())}:${pad(date.getMinutes())}`
  );
};

const relativeFormat = new Intl.RelativeTimeFormat("en", { numeric: "always" });

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/// Relative notation such as `3 days ago`. Under 1 minute is `just now`.
/// Months and years are counted as 30 days / 365 days rather than by the calendar (it is only a
/// rough guide in a list, so precision is not needed).
/// Even if a clock skew yields a future time, it is rounded to `just now` rather than
/// `in 5 minutes` (a file written elsewhere can have an mtime ahead of this PC's clock).
export const formatRelativeTime = (ms: number, now: number): string => {
  const elapsed = now - ms;
  if (elapsed < MINUTE) {
    return "just now";
  }
  const units: [Intl.RelativeTimeFormatUnit, number][] = [
    ["year", 365 * DAY],
    ["month", 30 * DAY],
    ["day", DAY],
    ["hour", HOUR],
    ["minute", MINUTE],
  ];
  for (const [unit, size] of units) {
    if (elapsed >= size) {
      return relativeFormat.format(-Math.floor(elapsed / size), unit);
    }
  }
  return "just now";
};

/// `512 B` / `4 KB` / `1.5 MB`. Uses 1024 units with at most one decimal digit (`.0` is not appended).
export const formatFileSize = (bytes: number): string => {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  // 1023.95 or more rounds to 1024 with one decimal digit, so carry over to the next unit
  while (value >= 1023.95 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  const rounded = Math.round(value * 10) / 10;
  return `${Number.isInteger(rounded) ? rounded : rounded.toFixed(1)} ${units[unit]}`;
};
