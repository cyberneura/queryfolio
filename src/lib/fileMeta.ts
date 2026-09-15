// FILES ペインの各行に出す更新日時とサイズの表示用フォーマッタ (CYBERNEURA-DEV-774)。
// Svelte / Tauri に依存しない純粋な関数にしてある。

const pad = (n: number) => String(n).padStart(2, "0");

/// ローカル時刻の `YYYY-mm-dd HH:MM`
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

/// `3 days ago` のような相対表記。1 分未満は `just now`。
/// 月と年は暦ではなく 30 日 / 365 日で数える (一覧の目安なので厳密さは要らない)。
/// 時計のずれで未来の日時が来ても `in 5 minutes` とはせず `just now` に丸める
/// (外部で書かれたファイルの mtime がこの PC の時計より進んでいることがある)。
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

/// `512 B` / `4 KB` / `1.5 MB`。1024 単位で、小数は 1 桁まで (`.0` は付けない)。
export const formatFileSize = (bytes: number): string => {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  // 1023.95 以上は小数 1 桁に丸めると 1024 になるので、上の単位へ繰り上げる
  while (value >= 1023.95 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  const rounded = Math.round(value * 10) / 10;
  return `${Number.isInteger(rounded) ? rounded : rounded.toFixed(1)} ${units[unit]}`;
};
