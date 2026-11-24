/// Conversations with the coding agents (Claude Code / Codex) behind the chat pane
/// (CYBERNEURA-DEV-1354). One conversation per connection: the agent's working directory and
/// the context it is given belong to the connection.
///
/// The protocol engines (`./beckwell`) and the rules for queueing, stopping and restarting are
/// the ones of Astragal's agent chat (`src/chat/pane.ts`), which were ported from Beckwell; this
/// module replaces Astragal's DOM renderer with Svelte state. See docs/agent-chat.md.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { SvelteMap } from "svelte/reactivity";
import { createProtocol, type ChatAgent } from "./beckwell";
import type {
  ChatHost,
  ChatProtocol,
  Item,
  PermissionDecision,
  PermissionItem,
  QuestionItem,
} from "./beckwell/types";
import { canDispatch, createQueued, mergeUp, takeNext, type QueuedMessage } from "./queue";

/// The turn started by the last dispatch, until its result arrives. Protocols report a turn
/// that never started (failed write, rejected turn/start) only by going idle without a result,
/// so the idle callback checks this.
interface InFlight {
  started: boolean;
  fail(error?: unknown): void;
}

export class Conversation {
  readonly connection: string;
  provider: ChatAgent;
  items = $state<Item[]>([]);
  /// Working directory the agent reported / was started in
  cwd = $state("");
  /// A directory chosen in the pane for this conversation (null: the configured one)
  chosenDirectory = $state<string | null>(null);
  process: number | null = null;
  protocol: ChatProtocol | null = null;
  agentSessionId: string | null = null;
  ready = $state(false);
  busy = $state(false);
  starting = $state(false);
  stopped = $state(false);
  /// Whether the agent process has been started for this conversation. It is started by the
  /// first message, not by opening the pane: an agent process per connection the user merely
  /// looked at would be a waste (each is a full CLI)
  launched = $state(false);
  usage = $state("");
  /// Model and settings line reported by the agent when the session starts
  sessionDetail = $state("");
  draft = $state("");
  /// Messages sent while busy, dispatched in order when the agent is idle
  queue = $state<QueuedMessage[]>([]);
  /// Set by Stop and New chat so the queue does not start on its own
  queueHeld = $state(false);
  inFlight: InFlight | null = null;
  /// Whether the agent has been sent a message in this session (the first one also carries the
  /// table list)
  introduced = false;

  constructor(connection: string, provider: ChatAgent) {
    this.connection = connection;
    this.provider = provider;
  }

  /// A message typed now is queued instead of sent
  get queueing(): boolean {
    return (
      this.busy ||
      (this.starting && !this.stopped) ||
      (this.queue.length > 0 && !this.queueHeld)
    );
  }
}

/// What the page supplies right before a message is sent: the text for the agent (the request
/// plus the editor context). Asked at dispatch time, not when the message was typed — a queued
/// message is sent later, and the file / cursor may have changed. It may save the open file
/// first, so that the agent reads what the editor shows. null: do not send (nothing to send to).
export type PromptBuilder = (
  connection: string,
  request: string,
  first: boolean,
) => Promise<string | null>;

const conversations = new SvelteMap<string, Conversation>();
let promptBuilder: PromptBuilder = async (_connection, request) => request;
let generation = 0;
let nextId = 0;
let initializing: Promise<void> | null = null;

/// Turns whose message has been handed to the protocol (from then on Stop has to interrupt
/// the agent; before that it only has to drop the dispatch)
const sentToAgent = new WeakSet<InFlight>();

const byProcess = (id: number): Conversation | undefined =>
  [...conversations.values()].find((c) => c.process === id);

const note = (c: Conversation, text: string, error = false) => {
  c.items.push({ kind: "note", text, level: error ? "error" : "info" });
};

/// Registers the event listeners (once per page) and synchronizes the process IDs / generation
/// with the backend, which ends agents left over from a previous page load or a settings
/// reload. Idempotent until resetConversations; every start awaits it.
const init = (): Promise<void> => {
  initializing ??= (async () => {
    await listening();
    const reset = await invoke<{ highestId: number; generation: number }>("agent_reset");
    nextId = Math.max(nextId, reset.highestId);
    generation = reset.generation;
  })();
  return initializing;
};

let listeners: Promise<void> | null = null;
const listening = (): Promise<void> => {
  listeners ??= (async () => {
    await listen<{ id: number; line: string }>("agent-event", ({ payload }) => {
      const c = byProcess(payload.id);
      if (!c) {
        return;
      }
      try {
        c.protocol?.handleMessage(JSON.parse(payload.line));
      } catch (e) {
        note(c, `Invalid agent message: ${String(e)}`, true);
      }
    });
    await listen<{ id: number; line: string }>("agent-stderr", ({ payload }) => {
      const c = byProcess(payload.id);
      if (c) {
        note(c, payload.line, true);
      }
    });
    await listen<{ id: number; code: number | null }>("agent-exit", ({ payload }) => {
      const c = byProcess(payload.id);
      if (!c) {
        return;
      }
      c.process = null;
      c.ready = false;
      c.busy = false;
      c.stopped = true;
      c.inFlight = null;
      note(
        c,
        `The agent exited (${payload.code ?? "unknown status"}). Use New chat to start it again.`,
        true,
      );
    });
  })();
  return listeners;
};

const providerName = (provider: ChatAgent) =>
  provider === "claude" ? "Claude Code" : "Codex";

const start = async (c: Conversation, resume: string | null = null): Promise<void> => {
  c.starting = true;
  c.ready = false;
  try {
    await init();
  } catch (e) {
    c.starting = false;
    c.stopped = true;
    // Keep what is queued, but do not let it look like it is about to be sent
    c.queueHeld = true;
    note(c, String(e), true);
    return;
  }
  const id = ++nextId;
  c.process = id;
  const live = () =>
    c.process === id && !c.stopped && conversations.get(c.connection) === c;
  const host: ChatHost = {
    get items() {
      return c.items;
    },
    push(item) {
      if (!live()) {
        return;
      }
      c.items.push(item);
      if (item.kind === "result") {
        c.inFlight = null;
      }
    },
    note(text, level) {
      if (live()) {
        note(c, text, level === "error");
      }
    },
    scrollToBottom() {},
    async write(payload) {
      if (!live()) {
        return false;
      }
      try {
        await invoke("agent_write", { id, line: JSON.stringify(payload) });
        return live();
      } catch (e) {
        if (live()) {
          c.ready = false;
          c.busy = false;
          note(c, String(e), true);
        }
        return false;
      }
    },
    setBusy(busy) {
      if (!live()) {
        return;
      }
      c.busy = busy;
      if (!busy) {
        afterIdle(c);
      }
    },
    turnStarted() {
      if (live() && c.inFlight) {
        c.inFlight.started = true;
      }
    },
    setReady(ready) {
      if (!live()) {
        return;
      }
      c.ready = ready;
      if (ready) {
        afterIdle(c);
      }
    },
    setSession(session) {
      if (!live()) {
        return;
      }
      c.agentSessionId = session?.sessionId || null;
      c.sessionDetail = session ? [session.model, session.detail].filter(Boolean).join(" · ") : "";
    },
    setUsage(usage) {
      if (live()) {
        c.usage = usage ?? "";
      }
    },
    setWorkingDirectory(value) {
      if (live()) {
        c.cwd = value;
      }
    },
    setCommands() {},
    setModels() {},
    clear() {
      if (live()) {
        c.items = [];
      }
    },
    restart(message) {
      if (!live()) {
        return;
      }
      note(c, message);
      void restart(c, c.agentSessionId);
    },
    interruptFailed(message) {
      if (live()) {
        note(c, message, true);
      }
    },
  };
  c.protocol = createProtocol(c.provider, host, {
    resume,
    replayHistory: false,
    async readClaudeSessions() {
      return "";
    },
  });
  try {
    // The backend builds the command line for the provider passed here, the same one whose
    // protocol was created above (the header lets the user pick another agent than the
    // configured one for this conversation)
    const started = await invoke<{ cwd: string }>("agent_start", {
      id,
      generation,
      connection: c.connection,
      provider: c.provider,
      directory: c.chosenDirectory,
    });
    if (c.process !== id || c.stopped || conversations.get(c.connection) !== c) {
      await invoke("agent_stop", { id });
      return;
    }
    c.cwd = started.cwd;
    c.protocol.start();
  } catch (e) {
    if (c.process === id) {
      c.process = null;
      c.stopped = true;
      c.queueHeld = true;
      note(c, `${String(e)} Use New chat to try again.`, true);
    }
  } finally {
    c.starting = false;
  }
};

/// Starts the agent again: a fresh session (New chat, another provider / directory), or the
/// same session after the process had to be replaced (`resume`).
export const restart = async (c: Conversation, resume: string | null = null): Promise<void> => {
  if (c.starting) {
    return;
  }
  if (!c.launched) {
    // Nothing is running yet (a provider / directory change before the first message): the
    // new settings apply when the first message starts the agent
    c.items = [];
    c.cwd = "";
    return;
  }
  c.starting = true;
  c.queueHeld = true;
  c.stopped = true;
  c.ready = false;
  c.busy = false;
  c.inFlight = null;
  const old = c.process;
  c.process = null;
  if (old !== null) {
    try {
      await invoke("agent_stop", { id: old });
    } catch (e) {
      c.starting = false;
      note(c, String(e), true);
      return;
    }
  }
  if (!resume) {
    c.items = [];
    c.usage = "";
    c.sessionDetail = "";
    c.agentSessionId = null;
    c.introduced = false;
  }
  if (conversations.get(c.connection) !== c) {
    c.starting = false;
    return;
  }
  c.stopped = false;
  c.launched = true;
  await start(c, resume);
};

/// The conversation of a connection, created on first use. Its agent is not started here
/// (see Conversation.launched)
export const conversationFor = (connection: string, provider: ChatAgent): Conversation => {
  const existing = conversations.get(connection);
  if (existing) {
    return existing;
  }
  const c = new Conversation(connection, provider);
  conversations.set(connection, c);
  return c;
};

/// The conversation of a connection if one exists (does not start an agent)
export const existingConversation = (connection: string | null): Conversation | undefined =>
  connection === null ? undefined : conversations.get(connection);

export const setPromptBuilder = (builder: PromptBuilder) => {
  promptBuilder = builder;
};

/// Sends a message, or queues it while the agent is answering (or starting), and behind
/// messages that are still waiting so that a new one never overtakes them
export const submit = async (c: Conversation, text: string): Promise<void> => {
  const request = text.trim();
  if (!request) {
    return;
  }
  if (!c.launched) {
    // The first message starts the agent; it is queued below and sent once the agent is ready
    c.launched = true;
    void start(c);
  }
  if (c.queueing) {
    if (c.queue.length === 0) {
      c.queueHeld = false;
    }
    c.queue.push(createQueued(request));
    c.draft = "";
    pump(c);
    return;
  }
  if (!c.ready) {
    return;
  }
  const sending = dispatch(c, request);
  if (!sending) {
    return;
  }
  // Clear and resume before awaiting: the user may type the next message, switch connections
  // or press Stop while the request is being written
  c.draft = "";
  c.queueHeld = false;
  await sending;
};

/// Starts one message as a new turn. Returns null if it could not be started.
/// `onFailed` runs only if the agent never accepted the message.
const dispatch = (
  c: Conversation,
  request: string,
  onFailed?: () => void,
): Promise<void> | null => {
  if (!c.ready || c.busy || !c.protocol) {
    return null;
  }
  const protocol = c.protocol;
  const process = c.process;
  c.items.push({ kind: "user", text: request });
  c.busy = true;
  // A failed turn holds the rest of the queue: later instructions usually depend on it. Only a
  // message the agent never accepted goes back to the queue (onFailed); one that started may
  // already have changed files and must not be resent
  const record: InFlight = {
    started: false,
    fail(error?: unknown) {
      if (c.inFlight !== record || c.stopped || c.protocol !== protocol || c.process !== process) {
        return;
      }
      c.inFlight = null;
      c.busy = false;
      c.queueHeld = true;
      if (error !== undefined) {
        note(c, String(error), true);
      }
      if (!record.started) {
        onFailed?.();
      }
    },
  };
  c.inFlight = record;
  const first = !c.introduced;
  return (async () => {
    const prompt = await promptBuilder(c.connection, request, first);
    // Stop / New chat / a process exit while the context was being prepared: do not send
    if (c.inFlight !== record || c.protocol !== protocol || c.process !== process) {
      return;
    }
    if (prompt === null) {
      record.fail("The message was not sent: its connection is no longer available.");
      return;
    }
    c.introduced = true;
    sentToAgent.add(record);
    await protocol.send(prompt);
  })().then(
    () => {},
    (e) => record.fail(e),
  );
};

/// A macrotask, not a microtask: Claude reports idle before it pushes the result, and the
/// result must be recorded (inFlight cleared) before the next queued message starts
const afterIdle = (c: Conversation) => {
  const record = c.inFlight;
  if (!record && c.queue.length === 0) {
    return;
  }
  setTimeout(() => {
    // Idle without a result: the turn was rejected, could not be written, or failed (e.g. a
    // Codex error notification). `fail` re-queues only the first two
    if (record && c.inFlight === record && !c.busy) {
      record.fail();
    }
    pump(c);
  }, 0);
};

const pump = (c: Conversation) => {
  if (conversations.get(c.connection) !== c) {
    return;
  }
  if (
    !canDispatch({
      queued: c.queue.length,
      held: c.queueHeld,
      busy: c.busy,
      ready: c.ready,
      running: c.process !== null && !c.stopped,
      starting: c.starting,
    })
  ) {
    return;
  }
  const request = takeNext(c.queue);
  if (request === null) {
    return;
  }
  // Keep a message that was not sent and wait for Resume instead of retrying in a loop
  const restore = () => {
    c.queue.unshift(createQueued(request));
    c.queueHeld = true;
  };
  if (!dispatch(c, request, restore)) {
    restore();
  }
};

/// Resume: lets the held queue start again
export const resumeQueue = (c: Conversation) => {
  c.queueHeld = false;
  pump(c);
};

export const removeQueued = (c: Conversation, id: number) => {
  const index = c.queue.findIndex((entry) => entry.id === id);
  if (index >= 0) {
    c.queue.splice(index, 1);
  }
};

export const mergeQueuedUp = (c: Conversation, id: number) => {
  mergeUp(
    c.queue,
    c.queue.findIndex((entry) => entry.id === id),
  );
};

/// Stop: interrupts the running turn. Pending approvals / questions are declined first (the
/// agent is waiting for them), and the queue is held — Stop is usually pressed to change
/// direction
export const interrupt = async (c: Conversation): Promise<void> => {
  const protocol = c.protocol;
  const process = c.process;
  const live = () => !c.stopped && c.protocol === protocol && c.process === process;
  if (!protocol || !live()) {
    return;
  }
  c.queueHeld = true;
  const pending = c.inFlight;
  if (pending && !pending.started && c.busy && !sentToAgent.has(pending)) {
    // Stop arrived while the message was still being prepared (the open file is saved and the
    // editor context collected first). Nothing has reached the agent: drop the dispatch, so
    // that it is not sent after the user stopped it. The dispatch sees that it is no longer
    // the turn in flight and returns without sending
    c.inFlight = null;
    c.busy = false;
    note(c, "Stopped before the message was sent.");
    return;
  }
  for (const item of c.items) {
    if (!live()) {
      return;
    }
    if (item.kind === "permission" && !item.decision) {
      await respondPermission(c, item, "deny");
    }
    if (item.kind === "question" && !item.decision) {
      await answerQuestion(c, item, null);
    }
  }
  if (!live()) {
    return;
  }
  const interrupted = await protocol.interrupt();
  if (live() && !interrupted) {
    note(c, "Could not interrupt the agent. Use New chat to stop its process.", true);
  }
};

/// Answers an approval request. The decision is recorded before it is written, so that a
/// second click cannot send another answer
export const respondPermission = async (
  c: Conversation,
  item: PermissionItem,
  decision: PermissionDecision,
): Promise<void> => {
  if (c.stopped || !c.protocol || !c.items.includes(item) || item.decision) {
    return;
  }
  item.decision = decision;
  await c.protocol.respondPermission(item, decision);
};

/// Answers a question of the agent (null: decline)
export const answerQuestion = async (
  c: Conversation,
  item: QuestionItem,
  answers: Record<string, string> | null,
): Promise<void> => {
  if (c.stopped || !c.protocol?.answerQuestion || !c.items.includes(item) || item.decision) {
    return;
  }
  item.decision = answers ? "answered" : "declined";
  item.answers = answers ?? {};
  await c.protocol.answerQuestion(item, answers);
};

/// Switches the provider or the working directory of a conversation (a fresh session)
export const reconfigure = async (
  c: Conversation,
  changes: { provider?: ChatAgent; directory?: string | null },
): Promise<void> => {
  if (changes.provider) {
    c.provider = changes.provider;
  }
  if (changes.directory !== undefined) {
    c.chosenDirectory = changes.directory;
  }
  await restart(c);
};

/// Drops every conversation (settings reload: the backend ends the agents in
/// reset_connections; the provider, directories and connections may all have changed)
export const resetConversations = () => {
  for (const c of conversations.values()) {
    c.stopped = true;
    c.process = null;
    c.protocol = null;
  }
  conversations.clear();
  // The backend advanced its generation when it stopped the agents; fetch it again on next use
  initializing = null;
};

export { providerName };
