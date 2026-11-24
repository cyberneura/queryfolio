// Adapted from Beckwell (Cyberneura), MIT, via Astragal. See docs/agent-chat.md.
// Interprets Codex's app-server (`codex app-server`). JSON-RPC over stdio (newline-delimited, no `jsonrpc` field)
import { finishInterruptedTurn, stringifyContent, type ChatHost, type ChatProtocol, type PermissionDecision, type PermissionItem, type ProtocolOptions, type SessionSummary } from './types';

type ResponseHandler = { onResult: (result: any) => void; onError?: (message: string) => void };

// For each approval request, remember the value to return when declining (it depends on the request type and the values selectable at that time)
type PendingApproval = { declineDecision: string };

const JSONRPC_METHOD_NOT_FOUND = -32601;
// Number of threads shown in the list
const SESSION_LIST_LIMIT = 30;
// Number of past turns laid out on screen when resuming (from the newest side)
const HISTORY_TURNS = 100;

export function createCodexProtocol(host: ChatHost, options: ProtocolOptions): ChatProtocol {
  let nextRequestId = 1;
  const responseHandlers = new Map<number, ResponseHandler>();
  const pendingApprovals = new Map<string | number, PendingApproval>();
  let threadId: string | null = null;
  // ID of the running turn (needed for turn/interrupt). null once the turn ends
  let turnId: string | null = null;
  // When an interrupt is requested before the turn ID arrives, interrupt as soon as it arrives
  let interruptPending = false;
  // While waiting for the end of the turn after requesting an interrupt
  let interrupting = false;
  // Sequence number of the turn/interrupt sent (only the failure of the last one is reported)
  let interruptSerial = 0;
  // IDs of command items that have started and not finished yet. turn/interrupt only ends the turn and
  // does not stop the running command (confirmed with codex 0.156). If any remain, restart the whole process to end them
  const runningCommands = new Set<string>();
  // The thread's working directory (used to narrow past threads to those in the same directory)
  let threadCwd: string | null = null;
  // Cumulative token count of the whole thread, and its value when the current turn started (the difference is what that turn used)
  let totalTokens = 0;
  let turnStartTokens = 0;
  // Write-back uses only the last completed answer and does not carry over intermediate proposals.
  let finalText: string | undefined;
  // Latest rate limit values (per window)
  const rateLimits: { primary: any; secondary: any } = { primary: null, secondary: null };

  async function request(method: string, params: unknown, handler: ResponseHandler) {
    const id = nextRequestId++;
    responseHandlers.set(id, handler);
    const written = await host.write({ id, method, params });
    if (!written) {
      responseHandlers.delete(id);
      handler.onError?.('The request could not be sent');
    }
  }

  function findItem<K extends 'text' | 'thinking' | 'tool_use'>(kind: K, id: string) {
    for (let i = host.items.length - 1; i >= 0; i--) {
      const item = host.items[i];
      if (item.kind === kind && (item.kind === 'tool_use' ? item.toolUseId === id : (item as any).id === id)) {
        return item as Extract<(typeof host.items)[number], { kind: K }>;
      }
    }
    return null;
  }

  function appendText(kind: 'text' | 'thinking', id: string, delta: string) {
    finalText = undefined;
    const target = findItem(kind, id);
    if (target) target.text += delta;
    else host.push({ kind, id, text: delta, streaming: true });
    if (kind === 'text') host.scrollToBottom();
  }

  // Display name and input of items that correspond to tool execution
  function toolOf(item: any): { name: string; input: Record<string, unknown> } | null {
    switch (item.type) {
      case 'commandExecution':
        return { name: 'Shell', input: { command: item.command, cwd: item.cwd } };
      case 'fileChange':
        return { name: 'Edit', input: { changes: item.changes } };
      case 'mcpToolCall':
        return { name: `${item.server}.${item.tool}`, input: item.arguments ?? {} };
      case 'webSearch':
        return { name: 'WebSearch', input: { query: item.query } };
      default:
        return null;
    }
  }

  function toolResultOf(item: any, streamed: string | null): { result: string; isError: boolean } {
    const failed = item.status === 'failed' || item.status === 'declined';
    switch (item.type) {
      case 'commandExecution': {
        const output = item.aggregatedOutput ?? streamed ?? '';
        const exit = item.exitCode === null || item.exitCode === undefined ? item.status : `exit ${item.exitCode}`;
        return { result: `${output}${output && !output.endsWith('\n') ? '\n' : ''}(${exit})`, isError: failed || (item.exitCode ?? 0) !== 0 };
      }
      case 'mcpToolCall':
        return {
          result: item.error ? stringifyContent(item.error.message ?? item.error) : stringifyContent(item.result?.content ?? item.result ?? ''),
          isError: failed || Boolean(item.error),
        };
      default:
        return { result: String(item.status ?? 'completed'), isError: failed };
    }
  }

  function reasoningText(item: any): string {
    const parts = [...(item.summary ?? []), ...(item.content ?? [])];
    return parts.map((part: any) => (typeof part === 'string' ? part : (part?.text ?? ''))).join('\n\n');
  }

  function handleItemStarted(item: any) {
    finalText = undefined;
    if (item.type === 'agentMessage') {
      if (!findItem('text', item.id)) host.push({ kind: 'text', id: item.id, text: item.text ?? '', streaming: true });
    } else if (item.type === 'reasoning') {
      if (!findItem('thinking', item.id)) host.push({ kind: 'thinking', id: item.id, text: reasoningText(item), streaming: true });
    } else {
      if (item.type === 'commandExecution') runningCommands.add(item.id);
      const tool = toolOf(item);
      if (tool && !findItem('tool_use', item.id)) {
        host.push({ kind: 'tool_use', toolUseId: item.id, name: tool.name, input: tool.input, result: null, isError: false });
      }
    }
  }

  function handleItemCompleted(item: any) {
    finalText = item.type === 'agentMessage' && (item.phase == null || item.phase === 'final_answer')
      && typeof item.text === 'string' ? item.text : undefined;
    if (item.type === 'agentMessage' || item.type === 'reasoning') {
      const kind = item.type === 'agentMessage' ? 'text' : 'thinking';
      const text = item.type === 'agentMessage' ? (item.text ?? '') : reasoningText(item);
      const target = findItem(kind, item.id);
      if (target) {
        // For reasoning, models without a summary may deliver only deltas. If the body at completion is empty, keep the deltas
        if (text) target.text = text;
        target.streaming = false;
      } else if (text) {
        host.push({ kind, id: item.id, text, streaming: false });
      }
      return;
    }

    runningCommands.delete(item.id);
    const tool = toolOf(item);
    if (!tool) return;
    let target = findItem('tool_use', item.id);
    if (!target) {
      host.push({ kind: 'tool_use', toolUseId: item.id, name: tool.name, input: tool.input, result: null, isError: false });
      target = findItem('tool_use', item.id);
    }
    if (target) {
      const { result, isError } = toolResultOf(item, target.result);
      target.input = tool.input;
      target.result = result;
      target.isError = isError;
    }
  }

  // Lays out the earlier conversation of a resumed thread on screen
  function showHistory(turns: any[]) {
    if (turns.length > HISTORY_TURNS) host.note(`(${turns.length - HISTORY_TURNS} earlier turns are not shown)`);
    for (const turn of turns.slice(-HISTORY_TURNS)) {
      for (const item of turn.items ?? []) {
        if (item.type === 'userMessage') {
          const text = (item.content ?? [])
            .filter((input: any) => input?.type === 'text')
            .map((input: any) => input.text)
            .join('\n');
          if (text) host.push({ kind: 'user', text });
        } else {
          handleItemCompleted(item);
        }
      }
    }
  }

  function handleNotification(method: string, params: any) {
    // Even if notifications for another thread (a subagent, etc.) arrive on the same connection, do not show them on our thread's screen
    if (threadId && typeof params.threadId === 'string' && params.threadId !== threadId) return;
    switch (method) {
      case 'item/started':
        handleItemStarted(params.item);
        break;
      case 'item/completed':
        handleItemCompleted(params.item);
        break;
      case 'item/agentMessage/delta':
        appendText('text', params.itemId, params.delta ?? '');
        break;
      case 'item/reasoning/summaryTextDelta':
      case 'item/reasoning/textDelta':
        appendText('thinking', params.itemId, params.delta ?? '');
        break;
      case 'item/commandExecution/outputDelta': {
        const target = findItem('tool_use', params.itemId);
        if (target) target.result = (target.result ?? '') + (params.delta ?? '');
        break;
      }
      case 'thread/tokenUsage/updated':
        totalTokens = params.tokenUsage?.total?.totalTokens ?? totalTokens;
        break;
      case 'account/rateLimits/updated': {
        // Sometimes only the windows that changed arrive, so overwrite just those onto the previous values and display
        for (const key of ['primary', 'secondary'] as const) {
          const limit = params.rateLimits?.[key];
          if (limit && typeof limit === 'object') rateLimits[key] = limit;
        }
        const window = (limit: any) => {
          if (!limit) return null;
          const minutes = limit.windowDurationMins ?? 0;
          const label = minutes >= 1440 ? `${Math.round(minutes / 1440)}d` : `${Math.round(minutes / 60)}h`;
          return `${label} ${Math.round(limit.usedPercent ?? 0)}%`;
        };
        const parts = [window(rateLimits.primary), window(rateLimits.secondary)].filter(Boolean);
        host.setUsage(parts.length ? parts.join(' · ') : null);
        break;
      }
      case 'turn/started':
        startTurn(params.turn?.id);
        break;
      case 'turn/completed': {
        const turn = params.turn ?? {};
        // If an interrupt was requested but the turn had already ended before it (completed), do not treat it as interrupted
        const interrupted = turn.status === 'interrupted' || (interrupting && turn.status !== 'completed');
        const seconds = ((turn.durationMs ?? 0) / 1000).toFixed(1);
        host.push({ kind: 'result', summary: `${turn.status ?? 'completed'} · ${seconds}s · ${totalTokens - turnStartTokens} tokens`, at: Date.now(), finalText: turn.status === 'completed' ? finalText : undefined });
        if (turn.error) host.note(stringifyContent(turn.error.message ?? turn.error), 'error');
        endTurn(interrupted);
        break;
      }
      case 'error':
        host.note(stringifyContent(params.error?.message ?? params.error ?? 'error'), 'error');
        // A non-retried error aborts the turn. turn/completed may not arrive after this, so end the turn here
        // (the cleanup for a requested interrupt is also done, the same as for turn/completed)
        if (params.willRetry === false) endTurn(interrupting);
        break;
      case 'warning':
      case 'configWarning':
      case 'deprecationNotice':
        host.note(String(params.message ?? params.summary ?? method));
        break;
    }
  }

  // End of a turn (turn/completed, or a non-retried error). For an interrupted turn, close the in-progress items and,
  // if unfinished commands remain, restart the whole process to stop them
  function endTurn(interrupted: boolean) {
    host.setBusy(false);
    turnId = null;
    interruptPending = false;
    interrupting = false;
    const leftCommands = runningCommands.size > 0;
    // So that the next turn's interrupt does not count this turn's commands
    runningCommands.clear();
    if (!interrupted) return;
    finishInterruptedTurn(host);
    if (leftCommands) host.restart('(interrupted. Restarted Codex to stop the commands that were still running)');
    else host.note('(interrupted)');
  }

  function startTurn(id: unknown) {
    if (typeof id !== 'string' || !id || turnId === id) return;
    turnId = id;
    if (interruptPending) {
      interruptPending = false;
      sendInterrupt();
    }
  }

  function sendInterrupt() {
    if (!threadId || !turnId) return;
    interrupting = true;
    // A failure of an earlier request that arrives after pressing again is ignored
    const serial = ++interruptSerial;
    request(
      'turn/interrupt',
      { threadId, turnId },
      {
        onResult: () => {},
        onError: (message) => {
          if (serial !== interruptSerial || !interrupting) return;
          interrupting = false;
          host.interruptFailed(`Could not stop the response: ${message}`);
        },
      },
    );
  }

  function handleServerRequest(id: string | number, method: string, params: any) {
    if (method === 'item/commandExecution/requestApproval' || method === 'item/fileChange/requestApproval') {
      const isCommand = method === 'item/commandExecution/requestApproval';
      // A command approval comes with the values selectable at that time. If there is no decline, refuse with cancel (which aborts the turn)
      const available: unknown[] = params.availableDecisions ?? [];
      const declineDecision = !isCommand || available.length === 0 || available.includes('decline') ? 'decline' : 'cancel';
      pendingApprovals.set(id, { declineDecision });
      const tool = findItem('tool_use', params.itemId);
      host.push({
        kind: 'permission',
        requestId: id,
        toolName: isCommand ? 'Shell' : 'Edit',
        description: params.reason ?? params.command ?? '',
        input: isCommand ? { command: params.command, cwd: params.cwd } : (tool?.input ?? {}),
        decision: null,
      });
      return;
    }
    // Codex keeps waiting if an unsupported request gets no response, so reply with an error
    host.write({ id, error: { code: JSONRPC_METHOD_NOT_FOUND, message: `Unsupported request: ${method}` } });
    host.note(`Unsupported request: ${method}`, 'error');
  }

  return {
    start() {
      const fail = (message: string) => host.note(`Failed to start the Codex thread: ${message}`, 'error');
      request(
        'initialize',
        { clientInfo: { name: 'queryfolio', title: 'Queryfolio', version: '0' } },
        {
          onResult: async () => {
            // Concurrent invokes have no guaranteed arrival order, so send thread/start after initialized has been written
            if (!(await host.write({ method: 'initialized' }))) return;
            // Do not pass the approval policy, sandbox or model; follow the defaults of the other side's configuration
            request(
              options.resume ? 'thread/resume' : 'thread/start',
              options.resume ? { threadId: options.resume } : {},
              {
                onResult: (result) => {
                  threadId = result.thread?.id ?? null;
                  threadCwd = result.cwd ?? null;
                  if (options.resume) {
                    if (options.replayHistory !== false) showHistory(result.thread?.turns ?? []);
                    host.note(`resumed session ${threadId}`);
                  }
                  const sandbox = result.sandbox?.type ?? '';
                  host.setSession({
                    sessionId: threadId ?? '',
                    model: result.model ?? '',
                    detail: [result.reasoningEffort, result.approvalPolicy, sandbox].filter((v) => typeof v === 'string' && v).join(' · '),
                  });
                  if (result.cwd) host.setWorkingDirectory(result.cwd);
                  host.setReady(threadId !== null);
                },
                onError: fail,
              },
            );
          },
          onError: fail,
        },
      );
    },

    listSessions() {
      return new Promise<SessionSummary[]>((resolve, reject) => {
        request(
          'thread/list',
          {
            limit: SESSION_LIST_LIMIT,
            sortKey: 'updated_at',
            // Only threads used interactively (do not show those of codex exec or subagents)
            sourceKinds: ['cli', 'vscode', 'appServer'],
            // An empty array means "all providers". Omitting it can limit the result to threads created with the current default provider
            modelProviders: [],
            ...(threadCwd ? { cwd: threadCwd } : {}),
          },
          {
            onResult: (result) =>
              resolve(
                (result.data ?? [])
                  .filter((thread: any) => thread.id !== threadId)
                  .map((thread: any) => ({
                    id: thread.id,
                    title: String(thread.name || thread.preview || '').trim(),
                    updatedAt: typeof thread.updatedAt === 'number' ? thread.updatedAt : null,
                  })),
              ),
            onError: (message) => reject(new Error(message)),
          },
        );
      });
    },

    handleMessage(message: any) {
      const hasId = message.id !== undefined && message.id !== null;
      if (typeof message.method === 'string') {
        if (hasId) handleServerRequest(message.id, message.method, message.params ?? {});
        else handleNotification(message.method, message.params ?? {});
        return;
      }
      if (!hasId) return;
      const handler = responseHandlers.get(message.id);
      responseHandlers.delete(message.id);
      if (message.error) {
        const text = stringifyContent(message.error.message ?? message.error);
        if (handler?.onError) handler.onError(text);
        else host.note(text, 'error');
      } else {
        handler?.onResult(message.result);
      }
    },

    supportsWorkingDirectory: true,

    async send(text: string, options?: { cwd?: string }) {
      finalText = undefined;
      if (!threadId) {
        host.setBusy(false);
        host.note('The Codex thread is not ready', 'error');
        return;
      }
      turnStartTokens = totalTokens;
      turnId = null;
      interruptPending = false;
      interrupting = false;
      await request(
        'turn/start',
        { threadId, input: [{ type: 'text', text, text_elements: [] }], ...(options?.cwd ? { cwd: options.cwd } : {}) },
        {
          onResult: (result) => {
            host.turnStarted?.();
            if (options?.cwd) {
              threadCwd = options.cwd;
              host.setWorkingDirectory(options.cwd);
            }
            startTurn(result?.turn?.id);
          },
          // A turn that did not start gets no turn/completed, so reset here
          onError: (message) => {
            host.setBusy(false);
            host.note(message, 'error');
          },
        },
      );
    },

    async interrupt() {
      if (!threadId) return false;
      // If pressed before the turn/start response (the turn ID) arrives, send it once that arrives
      if (!turnId) interruptPending = true;
      else sendInterrupt();
      return true;
    },

    async respondPermission(item: PermissionItem, decision: PermissionDecision) {
      const pending = pendingApprovals.get(item.requestId);
      const written = await host.write({
        id: item.requestId,
        result: { decision: decision === 'allow' ? 'accept' : (pending?.declineDecision ?? 'decline') },
      });
      // Codex keeps waiting if the response does not arrive, so reset to let the user press again
      if (written) pendingApprovals.delete(item.requestId);
      else item.decision = null;
    },
  };
}
