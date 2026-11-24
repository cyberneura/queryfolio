// Whether the user is reading the end of the conversation (CYBERNEURA-DEV-1308, ported from Beckwell's
// src/lib/chat/scroll.ts, CYBERNEURA-DEV-1116).
//
// New items and streaming output only scroll to the end while the user is reading the end. This is not
// decided by position ("within N px of the bottom"): streaming re-renders many times a second, and a
// single wheel tick that stays inside the threshold was pulled back to the end on the next render, so
// the user could not scroll up while the agent was answering. Instead, follow stops as soon as the user
// moves up and resumes once they come back to the bottom.

// Resume following once the user is back within this many px of the bottom
export const FOLLOW_BOTTOM_PX = 32;

export interface ScrollMetrics {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}

function maxScrollTop({ scrollHeight, clientHeight }: ScrollMetrics): number {
  return scrollHeight - clientHeight;
}

function distanceToBottom(metrics: ScrollMetrics): number {
  return maxScrollTop(metrics) - metrics.scrollTop;
}

export function isAtBottom(metrics: ScrollMetrics, threshold = FOLLOW_BOTTOM_PX): boolean {
  return distanceToBottom(metrics) <= threshold;
}

// At the very end (including overscroll). On high-DPI screens scrollTop is fractional, so allow < 1px.
function isAtEnd(metrics: ScrollMetrics): boolean {
  return distanceToBottom(metrics) <= 1;
}

// Called on every scroll event with the previous position. A decrease in scrollTop means the user moved
// up (wheel, trackpad, scrollbar drag, PageUp). Two decreases are not the user: the browser clamping
// scrollTop when the content shrinks (a tool item collapsed) or the window grows, and the trackpad
// rubber band settling back at the end.
export function shouldFollowBottom(following: boolean, previous: ScrollMetrics, metrics: ScrollMetrics): boolean {
  const movedUp = metrics.scrollTop < previous.scrollTop;
  const settledAtEnd = isAtEnd(metrics) && (isAtEnd(previous) || maxScrollTop(metrics) < maxScrollTop(previous));
  if (movedUp && !settledAtEnd) return false;
  if (isAtBottom(metrics)) return true;
  return following;
}
