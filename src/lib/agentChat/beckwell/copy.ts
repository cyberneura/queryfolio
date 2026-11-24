// Adapted from Beckwell (Cyberneura), MIT, via Astragal. See docs/agent-chat.md.
// Copy button for preformatted text (code blocks, tool input / output).
// The button is placed inside .code-block next to the <pre>. Markdown is emitted as an HTML string, so
// clicks are handled in one place on the conversation element (handleCopyClick)

export const COPY_BUTTON_CLASS = 'copy-code';
// How long the check mark stays visible after copying
export const COPIED_MS = 3000;

// HTML of the button. The icon is drawn with CSS (the content is empty so it does not mix into conversation search or selected text)
export const COPY_BUTTON_HTML = `<button type="button" class="${COPY_BUTTON_CLASS}" title="Copy" aria-label="Copy"></button>`;

// Timers that remove the check mark. Keeps an earlier timer from removing it too soon when pressed repeatedly
const resetTimers = new WeakMap<Element, ReturnType<typeof setTimeout>>();

async function writeClipboard(text: string): Promise<void> {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // If refused because of permissions or the like, try the older method below
    }
  }
  const textarea = document.createElement('textarea');
  textarea.value = text;
  textarea.setAttribute('readonly', '');
  textarea.style.position = 'fixed';
  textarea.style.opacity = '0';
  document.body.appendChild(textarea);
  try {
    textarea.select();
    if (!document.execCommand('copy')) throw new Error('execCommand("copy") failed');
  } finally {
    textarea.remove();
  }
}

// If it is a click on a copy button, copies the contents of the adjacent <pre> and returns true
export function handleCopyClick(event: MouseEvent): boolean {
  const button = (event.target as Element | null)?.closest?.(`button.${COPY_BUTTON_CLASS}`);
  if (!button) return false;
  event.preventDefault();
  const pre = button.closest('.code-block')?.querySelector(':scope > pre');
  if (!pre) return true;
  writeClipboard(pre.textContent ?? '').then(
    () => {
      clearTimeout(resetTimers.get(button));
      button.classList.add('copied');
      button.setAttribute('title', 'Copied');
      resetTimers.set(
        button,
        setTimeout(() => {
          button.classList.remove('copied');
          button.setAttribute('title', 'Copy');
          resetTimers.delete(button);
        }, COPIED_MS),
      );
    },
    (e) => console.error('Failed to copy:', e),
  );
  return true;
}
