<script lang="ts">
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";

  interface Props {
    /// The cell value to display (the value from result.rows as is)
    value: unknown;
    /// Column name (for the header display)
    column: string;
    /// Row index (0-based; converted to 1-based for display)
    rowIndex: number;
    /// Callback invoked by the close button / ESC
    onclose: () => void;
  }

  let { value, column, rowIndex, onclose }: Props = $props();

  let copiedKind = $state<"raw" | "pretty" | null>(null);

  // Above this size, tokenizing gets heavy, so give up highlighting
  const HIGHLIGHT_MAX_CHARS = 200_000;

  // Upper limit on the number of tokens (= span elements). Even when the character count is
  // within the limit, JSON with a huge number of short elements bloats the DOM nodes and freezes
  // rendering, so beyond this we fall back to plain display without highlighting
  const HIGHLIGHT_MAX_TOKENS = 5_000;

  const isNull = $derived(value === null || value === undefined);

  // Raw text representation of the cell (same rules as the table display)
  const rawText = $derived.by(() => {
    if (value === null || value === undefined) {
      return "NULL";
    }
    if (typeof value === "object") {
      return JSON.stringify(value);
    }
    return String(value);
  });

  // The parse result if it can be interpreted as JSON (otherwise undefined).
  // Scalars such as a bare number or true are pointless to pretty-print,
  // so only objects / arrays are treated as JSON
  const parsedJson = $derived.by((): unknown => {
    if (value !== null && typeof value === "object") {
      return value;
    }
    if (typeof value !== "string") {
      return undefined;
    }
    const trimmed = value.trim();
    if (!trimmed.startsWith("{") && !trimmed.startsWith("[")) {
      return undefined;
    }
    try {
      return JSON.parse(trimmed);
    } catch {
      return undefined;
    }
  });

  const isJson = $derived(parsedJson !== undefined);

  const prettyText = $derived(
    isJson ? JSON.stringify(parsedJson, null, 2) : null,
  );

  type TokenType = "key" | "string" | "number" | "keyword" | "plain";

  interface Token {
    text: string;
    type: TokenType;
  }

  // A simple tokenizer that assumes the output of JSON.stringify(_, null, 2).
  // It classifies into strings (keys / values), numbers, true/false/null, and everything else
  // (structural characters such as brackets and commas)
  const JSON_TOKEN_RE =
    /("(?:\\.|[^"\\])*")(\s*:)|("(?:\\.|[^"\\])*")|\b(true|false|null)\b|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/g;

  const tokenizeJson = (text: string): Token[] => {
    const tokens: Token[] = [];
    let last = 0;
    let match: RegExpExecArray | null;
    JSON_TOKEN_RE.lastIndex = 0;
    while ((match = JSON_TOKEN_RE.exec(text)) !== null) {
      if (match.index > last) {
        tokens.push({ text: text.slice(last, match.index), type: "plain" });
      }
      if (match[1] !== undefined) {
        // Key string + the following colon
        tokens.push({ text: match[1], type: "key" });
        tokens.push({ text: match[2], type: "plain" });
      } else if (match[3] !== undefined) {
        tokens.push({ text: match[3], type: "string" });
      } else if (match[4] !== undefined) {
        tokens.push({ text: match[4], type: "keyword" });
      } else {
        tokens.push({ text: match[5], type: "number" });
      }
      last = JSON_TOKEN_RE.lastIndex;
    }
    if (last < text.length) {
      tokens.push({ text: text.slice(last), type: "plain" });
    }
    return tokens;
  };

  // Token list for highlighting (null for huge values, which are shown plain)
  const tokens = $derived.by(() => {
    if (prettyText === null || prettyText.length > HIGHLIGHT_MAX_CHARS) {
      return null;
    }
    const tokenized = tokenizeJson(prettyText);
    if (tokenized.length > HIGHLIGHT_MAX_TOKENS) {
      return null;
    }
    return tokenized;
  });

  const TOKEN_CLASSES: Record<TokenType, string> = {
    key: "text-sky-300",
    string: "text-emerald-300",
    number: "text-amber-300",
    keyword: "text-purple-300",
    plain: "text-zinc-500",
  };

  const copy = async (kind: "raw" | "pretty") => {
    const text = kind === "pretty" ? (prettyText ?? rawText) : rawText;
    // navigator.clipboard can trigger an OS permission prompt in Tauri 2,
    // so write through the official plugin
    await writeText(text);
    copiedKind = kind;
    setTimeout(() => {
      copiedKind = null;
    }, 1500);
  };

  // Close the inspector with ESC
  const onWindowKeydown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      onclose();
    }
  };
</script>

<svelte:window onkeydown={onWindowKeydown} />

<div
  class="flex w-96 shrink-0 flex-col border-l border-zinc-700 bg-zinc-900"
  data-annotate="panel-cell-inspector"
>
  <!-- Header: column name, row number, close button -->
  <div
    class="flex shrink-0 items-center gap-2 border-b border-zinc-700 px-3 py-1.5 text-xs text-zinc-400"
  >
    <span
      class="min-w-0 truncate font-mono font-semibold text-zinc-300"
      title={column}
      data-annotate="text-cell-inspector-column"
    >
      {column}
    </span>
    <span class="shrink-0 text-zinc-500">Row {rowIndex + 1}</span>
    {#if isJson}
      <span
        class="shrink-0 rounded bg-zinc-700 px-1 py-px text-[10px] font-semibold text-zinc-300"
      >
        JSON
      </span>
    {/if}
    <button
      class="ml-auto shrink-0 rounded px-1 text-zinc-500 hover:bg-zinc-700 hover:text-zinc-200"
      title="Close the cell inspector (Esc)"
      aria-label="Close the cell inspector (Esc)"
      data-annotate="button-cell-inspector-close"
      onclick={onclose}
    >
      <i class="bi bi-x" aria-hidden="true"></i>
    </button>
  </div>

  <!-- Copy actions -->
  <div
    class="flex shrink-0 items-center gap-1 border-b border-zinc-700 px-3 py-1 text-xs text-zinc-400"
  >
    <button
      class="rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
      title="Copy the raw cell value"
      data-annotate="button-cell-inspector-copy-raw"
      onclick={() => copy("raw")}
    >
      {copiedKind === "raw" ? "Copied!" : "Copy raw"}
    </button>
    {#if isJson}
      <button
        class="rounded border border-zinc-700 px-1.5 py-0.5 hover:bg-zinc-700 hover:text-zinc-200"
        title="Copy the formatted JSON"
        data-annotate="button-cell-inspector-copy-pretty"
        onclick={() => copy("pretty")}
      >
        {copiedKind === "pretty" ? "Copied!" : "Copy pretty"}
      </button>
    {/if}
  </div>

  <!-- Body: JSON is pretty-printed + highlighted, anything else is wrapped text -->
  <div
    class="min-h-0 flex-1 overflow-auto px-3 py-2"
    data-annotate="text-cell-inspector-value"
  >
    {#if isNull}
      <p class="font-mono text-xs text-zinc-600 italic">NULL</p>
    {:else if prettyText !== null}
      {#if tokens !== null}
        <!-- Whitespace inside pre is displayed as is, so write it on one line -->
        <!-- prettier-ignore -->
        <pre class="font-mono text-xs break-all whitespace-pre-wrap">{#each tokens as token, i (i)}<span class={TOKEN_CLASSES[token.type]}>{token.text}</span>{/each}</pre>
      {:else}
        <pre
          class="font-mono text-xs break-all whitespace-pre-wrap text-zinc-200">{prettyText}</pre>
      {/if}
    {:else}
      <pre
        class="font-mono text-xs break-all whitespace-pre-wrap text-zinc-200">{rawText}</pre>
    {/if}
  </div>
</div>
