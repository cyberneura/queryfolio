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
    /// "config" = edit and save config.yml.
    /// "source" = show the YAML returned by config_override_command.
    ///            Editable, but only in memory; it cannot be saved (meant to be copied and used).
    mode: "config" | "source";
    onClose: () => void;
    /// Tells the parent whether there are unsaved changes (prevents discarding them as collateral when switching to another mode)
    onDirtyChange?: (dirty: boolean) => void;
  }

  let { mode, onClose, onDirtyChange }: Props = $props();

  /// source mode is fetched from an external command, so it cannot be written back. Editing itself is allowed.
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
  /// Show a discard confirmation when trying to close with unsaved changes
  let confirmDiscard = $state(false);

  const editorTheme = EditorView.theme({
    "&": { height: "100%", fontSize: "13px" },
    ".cm-content": { color: "#f3f5f9" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "#f3f5f9" },
    ".cm-scroller": {
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, Monaco, monospace",
    },
    // Match the search panel (Cmd+F) to the modal's color scheme.
    // Left as default, the light form parts would float on top of the dark editor.
    ".cm-panels": { backgroundColor: "#18181b", color: "#e4e4e7" },
    ".cm-panels.cm-panels-top": { borderBottom: "1px solid #3f3f46" },
    // Font size is specified **individually** for the input, buttons and labels. CodeMirror's
    // base theme applies `font-size: 70%` to `.cm-textfield` / `.cm-button` and `80%` to the
    // search panel's `label`, so a panel-level setting alone ends up multiplied by 70% / 80%
    // (measured 8.4px for a 12px setting). Other buttons in the modal use
    // Tailwind's text-xs (12px), so align the panel's controls to that.
    ".cm-panel.cm-search": { fontSize: "13px", padding: "4px 6px" },
    // Select the search / replace inputs with `.cm-textfield`. **`input[type=text]` does not match**:
    // @codemirror/search creates these two without a type attribute, so an attribute selector
    // does not match (even though the default type is text, the attribute is absent). The
    // colors below never applied while it was written that way.
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

  // Same as SqlEditor, a lighter color scheme than oneDark
  const brightHighlightStyle = HighlightStyle.define([
    { tag: [t.keyword, t.operatorKeyword, t.modifier], color: "#eac6ff" },
    { tag: [t.string, t.special(t.string)], color: "#d8f5b0" },
    { tag: [t.number, t.bool, t.null], color: "#ffd7a3" },
    { tag: [t.name, t.propertyName, t.variableName], color: "#b3ddff" },
    { tag: [t.comment], color: "#c0c7da", fontStyle: "italic" },
    { tag: [t.operator, t.punctuation, t.separator], color: "#e2e7f0" },
    { tag: [t.typeName, t.className], color: "#ffeab0" },
  ]);

  /// Show YAML parse errors and warnings in the editor.
  /// Enabled in both modes so that breakage is noticed even in source mode, which cannot be saved.
  /// Parsing always redoes the whole doc, but the target is config-file sized, so it is fast enough.
  const yamlLinter = linter((view): Diagnostic[] => {
    const docLength = view.state.doc.length;
    /// Positions returned by yaml are normally within the doc; clamp them so CodeMirror does not throw even if out of range
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
      // parseDocument normally accumulates into errors, but do not let an unexpected exception break lint
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
          // Show the search panel at the top of the editor (at the bottom it would sit next to the footer buttons and be confusing)
          search({ top: true }),
          // VSCode-compatible multi-cursor / multiple selections (same as SqlEditor).
          // Mod-d / Mod-Shift-l are in searchKeymap
          vscodeMultiSelection,
          keymap.of(searchKeymap),
          // How Escape is handled. After searchKeymap = while the search panel is open,
          // "close the panel" wins. Before defaultKeymap = even with text selected,
          // it closes the modal rather than simplifySelection (same behavior as before search was added)
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

  // Returning a Promise from onMount would be mistaken for a cleanup function, so fire and forget
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
      // Saving alone does not apply to the running connection, so reload right after
      if (await appStore.reloadConnections()) {
        toast.success(`Saved ${path}`);
        onClose();
        return;
      }
      // Saving itself succeeded, so make clear that it was the reload that failed
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

  /// Confirm if dirty so unsaved changes are not discarded as collateral
  const requestClose = () => {
    if (dirty) {
      confirmDiscard = true;
      return;
    }
    onClose();
  };

  /// Common Escape handling. Called from the CodeMirror keymap when the editor has focus,
  /// and from the window handler otherwise (buttons or the load error display).
  const handleEscape = () => {
    // Escape while the discard confirmation is shown means "back to editing" (never discard by accident)
    if (confirmDiscard) {
      confirmDiscard = false;
      return;
    }
    requestClose();
  };

  const onWindowKeydown = (e: KeyboardEvent) => {
    // Do not touch keys already handled by the editor side (CodeMirror keymap).
    // Escape while the search panel is open should only close the panel;
    // without this, the whole modal would close.
    if (e.defaultPrevented) {
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      handleEscape();
      return;
    }
    // Save with Cmd+S / Ctrl+S
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
  data-modal
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
  /* oneDark's background rule beats EditorView.theme, so override it reliably with CSS */
  .config-editor-host :global(.cm-editor),
  .config-editor-host :global(.cm-gutters) {
    background-color: #111111 !important;
  }
  .config-editor-host :global(.cm-editor) {
    height: 100%;
  }
</style>
