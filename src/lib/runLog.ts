import type { QueryResult } from "$lib/api";
import { toTsvCapped } from "$lib/export";

/// 書き戻したログブロックの先頭に置くマーカー (U+1F5D2 + 異体字セレクタ)。
/// 既存ブロックの検出はセレクタ無しの U+1F5D2 で行うため、手で消されても拾える
export const RUN_LOG_RESULT_MARKER = "\u{1F5D2}\u{FE0F}";

/// この行数以上の結果は、書き戻す前に確認ダイアログを出す
export const RUN_LOG_CONFIRM_ROWS = 500;

/// 書き戻す TSV の総文字数の上限。
/// **行数の確認ダイアログだけでは足りない** — セルの文字数に上限が無いため、
/// 499 行でも長い TEXT / JSON 列があれば数百 MB になり、CodeMirror への
/// 挿入と自動保存でアプリが固まる。超えた分は打ち切って本文にその旨を書く
const MAX_BODY_CHARS = 200_000;

/// 書き戻す TSV の 1 セルあたりの文字数の上限。
/// 総量の上限だけでは 1 行が巨大なケース (列数 × 長い TEXT) を防げない
const MAX_CELL_CHARS = 2_000;

/// ログのラベル (マーカー行のうち 📝 より後ろ) の長さ上限。
/// 1 行コメント全部が見出しになると読みにくいため切り詰める
const MAX_LABEL_CHARS = 200;

/// 実行対象と、そこに付いていた 📝 マーカーの情報。
/// エディタ (SqlEditor) が実行時に作り、結果が返った後の書き戻しに使う
export interface RunTarget {
  /// 実行する SQL (エディタ上の [from, to) のテキスト)
  sql: string;
  from: number;
  to: number;
  /// 📝 マーカーがあればそのラベル (マーカー無しなら null。ラベル省略時は空文字)
  logLabel: string | null;
}

/// 行コメント (`--`) の行か
const isLineComment = (line: string): boolean => line.trimStart().startsWith("--");

/// 行コメントが 📝 (U+1F4DD) マーカー行なら、そのラベル (📝 より後ろ) を返す。
/// マーカー行でなければ null。マーカーはコメント記号の直後に置く必要がある
/// (コメント本文の途中に出てくる 📝 はマーカーとみなさない)
const markerLabel = (line: string): string | null => {
  const match = line.match(/^\s*--+\s*\u{1F4DD}\u{FE0F}?\s*(.*)$/u);
  if (!match) {
    return null;
  }
  return match[1].trim().slice(0, MAX_LABEL_CHARS);
};

/// 位置 from を含む行の番号 (0 始まり) を返す
const lineIndexOf = (lines: string[], offset: number): number => {
  let start = 0;
  for (let i = 0; i < lines.length; i++) {
    // +1 は行末の改行分。行末位置 (改行の直前) は同じ行に含める
    const end = start + lines[i].length;
    if (offset <= end) {
      return i;
    }
    start = end + 1;
  }
  return lines.length - 1;
};

/// エディタ本文 doc の中で、実行対象 [from, to) に付いた
/// `-- 📝 <label>` マーカーを探す。無ければ null。
///
/// 見るのは実行対象の直前に**連続する**行コメントだけ (間に空行や別の文が
/// あれば、それは別の文に付いたマーカーなので対象外)。加えて実行対象の
/// 先頭がコメント行のこともある (lang-sql が直前のコメントを Statement に
/// 含める場合) ため、範囲の先頭側に続くコメント行も同じ並びとして扱う
/// (こちらは空行が挟まっていても同じ並びとみなす。理由は下記)。
///
/// マーカー行が複数あれば SQL に最も近いものを採用する。
export const findRunLogLabel = (
  doc: string,
  from: number,
  to: number,
): string | null => {
  const lines = doc.split("\n");
  const startLine = lineIndexOf(lines, from);
  // 実行範囲の先頭に含まれるコメント行を読み飛ばし、SQL 本体の開始行を得る。
  //
  // ここで**空行も読み飛ばす**のは、実行範囲の中では空行が並びの切れ目に
  // ならないため。lang-sql は中身の無い `--` の行を LineComment として
  // 扱わないので、そこから SQL までが丸ごと 1 つの Statement になる
  // (説明のコメント → 空行 → `-- 📝 ラベル` → SQL という書き方は普通に
  // ありうる)。空行で止めるとマーカー行まで辿り着けない (CYBERNEURA-DEV-516)。
  //
  // 読み飛ばしを実行範囲の中に閉じるために lastLine で止める。範囲の外へ
  // 出ると、次の文に付いたマーカーを自分のものとして拾ってしまう。
  //
  // to は範囲の終端 (排他) なので、行を引くのは to - 1。to をそのまま渡すと、
  // to がちょうど行頭に来た時に範囲外の行まで含んでしまう
  const lastLine = to > from ? lineIndexOf(lines, to - 1) : startLine;
  let end = startLine;
  while (
    end <= lastLine &&
    (isLineComment(lines[end]) || lines[end].trim() === "")
  ) {
    end++;
  }
  // その直前に連続するコメント行まで遡る。
  // こちらは実行範囲の外なので、空行は従来どおり並びの切れ目として扱う
  let begin = startLine;
  while (begin > 0 && isLineComment(lines[begin - 1])) {
    begin--;
  }
  for (let i = end - 1; i >= begin; i--) {
    const label = markerLabel(lines[i]);
    if (label !== null) {
      return label;
    }
  }
  return null;
};

/// SQL のブロックコメントとして貼るテキストを無害化する。
///
/// `*/` を残すとコメントがそこで閉じ、続きのデータ行がそのまま SQL として
/// 実行されてしまう。`/*` も残せない — PostgreSQL のブロックコメントは
/// **入れ子になる**ため、閉じられない `/*` があるとコメントが終わらず
/// 以降のファイル全体を飲み込む。
const escapeBlockComment = (text: string): string =>
  text.replace(/\/\*/g, "/ *").replace(/\*\//g, "* /");

/// ログブロックの見出しに入れる実行時刻 (ローカル時刻の YYYY-MM-DD HH:MM:SS)
export const formatRunLogTimestamp = (date: Date): string => {
  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
    ` ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  );
};

/// 結果をログの本文 (TSV) にする。
/// 行を返さない文 (INSERT 等) は影響行数を書く。全件でない場合は理由ごとに
/// その旨を残す (後から読んだ人が全件だと誤解しないように)。
/// 3 つは同時に起こりうるので、独立した注記として並べる
export const runLogBody = (result: QueryResult): string => {
  if (result.columns.length === 0) {
    return result.affected_rows === null
      ? "(no rows)"
      : `(${result.affected_rows} rows affected)`;
  }
  const { text, truncated } = toTsvCapped(result, MAX_BODY_CHARS, MAX_CELL_CHARS);
  const lines = [text];
  if (result.applied_limit !== null) {
    lines.push(`(limited to ${result.applied_limit} rows)`);
  }
  if (result.truncated) {
    lines.push("(the result itself was truncated)");
  }
  if (truncated) {
    lines.push("(this log was truncated — see the result table for the full output)");
  }
  return lines.join("\n");
};

/// ログブロック (SQL のブロックコメント) を組み立てる
export const formatRunLogBlock = (
  label: string,
  timestamp: string,
  body: string,
): string => {
  const heading = label
    ? `${RUN_LOG_RESULT_MARKER} ${label} ${timestamp}`
    : `${RUN_LOG_RESULT_MARKER} ${timestamp}`;
  return `/* ${escapeBlockComment(heading)}\n${escapeBlockComment(body)}\n*/`;
};

/// ログブロックの開始 (`/* 🗒️`)。異体字セレクタは任意
const RUN_LOG_BLOCK_OPEN = /^\/\*\s*\u{1F5D2}/u;

/// pos から続く空白のうち、**完全な空行だけ**を読み飛ばした位置を返す
/// (最後に越えた改行の直後。空行が 1 つも無ければ pos のまま)。
///
/// 空白をそのまま読み飛ばすと、次の行の字下げまで置換範囲に入ってしまい、
/// 書き戻すたびに無関係な行のインデントが消える。
const skipBlankLines = (doc: string, pos: number): number => {
  let lineStart = pos;
  for (let i = pos; i < doc.length && /\s/.test(doc[i]); i++) {
    if (doc[i] === "\n") {
      lineStart = i + 1;
    }
  }
  return lineStart;
};

/// エディタへ書き戻す変更 (置換範囲と挿入テキスト)
export interface RunLogWrite {
  from: number;
  to: number;
  insert: string;
}

/// 実行対象の直後へログブロックを書く変更を組み立てる。
///
/// 直後に既存のログブロックがあれば置き換える (runandlog と同じで、何度
/// 実行してもブロックは 1 つしか残らない)。無ければ挿入する。前後の空行は
/// 1 行に正規化するので、同じ結果を書けば同じテキストになる。
///
/// 既存ブロックが `*/` で閉じられていない場合は null を返す (壊れた
/// コメントの前に書き足すと入れ子が増えるだけなので、書かずに知らせる)。
export const runLogWrite = (
  doc: string,
  statementTo: number,
  block: string,
): RunLogWrite | null => {
  // 文末の空白と `;` は文の一部として扱い、その後ろに書く
  // (Statement 範囲が `;` を含まない場合に、セミコロンの手前へ
  // ブロックを挟み込まないため)
  let anchor = statementTo;
  while (anchor < doc.length && /[ \t;]/.test(doc[anchor])) {
    anchor++;
  }
  // 続く空白を飛ばした先が既存のログブロックか (字下げされていても拾う)
  let contentStart = anchor;
  while (contentStart < doc.length && /\s/.test(doc[contentStart])) {
    contentStart++;
  }
  // 既存ブロックが無い場合も、間の空行は置換範囲に含める
  // (毎回同じ空行数に揃え、書き戻しを繰り返しても空行が増えないようにする)
  let to = skipBlankLines(doc, anchor);
  if (RUN_LOG_BLOCK_OPEN.test(doc.slice(contentStart, contentStart + 16))) {
    // 本文中の `*/` は escapeBlockComment で潰してあるので、
    // 最初に見つかる `*/` がこのブロックの終端
    const close = doc.indexOf("*/", contentStart);
    if (close < 0) {
      return null;
    }
    to = skipBlankLines(doc, close + 2);
  }
  return { from: anchor, to, insert: `\n\n${block}\n\n` };
};
