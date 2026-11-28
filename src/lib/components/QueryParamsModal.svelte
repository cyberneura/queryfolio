<script lang="ts">
  import { invalidParamInput, type ParamInput } from "$lib/queryParams";

  // Asks for the values of the `:name` parameters a statement uses that the query file does not
  // define with `-- @set` (CYBERNEURA-DEV-1400). Values are string literals unless "raw" is checked.

  interface Props {
    /// The names to ask for, in order of first appearance
    names: string[];
    /// Initial values (the values used last time for this query file). A Map, not an object:
    /// a parameter may be named `__proto__`
    initial: ReadonlyMap<string, ParamInput>;
    /// Names already defined in the file with `-- @set` (shown for reference, not editable)
    defined: { name: string; value: string }[];
    /// `remember`: keep the values for the next run of this query file (localStorage, plain text)
    /// The values in the order of `names`
    onSubmit: (values: [string, ParamInput][], remember: boolean) => void;
    onCancel: () => void;
  }

  let { names, initial, defined, onSubmit, onCancel }: Props = $props();

  // Copied once: the dialog keeps its own working values
  // svelte-ignore state_referenced_locally
  // Kept by position (not keyed by name, for the same reason as `initial`)
  let values = $state<ParamInput[]>(
    names.map((n) => ({ value: initial.get(n)?.value ?? "", raw: initial.get(n)?.raw ?? false })),
  );

  let remember = $state(true);

  const errors = $derived(
    values.map((v) => invalidParamInput(v)),
  );
  const hasError = $derived(errors.some((e) => e !== null));

  function submit() {
    if (hasError) return;
    const snapshot = $state.snapshot(values);
    onSubmit(
      names.map((n, i) => [n, snapshot[i]]),
      remember,
    );
  }
</script>

<div
  class="fixed inset-0 z-30 flex items-center justify-center bg-black/60"
  role="presentation"
  data-modal
  data-annotate="backdrop-query-params-modal"
  onclick={(e) => {
    if (e.target === e.currentTarget) onCancel();
  }}
  onkeydown={(e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onCancel();
    }
  }}
>
  <form
    class="flex max-h-[80vh] w-[520px] flex-col gap-3 rounded-lg border border-zinc-700 bg-zinc-900 p-4 shadow-xl"
    aria-labelledby="query-params-title"
    onsubmit={(e) => {
      e.preventDefault();
      submit();
    }}
  >
    <h2 id="query-params-title" class="text-sm font-semibold text-zinc-200">
      Query parameters
    </h2>
    <p class="text-xs leading-relaxed text-zinc-400">
      Values are inserted as string literals. Check <span class="font-mono">raw</span> to insert
      the text as written (a number, <span class="font-mono">NULL</span>, an expression).
    </p>
    <div class="min-h-0 overflow-y-auto">
      <table class="w-full text-xs">
        <tbody>
          {#each names as name, i (name)}
            <tr class="align-top">
              <th class="w-32 py-1 pr-2 text-left font-mono font-normal text-zinc-300">
                :{name}
              </th>
              <td class="py-1">
                <!-- svelte-ignore a11y_autofocus -->
                <input
                  type="text"
                  class="w-full rounded border bg-zinc-950 px-2 py-1 font-mono text-xs text-zinc-200 outline-none focus:border-blue-500 {errors[i]
                    ? 'border-red-600'
                    : 'border-zinc-600'}"
                  data-annotate="input-query-param"
                  aria-label="Value of :{name}"
                  autofocus={i === 0}
                  bind:value={values[i].value}
                />
                {#if errors[i]}
                  <p class="mt-0.5 text-[10px] text-red-400">{errors[i]}</p>
                {/if}
              </td>
              <td class="py-1 pl-2">
                <label class="flex items-center gap-1 whitespace-nowrap pt-1 text-[11px] text-zinc-400">
                  <input
                    type="checkbox"
                    class="accent-blue-500"
                    data-annotate="toggle-query-param-raw"
                    bind:checked={values[i].raw}
                  />
                  raw
                </label>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if defined.length > 0}
        <p class="mt-3 text-[11px] text-zinc-500">Defined in the file with -- @set:</p>
        <ul class="mt-1 font-mono text-[11px] text-zinc-500">
          {#each defined as d (d.name)}
            <li class="truncate">:{d.name} = {d.value}</li>
          {/each}
        </ul>
      {/if}
    </div>
    <div class="flex items-center justify-end gap-2">
      <label
        class="mr-auto flex items-center gap-1 text-[11px] text-zinc-400"
        title="Stored in plain text on this machine. Turn off for secrets"
      >
        <input
          type="checkbox"
          class="accent-blue-500"
          data-annotate="toggle-query-params-remember"
          bind:checked={remember}
        />
        Remember values for this file
      </label>
      <button
        type="button"
        class="rounded border border-zinc-600 px-3 py-1 text-xs text-zinc-300 hover:bg-zinc-800"
        data-annotate="button-query-params-cancel"
        onclick={onCancel}
      >
        Cancel
      </button>
      <button
        type="submit"
        class="rounded bg-blue-600 px-3 py-1 text-xs text-white hover:bg-blue-500 disabled:opacity-50"
        data-annotate="button-query-params-run"
        disabled={hasError}
      >
        Run
      </button>
    </div>
  </form>
</div>
