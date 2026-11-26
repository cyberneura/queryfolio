// Adapted from Beckwell (Cyberneura), MIT, via Astragal. See docs/agent-chat.md.
import { createClaudeProtocol } from './claude';
import { createCodexProtocol } from './codex';
import type { ChatHost, ChatProtocol, ModelChoice, ProtocolOptions } from './types';

// Agents available in the chat tab
export type ChatAgent = 'claude' | 'codex';

export function createProtocol(agent: ChatAgent, host: ChatHost, options: ProtocolOptions): ChatProtocol {
  return agent === 'codex' ? createCodexProtocol(host, options) : createClaudeProtocol(host, options);
}

// Models selectable at startup. If empty, the model selector is not shown.
// For Claude, it is replaced by the list the agent reports after startup (ChatHost.setModels).
// Codex follows the default in the other side's ~/.codex/config.toml
export function modelChoices(agent: ChatAgent): ModelChoice[] {
  if (agent === 'codex') return [];
  return [
    { value: 'opus', label: 'opus' },
    { value: 'sonnet', label: 'sonnet' },
    { value: 'haiku', label: 'haiku' },
  ];
}

// Expresses a session's update time as "how long ago" (now is also in seconds)
export function formatAge(updatedAt: number, now: number): string {
  const seconds = Math.max(0, now - updatedAt);
  if (seconds < 3600) return `${Math.max(1, Math.round(seconds / 60))}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

// Expresses the time of the last response as "how long ago" (milliseconds). Steps are coarse so the display does not change often
export function formatElapsed(at: number, now: number): string {
  const seconds = Math.max(0, (now - at) / 1000);
  if (seconds < 10) return 'just now';
  if (seconds < 30) return '10 seconds ago';
  if (seconds < 60) return '30 seconds ago';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 90) return minutes === 1 ? '1 minute ago' : `${minutes} minutes ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 36) return `${hours} hours ago`;
  const days = Math.round(hours / 24);
  return days === 1 ? '1 day ago' : `${days} days ago`;
}
