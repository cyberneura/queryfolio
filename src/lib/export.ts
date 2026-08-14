import type { QueryResult } from "$lib/api";

const cellToString = (value: unknown): string => {
  if (value === null || value === undefined) {
    return "";
  }
  if (typeof value === "object") {
    return JSON.stringify(value);
  }
  return String(value);
};

// 表計算ソフトへの貼り付けで数式として解釈される文字。
// DB 由来の文字列値のみエスケープ対象とし、数値型 (-1 等) は壊さない。
const FORMULA_TRIGGER = /^[=+\-@\t\r]/;

const escapeFormulaInjection = (value: unknown, text: string): string => {
  if (typeof value === "string" && FORMULA_TRIGGER.test(text)) {
    return `'${text}`;
  }
  return text;
};

// ヘッダ (カラム名 / エイリアス) は常に文字列なので無条件に判定する
const escapeHeaderFormula = (header: string): string =>
  FORMULA_TRIGGER.test(header) ? `'${header}` : header;

const escapeCsvField = (field: string): string => {
  if (/[",\n\r]/.test(field)) {
    return `"${field.replace(/"/g, '""')}"`;
  }
  return field;
};

export const toCsv = (result: QueryResult): string => {
  const lines = [
    result.columns.map((c) => escapeCsvField(escapeHeaderFormula(c))).join(","),
  ];
  for (const row of result.rows) {
    lines.push(
      row
        .map((v) => escapeCsvField(escapeFormulaInjection(v, cellToString(v))))
        .join(","),
    );
  }
  return lines.join("\n");
};

// 結果テーブルの矩形選択範囲。行・列とも 0 始まりの閉区間 (両端を含む)。
export interface CellRange {
  rowStart: number;
  rowEnd: number;
  colStart: number;
  colEnd: number;
}

// 選択範囲だけを CSV 化する (Cmd+C コピー用)。withHeaders でヘッダ行を含める。
// 範囲は呼び出し側で結果サイズにクランプ済みである前提。
export const toCsvRange = (
  result: QueryResult,
  range: CellRange,
  withHeaders: boolean,
): string => {
  const { rowStart, rowEnd, colStart, colEnd } = range;
  const lines: string[] = [];
  if (withHeaders) {
    const header: string[] = [];
    for (let c = colStart; c <= colEnd; c++) {
      header.push(escapeCsvField(escapeHeaderFormula(result.columns[c])));
    }
    lines.push(header.join(","));
  }
  for (let r = rowStart; r <= rowEnd; r++) {
    const row = result.rows[r];
    if (!row) {
      continue;
    }
    const fields: string[] = [];
    for (let c = colStart; c <= colEnd; c++) {
      const v = row[c];
      fields.push(escapeCsvField(escapeFormulaInjection(v, cellToString(v))));
    }
    lines.push(fields.join(","));
  }
  return lines.join("\n");
};

const sanitizeTsv = (field: string) =>
  field.replace(/\t/g, " ").replace(/\r?\n/g, " ");

// 選択範囲だけを TSV 化する (Cmd+C コピー用)。withHeaders でヘッダ行を含める。
// 範囲は呼び出し側で結果サイズにクランプ済みである前提。
export const toTsvRange = (
  result: QueryResult,
  range: CellRange,
  withHeaders: boolean,
): string => {
  const { rowStart, rowEnd, colStart, colEnd } = range;
  const lines: string[] = [];
  if (withHeaders) {
    const header: string[] = [];
    for (let c = colStart; c <= colEnd; c++) {
      header.push(sanitizeTsv(escapeHeaderFormula(result.columns[c])));
    }
    lines.push(header.join("\t"));
  }
  for (let r = rowStart; r <= rowEnd; r++) {
    const row = result.rows[r];
    if (!row) {
      continue;
    }
    const fields: string[] = [];
    for (let c = colStart; c <= colEnd; c++) {
      const v = row[c];
      fields.push(sanitizeTsv(escapeFormulaInjection(v, cellToString(v))));
    }
    lines.push(fields.join("\t"));
  }
  return lines.join("\n");
};

/// 総文字数とセルあたりの文字数に上限を設けた TSV 化。
/// 打ち切ったかどうかを一緒に返す。
///
/// `db.rs` は sqlx 経路でセルの文字数を切り詰めないため、100KB の TEXT や
/// 長い JSON がそのままフロントへ届く。全セルを走査する処理は行数ではなく
/// **総文字数**にコストが比例するので、行数の上限だけでは足りない
/// (499 行 × 100KB のセルでも数百 MB になりうる)。
export const toTsvCapped = (
  result: QueryResult,
  maxChars: number,
  maxCellChars: number,
): { text: string; truncated: boolean } => {
  let truncated = false;
  const cap = (field: string): string => {
    if (field.length <= maxCellChars) {
      return field;
    }
    truncated = true;
    return `${field.slice(0, maxCellChars)}…`;
  };
  const lines = [
    result.columns.map((c) => cap(sanitizeTsv(escapeHeaderFormula(c)))).join("\t"),
  ];
  let total = lines[0].length;
  for (const row of result.rows) {
    if (total >= maxChars) {
      truncated = true;
      break;
    }
    const line = row
      .map((v) => cap(sanitizeTsv(escapeFormulaInjection(v, cellToString(v)))))
      .join("\t");
    lines.push(line);
    total += line.length + 1;
  }
  return { text: lines.join("\n"), truncated };
};

export const toTsv = (result: QueryResult): string => {
  const sanitize = sanitizeTsv;
  const lines = [
    result.columns.map((c) => sanitize(escapeHeaderFormula(c))).join("\t"),
  ];
  for (const row of result.rows) {
    lines.push(
      row
        .map((v) => sanitize(escapeFormulaInjection(v, cellToString(v))))
        .join("\t"),
    );
  }
  return lines.join("\n");
};

// JOIN 等で同名カラムが並んだ場合に後勝ちで値が消えないよう、
// 2 個目以降に _2, _3 ... を付けて一意化する。
const uniqueColumnKeys = (columns: string[]): string[] => {
  const counts = new Map<string, number>();
  return columns.map((column) => {
    const seen = counts.get(column) ?? 0;
    counts.set(column, seen + 1);
    return seen === 0 ? column : `${column}_${seen + 1}`;
  });
};

export const toJson = (result: QueryResult): string => {
  const keys = uniqueColumnKeys(result.columns);
  const records = result.rows.map((row) =>
    Object.fromEntries(keys.map((key, i) => [key, row[i]])),
  );
  return JSON.stringify(records, null, 2);
};

// 選択範囲だけを JSON 化する (Cmd+C コピー用)。選択列だけをキーにしたオブジェクト配列。
// withHeaders は JSON では常にキーを持つため無視する (呼び出し側の分岐を単純化するための引数)。
// 範囲は呼び出し側で結果サイズにクランプ済みである前提。
export const toJsonRange = (
  result: QueryResult,
  range: CellRange,
  _withHeaders: boolean,
): string => {
  const { rowStart, rowEnd, colStart, colEnd } = range;
  const cols = result.columns.slice(colStart, colEnd + 1);
  const keys = uniqueColumnKeys(cols);
  const records: Record<string, unknown>[] = [];
  for (let r = rowStart; r <= rowEnd; r++) {
    const row = result.rows[r];
    if (!row) {
      continue;
    }
    records.push(
      Object.fromEntries(keys.map((key, i) => [key, row[colStart + i]])),
    );
  }
  return JSON.stringify(records, null, 2);
};
