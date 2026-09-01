<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import { toast } from "svelte-sonner";
  import { EditorState } from "@codemirror/state";
  import {
    EditorView,
    keymap,
    lineNumbers,
    drawSelection,
    highlightActiveLineGutter,
  } from "@codemirror/view";
  import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
  import { search, searchKeymap } from "@codemirror/search";
  import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
  import { tags as t } from "@lezer/highlight";
  import { yaml } from "@codemirror/lang-yaml";
  import { linter, lintGutter, type Diagnostic } from "@codemirror/lint";
  import { parseDocument } from "yaml";
  import { oneDark } from "@codemirror/theme-one-dark";
  import { vscodeMultiSelection } from "$lib/editor/vscodeEditing";
  import {
    readConfigFile,
    readOverrideConfigYaml,
    writeConfigFile,
  } from "$lib/api";
  import appStore from "$lib/stores/app.svelte";

  interface Props {
    /// "config" = config.yml を編集・保存する。
    /// "source" = config_override_command が返す YAML を表示する。
    ///            編集はできるがメモリ上だけで、保存はできない (コピーして使う想定)。
    mode: "config" | "source";
    onClose: () => void;
    /// 未保存の変更の有無を親に知らせる (別のモードへ切り替える時の巻き添え破棄を防ぐ)
    onDirtyChange?: (dirty: boolean) => void;
  }

  let { mode, onClose, onDirtyChange }: Props = $props();

  /// source モードは取得元が外部コマンドなので書き戻せない。編集自体は許可する。
  const canSave = $derived(mode === "config");
  const title = $derived(
    mode === "config" ? "Edit config.yml" : "Override config yaml (Copy only)",
  );

  let editorElement = $state<HTMLDivElement | null>(null);
  let view: EditorView | null = null;
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let saveError = $state<string | null>(null);
  let saving = $state(false);
  let dirty = $state(false);
  /// 未保存の変更がある状態で閉じようとした時に、破棄の確認を出す
  let confirmDiscard = $state(false);

  const editorTheme = EditorView.theme({
    "&": { height: "100%", fontSize: "13px" },
    ".cm-content": { color: "#f3f5f9" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "#f3f5f9" },
    ".cm-scroller": {
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, monospace",
    },
    // 検索パネル (Cmd+F) をモーダルの配色に合わせる。
    // 既定のままだと明るいフォームパーツが暗いエディタの上に乗って浮く。
    ".cm-panels": { backgroundColor: "#18181b", color: "#e4e4e7" },
    ".cm-panels.cm-panels-top": { borderBottom: "1px solid #3f3f46" },
    // フォントサイズは入力欄・ボタン・ラベルに**個別に**指定する。CodeMirror の
    // 基本テーマが `.cm-textfield` / `.cm-button` に `font-size: 70%`、検索パネルの
    // `label` に `80%` を当てているため、パネル側の指定だけでは 70% / 80% を掛けた
    // 大きさになってしまう (12px 指定で実測 8.4px)。モーダル内の他のボタンが
    // Tailwind の text-xs (12px) なので、パネルの操作系もそこへ揃える。
    ".cm-panel.cm-search": { fontSize: "13px", padding: "4px 6px" },
    // 検索・置換の入力欄は `.cm-textfield` で引く。**`input[type=text]` では引けない**:
    // @codemirror/search はこの 2 つを type 属性なしで生成するため、属性セレクタに
    // 一致しない (既定の type が text でも属性は無い)。以前この指定だったので、
    // 下の配色は一度も当たっていなかった。
    ".cm-panel.cm-search input.cm-textfield": {
      backgroundColor: "#27272a",
      color: "#f3f5f9",
      border: "1px solid #52525b",
      borderRadius: "3px",
      fontSize: "12px",
      padding: "2px 4px",
    },
    ".cm-panel.cm-search button": {
      backgroundColor: "#27272a",
      backgroundImage: "none",
      color: "#e4e4e7",
      border: "1px solid #52525b",
      borderRadius: "3px",
      fontSize: "12px",
      padding: "2px 6px",
    },
    ".cm-panel.cm-search label": { fontSize: "12px" },
  });

  // SqlEditor と同じ、oneDark より明るい配色
  const brightHighlightStyle = HighlightStyle.define([
    { tag: [t.keyword, t.operatorKeyword, t.modifier], color: "#eac6ff" },
    { tag: [t.string, t.special(t.string)], color: "#d8f5b0" },
    { tag: [t.number, t.bool, t.null], color: "#ffd7a3" },
    { tag: [t.name, t.propertyName, t.variableName], color: "#b3ddff" },
    { tag: [t.comment], color: "#c0c7da", fontStyle: "italic" },
    { tag: [t.operator, t.punctuation, t.separator], color: "#e2e7f0" },
    { tag: [t.typeName, t.className], color: "#ffeab0" },
  ]);

  /// YAML のパースエラー・警告をエディタ上に表示する。
  /// 保存できない source モードでも崩れに気付けるよう、両モードで有効にする。
  /// パースは常に doc 全体をやり直すが、対象は設定ファイル規模なので十分速い。
  const yamlLinter = linter((view): Diagnostic[] => {
    const docLength = view.state.doc.length;
    /// yaml が返す位置は基本 doc 内だが、範囲外でも CodeMirror が例外を投げないよう丸める
    const toDiagnostic = (
      err: { pos?: [number, number]; message: string },
      severity: "error" | "warning",
    ): Diagnostic => {
      const [rawFrom, rawTo] = err.pos ?? [0, 0];
      const from = Math.max(0, Math.min(rawFrom, docLength));
      const to = Math.max(from, Math.min(rawTo, docLength));
      return { from, to, severity, message: err.message };
    };
    try {
      const parsed = parseDocument(view.state.doc.toString(), { prettyErrors: false });
      return [
        ...parsed.errors.map((e) => toDiagnostic(e, "error")),
        ...parsed.warnings.map((w) => toDiagnostic(w, "warning")),
      ];
    } catch (e) {
      // parseDocument は基本 errors に積むが、想定外の例外でも lint を落とさない
      return [
        {
          from: 0,
          to: Math.min(1, docLength),
          severity: "error",
          message: String(e),
        },
      ];
    }
  });

  const createEditor = (doc: string) => {
    if (!editorElement) {
      return;
    }
    view = new EditorView({
      state: EditorState.create({
        doc,
        extensions: [
          lineNumbers(),
          highlightActiveLineGutter(),
          drawSelection(),
          history(),
          // 検索パネルはエディタの上端に出す (下端だとフッターのボタン列と隣接して紛らわしい)
          search({ top: true }),
          // VSCode 互換のマルチカーソル / 複数選択 (SqlEditor と揃える)。
          // Mod-d / Mod-Shift-l は searchKeymap 側にある
          vscodeMultiSelection,
          keymap.of(searchKeymap),
          // Escape の扱い。searchKeymap より後 = 検索パネルを開いている間は
          // 「パネルを閉じる」が勝つ。defaultKeymap より前 = テキスト選択中でも
          // simplifySelection ではなくモーダルを閉じる (検索を足す前と同じ挙動)
          keymap.of([
            {
              key: "Escape",
              run: () => {
                handleEscape();
                return true;
              },
            },
          ]),
          keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
          yaml(),
          yamlLinter,
          lintGutter(),
          oneDark,
          syntaxHighlighting(brightHighlightStyle),
          editorTheme,
          EditorView.updateListener.of((update) => {
            if (update.docChanged) {
              dirty = true;
              onDirtyChange?.(true);
              saveError = null;
            }
          }),
        ],
      }),
      parent: editorElement,
    });
    view.focus();
  };

  const load = async () => {
    try {
      const text =
        mode === "config" ? await readConfigFile() : await readOverrideConfigYaml();
      createEditor(text);
    } catch (e) {
      loadError = String(e);
    } finally {
      loading = false;
    }
  };

  // onMount から Promise を返すとクリーンアップ関数と誤認されるため、投げっぱなしにする
  onMount(() => {
    void load();
  });

  onDestroy(() => {
    view?.destroy();
    view = null;
  });

  const save = async () => {
    if (!view || saving) {
      return;
    }
    saving = true;
    saveError = null;
    try {
      const path = await writeConfigFile(view.state.doc.toString());
      dirty = false;
      onDirtyChange?.(false);
      // 保存しただけでは実行中の接続に反映されないため、続けて再読込する
      if (await appStore.reloadConnections()) {
        toast.success(`Saved ${path}`);
        onClose();
        return;
      }
      // 保存自体は成功しているので、失敗したのは再読込であることを明示する
      saveError = `Saved ${path}, but reloading the config failed: ${
        appStore.errorMessage ?? "unknown error"
      }`;
    } catch (e) {
      saveError = String(e);
    } finally {
      saving = false;
    }
  };

  const copyAll = async () => {
    if (!view) {
      return;
    }
    await writeText(view.state.doc.toString());
    toast.success("Copied to the clipboard");
  };

  /// 未保存の変更を巻き添えで捨てないよう、dirty なら確認を挟む
  const requestClose = () => {
    if (dirty) {
      confirmDiscard = true;
      return;
    }
    onClose();
  };

  /// Escape の共通処理。エディタにフォーカスがある時は CodeMirror のキーマップから、
  /// それ以外 (ボタンや読み込みエラー表示) は window のハンドラから呼ばれる。
  const handleEscape = () => {
    // 破棄確認を出している間の Escape は「編集に戻る」(誤って捨てない)
    if (confirmDiscard) {
      confirmDiscard = false;
      return;
    }
    requestClose();
  };

  const onWindowKeydown = (e: KeyboardEvent) => {
    // エディタ側 (CodeMirror のキーマップ) が処理済みのキーには手を出さない。
    // 検索パネルを開いている間の Escape はパネルを閉じるだけにしたいので、
    // これが無いとモーダルごと閉じてしまう。
    if (e.defaultPrevented) {
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      handleEscape();
      return;
    }
    // Cmd+S / Ctrl+S で保存
    if (canSave && (e.metaKey || e.ctrlKey) && e.key === "s") {
      e.preventDefault();
      void save();
    }
  };
</script>

<svelte:window onkeydown={onWindowKeydown} />

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-config-editor-modal"
  onclick={(e) => {
    if (e.target === e.currentTarget) {
      requestClose();
    }
  }}
>
  <div
    class="flex h-[80vh] w-[860px] max-w-[92vw] flex-col gap-3 rounded-lg border border-zinc-700 bg-zinc-900 p-4 shadow-xl"
  >
    <h2 class="text-sm font-semibold text-zinc-200" data-annotate="text-config-editor-title">
      {title}
    </h2>

    {#if !canSave}
      <p class="text-xs text-zinc-400">
        This YAML comes from config_override_command. You can edit it here, but the
        changes stay in memory and are never saved. Copy the result and store it where it
        is managed.
      </p>
    {/if}

    {#if loadError}
      <pre
        class="whitespace-pre-wrap font-mono text-xs text-red-400"
        data-annotate="text-config-editor-load-error">{loadError}</pre>
    {:else if loading}
      <p class="text-xs text-zinc-500">Loading...</p>
    {/if}

    <div
      bind:this={editorElement}
      class="config-editor-host min-h-0 flex-1 overflow-hidden rounded border border-zinc-700"
      class:hidden={loadError !== null}
      data-annotate="editor-config-yaml"
    ></div>

    {#if saveError}
      <pre
        class="max-h-24 overflow-auto whitespace-pre-wrap font-mono text-xs text-red-400"
        data-annotate="text-config-editor-save-error">{saveError}</pre>
    {/if}

    {#if confirmDiscard}
      <div class="flex items-center justify-end gap-2">
        <span class="mr-auto text-xs text-amber-400">
          {canSave ? "Discard unsaved changes?" : "Discard your edits? They are never saved."}
        </span>
        <button
          class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
          data-annotate="button-config-editor-keep-editing"
          onclick={() => (confirmDiscard = false)}
        >
          Keep editing
        </button>
        <button
          class="rounded bg-red-600 px-3 py-1 text-xs text-white hover:bg-red-500"
          data-annotate="button-config-editor-discard"
          onclick={onClose}
        >
          Discard
        </button>
      </div>
    {:else}
      <div class="flex items-center justify-end gap-2">
        {#if dirty}
          <span class="mr-auto text-xs text-zinc-500">
            {canSave ? "Unsaved changes" : "Edited (in memory only)"}
          </span>
        {/if}
        <button
          class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
          data-annotate="button-config-editor-copy"
          onclick={copyAll}
        >
          Copy
        </button>
        <button
          class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
          data-annotate="button-config-editor-close"
          onclick={requestClose}
        >
          Close
        </button>
        {#if canSave}
          <button
            class="rounded bg-blue-600 px-3 py-1 text-xs text-white hover:bg-blue-500 disabled:opacity-50"
            data-annotate="button-config-editor-save"
            disabled={saving || loadError !== null}
            onclick={save}
          >
            {saving ? "Saving..." : "Save"}
          </button>
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  /* oneDark の背景指定が EditorView.theme に勝つため、CSS で確実に上書きする */
  .config-editor-host :global(.cm-editor),
  .config-editor-host :global(.cm-gutters) {
    background-color: #111111 !important;
  }
  .config-editor-host :global(.cm-editor) {
    height: 100%;
  }
</style>
