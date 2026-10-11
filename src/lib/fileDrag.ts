/**
 * Drag & drop of a query file from the FILES pane to the CONNECTIONS pane.
 *
 * The key point is using a custom MIME type. `dataTransfer.getData()` can only be read
 * on drop (the spec blocks it during dragover), but **the list of types (`types`) can
 * be read even during dragover**. So whether a drop is allowed and the highlight are decided
 * by type, and the contents are retrieved on drop. With text/plain alone, drags of unrelated
 * text coming from outside would be accepted too.
 */

export const FILE_DRAG_MIME = "application/x-queryfolio-query-file";

export interface FileDragPayload {
  /// Name of the source connection (where it is moved from)
  connection: string;
  /// Query file name (with extension)
  fileName: string;
}

/// Put it into dataTransfer when the drag starts.
export const setFileDragPayload = (
  dataTransfer: DataTransfer,
  payload: FileDragPayload,
): void => {
  dataTransfer.setData(FILE_DRAG_MIME, JSON.stringify(payload));
  // Also add plain text so that the file name is inserted when dropped onto an editor, etc.
  dataTransfer.setData("text/plain", payload.fileName);
  dataTransfer.effectAllowed = "move";
};

/// Whether the data being dragged is a query file (used in dragover).
export const hasFileDragPayload = (dataTransfer: DataTransfer | null): boolean =>
  !!dataTransfer && Array.from(dataTransfer.types).includes(FILE_DRAG_MIME);

/// Retrieve on drop. Returns null if the contents are broken.
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
    // If it is not JSON, ignore the drop
  }
  return null;
};

/// Whether this is a file drag from the OS (Finder / Explorer).
///
/// Because `tauri.conf.json` sets `dragDropEnabled: false`, drops of external files also
/// fall through to the WebView's default behavior. The default behavior is "navigate to that
/// file", which replaces the whole SPA and loses the state being edited. The app does not
/// accept file drops, so `+layout.svelte` uses this to detect them and swallow them.
export const isExternalFileDrag = (dataTransfer: DataTransfer | null): boolean =>
  !!dataTransfer && Array.from(dataTransfer.types).includes("Files");
