<script lang="ts">
  /// The small filter input shown right under a pane header (CONNECTIONS / FILES) after
  /// clicking its magnifier button. It is focused as soon as it appears.
  /// Escape closes it (the owner clears the query in onClose); Enter calls onSubmit
  /// (used to open the first match).
  interface Props {
    value: string;
    placeholder: string;
    /// data-annotate of the input
    annotate: string;
    onClose: () => void;
    onSubmit?: () => void;
  }

  let { value = $bindable(), placeholder, annotate, onClose, onSubmit }: Props =
    $props();

  let inputEl = $state<HTMLInputElement | null>(null);

  $effect(() => {
    inputEl?.focus();
  });
</script>

<div class="border-b border-zinc-700 px-2 py-1.5">
  <div class="relative">
    <i
      class="bi bi-search pointer-events-none absolute left-1.5 top-1/2 -translate-y-1/2 text-[10px] text-zinc-500"
      aria-hidden="true"
    ></i>
    <input
      bind:this={inputEl}
      bind:value
      class="w-full rounded border border-zinc-600 bg-zinc-800 py-0.5 pl-5 pr-1.5 text-xs text-zinc-200 outline-none placeholder:text-zinc-500 focus:border-blue-400"
      type="text"
      spellcheck="false"
      autocomplete="off"
      aria-label={placeholder}
      {placeholder}
      data-annotate={annotate}
      onkeydown={(e) => {
        // WebKit can deliver the Enter that commits an IME composition after compositionend,
        // with isComposing already false; keyCode 229 still marks it as an IME key
        if (e.isComposing || e.keyCode === 229) {
          return;
        }
        if (e.key === "Escape") {
          e.preventDefault();
          onClose();
        } else if (e.key === "Enter" && onSubmit) {
          e.preventDefault();
          onSubmit();
        }
      }}
    />
  </div>
</div>
