<script lang="ts">
  /// The chat pane when a coding agent (Claude Code / Codex) is configured
  /// (CYBERNEURA-DEV-1354). The layout and behavior follow the agent chat of Astragal /
  /// Beckwell: provider and working directory in the header, a log of chat items (text,
  /// tool calls, approvals, questions), and a composer that queues messages sent while the
  /// agent is answering. The conversation state lives in $lib/agentChat/conversations.
  import { tick, untrack } from "svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import appStore from "$lib/stores/app.svelte";
  import {
    answerQuestion,
    conversationFor,
    existingConversation,
    interrupt,
    mergeQueuedUp,
    providerName,
    reconfigure,
    removeQueued,
    respondPermission,
    restart,
    resumeQueue,
    submit,
    type Conversation,
  } from "$lib/agentChat/conversations.svelte";
  import type { ChatAgent } from "$lib/agentChat/beckwell";
  import type { Item, QuestionItem } from "$lib/agentChat/beckwell/types";
  import {
    isExternalLink,
    LINK_ATTRIBUTE,
    renderMarkdown,
  } from "$lib/agentChat/beckwell/markdown";
  import { handleCopyClick } from "$lib/agentChat/beckwell/copy";
  import { rowsFor } from "$lib/agentChat/queue";
  import { shouldFollowBottom, type ScrollMetrics } from "$lib/agentChat/scroll";

  interface Props {
    /// Close the pane (flips the same state as the toolbar toggle)
    onClose: () => void;
    /// Insert a code block into the editor
    onInsert: (sql: string) => void;
  }

  let { onClose, onInsert }: Props = $props();

  /// The conversation of the selected connection
  const conversation = $derived.by((): Conversation | null => {
    if (!appStore.agentInfo?.provider) {
      return null;
    }
    return existingConversation(appStore.selectedConnection) ?? null;
  });

  // Created (and its agent started) when the pane is shown for a connection for the first
  // time. In an effect, not in the derived above: creating it writes state
  $effect(() => {
    const connection = appStore.selectedConnection;
    const provider = appStore.agentInfo?.provider;
    if (connection && provider) {
      untrack(() => conversationFor(connection, provider));
    }
  });

  const agentLabel = $derived(
    conversation ? providerName(conversation.provider) : "Agent",
  );

  const statusText = $derived.by(() => {
    const c = conversation;
    if (!c) {
      return "";
    }
    const state = !c.launched
      ? "Starts with your first message"
      : c.starting || (!c.ready && !c.stopped)
      ? "Starting…"
      : c.busy
        ? "Working…"
        : c.ready
          ? "Ready"
          : "Disconnected";
    return c.usage ? `${state} · ${c.usage}` : state;
  });

  /// The directory shown in the header: where the agent runs, or (before it is started) the
  /// one chosen for this chat
  const shownDirectory = (c: Conversation | null | undefined): string =>
    c ? c.cwd || (c.chosenDirectory ?? "") : "";

  let logEl: HTMLDivElement | undefined = $state();
  let directoryInput = $state("");
  let editingDirectory = $state(false);

  // Whether new output scrolls the log to the end ($lib/agentChat/scroll). Stops when the user
  // scrolls up and resumes at the bottom. The position is recorded on every scroll, so that the
  // decision uses the position before new items were appended
  let followBottom = true;
  let lastScroll: ScrollMetrics = { scrollTop: 0, scrollHeight: 0, clientHeight: 0 };
  const readScroll = (): ScrollMetrics => ({
    scrollTop: logEl?.scrollTop ?? 0,
    scrollHeight: logEl?.scrollHeight ?? 0,
    clientHeight: logEl?.clientHeight ?? 0,
  });
  const onLogScroll = () => {
    const metrics = readScroll();
    followBottom = shouldFollowBottom(followBottom, lastScroll, metrics);
    lastScroll = metrics;
  };

  /// A cheap fingerprint of what the log shows: it changes when an item is added and while the
  /// last item streams
  const logVersion = $derived.by(() => {
    const items = conversation?.items ?? [];
    const last = items[items.length - 1];
    const tail =
      last && (last.kind === "text" || last.kind === "thinking")
        ? last.text.length
        : last && last.kind === "tool_use"
          ? (last.result?.length ?? -1)
          : 0;
    return `${items.length}:${tail}:${conversation?.busy ? 1 : 0}`;
  });

  $effect(() => {
    void logVersion;
    void tick().then(() => {
      if (logEl && followBottom) {
        logEl.scrollTop = logEl.scrollHeight;
        // Scroll events are coalesced per frame, so remember where the log was sent; an upward
        // move made in the same frame is then still seen as a decrease from here
        lastScroll = readScroll();
      }
    });
  });

  // Another connection's conversation: start at its end, and show its directory
  $effect(() => {
    const c = conversation;
    followBottom = true;
    editingDirectory = false;
    directoryInput = shownDirectory(c);
  });

  $effect(() => {
    if (!editingDirectory) {
      directoryInput = shownDirectory(conversation);
    }
  });

  // The agent edits query files on disk. When a turn ends, take its edits into the editor now
  // instead of waiting for the next tick of the external-change watcher
  let wasBusy = false;
  $effect(() => {
    const busy = conversation?.busy ?? false;
    if (wasBusy && !busy) {
      void appStore.checkOpenFilesForExternalChanges();
    }
    wasBusy = busy;
  });

  const send = async () => {
    const c = conversation;
    if (!c || !c.draft.trim()) {
      return;
    }
    // The user just sent this: show it and the answer even if they had scrolled up
    followBottom = true;
    await submit(c, c.draft);
  };

  let composing = false;
  const onComposerKeydown = (e: KeyboardEvent) => {
    // WebKit reports keyCode 229 for the Enter that confirms an IME conversion even when
    // isComposing is already false
    if (composing || e.isComposing || e.keyCode === 229 || e.repeat) {
      return;
    }
    if (e.key === "Enter" && !e.shiftKey && !e.altKey) {
      e.preventDefault();
      void send();
    }
  };

  const applyDirectory = () => {
    const c = conversation;
    editingDirectory = false;
    if (!c) {
      return;
    }
    const value = directoryInput.trim();
    if (value === (c.cwd || (c.chosenDirectory ?? ""))) {
      return;
    }
    // Empty: back to the configured directory (or the connection's query folder)
    void reconfigure(c, { directory: value === "" ? null : value });
  };

  const onProviderChange = (e: Event) => {
    const c = conversation;
    if (c) {
      void reconfigure(c, {
        provider: (e.currentTarget as HTMLSelectElement).value as ChatAgent,
      });
    }
  };

  /// A thinking block is shown only as an animated line while it streams in the current turn.
  /// A stream left open by a turn that ended without closing it must not animate forever
  /// (hence busy), nor come back when the next turn starts (hence "after the last user item")
  const thinkingActive = (c: Conversation, item: Item): boolean => {
    if (item.kind !== "thinking" || !item.streaming || !c.busy) {
      return false;
    }
    for (let i = c.items.length - 1; i >= 0; i--) {
      if (c.items[i] === item) {
        return true;
      }
      if (c.items[i].kind === "user") {
        return false;
      }
    }
    return false;
  };

  const onLogClick = (e: MouseEvent) => {
    if (handleCopyClick(e)) {
      return;
    }
    const target = e.target as Element;
    const insert = target.closest("button.insert-code");
    if (insert) {
      e.preventDefault();
      const pre = insert.closest(".code-block")?.querySelector(":scope > pre");
      if (pre?.textContent) {
        onInsert(pre.textContent.replace(/\n$/, ""));
      }
      return;
    }
    const link = target.closest(`[${LINK_ATTRIBUTE}]`);
    const href = link?.getAttribute(LINK_ATTRIBUTE);
    if (href && isExternalLink(href)) {
      e.preventDefault();
      void openUrl(href).catch(() => {});
    }
  };

  /// Markdown of an agent message, with an "insert into the editor" button next to the copy
  /// button of each code block (the HTML is sanitized by renderMarkdown; the button is added
  /// after that, as fixed markup)
  const INSERT_BUTTON_HTML =
    '<button type="button" class="insert-code" title="Insert into the editor" ' +
    'aria-label="Insert into the editor"></button>';
  const renderAgentText = (text: string, streaming: boolean): string => {
    const html = renderMarkdown(text);
    return streaming
      ? html
      : html.replaceAll(
          '<button type="button" class="copy-code"',
          `${INSERT_BUTTON_HTML}<button type="button" class="copy-code"`,
        );
  };

  const toolTitle = (item: Extract<Item, { kind: "tool_use" }>): string => {
    const input = item.input ?? {};
    const hint =
      (typeof input.file_path === "string" && input.file_path) ||
      (typeof input.path === "string" && input.path) ||
      (typeof input.command === "string" && input.command) ||
      (typeof input.pattern === "string" && input.pattern) ||
      "";
    return hint ? `${item.name} — ${hint}` : item.name;
  };

  /// Answers being typed for the questions of the agent, keyed by request + question text
  let questionDrafts = $state<Record<string, string>>({});
  const draftKey = (item: QuestionItem, question: string) =>
    `${String(item.requestId)}\u0001${question}`;
  const pickOption = (item: QuestionItem, question: string, label: string, multi: boolean) => {
    const key = draftKey(item, question);
    const current = questionDrafts[key] ?? "";
    questionDrafts[key] = multi && current ? `${current}, ${label}` : label;
  };
  const submitAnswers = (c: Conversation, item: QuestionItem) => {
    const answers: Record<string, string> = {};
    for (const question of item.questions) {
      const value = String(questionDrafts[draftKey(item, question.question)] ?? "").trim();
      if (!value) {
        return;
      }
      answers[question.question] = value;
    }
    void answerQuestion(c, item, answers);
  };
</script>

<div
  class="agent-chat flex h-full w-full flex-col border-l border-zinc-700 bg-zinc-900"
  data-annotate="pane-agent-chat"
>
  <div class="flex shrink-0 items-center gap-2 border-b border-zinc-700 px-3 py-1">
    <span class="text-xs font-semibold tracking-wide text-zinc-300">
      <i class="bi bi-robot" aria-hidden="true"></i>
    </span>
    {#if conversation}
      <select
        class="min-w-0 rounded border border-transparent bg-transparent px-1 py-0.5 text-xs font-semibold text-zinc-200 hover:border-zinc-600 disabled:opacity-50"
        data-annotate="select-agent-provider"
        aria-label="Agent"
        title="Agent (changing it starts a new chat)"
        value={conversation.provider}
        disabled={conversation.starting}
        onchange={onProviderChange}
      >
        <option value="claude">Claude Code</option>
        <option value="codex">Codex</option>
      </select>
    {:else}
      <span class="text-xs font-semibold tracking-wide text-zinc-300">Agent Chat</span>
    {/if}
    <div class="ml-auto flex shrink-0 items-center gap-1">
      <button
        type="button"
        class="rounded px-1.5 py-0.5 text-xs text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200 disabled:cursor-not-allowed disabled:opacity-50"
        data-annotate="button-agent-new-chat"
        title="New chat (ends this conversation and starts the agent again)"
        aria-label="New chat"
        disabled={!conversation || conversation.starting}
        onclick={() => conversation && restart(conversation)}
      >
        <i class="bi bi-plus-lg" aria-hidden="true"></i>
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

  {#if conversation}
    <div class="flex shrink-0 items-center gap-2 border-b border-zinc-800 px-3 py-1">
      <i class="bi bi-folder2 text-[11px] text-zinc-500" aria-hidden="true"></i>
      <input
        class="min-w-0 flex-1 rounded border border-zinc-700 bg-zinc-950 px-1.5 py-0.5 font-mono text-[11px] text-zinc-300 outline-none focus:border-blue-400 disabled:opacity-60"
        data-annotate="input-agent-directory"
        aria-label="Agent working directory"
        title={`Working directory of the agent: ${conversation.cwd || "(not started)"}\n` +
          "Edit and press Enter to use another directory for this chat (for example a " +
          "clone of the application's repository). Empty: back to the configured one."}
        spellcheck="false"
        placeholder={conversation.chosenDirectory ?? "(the configured directory, or the query file folder)"}
        bind:value={directoryInput}
        disabled={conversation.starting}
        onfocus={() => (editingDirectory = true)}
        onblur={() => {
          editingDirectory = false;
          directoryInput = shownDirectory(conversation);
        }}
        onkeydown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            applyDirectory();
            (e.currentTarget as HTMLInputElement).blur();
          } else if (e.key === "Escape") {
            e.stopPropagation();
            (e.currentTarget as HTMLInputElement).blur();
          }
        }}
      />
    </div>
  {/if}

  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div
    class="chat-log min-h-0 flex-1 overflow-y-auto px-3 py-2"
    bind:this={logEl}
    onscroll={onLogScroll}
    onclick={onLogClick}
    data-annotate="list-agent-chat"
    aria-label="Agent conversation"
  >
    {#if appStore.selectedConnection === null}
      <p class="text-xs leading-relaxed text-zinc-500">Select a connection first.</p>
    {:else if conversation && conversation.items.length === 0 && !conversation.starting && conversation.queue.length === 0}
      <p class="text-xs leading-relaxed text-zinc-500" data-annotate="text-agent-chat-empty">
        Ask {agentLabel} to write or change your queries. It works in the directory above
        and is told which query file is open and the statement at the cursor, so it can
        edit the file directly; the editor picks the change up.
        {#if appStore.agentInfo?.auto_accept_edits && conversation.provider === "claude"}
          File edits are accepted without asking; commands still ask for approval here.
        {/if}
      </p>
    {/if}

    {#if conversation}
      {@const c = conversation}
      {#each c.items as item, index (index)}
        {#if item.kind === "user"}
          <div class="chat-item chat-user" data-annotate="item-agent-chat-user">
            <div class="chat-role">You</div>
            <p class="whitespace-pre-wrap text-[13px] leading-relaxed text-zinc-200">{item.text}</p>
          </div>
        {:else if item.kind === "text"}
          <div class="chat-item" data-annotate="item-agent-chat-text">
            <div class="chat-role">{agentLabel}</div>
            <!-- eslint-disable-next-line svelte/no-at-html-tags -->
            <div class="chat-markdown">{@html renderAgentText(item.text, item.streaming)}</div>
          </div>
        {:else if item.kind === "thinking"}
          {#if thinkingActive(c, item)}
            <div class="chat-item chat-thinking" role="status" aria-label="Thinking…">
              <span class="chat-sparkle" aria-hidden="true">✦</span>
              <span aria-hidden="true">
                {#each Array.from("Thinking…") as char, i (i)}
                  <span class="chat-wave" style="animation-delay: {i * 70}ms">{char}</span>
                {/each}
              </span>
            </div>
          {/if}
        {:else if item.kind === "tool_use"}
          <details class="chat-item chat-tool" data-annotate="item-agent-chat-tool">
            <summary class="truncate">
              <i
                class={item.isError
                  ? "bi bi-x-circle text-red-400"
                  : item.result === null
                    ? "bi bi-hourglass-split text-amber-300"
                    : "bi bi-check2-circle text-emerald-400"}
                aria-hidden="true"
              ></i>
              {toolTitle(item)}{item.isError
                ? " · failed"
                : item.result === null
                  ? " · running"
                  : ""}
            </summary>
            <pre>{JSON.stringify(item.input, null, 2)}{item.result ? `\n${item.result}` : ""}</pre>
          </details>
        {:else if item.kind === "permission"}
          <div class="chat-item chat-ask" data-annotate="item-agent-chat-permission">
            <strong class="text-xs text-amber-200">
              <i class="bi bi-shield-exclamation" aria-hidden="true"></i>
              {agentLabel} asks to use {item.toolName}
            </strong>
            {#if item.description}
              <pre>{item.description}</pre>
            {/if}
            <pre>{JSON.stringify(item.input, null, 2)}</pre>
            {#if item.decision}
              <p class="mt-1 text-[11px] text-zinc-400">
                {item.decision === "allow" ? "Allowed" : "Denied"}
              </p>
            {:else}
              <div class="mt-2 flex gap-2">
                <button
                  type="button"
                  class="rounded border border-zinc-600 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-800"
                  data-annotate="button-agent-permission-deny"
                  onclick={() => respondPermission(c, item, "deny")}
                >
                  Deny
                </button>
                <button
                  type="button"
                  class="ml-auto rounded border border-emerald-500/50 bg-emerald-500/15 px-2 py-0.5 text-xs text-emerald-300 hover:bg-emerald-500/25"
                  data-annotate="button-agent-permission-allow"
                  onclick={() => respondPermission(c, item, "allow")}
                >
                  Allow
                </button>
              </div>
            {/if}
          </div>
        {:else if item.kind === "question"}
          <div class="chat-item chat-ask" data-annotate="item-agent-chat-question">
            <strong class="text-xs text-amber-200">
              <i class="bi bi-question-circle" aria-hidden="true"></i>
              {item.title || `${agentLabel} asks`}
            </strong>
            {#if item.decision}
              <pre>{item.decision === "declined"
                  ? "Declined"
                  : Object.entries(item.answers)
                      .map(([question, answer]) => `${question}\n→ ${answer}`)
                      .join("\n\n")}</pre>
            {:else}
              {#each item.questions as question (question.question)}
                <label class="mt-2 block text-xs text-zinc-200">
                  {question.question}
                  {#if question.description}
                    <span class="block text-[11px] text-zinc-400">{question.description}</span>
                  {/if}
                  <input
                    class="mt-1 block w-full rounded border border-zinc-600 bg-zinc-950 px-1.5 py-0.5 text-xs text-zinc-200 outline-none focus:border-blue-400"
                    data-annotate="input-agent-question-answer"
                    type={question.kind === "number" ? "number" : "text"}
                    placeholder={question.placeholder}
                    min={question.min ?? undefined}
                    max={question.max ?? undefined}
                    step={question.step ?? undefined}
                    bind:value={questionDrafts[draftKey(item, question.question)]}
                  />
                </label>
                {#if question.options.length > 0}
                  <div class="mt-1 flex flex-wrap gap-1">
                    {#each question.options as option (option.label)}
                      <button
                        type="button"
                        class="rounded border border-zinc-600 px-1.5 py-0.5 text-[11px] text-zinc-300 hover:bg-zinc-800"
                        data-annotate="button-agent-question-option"
                        title={option.description}
                        onclick={() =>
                          pickOption(item, question.question, option.label, question.multiSelect)}
                      >
                        {option.label}
                      </button>
                    {/each}
                  </div>
                {/if}
              {/each}
              <div class="mt-2 flex gap-2">
                <button
                  type="button"
                  class="rounded border border-zinc-600 px-2 py-0.5 text-xs text-zinc-300 hover:bg-zinc-800"
                  data-annotate="button-agent-question-decline"
                  onclick={() => answerQuestion(c, item, null)}
                >
                  Decline
                </button>
                <button
                  type="button"
                  class="ml-auto rounded border border-blue-500/50 bg-blue-500/15 px-2 py-0.5 text-xs text-blue-300 hover:bg-blue-500/25"
                  data-annotate="button-agent-question-submit"
                  onclick={() => submitAnswers(c, item)}
                >
                  Submit
                </button>
              </div>
            {/if}
          </div>
        {:else if item.kind === "result"}
          <div class="chat-item chat-meta" data-annotate="item-agent-chat-result">{item.summary}</div>
        {:else}
          <div
            class="chat-item chat-meta whitespace-pre-wrap {item.level === 'error'
              ? 'text-red-300'
              : ''}"
            data-annotate="item-agent-chat-note"
          >
            {item.text}
          </div>
        {/if}
      {/each}
    {/if}
  </div>

  {#if conversation}
    {@const c = conversation}
    <form
      class="shrink-0 border-t border-zinc-700 px-3 py-2"
      onsubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      {#if c.queue.length > 0}
        <div
          class="mb-2 overflow-hidden rounded border border-amber-900/70 bg-amber-950/20 text-[11px]"
          data-annotate="list-agent-chat-queue"
        >
          <div class="flex items-center gap-2 px-2 py-1 text-zinc-400">
            <span class="font-semibold text-amber-300">Queued ({c.queue.length})</span>
            <span class="min-w-0 flex-1 truncate {c.queueHeld ? 'text-amber-400' : ''}">
              {c.queueHeld ? "held" : "sent in order when the agent is idle"}
            </span>
            {#if c.queueHeld}
              <button
                type="button"
                class="rounded border border-amber-700 bg-amber-900/40 px-2 text-[11px] text-amber-200 hover:bg-amber-900/70"
                data-annotate="button-agent-queue-resume"
                title="Send the queued messages in order when the agent is idle"
                onclick={() => resumeQueue(c)}
              >
                Resume
              </button>
            {/if}
          </div>
          <div class="max-h-44 overflow-y-auto">
            {#each c.queue as entry, index (entry.id)}
              <div class="flex items-start gap-1 border-t border-amber-900/40 px-2 py-1">
                <span class="w-3 pt-1 text-right font-mono text-zinc-500" aria-hidden="true">
                  {index + 1}
                </span>
                <textarea
                  class="min-w-0 flex-1 resize-y rounded border border-zinc-700 bg-zinc-950 px-1.5 py-0.5 font-mono text-xs text-zinc-200 outline-none focus:border-amber-600"
                  data-annotate="input-agent-queue-text"
                  aria-label={`Queued message ${index + 1}`}
                  spellcheck="false"
                  rows={rowsFor(entry.text)}
                  bind:value={entry.text}
                ></textarea>
                <button
                  type="button"
                  class="rounded px-1 text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200 disabled:opacity-30"
                  data-annotate="button-agent-queue-merge"
                  title="Append to the message above"
                  aria-label="Append to the message above"
                  disabled={index === 0}
                  onclick={() => mergeQueuedUp(c, entry.id)}
                >
                  <i class="bi bi-arrow-up" aria-hidden="true"></i>
                </button>
                <button
                  type="button"
                  class="rounded px-1 text-zinc-400 hover:bg-zinc-800 hover:text-zinc-200"
                  data-annotate="button-agent-queue-remove"
                  title="Remove from the queue"
                  aria-label="Remove from the queue"
                  onclick={() => removeQueued(c, entry.id)}
                >
                  <i class="bi bi-x-lg" aria-hidden="true"></i>
                </button>
              </div>
            {/each}
          </div>
        </div>
      {/if}
      <textarea
        bind:value={c.draft}
        class="min-h-[3.25rem] w-full resize-y rounded border border-zinc-600 bg-zinc-800 px-2 py-1 text-xs text-zinc-200 outline-none placeholder:text-zinc-500 focus:border-blue-400 disabled:opacity-50"
        data-annotate="input-agent-chat-message"
        aria-label="Message to the agent"
        rows="3"
        placeholder={c.queueing
          ? "Queue a message for when the agent is idle…"
          : `Ask ${agentLabel}… (Enter to send, Shift+Enter for a newline)`}
        oncompositionstart={() => (composing = true)}
        oncompositionend={() => (composing = false)}
        onkeydown={onComposerKeydown}
      ></textarea>
      <div class="mt-1 flex items-center gap-2">
        <span
          class="min-w-0 flex-1 truncate text-[11px] text-zinc-500"
          role="status"
          data-annotate="text-agent-chat-status"
          title={c.sessionDetail}
        >
          {statusText}{c.sessionDetail ? ` · ${c.sessionDetail}` : ""}
        </span>
        <button
          type="button"
          class="shrink-0 rounded border border-zinc-600 px-2 py-1 text-xs text-zinc-300 hover:bg-zinc-800 disabled:cursor-not-allowed disabled:opacity-40"
          data-annotate="button-agent-chat-stop"
          title="Stop the agent (the conversation is kept)"
          disabled={!c.busy}
          onclick={() => interrupt(c)}
        >
          <i class="bi bi-stop-fill" aria-hidden="true"></i> Stop
        </button>
        <button
          type="submit"
          class="shrink-0 rounded border border-blue-500/50 bg-blue-500/15 px-2 py-1 text-xs text-blue-300 hover:bg-blue-500/25 disabled:cursor-not-allowed disabled:opacity-50"
          data-annotate="button-agent-chat-send"
          disabled={!c.draft.trim() || (c.launched && !c.ready && !c.queueing)}
        >
          {c.queueing ? "Queue" : "Send"}
        </button>
      </div>
    </form>
  {/if}
</div>

<style>
  /* The log holds HTML produced by renderMarkdown, so its rules are :global under the pane.
     Adapted from the agent chat of Astragal (src/chat/pane.css). */
  .chat-log {
    user-select: text;
    -webkit-user-select: text;
  }
  .chat-item {
    padding: 4px 0;
    overflow-wrap: anywhere;
  }
  .chat-user {
    margin-top: 8px;
    border-top: 1px solid #343c47;
    padding-top: 8px;
  }
  .chat-user:first-child {
    margin-top: 0;
    border-top: 0;
    padding-top: 2px;
  }
  .chat-role {
    margin-bottom: 3px;
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: #96aecb;
  }
  .chat-tool,
  .chat-meta {
    font-size: 11px;
    color: #a6b0bd;
  }
  .chat-tool summary {
    cursor: pointer;
  }
  .chat-ask {
    margin: 4px 0;
    border-radius: 6px;
    background: #30302a;
    padding: 10px;
  }
  .agent-chat pre {
    margin-top: 6px;
    max-height: 240px;
    overflow: auto;
    font-family: ui-monospace, Menlo, Consolas, monospace;
    font-size: 11px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: #c5ced9;
  }
  .chat-thinking {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: #c9a56a;
  }
  .chat-sparkle {
    display: inline-block;
    animation: chat-twinkle 1.4s ease-in-out infinite;
  }
  .chat-wave {
    display: inline-block;
    opacity: 0.45;
    animation: chat-wave 1.8s ease-in-out infinite;
  }
  @keyframes chat-twinkle {
    0%,
    100% {
      opacity: 0.3;
    }
    50% {
      opacity: 1;
    }
  }
  @keyframes chat-wave {
    0%,
    40%,
    100% {
      transform: translateY(0);
      opacity: 0.45;
    }
    20% {
      transform: translateY(-1.5px);
      opacity: 1;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .chat-sparkle,
    .chat-wave {
      animation: none;
    }
  }

  .chat-markdown {
    font-size: 13px;
    line-height: 1.55;
    color: #dfe5ec;
  }
  .chat-markdown :global(p),
  .chat-markdown :global(pre),
  .chat-markdown :global(ul),
  .chat-markdown :global(ol),
  .chat-markdown :global(table),
  .chat-markdown :global(blockquote),
  .chat-markdown :global(.code-block),
  .chat-markdown :global(.details) {
    margin: 0 0 6px;
  }
  .chat-markdown :global(> :last-child),
  .chat-markdown :global(li > :last-child),
  .chat-markdown :global(blockquote > :last-child),
  .chat-markdown :global(.details > :last-child) {
    margin-bottom: 0;
  }
  .chat-markdown :global(ul),
  .chat-markdown :global(ol) {
    padding-left: 20px;
  }
  .chat-markdown :global(ul) {
    list-style: disc;
  }
  .chat-markdown :global(ol) {
    list-style: decimal;
  }
  .chat-markdown :global(li) {
    margin: 2px 0;
    padding-left: 2px;
  }
  .chat-markdown :global(li::marker) {
    color: #93b2d3;
  }
  .chat-markdown :global(code) {
    border: 1px solid #37414e;
    border-radius: 4px;
    background: #242b35;
    padding: 1px 4px;
    font-family: ui-monospace, Menlo, Consolas, monospace;
    font-size: 12px;
    color: #d3e6fc;
  }
  .chat-markdown :global(pre) {
    max-height: 320px;
    overflow: auto;
    border: 1px solid #343e4b;
    border-radius: 6px;
    background: #11161d;
    padding: 8px 10px;
    font-size: 12px;
    line-height: 1.5;
    white-space: pre;
    overflow-wrap: normal;
  }
  .chat-markdown :global(pre code) {
    border: 0;
    border-radius: 0;
    background: none;
    padding: 0;
    font-size: inherit;
    color: #a7f3d0;
  }
  .chat-markdown :global(h1),
  .chat-markdown :global(h2),
  .chat-markdown :global(h3),
  .chat-markdown :global(h4),
  .chat-markdown :global(h5),
  .chat-markdown :global(h6) {
    margin: 9px 0 4px;
    font-size: 14px;
    font-weight: 600;
    line-height: 1.4;
  }
  .chat-markdown :global(h1) {
    font-size: 16px;
  }
  .chat-markdown :global(> :first-child) {
    margin-top: 0;
  }
  .chat-markdown :global(a) {
    cursor: pointer;
    color: #8cc5ff;
    text-decoration: underline;
  }
  .chat-markdown :global(table) {
    display: block;
    width: max-content;
    max-width: 100%;
    overflow-x: auto;
    border: 1px solid #394451;
    border-radius: 6px;
    border-collapse: separate;
    border-spacing: 0;
    font-size: 12px;
  }
  .chat-markdown :global(td),
  .chat-markdown :global(th) {
    border-right: 1px solid #303b48;
    border-bottom: 1px solid #303b48;
    padding: 5px 9px;
    vertical-align: top;
  }
  .chat-markdown :global(th) {
    background: #26313e;
    text-align: left;
    font-weight: 600;
    color: #d7e7f8;
  }
  .chat-markdown :global(tr > :last-child) {
    border-right: 0;
  }
  .chat-markdown :global(tbody tr:last-child > td) {
    border-bottom: 0;
  }
  .chat-markdown :global(blockquote) {
    border-left: 2px solid #6c91b9;
    border-radius: 0 4px 4px 0;
    background: #1d2631;
    padding: 5px 10px;
    color: #b5c5d8;
  }
  .chat-markdown :global(hr) {
    margin: 8px 0;
    border: 0;
    border-top: 1px solid #343e4b;
  }
  .chat-markdown :global(.details) {
    border: 1px solid #343e4b;
    border-radius: 6px;
    padding: 7px 9px;
  }
  .chat-markdown :global(.summary) {
    margin-bottom: 4px;
    font-weight: 600;
    color: #b9d2ee;
  }
  .chat-markdown :global(.code-block) {
    position: relative;
    overflow: hidden;
    border: 1px solid #343e4b;
    border-radius: 6px;
    background: #11161d;
  }
  .chat-markdown :global(.code-block::before) {
    display: block;
    border-bottom: 1px solid #303a46;
    background: #202832;
    padding: 4px 10px;
    font-size: 10px;
    color: #9aafc6;
    content: "Code";
  }
  .chat-markdown :global(.code-block pre) {
    margin: 0;
    border: 0;
    border-radius: 0;
  }
  .chat-markdown :global(.copy-code),
  .chat-markdown :global(.insert-code) {
    position: absolute;
    top: 2px;
    right: 5px;
    cursor: pointer;
    border: 0;
    border-radius: 3px;
    background: transparent;
    padding: 2px 5px;
    color: #ced9e6;
  }
  .chat-markdown :global(.insert-code) {
    right: 30px;
  }
  .chat-markdown :global(.copy-code:hover),
  .chat-markdown :global(.insert-code:hover) {
    background: #354358;
  }
  /* Icons from the bootstrap-icons font (bi-copy / bi-check2 / bi-box-arrow-in-left) */
  .chat-markdown :global(.copy-code::after),
  .chat-markdown :global(.insert-code::after) {
    font: normal normal normal 12px/1 "bootstrap-icons";
  }
  .chat-markdown :global(.copy-code::after) {
    content: "\f759";
  }
  .chat-markdown :global(.copy-code.copied::after) {
    color: #a7d8b3;
    content: "\f272";
  }
  .chat-markdown :global(.insert-code::after) {
    content: "\f1bd";
  }
</style>
