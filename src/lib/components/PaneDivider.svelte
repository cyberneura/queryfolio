<script lang="ts">
  // A draggable divider line between panes.
  // The transparent hit area is meant to overlay the existing border (the visible line),
  // so it is pulled 2px into each neighbor with negative margins.
  // Dragging is tracked with Pointer Events + setPointerCapture, so it keeps following
  // even when the cursor leaves the divider (including over CodeMirror or a webview).
  interface Props {
    /// vertical = vertical line (drag left/right) / horizontal = horizontal line (drag up/down)
    direction: "vertical" | "horizontal";
    /// At drag start. The parent snapshots the base size here
    onDragStart?: () => void;
    /// Cumulative movement (px) from the drag start position. X for vertical, Y for horizontal.
    /// Using a cumulative value rather than a relative delta means the base size does not move even
    /// when clamping saturates, so the pointer and the pane edge do not drift after going past the limits and back.
    onDrag: (totalDelta: number) => void;
    /// At drag end (for persisting the size)
    onDragEnd?: () => void;
    /// data-annotate identifier
    annotate: string;
  }

  let { direction, onDragStart, onDrag, onDragEnd, annotate }: Props = $props();

  let dragging = $state(false);
  let startPos = 0;

  function position(e: PointerEvent): number {
    return direction === "vertical" ? e.clientX : e.clientY;
  }

  function handlePointerDown(e: PointerEvent) {
    if (e.button !== 0) return;
    dragging = true;
    startPos = position(e);
    onDragStart?.();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }

  function handlePointerMove(e: PointerEvent) {
    if (!dragging) return;
    onDrag(position(e) - startPos);
  }

  function handlePointerUp(e: PointerEvent) {
    if (!dragging) return;
    dragging = false;
    (e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId);
    onDragEnd?.();
  }
</script>

<div
  data-annotate={annotate}
  role="separator"
  aria-orientation={direction}
  class="relative z-10 shrink-0 select-none transition-colors {direction ===
  'vertical'
    ? '-mx-[3px] w-[6px] cursor-col-resize'
    : '-my-[3px] h-[6px] cursor-row-resize'} {dragging
    ? 'bg-blue-500/60'
    : 'bg-transparent hover:bg-blue-500/40'}"
  onpointerdown={handlePointerDown}
  onpointermove={handlePointerMove}
  onpointerup={handlePointerUp}
  onpointercancel={handlePointerUp}
></div>
