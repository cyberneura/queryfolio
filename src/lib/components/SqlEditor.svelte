<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { EditorState, Compartment, StateEffect } from "@codemirror/state";
  import {
    EditorView,
    Decoration,
    ViewPlugin,
    ViewUpdate,
    keymap,
    lineNumbers,
    drawSelection,
    highlightActiveLineGutter,
  } from "@codemirror/view";
  import type { DecorationSet } from "@codemirror/view";
  import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
  import { syntaxTree, HighlightStyle, syntaxHighlighting } from "@codemirror/language";
  import { tags as t } from "@lezer/highlight";
  import { autocompletion, completionKeymap } from "@codemirror/autocomplete";
  import { sql, MySQL, PostgreSQL, SQLite } from "@codemirror/lang-sql";
  import type { SQLNamespace } from "@codemirror/lang-sql";
  import { oneDark } from "@codemirror/theme-one-dark";
  import { formatSql } from "$lib/sqlFormat";
  import { redisLanguage } from "$lib/editor/redisLanguage";

  interface Props {
    content: string;
    engine: string | null;
    /// エディタ言語 (capabilities.editor_language)。null は "sql" 扱い
    editorLanguage: string | null;
    /// スキーマベース補完用のテーブル名 → カラム名リスト (未取得なら null)
    schemaMap: Record<string, string[]> | null;
    onChange: (content: string) => void;
    onRun: (sql: string) => void;
    /// 選択範囲が変わるたびに呼ぶ。複数行選択かどうか (Replace Multiline
    /// ボタンの表示条件) を通知する。選択テキスト自体は開く時点で
    /// getMainSelection() で snapshot するのでここでは渡さない
    onSelectionChange?: (info: { hasMultilineSelection: boolean }) => void;
  }

  let {
    content,
    engine,
    editorLanguage,
    schemaMap,
    onChange,
    onRun,
    onSelectionChange,
  }: Props = $props();

  /// 行単位で実行する言語か (redis: 1 行 = 1 コマンド)。
  /// SQL は構文木の Statement 単位で実行する
  const isLineBased = () => editorLanguage === "redis";

  let editorElement: HTMLDivElement;
  let view: EditorView | null = null;
  const languageCompartment = new Compartment();

  /// スキーマ補完に使うテーブル数の上限。超えた場合はカラムを渡さず
  /// テーブル名のみにする (補完候補の構築コストとメモリの抑制)
  const MAX_SCHEMA_TABLES_WITH_COLUMNS = 2000;

  const dialectFor = (engineName: string | null) => {
    switch ((engineName ?? "").toLowerCase()) {
      case "mysql":
      case "mariadb":
        return MySQL;
      case "postgres":
      case "postgresql":
        return PostgreSQL;
      case "sqlite":
      case "sqlite3":
        return SQLite;
      default:
        return PostgreSQL;
    }
  };

  // スキーママップを lang-sql の schema オプションへ変換する。
  // Record<string, string[]> はそのまま SQLNamespace として渡せる
  // (PostgreSQL の "schema.table" のようなドット付きキーは lang-sql 側で
  // 階層に分解される)。巨大スキーマではカラムを省いてテーブル名のみにする。
  const schemaNamespace = (
    map: Record<string, string[]> | null,
  ): SQLNamespace | undefined => {
    if (!map) {
      return undefined;
    }
    const tables = Object.keys(map);
    if (tables.length <= MAX_SCHEMA_TABLES_WITH_COLUMNS) {
      return map;
    }
    return Object.fromEntries(tables.map((table) => [table, []]));
  };

  // languageCompartment に入れる言語拡張。エンジンの editor_language で切り替える
  // (sql: 方言 + スキーマ補完。予約語の補完は大文字で挿入する SQL の慣習に合わせる)。
  // 新しいエディタ言語を追加する時はここに分岐を足す
  const languageExtension = (
    language: string | null,
    engineName: string | null,
    map: Record<string, string[]> | null,
  ) => {
    if (language === "redis") {
      return redisLanguage;
    }
    return sql({
      dialect: dialectFor(engineName),
      schema: schemaNamespace(map),
      upperCaseKeywords: true,
    });
  };

  // カーソル位置を含む Statement ノードの範囲を返す。
  // カーソルが文と文の間にある場合は直前の文を返す (一般的な SQL エディタと同様の挙動)。
  const statementRangeAt = (
    state: EditorState,
    pos: number,
  ): { from: number; to: number } | null => {
    const top = syntaxTree(state).topNode;
    let previous: { from: number; to: number } | null = null;
    for (
      let node = top.firstChild;
      node !== null;
      node = node.nextSibling
    ) {
      if (node.name !== "Statement") {
        continue;
      }
      if (pos >= node.from && pos <= node.to) {
        return { from: node.from, to: node.to };
      }
      if (node.to < pos) {
        previous = { from: node.from, to: node.to };
      }
    }
    return previous;
  };

  // ある行の trim 済みテキストの範囲を返す (空行なら null)
  const trimmedLineRange = (
    state: EditorState,
    pos: number,
  ): { from: number; to: number } | null => {
    const line = state.doc.lineAt(pos);
    const text = state.sliceDoc(line.from, line.to);
    const leading = text.length - text.trimStart().length;
    const trailing = text.length - text.trimEnd().length;
    if (leading + trailing >= text.length) {
      return null;
    }
    return { from: line.from + leading, to: line.to - trailing };
  };

  // 実行対象の範囲を返す。**ハイライトと実行は必ずこの同じ範囲を使うこと**
  // (別ロジックにすると表示と実行される SQL が乖離するバグになる)。
  //
  // 注意: lezer のエラー回復は "\d" を ⚠(バックスラッシュ) + Statement("d")
  // とパースするため、Statement 範囲をそのまま使うとバックスラッシュが
  // 欠落した SQL が実行される。以下のルールで補正する:
  // - カーソル行が \ 始まりで、カーソルを含む Statement が無い、または
  //   その Statement の開始行も \ 始まり (メタ行由来の誤パース) の場合は、
  //   カーソル行の trim 範囲を実行対象にする
  // - それ以外は Statement 範囲を使うが、範囲の直前 (行頭から Statement
  //   開始まで) が空白とバックスラッシュのみなら範囲を行頭側へ拡張して
  //   バックスラッシュを含める (直前の文フォールバックがメタ行を返す場合)
  const executionTargetRange = (
    state: EditorState,
    pos: number,
  ): { from: number; to: number } | null => {
    // 行単位の言語 (redis): 選択があれば選択範囲 (複数行の一括実行)、
    // 無ければカーソル行を実行対象にする。カーソル行が空なら直前の
    // 非空行へフォールバックする (SQL の「直前の文」と同じ挙動)
    if (isLineBased()) {
      const sel = state.selection.main;
      if (!sel.empty) {
        return { from: sel.from, to: sel.to };
      }
      const current = trimmedLineRange(state, pos);
      if (current) {
        return current;
      }
      for (
        let n = state.doc.lineAt(pos).number - 1;
        n >= 1;
        n--
      ) {
        const range = trimmedLineRange(state, state.doc.line(n).from);
        if (range) {
          return range;
        }
      }
      return null;
    }
    const range = statementRangeAt(state, pos);
    const cursorLine = trimmedLineRange(state, pos);
    const cursorLineText = cursorLine
      ? state.sliceDoc(cursorLine.from, cursorLine.to)
      : "";

    if (cursorLineText.startsWith("\\")) {
      const inStatement =
        range !== null && pos >= range.from && pos <= range.to;
      if (!inStatement) {
        return cursorLine;
      }
      const firstLine = trimmedLineRange(state, range.from);
      const firstLineText = firstLine
        ? state.sliceDoc(firstLine.from, firstLine.to)
        : "";
      if (firstLineText.startsWith("\\")) {
        return cursorLine;
      }
    }

    if (!range) {
      return null;
    }

    // Statement 直前のバックスラッシュ (エラートークン) を範囲に含める
    const startLine = state.doc.lineAt(range.from);
    const beforeStatement = state.sliceDoc(startLine.from, range.from);
    const match = beforeStatement.match(/^(\s*)\\+$/);
    if (match) {
      return { from: startLine.from + match[1].length, to: range.to };
    }
    return range;
  };

  // 現在の主選択が複数行にまたがっているかを通知する
  const emitSelectionChange = (state: EditorState) => {
    if (!onSelectionChange) {
      return;
    }
    const sel = state.selection.main;
    const startLine = state.doc.lineAt(sel.from).number;
    const endLine = state.doc.lineAt(sel.to).number;
    onSelectionChange({
      hasMultilineSelection: !sel.empty && endLine > startLine,
    });
  };

  // 現在の主選択のスナップショット (Replace Multiline を開いた時点で取得)。
  // 後で範囲がドキュメント編集でズレていないかを text で照合する
  export function getMainSelection(): {
    from: number;
    to: number;
    text: string;
  } {
    if (!view) {
      return { from: 0, to: 0, text: "" };
    }
    const sel = view.state.selection.main;
    return {
      from: sel.from,
      to: sel.to,
      text: view.state.sliceDoc(sel.from, sel.to),
    };
  }

  // スナップショットした範囲 [from, to) を text で置換する公開メソッド。
  // その範囲の現在テキストが expected と一致する時だけ実行し、ファイル切替や
  // 編集で範囲がズレている場合は何もせず false を返す (無関係な箇所への
  // 誤挿入・破壊を防ぐ)。置換後は挿入テキスト全体を選択状態にする
  export function replaceRangeIfMatches(
    from: number,
    to: number,
    expected: string,
    text: string,
  ): boolean {
    if (!view) {
      return false;
    }
    const docLength = view.state.doc.length;
    if (from < 0 || to < from || to > docLength) {
      return false;
    }
    if (view.state.sliceDoc(from, to) !== expected) {
      return false;
    }
    view.dispatch({
      changes: { from, to, insert: text },
      selection: { anchor: from, head: from + text.length },
    });
    view.focus();
    return true;
  }

  const currentStatementText = (state: EditorState): string => {
    const range = executionTargetRange(state, state.selection.main.head);
    if (!range) {
      return "";
    }
    return state.sliceDoc(range.from, range.to);
  };

  // カーソル位置の文の行に枠線・背景を付けるハイライトプラグイン
  const statementHighlight = ViewPlugin.fromClass(
    class {
      decorations: DecorationSet;

      constructor(view: EditorView) {
        this.decorations = this.build(view);
      }

      update(update: ViewUpdate) {
        if (update.docChanged || update.selectionSet) {
          this.decorations = this.build(update.view);
        }
      }

      build(view: EditorView): DecorationSet {
        // 実行対象と同じ範囲をハイライトする (表示と実行の乖離を防ぐ)
        const range = executionTargetRange(
          view.state,
          view.state.selection.main.head,
        );
        if (!range) {
          return Decoration.none;
        }
        const decorations = [];
        const firstLine = view.state.doc.lineAt(range.from);
        const lastLine = view.state.doc.lineAt(range.to);
        for (let n = firstLine.number; n <= lastLine.number; n++) {
          const line = view.state.doc.line(n);
          let className = "cm-active-statement";
          if (n === firstLine.number) {
            className += " cm-active-statement-first";
          }
          if (n === lastLine.number) {
            className += " cm-active-statement-last";
          }
          decorations.push(
            Decoration.line({ class: className }).range(line.from),
          );
        }
        return Decoration.set(decorations);
      }
    },
    { decorations: (plugin) => plugin.decorations },
  );

  // ツールバーの Run ボタンから呼ぶための公開メソッド
  export function runCurrentStatement() {
    const statement = getCurrentStatement();
    if (statement.trim()) {
      onRun(statement);
    }
  }

  // カーソル位置の文 (実行対象と同じ範囲) を返す公開メソッド。
  // ツールバーの Explain ボタンが EXPLAIN の対象文を取るのに使う
  export function getCurrentStatement(): string {
    return view ? currentStatementText(view.state) : "";
  }

  // カーソル位置の文を整形して、その範囲を整形結果で置換する公開メソッド。
  // 整形できない (未対応構文・壊す恐れ) 場合は formatSql が原文を返すため
  // 変化がなく、何もしない。
  export function formatCurrentStatement() {
    // SQL 整形器は SQL 専用 (Format ボタン自体も capability で隠れる)
    if (!view || isLineBased()) {
      return;
    }
    const state = view.state;
    const range = executionTargetRange(state, state.selection.main.head);
    if (!range) {
      return;
    }
    const original = state.sliceDoc(range.from, range.to);
    const formatted = formatSql(original);
    if (formatted === original) {
      return;
    }
    view.dispatch({
      changes: { from: range.from, to: range.to, insert: formatted },
      selection: { anchor: range.from + formatted.length },
    });
    view.focus();
  }

  const runKeymap = keymap.of([
    {
      key: "Mod-Enter",
      run: (editorView) => {
        const statement = currentStatementText(editorView.state);
        if (statement.trim()) {
          onRun(statement);
        }
        return true;
      },
    },
  ]);

  const editorTheme = EditorView.theme({
    "&": {
      height: "100%",
      fontSize: "13px",
    },
    // Base text (identifiers, table/column names) is brighter than oneDark's default.
    ".cm-content": {
      color: "#f3f5f9",
    },
    ".cm-cursor, .cm-dropCursor": {
      borderLeftColor: "#f3f5f9",
    },
    ".cm-scroller": {
      fontFamily:
        "ui-monospace, SFMono-Regular, Menlo, Monaco, monospace",
    },
    ".cm-active-statement": {
      backgroundColor: "rgba(96, 165, 250, 0.08)",
      borderLeft: "2px solid rgba(96, 165, 250, 0.6)",
      borderRight: "1px solid rgba(96, 165, 250, 0.25)",
    },
    ".cm-active-statement-first": {
      borderTop: "1px solid rgba(96, 165, 250, 0.25)",
    },
    ".cm-active-statement-last": {
      borderBottom: "1px solid rgba(96, 165, 250, 0.25)",
    },
  });

  // Brighter variants of oneDark's syntax palette (overrides oneDark's HighlightStyle).
  const brightHighlightStyle = HighlightStyle.define([
    { tag: [t.keyword, t.operatorKeyword, t.modifier], color: "#eac6ff" },
    { tag: [t.string, t.special(t.string)], color: "#d8f5b0" },
    { tag: [t.number, t.bool, t.null], color: "#ffd7a3" },
    { tag: [t.function(t.variableName), t.function(t.propertyName)], color: "#b3ddff" },
    { tag: [t.name, t.propertyName, t.variableName], color: "#f3f5f9" },
    { tag: [t.comment], color: "#c0c7da", fontStyle: "italic" },
    { tag: [t.operator, t.punctuation, t.separator], color: "#e2e7f0" },
    { tag: [t.typeName, t.className], color: "#ffeab0" },
  ]);

  onMount(() => {
    view = new EditorView({
      state: EditorState.create({
        doc: content,
        extensions: [
          lineNumbers(),
          highlightActiveLineGutter(),
          drawSelection(),
          history(),
          autocompletion(),
          // Mod-Enter を defaultKeymap より先に評価させる
          runKeymap,
          keymap.of([
            ...defaultKeymap,
            ...historyKeymap,
            ...completionKeymap,
            indentWithTab,
          ]),
          languageCompartment.of(
            languageExtension(editorLanguage, engine, schemaMap),
          ),
          oneDark,
          syntaxHighlighting(brightHighlightStyle),
          editorTheme,
          statementHighlight,
          EditorView.updateListener.of((update) => {
            if (update.docChanged) {
              onChange(update.state.doc.toString());
            }
            // 選択の変化 (ドキュメント変更でズレる場合も含む) を通知する
            if (update.selectionSet || update.docChanged) {
              emitSelectionChange(update.state);
            }
          }),
        ],
      }),
      parent: editorElement,
    });
  });

  onDestroy(() => {
    view?.destroy();
    view = null;
  });

  // ファイル切り替え等で外部から content が変わった時にエディタへ反映する
  $effect(() => {
    const nextContent = content;
    if (!view) {
      return;
    }
    const currentDoc = view.state.doc.toString();
    if (nextContent !== currentDoc) {
      view.dispatch({
        changes: { from: 0, to: currentDoc.length, insert: nextContent },
      });
    }
  });

  // エンジン・スキーママップ変更時にエディタ言語と補完スキーマを差し替える
  $effect(() => {
    // view のガードより先に評価し、依存をリアクティブ依存として追跡させる
    const extension = languageExtension(editorLanguage, engine, schemaMap);
    if (!view) {
      return;
    }
    view.dispatch({
      effects: languageCompartment.reconfigure(extension),
    });
  });
</script>

<div
  bind:this={editorElement}
  class="sql-editor-host h-full min-h-0 overflow-hidden"
  data-annotate="editor-sql"
></div>

<style>
  /* oneDark テーマの背景指定は CodeMirror のテーマ優先順位で
     EditorView.theme の上書きに勝つことがあるため、CSS で確実に上書きする */
  .sql-editor-host :global(.cm-editor),
  .sql-editor-host :global(.cm-gutters) {
    background-color: #111111 !important;
  }
</style>
