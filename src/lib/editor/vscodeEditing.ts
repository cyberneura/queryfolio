import { EditorState } from "@codemirror/state";
import type { Extension } from "@codemirror/state";
import { EditorView, keymap, rectangularSelection } from "@codemirror/view";
import type { KeyBinding } from "@codemirror/view";
import { selectLine } from "@codemirror/commands";
import { highlightSelectionMatches } from "@codemirror/search";

/// Extension that makes CodeMirror usable with VSCode-compatible multi-cursor / multiple selections
/// (CYBERNEURA-DEV-647).
///
/// CodeMirror 6's `defaultKeymap` already includes Mod-Alt-ArrowUp / ArrowDown
/// (add cursor), and `searchKeymap` already includes Mod-d (add next match to the selection) and
/// Mod-Shift-l (select all matches). Multiple cursors still did not work because
/// `EditorState.allowMultipleSelections` was not set: while this facet stays false, a selection with
/// multiple ranges is collapsed to the main range at transaction time. The command succeeds but no
/// cursors are added, which is a confusing way to break, so always include this whole extension.
///
/// Mouse handling is aligned because CodeMirror's defaults differ from VSCode.
/// - Adding a cursor: the default is Cmd+click on macOS / Ctrl+click elsewhere.
///   VSCode uses Alt (Option)+click for both, so we override it.
/// - Rectangular selection: the `rectangularSelection` default is Alt+drag, but that would use the
///   same modifier as the Alt+click above. Match VSCode with Shift+Alt+drag.

/// Add only what VSCode has and CodeMirror's default keymap lacks.
/// Do not duplicate what the defaults already cover (Mod-Alt-ArrowUp/Down, Mod-/, Mod-[ , Mod-] ,
/// Shift-Mod-k, Alt-ArrowUp/Down, Shift-Alt-ArrowUp/Down).
export const vscodeExtraKeymap: readonly KeyBinding[] = [
  // VSCode's "select current line". CodeMirror's default is Alt-l, which we keep too
  { key: "Mod-l", run: selectLine, preventDefault: true },
];

/// Shared extension used by both SqlEditor and ConfigEditorModal.
/// `searchKeymap` (Mod-d / Mod-Shift-l) goes together with the search panel, so it is not included
/// here; each editor adds it along with `search()`.
export const vscodeMultiSelection: Extension[] = [
  EditorState.allowMultipleSelections.of(true),
  // Alt+click adds a cursor (VSCode compatible)
  EditorView.clickAddsSelectionRange.of((e) => e.altKey),
  // We want Alt+drag for "adding a selection range", so keep it from being taken as a drag-move of
  // text (by default it is disabled with Alt on macOS and Ctrl elsewhere)
  EditorView.dragMovesSelection.of((e) => !e.altKey),
  // Shift+Alt+drag for rectangular selection (VSCode compatible). button == 0 = left button
  rectangularSelection({
    eventFilter: (e) => e.altKey && e.shiftKey && e.button === 0,
  }),
  // Highlight words matching the selected word. When extending the selection by pressing Mod-d
  // repeatedly, this shows what will be selected next
  highlightSelectionMatches(),
  keymap.of([...vscodeExtraKeymap]),
];
