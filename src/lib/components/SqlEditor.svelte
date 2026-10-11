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
  import { search, searchKeymap } from "@codemirror/search";
  import { syntaxTree, HighlightStyle, syntaxHighlighting } from "@codemirror/language";
  import { tags as t } from "@lezer/highlight";
  import { acceptCompletion, autocompletion, completionKeymap } from "@codemirror/autocomplete";
  import { sql, MSSQL, MySQL, PostgreSQL, SQLite } from "@codemirror/lang-sql";
  import type { SQLNamespace } from "@codemirror/lang-sql";
  import { oneDark } from "@codemirror/theme-one-dark";
  import { formatSql } from "$lib/sqlFormat";
  import { vscodeMultiSelection } from "$lib/editor/vscodeEditing";
  import { redisLanguage } from "$lib/editor/redisLanguage";
  import { esLanguage } from "$lib/editor/esLanguage";
  import { findRunLogLabel, planRunLogWrite } from "$lib/runLog";
  import type { RunLogOutcome, RunTarget } from "$lib/runLog";

  interface Props {
    content: string;
    engine: string | null;
    /// Editor language (capabilities.editor_language). null is treated as "sql"
    editorLanguage: string | null;
    /// Table name -> column name list for schema-based completion (null if not fetched yet)
    schemaMap: Record<string, string[]> | null;
    onChange: (content: string) => void;
    /// Passes the execution target (the SQL, its range in the editor, and whether there is a 📝 marker).
    /// After the result comes back, the caller returns the same target via writeRunLog()
    onRun: (target: RunTarget) => void;
    /// Called every time the selection changes. Notifies whether it spans multiple lines (the display
    /// condition of the Replace Multiline button). The selected text itself is snapshotted with
    /// getMainSelection() at the time the dialog opens, so it is not passed here
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

  /// Whether the language executes line by line (redis: 1 line = 1 command).
  /// SQL executes per Statement of the syntax tree
  const isLineBased = () => editorLanguage === "redis";

  /// Whether the language executes per request block (es: method line + JSON body)
  const isBlockBased = () => editorLanguage === "es";

  /// Whether it is the SQL language (a target of SQL-only processing such as Format). null is treated as "sql"
  const isSqlLanguage = () =>
    editorLanguage === null || editorLanguage === "sql";

  let editorElement: HTMLDivElement;
  let view: EditorView | null = null;
  const languageCompartment = new Compartment();

  /// Upper limit on the number of tables used for schema completion. Beyond it, columns are not
  /// passed and only table names are used (limits the build cost and memory of completion candidates)
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
      case "mssql":
      case "sqlserver":
        return MSSQL;
      default:
        return PostgreSQL;
    }
  };

  // Convert the schema map into the schema option of lang-sql.
  // Record<string, string[]> can be passed as is as a SQLNamespace
  // (dotted keys such as PostgreSQL's "schema.table" are split into a hierarchy
  // on the lang-sql side). For a huge schema, omit the columns and keep only table names.
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

  // The language extension put into languageCompartment. Switched by the engine's editor_language
  // (sql: dialect + schema completion. Reserved-word completion is inserted in uppercase, following SQL convention).
  // When adding a new editor language, add a branch here
  const languageExtension = (
    language: string | null,
    engineName: string | null,
    map: Record<string, string[]> | null,
  ) => {
    if (language === "redis") {
      return redisLanguage;
    }
    if (language === "es") {
      return esLanguage;
    }
    return sql({
      dialect: dialectFor(engineName),
      schema: schemaNamespace(map),
      upperCaseKeywords: true,
    });
  };

  // Return the range of the Statement node that contains the cursor.
  // If the cursor is between two statements, return the previous one (same behavior as common SQL editors).
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

  // Whether it is an ES method line (the first line of a request block).
  // Same rule as the backend (leading_method in elasticsearch.rs):
  // if the first token of the line is an HTTP method, it is a method line (a line of the JSON body
  // never starts with a method name)
  const ES_METHOD_LINE = /^(GET|POST|PUT|DELETE|HEAD|PATCH)(\s|$)/i;

  // Return the range of the ES request block that contains the cursor.
  // Search upward from the cursor line for the first method line, then downward from there
  // up to just before the next method line (or EOF). If there is no method line
  // upward, null (no execution target).
  const esBlockRange = (
    state: EditorState,
    pos: number,
  ): { from: number; to: number } | null => {
    const doc = state.doc;
    const cursorLine = doc.lineAt(pos).number;
    let startLine: number | null = null;
    for (let n = cursorLine; n >= 1; n--) {
      if (ES_METHOD_LINE.test(doc.line(n).text.trimStart())) {
        startLine = n;
        break;
      }
    }
    if (startLine === null) {
      return null;
    }
    let endLine = doc.lines;
    for (let n = startLine + 1; n <= doc.lines; n++) {
      if (ES_METHOD_LINE.test(doc.line(n).text.trimStart())) {
        endLine = n - 1;
        break;
      }
    }
    // Do not include trailing blank lines in the block (so the highlight does not stretch)
    while (endLine > startLine && doc.line(endLine).text.trim() === "") {
      endLine--;
    }
    const first = trimmedLineRange(state, doc.line(startLine).from);
    const last = trimmedLineRange(state, doc.line(endLine).from);
    if (!first || !last) {
      return null;
    }
    return { from: first.from, to: last.to };
  };

  // Return the range of the trimmed text of a line (null for a blank line)
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

  // Return the range of the execution target. **Always use this same range for both highlighting and execution**
  // (using separate logic would cause a bug where the displayed and the executed SQL diverge).
  //
  // Note: lezer's error recovery parses "\d" as ⚠(backslash) + Statement("d"),
  // so using the Statement range as is would execute SQL with the backslash
  // missing. Correct it with the following rules:
  // - If the cursor line starts with \ and there is no Statement containing the cursor, or
  //   that Statement's start line also starts with \ (a misparse caused by a meta line), use the
  //   trimmed range of the cursor line as the execution target
  // - Otherwise use the Statement range, but if what precedes the range (from the line start to
  //   the Statement start) is only whitespace and backslashes, extend the range toward the line
  //   start to include the backslash (when the previous-statement fallback returns a meta line)
  const executionTargetRange = (
    state: EditorState,
    pos: number,
  ): { from: number; to: number } | null => {
    // Line-based language (redis): if there is a selection, use it (batch execution of multiple
    // lines); otherwise use the cursor line. If the cursor line is empty, fall back to the previous
    // non-empty line (same behavior as the SQL "previous statement")
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
    // Block-based language (es): if there is a selection, use it; otherwise use the request block
    // containing the cursor (method line + body)
    if (isBlockBased()) {
      const sel = state.selection.main;
      if (!sel.empty) {
        return { from: sel.from, to: sel.to };
      }
      return esBlockRange(state, pos);
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

    // Include the backslash (error token) just before the Statement in the range
    const startLine = state.doc.lineAt(range.from);
    const beforeStatement = state.sliceDoc(startLine.from, range.from);
    const match = beforeStatement.match(/^(\s*)\\+$/);
    if (match) {
      return { from: startLine.from + match[1].length, to: range.to };
    }
    return range;
  };

  // Notify whether the current main selection spans multiple lines
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

  // Snapshot of the current main selection (taken when Replace Multiline is opened).
  // Later, verify by text that the range has not shifted due to document edits
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

  // Public method that replaces the snapshotted range [from, to) with text.
  // It runs only when the current text of that range matches expected; if the range has shifted
  // because of a file switch or edit, it does nothing and returns false (prevents inserting into
  // or destroying an unrelated place). After replacing, the whole inserted text is selected
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

  /// Extract the execution target together with the information needed for writing back (range and 📝 label).
  /// null if there is no execution target / it is only whitespace.
  ///
  /// The log write-back uses a SQL block comment (`/* ... */`), so
  /// the marker is looked at only in SQL-language editors (redis / es differ in both the line
  /// comment syntax and the block comment)
  const runTargetAt = (state: EditorState): RunTarget | null => {
    const range = executionTargetRange(state, state.selection.main.head);
    if (!range) {
      return null;
    }
    const sql = state.sliceDoc(range.from, range.to);
    if (!sql.trim()) {
      return null;
    }
    return {
      sql,
      from: range.from,
      to: range.to,
      logLabel: isSqlLanguage()
        ? findRunLogLabel(state.doc.toString(), range.from, range.to)
        : null,
    };
  };

  /// Public method that writes the result-log block comment back right after the executed statement.
  /// For deciding whether it can be written (range verification and re-fetching the label),
  /// see planRunLogWrite (runLog.ts).
  ///
  /// The cursor position and focus are not moved — the write-back is an asynchronous insertion
  /// after execution completes, and the user may be editing another place in the meantime.
  export function writeRunLog(
    target: RunTarget,
    buildBlock: (label: string) => string,
  ): RunLogOutcome {
    if (!view) {
      return "stale";
    }
    const write = planRunLogWrite(view.state.doc.toString(), target, buildBlock);
    if (typeof write === "string") {
      return write;
    }
    view.dispatch({
      changes: { from: write.from, to: write.to, insert: write.insert },
    });
    return "written";
  }

  // Highlight plugin that adds a border and background to the line of the statement at the cursor
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
        // Highlight the same range as the execution target (prevents display and execution from diverging)
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

  // Public method to be called from the toolbar's Run button
  export function runCurrentStatement() {
    if (!view) {
      return;
    }
    const target = runTargetAt(view.state);
    if (target) {
      onRun(target);
    }
  }

  // Public method that returns the statement at the cursor (the same range as the execution target).
  // Used by the toolbar's Explain button to get the statement for EXPLAIN
  export function getCurrentStatement(): string {
    return view ? currentStatementText(view.state) : "";
  }

  // Public method that formats the statement at the cursor and replaces its range with the result.
  // When it cannot be formatted (unsupported syntax, risk of breaking it), formatSql returns the
  // original text, so nothing changes and nothing is done.
  export function formatCurrentStatement() {
    // The SQL formatter is SQL-only (the Format button itself is also hidden by capability)
    if (!view || !isSqlLanguage()) {
      return;
    }
    const state = view.state;
    const range = executionTargetRange(state, state.selection.main.head);
    if (!range) {
      return;
    }
    const original = state.sliceDoc(range.from, range.to);
    // T-SQL breaks unless the formatter lexes bracket identifiers and #temp (sqlFormat.ts)
    const dialect = /^(mssql|sqlserver)$/i.test(engine ?? "") ? "mssql" : undefined;
    const formatted = formatSql(original, dialect);
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
        const target = runTargetAt(editorView.state);
        if (target) {
          onRun(target);
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
          // Show the search panel at the top of the editor (aligned with ConfigEditorModal)
          search({ top: true }),
          // VSCode-compatible multi-cursor / multiple selections (Alt+click, Shift+Alt+drag,
          // Mod-l). Mod-d / Mod-Shift-l are in the searchKeymap below
          vscodeMultiSelection,
          // Evaluate Mod-Enter before defaultKeymap
          runKeymap,
          // Put searchKeymap before defaultKeymap. Escape is in both, and
          // while the search panel is open we want "close the panel" to win
          // (if there is no panel to close it returns false and falls through to defaultKeymap's
          // simplifySelection)
          keymap.of(searchKeymap),
          keymap.of([
            ...defaultKeymap,
            ...historyKeymap,
            ...completionKeymap,
            // While completion candidates are shown, Tab confirms the candidate (same as VSCode). Added because completionKeymap
            // confirms only with Enter. With no candidates, acceptCompletion
            // returns false and the next indentWithTab indents
            { key: "Tab", run: acceptCompletion },
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
            // Notify of selection changes (including shifts caused by document changes)
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

  // Reflect it in the editor when content is changed externally, e.g. by switching files
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

  // Swap the editor language and the completion schema when the engine or schema map changes
  $effect(() => {
    // Evaluate before the view guard so the dependencies are tracked as reactive dependencies
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
  /* oneDark theme background rules can win over the EditorView.theme override
     due to CodeMirror's theme precedence, so override reliably with CSS */
  .sql-editor-host :global(.cm-editor),
  .sql-editor-host :global(.cm-gutters) {
    background-color: #111111 !important;
  }
</style>
