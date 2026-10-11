<script lang="ts">
  import { tick } from "svelte";
  import { writeText } from "@tauri-apps/plugin-clipboard-manager";
  import appStore from "$lib/stores/app.svelte";
  import { splitMarkdownSegments } from "$lib/markdown";

  interface Props {
    /// Whether the selected connection's engine supports AI features
    /// (redis / elasticsearch / dynamodb are unsupported. false when no connection is selected)
    supportsAi: boolean;
    /// Close the pane (flips the same state as the toolbar toggle)
    onClose: () => void;
    /// Insert a code block into the editor (even if called when insertion is not possible,
    /// the store side shows a warning)
    onInsert: (sql: string) => void;
  }

  let { supportsAi, onClose, onInsert }: Props = $props();

  let input = $state("");
  let listEl: HTMLDivElement | undefined = $state();
  /// Identifier of the code block whose Copy was pressed ("<messageId>:<index>")
  let copiedKey = $state<string | null>(null);

  /// Whether chat is usable (connection selected + AI configured + engine supports AI)
  const chatAvailable = $derived(
    appStore.selectedConnection !== null &&
      (appStore.aiInfo?.configured ?? false) &&
      supportsAi,
  );
  const canSend = $derived(
    chatAvailable &&
      !appStore.chatSending &&
      input.trim().length > 0 &&
      appStore.selectedConnection !== null,
  );

  /// Scroll to the bottom when messages are added (right after sending / on receiving a response)
  $effect(() => {
    void appStore.chatMessages.length;
    void appStore.chatSending;
    void tick().then(() => {
      if (listEl) {
        listEl.scrollTop = listEl.scrollHeight;
      }
    });
  });

  const send = async () => {
    if (!canSend) {
      return;
    }
    const text = input;
    input = "";
    await appStore.sendChatMessage(text);
  };

  const onKeydown = (e: KeyboardEvent) => {
    // Enter sends, Shift+Enter inserts a newline (Enter during IME composition does not send)
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      void send();
    }
  };

  const copy = async (key: string, text: string) => {
    await writeText(text);
    copiedKey = key;
    setTimeout(() => {
      if (copiedKey === key) {
        copiedKey = null;
      }
    }, 1500);
  };
</script>

<div
  class="flex h-full w-full flex-col border-l border-zinc-700 bg-zinc-900"
  data-annotate="pane-chat"
>
  <div
    class="flex shrink-0 items-center gap-2 border-b border-zinc-700 px-3 py-1"
  >
    <span class="text-xs font-semibold tracking-wide text-zinc-300">
      <i class="bi bi-chat-dots" aria-hidden="true"></i> AI Chat
    </span>
    {#if chatAvailable && appStore.aiInfo?.model}
      <span
        class="truncate rounded bg-zinc-800 px-1.5 py-0.5 text-[10px] text-zinc-400"
        data-annotate="text-chat-model"
      >
        {appStore.aiInfo.model}
      </span>
    {/if}
    <div class="ml-auto flex shrink-0 items-center gap-1">
      <button
        type="button"
        class="rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200 disabled:cursor-not-allowed disabled:opacity-50"
        data-annotate="button-chat-clear"
        title="Clear the conversation"
        aria-label="Clear the conversation"
        disabled={appStore.chatMessages.length === 0 || appStore.chatSending}
        onclick={() => appStore.clearChat()}
      >
        <i class="bi bi-trash3" aria-hidden="true"></i>
      </button>
      <button
        type="button"
        class="rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200"
        data-annotate="button-chat-close"
        title="Close the chat pane"
        aria-label="Close the chat pane"
        onclick={onClose}
      >
        <i class="bi bi-x-lg" aria-hidden="true"></i>
      </button>
    </div>
  </div>

  <div
    class="min-h-0 flex-1 space-y-3 overflow-y-auto px-3 py-2"
    bind:this={listEl}
    data-annotate="list-chat-messages"
  >
    {#if appStore.selectedConnection === null}
      <!-- Do not confuse "no connection selected" with "engine does not support AI"
           (supportsAi is false in both cases, so check whether a connection exists first) -->
      <p class="text-xs leading-relaxed text-zinc-500">
        Select a connection first.
      </p>
    {:else if !supportsAi}
      <p
        class="text-xs leading-relaxed text-zinc-500"
        data-annotate="text-chat-unsupported"
      >
        The selected connection's engine does not support the AI features.
      </p>
    {:else if !(appStore.aiInfo?.configured ?? false)}
      <p class="text-xs leading-relaxed text-zinc-500">
        AI is not configured. Add an <code class="text-zinc-400">ai:</code>
        section (provider: openai, api_key) to config.yml or the YAML fetched by
        config_override_command.
      </p>
    {:else if appStore.chatMessages.length === 0}
      <p class="text-xs leading-relaxed text-zinc-500">
        Ask about your data or your queries. The assistant can run
        <strong class="text-zinc-400">read-only</strong> SQL on the selected
        connection to look things up.
      </p>
    {/if}

    {#each appStore.chatMessages as message (message.id)}
      <div
        class="flex flex-col gap-1"
        class:items-end={message.role === "user"}
        data-annotate="item-chat-message"
      >
        <span class="text-[10px] uppercase tracking-wide text-zinc-500">
          {message.role === "user" ? "You" : "Assistant"}
        </span>
        {#if message.role === "user"}
          <p
            class="max-w-full whitespace-pre-wrap rounded border border-zinc-700 bg-zinc-800 px-2 py-1 text-xs leading-relaxed text-zinc-200"
          >
            {message.content}
          </p>
        {:else}
          {#if message.toolCalls && message.toolCalls.length > 0}
            <!-- Read queries the agent actually ran. Always shown so that
                 what it looked at to answer is not hidden -->
            <div class="flex w-full flex-col gap-1">
              {#each message.toolCalls as call, i (i)}
                <details
                  class="rounded border border-zinc-700 bg-zinc-950/60 px-2 py-1"
                  data-annotate="item-chat-tool-call"
                >
                  <summary
                    class="cursor-pointer truncate text-[10px] text-zinc-400"
                  >
                    <i
                      class={call.ok
                        ? "bi bi-database-check text-emerald-400"
                        : "bi bi-database-exclamation text-red-400"}
                      aria-hidden="true"
                    ></i>
                    {call.name} — {call.summary}
                  </summary>
                  <pre
                    class="mt-1 overflow-x-auto font-mono text-[10px] leading-relaxed text-zinc-400">{call.argument}</pre>
                </details>
              {/each}
            </div>
          {/if}
          {#if message.failed}
            <p
              class="w-full whitespace-pre-wrap rounded border border-red-500/40 bg-red-500/10 px-2 py-1 text-xs leading-relaxed text-red-300"
              data-annotate="text-chat-error"
            >
              {message.content}
            </p>
          {:else}
            <div class="flex w-full flex-col gap-2">
              {#each splitMarkdownSegments(message.content) as segment, i (i)}
                {#if segment.type === "code"}
                  <div class="flex flex-col gap-1">
                    <pre
                      class="overflow-x-auto rounded border border-zinc-700 bg-zinc-950 p-2 font-mono text-xs leading-relaxed text-emerald-300">{segment.content}</pre>
                    <div class="flex justify-end gap-1">
                      <button
                        type="button"
                        class="rounded border border-zinc-600 px-1.5 py-0.5 text-[10px] text-zinc-300 hover:bg-zinc-800"
                        data-annotate="button-chat-copy-code"
                        onclick={() =>
                          copy(`${message.id}:${i}`, segment.content)}
                      >
                        {copiedKey === `${message.id}:${i}` ? "Copied!" : "Copy"}
                      </button>
                      <button
                        type="button"
                        class="rounded border border-zinc-600 px-1.5 py-0.5 text-[10px] text-zinc-300 hover:bg-zinc-800"
                        data-annotate="button-chat-insert-code"
                        title="Insert into the editor"
                        onclick={() => onInsert(segment.content)}
                      >
                        Insert
                      </button>
                    </div>
                  </div>
                {:else}
                  <p
                    class="whitespace-pre-wrap text-xs leading-relaxed text-zinc-300"
                  >
                    {segment.content}
                  </p>
                {/if}
              {/each}
            </div>
          {/if}
        {/if}
      </div>
    {/each}

    {#if appStore.chatSending}
      <div
        class="flex items-center gap-2 text-xs text-zinc-500"
        data-annotate="spinner-chat-sending"
      >
        <span
          class="inline-block size-3 animate-spin rounded-full border-2 border-zinc-500 border-t-transparent"
        ></span>
        Thinking...
        <!-- A way to stop the agent when it is running a heavy query.
             The conversation is kept, and the interruption is shown as a message -->
        <button
          type="button"
          class="rounded border border-zinc-600 px-1.5 py-0.5 text-[10px] text-zinc-300 hover:bg-zinc-800"
          data-annotate="button-chat-stop"
          title="Stop the assistant (cancels the query it is running)"
          onclick={() => appStore.stopChat()}
        >
          <i class="bi bi-stop-fill" aria-hidden="true"></i> Stop
        </button>
      </div>
    {/if}
  </div>

  <form
    class="flex shrink-0 items-end gap-2 border-t border-zinc-700 px-3 py-2"
    onsubmit={(e) => {
      e.preventDefault();
      void send();
    }}
  >
    <textarea
      bind:value={input}
      class="min-h-[2.25rem] w-full flex-1 resize-none rounded border border-zinc-600 bg-zinc-800 px-2 py-1 text-xs text-zinc-200 outline-none placeholder:text-zinc-500 focus:border-blue-400 disabled:opacity-50"
      data-annotate="input-chat-message"
      rows="2"
      placeholder="Ask about your data... (Enter to send, Shift+Enter for a newline)"
      disabled={!chatAvailable || appStore.chatSending}
      onkeydown={onKeydown}
    ></textarea>
    <button
      type="submit"
      class="shrink-0 rounded border border-blue-500/50 bg-blue-500/15 px-2 py-1 text-xs text-blue-300 hover:bg-blue-500/25 disabled:cursor-not-allowed disabled:opacity-50"
      data-annotate="button-chat-send"
      disabled={!canSend}
    >
      Send
    </button>
  </form>
</div>
