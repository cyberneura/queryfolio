// Messages sent while the agent is still answering. They are dispatched one at a
// time, oldest first, whenever the agent can accept a new turn, and can be edited
// or removed until then. Ported from Beckwell's src/lib/chat/queue.ts.

export type QueuedMessage = { id: number; text: string };

let nextId = 0;

export function createQueued(text: string): QueuedMessage {
  return { id: ++nextId, text };
}

// Separator used when a queued message is appended to the one above it.
export const MERGE_SEPARATOR = "\n\n";

// Appends the message at `index` to the previous one and removes it. The first
// message has nothing above it, so it is left unchanged.
export function mergeUp(queue: QueuedMessage[], index: number): boolean {
  if (index <= 0 || index >= queue.length) return false;
  const above = queue[index - 1];
  const text = queue[index].text;
  above.text = above.text && text ? `${above.text}${MERGE_SEPARATOR}${text}` : above.text || text;
  queue.splice(index, 1);
  return true;
}

// Removes and returns the next message to send. Messages that were emptied are
// dropped instead of being sent. Returns null when nothing is left to send.
export function takeNext(queue: QueuedMessage[]): string | null {
  while (queue.length > 0) {
    const text = queue.shift()!.text.trim();
    if (text) return text;
  }
  return null;
}

// `held` is set by Stop and New chat: the queue waits for Resume (or a message
// sent directly from the input) instead of starting as soon as the turn ends.
export type DispatchState = {
  queued: number;
  held: boolean;
  busy: boolean;
  ready: boolean;
  running: boolean;
  starting: boolean;
};

export function canDispatch(state: DispatchState): boolean {
  return state.queued > 0 && !state.held && !state.busy && state.ready && state.running && !state.starting;
}

// Textarea height in rows: most queued messages are one line, long ones scroll after 6.
export function rowsFor(text: string): number {
  return Math.min(6, Math.max(1, text.split("\n").length));
}
