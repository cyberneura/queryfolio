<script lang="ts">
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import {
    generateLineReplace,
    countLineReplaceResults,
    PLACEHOLDER,
  } from "$lib/lineReplace";

  interface Props {
    /// The lines selected in the editor when the pane was opened (initial value of the Lines field)
    initialLines: string;
    /// Insert the generated result into the editor's selection
    onReplace: (result: string) => void;
    /// Close the pane
    onClose: () => void;
  }

  let { initialLines, onReplace, onClose }: Props = $props();

  // The template is saved to localStorage and kept after reopening.
  // The default is the KILL statement example, which is the main purpose of the task
  const TEMPLATE_KEY = "queryfolio.replaceMultiline.template";
  const loadTemplate = (): string => {
    try {
      return localStorage.getItem(TEMPLATE_KEY) ?? "KILL %%%;";
    } catch {
      return "KILL %%%;";
    }
  };

  let template = $state(loadTemplate());
  // Initialize with the lines selected at open time. The parent remounts via #key,
  // so the prop's initial value can be used as-is (it is local editable state afterwards)
  // svelte-ignore state_referenced_locally
  let linesText = $state(initialLines);
  let copied = $state(false);

  $effect(() => {
    try {
      localStorage.setItem(TEMPLATE_KEY, template);
    } catch {
      // Keep working even if localStorage is unavailable
    }
  });

  const output = $derived(generateLineReplace(linesText, template));
  const resultCount = $derived(countLineReplaceResults(linesText, template));

  const copy = async () => {
    if (output === "") {
      return;
    }
    await writeText(output);
    copied = true;
    setTimeout(() => {
      copied = false;
    }, 1500);
  };

  const replace = () => {
    // Do nothing for empty output (all lines skipped). Match the button's disabled state, and
    // prevent Cmd+Enter from replacing the selection with an empty string (= deleting it)
    if (output === "") {
      return;
    }
    onReplace(output);
  };

  // Insert with Cmd+Enter (aligned with the editor's execute shortcut)
  const onKeydown = (e: KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      replace();
    }
  };
</script>

<div
  class="flex h-full min-h-0 flex-col bg-zinc-900 text-xs text-zinc-300"
  data-annotate="pane-replace-multiline"
>
  <!-- Header -->
  <div
    class="flex shrink-0 items-center gap-2 border-b border-zinc-700 px-3 py-1.5"
  >
    <span class="font-semibold tracking-wide text-zinc-400">
      REPLACE MULTILINE
    </span>
    <button
      type="button"
      class="ml-auto rounded px-1 text-zinc-500 hover:bg-zinc-700 hover:text-zinc-200"
      title="Close"
      aria-label="Close"
      data-annotate="button-replace-multiline-close"
      onclick={onClose}
    >
      <i class="bi bi-x-lg" aria-hidden="true"></i>
    </button>
  </div>

  <div class="flex min-h-0 flex-1 flex-col gap-2 overflow-auto p-3">
    <!-- Template -->
    <label class="flex flex-col gap-1">
      <span class="text-zinc-500">
        Template (use <code class="text-sky-400">{PLACEHOLDER}</code> as the placeholder)
      </span>
      <input
        type="text"
        class="rounded border border-zinc-600 bg-zinc-800 px-2 py-1 font-mono text-zinc-200 outline-none focus:border-blue-400"
        data-annotate="input-replace-multiline-template"
        placeholder="KILL %%%;"
        bind:value={template}
        onkeydown={onKeydown}
      />
    </label>

    <!-- Input lines (initialized with the selected lines; editable) -->
    <label class="flex min-h-0 flex-1 flex-col gap-1">
      <span class="text-zinc-500">
        Lines (empty lines and lines starting with # or // are skipped)
      </span>
      <textarea
        class="min-h-24 flex-1 resize-none rounded border border-zinc-600 bg-zinc-800 px-2 py-1 font-mono text-zinc-200 outline-none focus:border-blue-400"
        data-annotate="textarea-replace-multiline-lines"
        spellcheck="false"
        bind:value={linesText}
        onkeydown={onKeydown}
      ></textarea>
    </label>

    <!-- Preview of the generated result -->
    <div class="flex min-h-0 flex-1 flex-col gap-1">
      <span class="text-zinc-500">Result ({resultCount} lines)</span>
      <textarea
        class="min-h-24 flex-1 resize-none rounded border border-zinc-700 bg-zinc-950 px-2 py-1 font-mono text-emerald-300 outline-none"
        data-annotate="text-replace-multiline-output"
        readonly
        spellcheck="false"
        value={output}
      ></textarea>
    </div>
  </div>

  <!-- Footer actions -->
  <div
    class="flex shrink-0 items-center gap-2 border-t border-zinc-700 px-3 py-2"
  >
    <button
      type="button"
      class="rounded border border-blue-500/50 bg-blue-500/15 px-2 py-0.5 text-blue-300 hover:bg-blue-500/25 disabled:cursor-not-allowed disabled:opacity-50"
      title="Replace the selected text in the editor with the result (Cmd+Enter)"
      data-annotate="button-replace-multiline-apply"
      disabled={output === ""}
      onclick={replace}
    >
      <i class="bi bi-arrow-left-right" aria-hidden="true"></i> Replace selection
    </button>
    <button
      type="button"
      class="rounded border border-zinc-600 bg-zinc-800 px-2 py-0.5 text-zinc-300 hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50"
      title="Copy the result to the clipboard"
      data-annotate="button-replace-multiline-copy"
      disabled={output === ""}
      onclick={copy}
    >
      {copied ? "Copied!" : "Copy"}
    </button>
  </div>
</div>
