// Adapted from Beckwell (Cyberneura), MIT, via Astragal. See docs/agent-chat.md.
// Interprets Claude Code's stream-json (`claude -p --input-format stream-json --output-format stream-json`)
import { parseHistory, parseSessionList, userText } from './claude-sessions';
import { parseQuestions } from './questions';
import { finishInterruptedTurn, stringifyContent, type ChatHost, type ChatProtocol, type PermissionDecision, type PermissionItem, type ProtocolOptions, type QuestionItem } from './types';

// ID for the control_request messages we send
const INITIALIZE_REQUEST_ID = 'initialize';
const SET_MODEL_REQUEST_PREFIX = 'set-model-';
const INTERRUPT_REQUEST_PREFIX = 'interrupt-';

const LOCAL_COMMAND_OUTPUT = /^<local-command-stdout>([\s\S]*)<\/local-command-stdout>$/;

export function createClaudeProtocol(host: ChatHost, options: ProtocolOptions): ChatProtocol {
  let nextSetModelId = 1;
  let nextInterruptId = 1;
  // While waiting for the end of the turn (result) after requesting an interrupt. The result that arrives is not shown as an error
  let interrupting = false;
  // While the earlier output is being replayed right after attaching to a persistent session (until beckwell_live arrives).
  // User messages in the replay are not on screen yet, so they are added (ones sent while attached were already added by ChatTab)
  let replaying = Boolean(options.persistent);
  let endReplay: () => void = () => {};
  const replayed = options.persistent ? new Promise<void>((resolve) => (endReplay = resolve)) : Promise.resolve();
  // control_responses returned on a previous connection (request_id -> response). Used to tell which replayed permission prompts were already answered
  const answered = new Map<string, any>();
  // User messages sent on this connection whose echo has not come back yet (in send order). ChatTab already added them, so the echo is not added.
  // They are matched against echoes because the echo of a message sent just before a disconnect can arrive late, after the replay (that one is added)
  const awaitingEcho: string[] = [];
  // Number of turns whose reply (result) has not arrived yet (persistent sessions only). When the result of a message sent just before a disconnect arrives late,
  // this keeps busy from being cleared while a later message is still being answered
  let outstanding = 0;
  // total_cost_usd in result is the cumulative total since this process started. The per-turn amount is the difference
  let costSoFar = 0;
  // User messages handed to claude up to the previous connection (beckwell_sent) whose echo has not arrived yet.
  // Turns are counted by handed-over lines, not by echoes (slash commands return no echo, so a disconnect during their response would leave busy unrestored)
  const sentBefore: string[] = [];
  // ID of the last interrupt request sent. A failure of an earlier request that arrives after pressing again is ignored
  let interruptRequestId: string | null = null;

  // The items being streamed in the current turn. Deltas are appended to the last block, and
  // the final assistant message settles them in order starting from the first unsettled block
  function streamingItem(kind: 'text' | 'thinking', which: 'first' | 'last') {
    let found = null;
    for (let i = host.items.length - 1; i >= 0; i--) {
      const item = host.items[i];
      if (item.kind === 'user') break;
      if (item.kind === kind && item.streaming) {
        found = item;
        if (which === 'last') break;
      }
    }
    return found;
  }

  function handleStreamEvent(event: any) {
    if (event.type === 'content_block_start') {
      const type = event.content_block?.type;
      if (type === 'text' || type === 'thinking') {
        host.push({ kind: type, text: '', streaming: true });
      }
    } else if (event.type === 'content_block_delta') {
      const delta = event.delta;
      if (delta?.type === 'text_delta') {
        const target = streamingItem('text', 'last');
        if (target) target.text += delta.text;
        else host.push({ kind: 'text', text: delta.text, streaming: true });
        host.scrollToBottom();
      } else if (delta?.type === 'thinking_delta') {
        const target = streamingItem('thinking', 'last');
        if (target) target.text += delta.thinking;
      }
    }
  }

  function handleAssistant(message: any) {
    for (const block of message.content ?? []) {
      if (block.type === 'text' || block.type === 'thinking') {
        const text = block.type === 'text' ? block.text : block.thinking;
        const target = streamingItem(block.type, 'first');
        if (target) {
          target.text = text;
          target.streaming = false;
        } else if (text) {
          host.push({ kind: block.type, text, streaming: false });
        }
      } else if (block.type === 'tool_use') {
        host.push({
          kind: 'tool_use',
          toolUseId: block.id,
          name: block.name,
          input: block.input ?? {},
          result: null,
          isError: false,
        });
      }
    }
  }

  function handleUser(message: any) {
    const content = message.content;
    if (!Array.isArray(content)) return;
    for (const block of content) {
      if (block.type !== 'tool_result') continue;
      const target = host.items.find((i) => i.kind === 'tool_use' && i.toolUseId === block.tool_use_id);
      if (target && target.kind === 'tool_use') {
        target.result = stringifyContent(block.content);
        target.isError = Boolean(block.is_error);
      }
    }
  }

  function handleControlRequest(event: any) {
    const request = event.request;
    const prior = answered.get(String(event.request_id));
    const allowed = prior?.subtype === 'success' && prior.response?.behavior === 'allow';
    if (request?.subtype === 'can_use_tool') {
      const input = request.input ?? {};
      // Questions to the user also arrive as permission prompts. The answer is returned in answers of updatedInput
      const questions = request.tool_name === 'AskUserQuestion' ? parseQuestions(input) : null;
      if (questions) {
        host.push({
          kind: 'question',
          requestId: event.request_id,
          input,
          title: typeof input.title === 'string' ? input.title : '',
          questions,
          decision: prior ? (allowed ? 'answered' : 'declined') : null,
          answers: (allowed && prior.response.updatedInput?.answers) || {},
        });
        return;
      }
      host.push({
        kind: 'permission',
        requestId: event.request_id,
        toolName: request.tool_name,
        description: request.description ?? '',
        input,
        decision: prior ? (allowed ? 'allow' : 'deny') : null,
      });
      return;
    }
    // An error was already returned on a previous connection
    if (prior) return;
    // Claude Code keeps waiting if an unsupported request gets no response, so reply with an error
    host.write({
      type: 'control_response',
      response: { subtype: 'error', request_id: event.request_id, error: `Unsupported request: ${request?.subtype}` },
    });
    host.note(`Unsupported control_request: ${request?.subtype}`, 'error');
  }

  function handleControlResponse(response: any) {
    const requestId = String(response?.request_id ?? '');
    if (requestId.startsWith(INTERRUPT_REQUEST_PREFIX)) {
      if (response.subtype === 'error' && requestId === interruptRequestId) {
        interrupting = false;
        interruptRequestId = null;
        host.interruptFailed(`Could not stop the response: ${stringifyContent(response.error)}`);
      }
      return;
    }
    if (requestId.startsWith(SET_MODEL_REQUEST_PREFIX)) {
      if (response.subtype === 'error') host.note(`Could not change the model: ${stringifyContent(response.error)}`, 'error');
      return;
    }
    // With an old Claude Code that does not answer initialize, only completion and the model list are missing
    if (requestId !== INITIALIZE_REQUEST_ID || response.subtype !== 'success') return;
    const result = response.response ?? {};
    host.setCommands(
      (result.commands ?? [])
        .filter((command: any) => typeof command?.name === 'string' && command.name)
        .map((command: any) => ({
          name: command.name,
          description: String(command.description ?? ''),
          argumentHint: String(command.argumentHint ?? ''),
        })),
    );
    host.setModels(
      (result.models ?? [])
        // default corresponds to "default model" (unspecified) in the model selector
        .filter((model: any) => typeof model?.value === 'string' && model.value && model.value !== 'default')
        .map((model: any) => ({ value: model.value, label: String(model.displayName ?? model.value) })),
    );
  }

  // Lays out the earlier conversation of a resumed session on screen (stream-json does not send past conversation)
  async function showHistory(sessionId: string) {
    try {
      const entries = parseHistory(await options.readClaudeSessions(sessionId));
      for (const entry of entries) {
        if (entry.type === 'assistant') {
          handleAssistant(entry.message);
        } else if (entry.type === 'compacted') {
          host.note('(context compacted)');
        } else {
          const text = userText(entry.message.content);
          if (text) host.push({ kind: 'user', text });
          handleUser(entry.message);
        }
      }
      // Do not show tools with no recorded result (interrupted ones, or ones too long to read) as running
      for (const item of host.items) {
        if (item.kind === 'tool_use' && item.result === null) item.result = '(no result in the session record)';
      }
      host.note(`resumed session ${sessionId}`);
    } catch (e) {
      host.note(`Could not read the session record: ${e}`, 'error');
    }
  }

  return {
    // stream-json accepts messages right after startup (the init event arrives after the first message)
    async start() {
      // Ask for the available slash commands and models. Become ready only after the write finishes so it arrives before the first message
      if (!(await host.write({ type: 'control_request', request_id: INITIALIZE_REQUEST_ID, request: { subtype: 'initialize' } }))) return;
      // Do not allow sending until the past conversation has been laid out (sending midway would put the past conversation after the new message)
      if (options.resume) await showHistory(options.resume);
      // A persistent session allows sending only after the earlier output has been laid out (whether it is busy is also decided at the end of the replay)
      await replayed;
      host.setReady(true);
    },

    async listSessions() {
      return parseSessionList(await options.readClaudeSessions(null));
    },

    handleMessage(event: any) {
      switch (event.type) {
        case 'system':
          if (event.subtype === 'init') {
            host.setSession({
              sessionId: event.session_id,
              model: event.model ?? '',
              detail: `${event.tools?.length ?? 0} tools`,
            });
            if (event.cwd) host.setWorkingDirectory(event.cwd);
          }
          break;
        case 'stream_event':
          handleStreamEvent(event.event);
          break;
        case 'assistant':
          handleAssistant(event.message);
          break;
        case 'user': {
          // Results of set_model and the like arrive in the form of a user message
          const output = typeof event.message?.content === 'string' ? event.message.content.match(LOCAL_COMMAND_OUTPUT) : null;
          if (output) {
            if (output[1].trim()) host.note(output[1].trim());
          } else if (options.persistent && typeof event.message?.content === 'string') {
            // A user message returned by --replay-user-messages. If its reply (result) has not come yet, we attach while it is still responding.
            // Some messages return no echo (slash commands, etc.), so sends before the matched one stop waiting for an echo
            const content = event.message.content;
            const sent = replaying ? -1 : awaitingEcho.indexOf(content);
            if (sent >= 0) {
              awaitingEcho.splice(0, sent + 1);
            } else {
              const text = userText(content);
              const earlier = sentBefore.indexOf(content);
              if (earlier >= 0) sentBefore.splice(earlier, 1);
              if (text) {
                host.push({ kind: 'user', text });
                if (earlier < 0) outstanding++;
                host.setBusy(outstanding > 0);
              }
            }
          } else {
            handleUser(event.message);
          }
          break;
        }
        // Lines added by the persistent session relay (persistent_session.rs): control_responses and user messages sent on a previous connection, and the end of the replay
        case 'beckwell_sent': {
          const response = event.payload?.response;
          if (response?.request_id !== undefined) answered.set(String(response.request_id), response);
          const content = event.payload?.type === 'user' ? event.payload.message?.content : undefined;
          if (typeof content === 'string') {
            sentBefore.push(content);
            outstanding++;
          }
          break;
        }
        case 'beckwell_live':
          replaying = false;
          host.setBusy(outstanding > 0);
          endReplay();
          break;
        case 'control_response':
          handleControlResponse(event.response);
          break;
        // The conversation was restarted by /clear. The new session ID arrives in the init event that follows
        case 'conversation_reset':
          host.clear();
          host.note('(conversation cleared)');
          break;
        case 'control_request':
          handleControlRequest(event);
          break;
        case 'rate_limit_event': {
          const windows = event.rate_limit_info?.unifiedWindows;
          if (windows) {
            const percent = (value: number | undefined) => Math.round((value ?? 0) * 100);
            host.setUsage(`5h ${percent(windows.five_hour?.utilization)}% · 7d ${percent(windows.seven_day?.utilization)}%`);
          }
          break;
        }
        case 'result': {
          outstanding = Math.max(0, outstanding - 1);
          host.setBusy(options.persistent ? outstanding > 0 : false);
          // If an interrupt was requested but the turn had already ended before it (success), do not treat it as interrupted
          const interrupted = interrupting && event.subtype !== 'success';
          interrupting = false;
          interruptRequestId = null;
          if (interrupted) finishInterruptedTurn(host);
          const seconds = ((event.duration_ms ?? event.duration_api_ms ?? 0) / 1000).toFixed(1);
          const total = event.total_cost_usd ?? costSoFar;
          const cost = Math.max(0, total - costSoFar).toFixed(2);
          costSoFar = total;
          host.push({
            kind: 'result',
            summary: `${event.subtype} · ${event.num_turns ?? 0} turns · ${seconds}s · $${cost}`,
            at: replaying ? null : Date.now(),
            finalText: event.subtype === 'success' && typeof event.result === 'string' ? event.result : undefined,
          });
          // An interrupted turn arrives as error_during_execution (result is empty). Show that it was interrupted, not an error
          // If it ended with another error at the same time as the interrupt, show that error too
          if (interrupted) {
            host.note('(interrupted)');
            if (event.is_error && event.result) {
              host.note(stringifyContent(event.result), 'error');
            }
          } else if (event.is_error) {
            host.note(stringifyContent(event.result ?? event.errors ?? 'error'), 'error');
          }
          break;
        }
      }
    },

    async send(text: string) {
      interrupting = false;
      interruptRequestId = null;
      if (options.persistent) {
        awaitingEcho.push(text);
        outstanding++;
      }
      const written = await host.write({ type: 'user', message: { role: 'user', content: text } });
      // A message that was not delivered gets no result, so unless we reset here nothing can be sent afterwards
      if (written) host.turnStarted?.();
      if (!written) {
        host.setBusy(false);
        const index = awaitingEcho.lastIndexOf(text);
        if (index >= 0) {
          awaitingEcho.splice(index, 1);
          outstanding = Math.max(0, outstanding - 1);
        }
      }
    },

    // Claude Code stops the running tool's command (confirmed that no Bash child process is left either)
    async interrupt() {
      interrupting = true;
      const requestId = `${INTERRUPT_REQUEST_PREFIX}${nextInterruptId++}`;
      interruptRequestId = requestId;
      const written = await host.write({ type: 'control_request', request_id: requestId, request: { subtype: 'interrupt' } });
      if (!written && interruptRequestId === requestId) {
        interrupting = false;
        interruptRequestId = null;
      }
      return written;
    },

    async setModel(model: string) {
      await host.write({
        type: 'control_request',
        request_id: `${SET_MODEL_REQUEST_PREFIX}${nextSetModelId++}`,
        request: { subtype: 'set_model', model: model || 'default' },
      });
    },

    async respondPermission(item: PermissionItem, decision: PermissionDecision) {
      const response =
        decision === 'allow'
          ? { behavior: 'allow', updatedInput: item.input }
          : { behavior: 'deny', message: 'The user denied this tool call.' };
      const written = await host.write({
        type: 'control_response',
        response: { subtype: 'success', request_id: item.requestId, response },
      });
      // Claude Code keeps waiting if the response does not arrive, so reset to let the user press again
      if (!written) item.decision = null;
    },

    async answerQuestion(item: QuestionItem, answers: Record<string, string> | null) {
      const response = answers
        ? { behavior: 'allow', updatedInput: { ...item.input, answers } }
        : { behavior: 'deny', message: 'The user declined to answer.' };
      const written = await host.write({
        type: 'control_response',
        response: { subtype: 'success', request_id: item.requestId, response },
      });
      if (!written) item.decision = null;
    },
  };
}
