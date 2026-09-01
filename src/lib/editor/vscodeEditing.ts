import { EditorState } from "@codemirror/state";
import type { Extension } from "@codemirror/state";
import { EditorView, keymap, rectangularSelection } from "@codemirror/view";
import type { KeyBinding } from "@codemirror/view";
import { selectLine } from "@codemirror/commands";
import { highlightSelectionMatches } from "@codemirror/search";

/// CodeMirror を VSCode 互換のマルチカーソル / 複数選択で使えるようにする拡張
/// (CYBERNEURA-DEV-647)。
///
/// CodeMirror 6 の `defaultKeymap` には Mod-Alt-ArrowUp / ArrowDown
/// (カーソルの追加) が、`searchKeymap` には Mod-d (次の一致を選択に追加) と
/// Mod-Shift-l (一致をすべて選択) が最初から入っている。それでも複数カーソルが
/// 使えなかったのは `EditorState.allowMultipleSelections` が未設定だったため:
/// この facet が false のままだと、複数レンジを持つ選択がトランザクションの
/// 時点で主レンジ 1 つに畳まれる。コマンドは成功するのにカーソルが増えない、
/// という分かりにくい壊れ方をするので、必ずこの拡張ごと入れること。
///
/// マウス操作は CodeMirror の既定が VSCode と違うので合わせている。
/// - カーソルの追加: 既定は macOS が Cmd+click / それ以外が Ctrl+click。
///   VSCode はどちらも Alt (Option) +click なので上書きする。
/// - 矩形選択: `rectangularSelection` の既定は Alt+drag だが、それだと上の
///   Alt+click と同じ修飾になってしまう。VSCode に合わせて Shift+Alt+drag にする。

/// VSCode にあって CodeMirror の既定キーマップに無いものだけを足す。
/// 既定で足りているもの (Mod-Alt-ArrowUp/Down, Mod-/, Mod-[ , Mod-] ,
/// Shift-Mod-k, Alt-ArrowUp/Down, Shift-Alt-ArrowUp/Down) は重複させない。
export const vscodeExtraKeymap: readonly KeyBinding[] = [
  // VSCode の「現在行を選択」。CodeMirror の既定は Alt-l で、そちらも残す
  { key: "Mod-l", run: selectLine, preventDefault: true },
];

/// SqlEditor / ConfigEditorModal の両方から使う共通拡張。
/// `searchKeymap` (Mod-d / Mod-Shift-l) は検索パネルとセットなので、
/// ここには含めず各エディタ側で `search()` と一緒に入れる。
export const vscodeMultiSelection: Extension[] = [
  EditorState.allowMultipleSelections.of(true),
  // Alt+click でカーソルを追加する (VSCode 互換)
  EditorView.clickAddsSelectionRange.of((e) => e.altKey),
  // Alt+drag は「選択範囲の追加」として使いたいので、テキストのドラッグ移動に
  // 取られないようにする (既定は macOS が Alt、それ以外が Ctrl で無効化)
  EditorView.dragMovesSelection.of((e) => !e.altKey),
  // Shift+Alt+drag で矩形選択 (VSCode 互換)。button == 0 = 左ボタン
  rectangularSelection({
    eventFilter: (e) => e.altKey && e.shiftKey && e.button === 0,
  }),
  // 選択語と同じ語をハイライトする。Mod-d を連打して選択を伸ばす時に、
  // 次にどこが選ばれるかが見える
  highlightSelectionMatches(),
  keymap.of([...vscodeExtraKeymap]),
];
