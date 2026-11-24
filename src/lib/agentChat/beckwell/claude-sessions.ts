// Adapted from Beckwell (Cyberneura), MIT, via Astragal. See docs/agent-chat.md.
// Interprets the session records Claude Code left on disk (reads the string returned by the Rust claude_sessions)
import type { SessionSummary } from './types';

function parseLine(line: string): any {
  try {
    return JSON.parse(line);
  } catch {
    return null;
  }
}

// The list output. After each "@@session <ID> <update time (seconds)>" line come the lines that become that session's title
export function parseSessionList(output: string): SessionSummary[] {
  const sessions: (SessionSummary & { rank: number })[] = [];
  for (const line of output.split('\n')) {
    if (line.startsWith('@@session ')) {
      const [, id, seconds] = line.split(' ');
      const updatedAt = Number(seconds);
      if (id) sessions.push({ id, title: '', updatedAt: seconds && Number.isFinite(updatedAt) ? updatedAt : null, rank: 0 });
      continue;
    }
    const current = sessions[sessions.length - 1];
    const entry = current && line.startsWith('{') ? parseLine(line) : null;
    if (!entry) continue;
    // Priority order: the name the user gave, the automatically assigned title, then the last input
    const candidates: [number, unknown][] = [
      [3, entry.type === 'custom-title' ? entry.customTitle : null],
      [2, entry.type === 'ai-title' ? entry.aiTitle : null],
      [1, entry.type === 'last-prompt' ? entry.lastPrompt : null],
    ];
    for (const [rank, title] of candidates) {
      if (typeof title === 'string' && title.trim() && rank > current.rank) {
        current.title = title.trim();
        current.rank = rank;
      }
    }
  }
  return sessions.map(({ id, title, updatedAt }) => ({ id, title, updatedAt }));
}

// Messages Claude Code inserted into the conversation, not typed by the user
const INJECTED = /^<(local-command-(stdout|stderr|caveat)|bash-(stdout|stderr)|system-reminder|task-notification)>/;

function tagContent(text: string, tag: string): string | null {
  const match = text.match(new RegExp(`<${tag}>([\\s\\S]*?)</${tag}>`));
  return match ? match[1].trim() : null;
}

// Extracts the text to show on screen from a user message in the record. null if there is nothing to show
export function userText(content: unknown): string | null {
  const text = (
    typeof content === 'string'
      ? content
      : Array.isArray(content)
        ? content.filter((block) => block?.type === 'text').map((block) => block.text).join('\n')
        : ''
  ).trim();
  if (!text || INJECTED.test(text)) return null;
  const command = tagContent(text, 'command-name');
  if (command !== null) return `${command} ${tagContent(text, 'command-args') ?? ''}`.trim();
  const bash = tagContent(text, 'bash-input');
  if (bash !== null) return `! ${bash}`;
  return text;
}

export type HistoryEntry = { type: 'user' | 'assistant' | 'compacted'; message: any };

// Extracts the lines to show in the conversation from the output for one session
export function parseHistory(output: string): HistoryEntry[] {
  const entries: HistoryEntry[] = [];
  for (const line of output.split('\n')) {
    const entry = line.startsWith('{') ? parseLine(line) : null;
    // Do not show subagent conversations or messages Claude Code inserted for its own use
    if (!entry || entry.isSidechain || entry.isMeta || !entry.message) continue;
    if (entry.type === 'assistant') entries.push({ type: 'assistant', message: entry.message });
    else if (entry.type === 'user') entries.push({ type: entry.isCompactSummary ? 'compacted' : 'user', message: entry.message });
  }
  return entries;
}
