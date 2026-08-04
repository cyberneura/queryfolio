/**
 * FILES ペインから CONNECTIONS ペインへのクエリファイルのドラッグ & ドロップ。
 *
 * 独自の MIME タイプを使うのが要点。`dataTransfer.getData()` は drop の時にしか
 * 読めない (dragover では仕様上ブロックされる) が、**タイプの一覧 (`types`) は
 * dragover でも読める**。したがってドロップ可否の判定とハイライトは type で行い、
 * 中身は drop で取り出す。text/plain だけだと、外部から流れてきた無関係な
 * テキストのドラッグまで受け入れてしまう。
 */

export const FILE_DRAG_MIME = "application/x-queryfolio-query-file";

export interface FileDragPayload {
  /// ドラッグ元の接続名 (移動元)
  connection: string;
  /// クエリファイル名 (拡張子付き)
  fileName: string;
}

/// ドラッグ開始時に dataTransfer へ積む。
export const setFileDragPayload = (
  dataTransfer: DataTransfer,
  payload: FileDragPayload,
): void => {
  dataTransfer.setData(FILE_DRAG_MIME, JSON.stringify(payload));
  // エディタ等へ落とした時にファイル名が入るよう、素のテキストも入れておく
  dataTransfer.setData("text/plain", payload.fileName);
  dataTransfer.effectAllowed = "move";
};

/// ドラッグ中のデータがクエリファイルかどうか (dragover で使う)。
export const hasFileDragPayload = (dataTransfer: DataTransfer | null): boolean =>
  !!dataTransfer && Array.from(dataTransfer.types).includes(FILE_DRAG_MIME);

/// drop 時に取り出す。中身が壊れていたら null。
export const readFileDragPayload = (
  dataTransfer: DataTransfer | null,
): FileDragPayload | null => {
  const raw = dataTransfer?.getData(FILE_DRAG_MIME);
  if (!raw) {
    return null;
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (
      typeof parsed === "object" &&
      parsed !== null &&
      typeof (parsed as FileDragPayload).connection === "string" &&
      typeof (parsed as FileDragPayload).fileName === "string"
    ) {
      return parsed as FileDragPayload;
    }
  } catch {
    // JSON でなければドロップを無視する
  }
  return null;
};
