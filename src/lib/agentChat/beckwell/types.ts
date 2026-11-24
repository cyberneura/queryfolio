// Adapted from Beckwell (Cyberneura), MIT, via Astragal. See docs/agent-chat.md.
// Items laid out on the chat tab screen. Each agent's events are normalized into this shape
export type Item =
  | { kind: 'user'; text: string }
  // id is the ID the agent gave the item (used to find where to append deltas; some agents have none)
  | { kind: 'text'; id?: string; text: string; streaming: boolean }
  | { kind: 'thinking'; id?: string; text: string; streaming: boolean }
  | { kind: 'tool_use'; toolUseId: string; name: string; input: Record<string, unknown>; result: string | null; isError: boolean }
  | { kind: 'permission'; requestId: string | number; toolName: string; description: string; input: Record<string, unknown>; decision: PermissionDecision | null }
  // The agent asks the user by presenting choices (Claude's AskUserQuestion). answers is, once answered, question text -> answer
  | { kind: 'question'; requestId: string | number; input: Record<string, unknown>; title: string; questions: Question[]; decision: QuestionDecision | null; answers: Record<string, string> }
  // at is the time the turn ended (milliseconds). null for replayed past output, where it is unknown
  | { kind: 'result'; summary: string; at: number | null; finalText?: string }
  | { kind: 'note'; text: string; level: NoteLevel };

export type PermissionDecision = 'allow' | 'deny';
export type PermissionItem = Extract<Item, { kind: 'permission' }>;
export type QuestionDecision = 'answered' | 'declined';
export type QuestionItem = Extract<Item, { kind: 'question' }>;

// A single question. choice picks from options (free text is also allowed). text / number have only an input field
export type Question = {
  question: string;
  header: string;
  // One line of help shown below the question
  description: string;
  kind: 'choice' | 'text' | 'number';
  options: QuestionOption[];
  multiSelect: boolean;
  placeholder: string;
  min: number | null;
  max: number | null;
  step: number | null;
  unit: string;
};
export type QuestionOption = { label: string; description: string };
// An in-progress answer to one question (screen state). selected holds the labels of the chosen options, text the free text (text / number questions use only this)
export type QuestionDraft = { selected: string[]; text: string };
export type NoteLevel = 'info' | 'error';

// Session information shown below the header
export type SessionInfo = { sessionId: string; model: string; detail: string };

export type ModelChoice = { value: string; label: string };

// Slash commands completed in the input field (name has no leading /)
export type SlashCommand = { name: string; description: string; argumentHint: string };

// A past session that can be resumed. updatedAt is in seconds (null if unknown)
export type SessionSummary = { id: string; title: string; updatedAt: number | null };

// What is passed when creating a protocol
export type ProtocolOptions = {
  // ID of the session to resume. null when starting a new one
  resume: string | null;
  // False when the host already retains the transcript during process recovery.
  replayHistory?: boolean;
  // Reads the session records Claude Code left on disk. If sessionId is null, the list; otherwise that conversation
  readClaudeSessions(sessionId: string | null): Promise<string>;
  // Persistent session (Claude only). The earlier output is replayed right after attaching
  persistent?: boolean;
};

// The interface to the screen and the child process that ChatTab hands to the protocol
export type ChatHost = {
  // Items of the current session (Svelte $state; rewriting a found item is reflected on screen)
  readonly items: Item[];
  push(item: Item): void;
  note(text: string, level?: NoteLevel): void;
  scrollToBottom(): void;
  // Writes one JSON line to the child process's stdin. false if it was not delivered (the reason is already noted)
  write(payload: unknown): Promise<boolean>;
  // Sending is not possible while waiting for a response
  setBusy(busy: boolean): void;
  // Astragal: the agent accepted the message sent by `send` (Claude: written to stdin;
  // Codex: turn/start succeeded). A turn that ends after this is not resent.
  turnStarted?(): void;
  // Whether it has become able to accept messages
  setReady(ready: boolean): void;
  setSession(session: SessionInfo | null): void;
  // Rate limit display (e.g. "5h 25% · 7d 43%")
  setUsage(usage: string | null): void;
  // The working directory reported by the agent
  setWorkingDirectory(cwd: string): void;
  // The available slash commands and models reported by the agent
  setCommands(commands: SlashCommand[]): void;
  setModels(models: ModelChoice[]): void;
  // Called when the agent restarts the conversation. Clears the on-screen items and the session information
  clear(): void;
  // Restarts the process as a continuation of the same session. Used to end, along with the whole process, a tool command
  // that did not stop on interrupt (Codex). note is shown on screen after the restart
  restart(note: string): void;
  // Called when the agent refuses an interrupt request. Shows the reason and returns to a state where Stop can be pressed again
  interruptFailed(message: string): void;
};

// Per-agent interpretation of the communication
export type ChatProtocol = {
  // Called right after the child process starts. When resuming, lays out the earlier conversation on screen
  start(): void;
  // Past sessions that can be resumed in the launch directory (newest first). Call after becoming ready
  listSessions(): Promise<SessionSummary[]>;
  // One JSON line the child process wrote to stdout
  handleMessage(message: any): void;
  // Sends a user message (ChatTab has already added it to the screen and set busy)
  send(text: string, options?: { cwd?: string }): Promise<void>;
  /** Supports changing cwd at the next turn without restarting the conversation. */
  supportsWorkingDirectory?: boolean;
  respondPermission(item: PermissionItem, decision: PermissionDecision): Promise<void>;
  // Answers a question (answers is question text -> answer). null declines without answering. Only agents that emit question items have this
  answerQuestion?(item: QuestionItem, answers: Record<string, string> | null): Promise<void>;
  // Switches the model while running (absent on agents that cannot). An empty string means the default model
  setModel?(model: string): Promise<void>;
  // Interrupts a turn in the middle of a response. Keeps the process and returns to a state where messages can be sent again.
  // busy is cleared when the end of the turn arrives. false if the request could not be sent
  interrupt(): Promise<boolean>;
};

// Closes the in-progress items of an interrupted turn (so that the streaming display and tools without a result are not left as running)
export function finishInterruptedTurn(host: ChatHost): void {
  for (let i = host.items.length - 1; i >= 0; i--) {
    const item = host.items[i];
    if (item.kind === 'user') break;
    if ((item.kind === 'text' || item.kind === 'thinking') && item.streaming) item.streaming = false;
    if (item.kind === 'tool_use' && item.result === null) {
      item.result = '(interrupted)';
      item.isError = true;
    }
  }
}

export function stringifyContent(content: unknown): string {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) {
    return content.map((c) => (c?.type === 'text' ? c.text : `[${c?.type ?? 'unknown'}]`)).join('\n');
  }
  return JSON.stringify(content, null, 2);
}
