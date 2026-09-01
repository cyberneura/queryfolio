<script lang="ts">
  import "../app.css";
  import { onMount } from "svelte";
  import { Toaster } from "svelte-sonner";
  import { isReloadShortcut } from "$lib/reloadGuard";

  let { children } = $props();

  /// リロードのショートカットを無効化する (CYBERNEURA-DEV-648)。
  ///
  /// capture フェーズで window に付ける。バブルフェーズの
  /// `<svelte:window onkeydown>` (+page.svelte のアプリ内ショートカット) と違い、
  /// 途中のコンポーネントが stopPropagation しても必ず先に通るため、
  /// モーダルや CodeMirror にフォーカスがある時も取りこぼさない。
  onMount(() => {
    const suppressReload = (e: KeyboardEvent) => {
      if (isReloadShortcut(e)) {
        e.preventDefault();
      }
    };
    window.addEventListener("keydown", suppressReload, { capture: true });
    return () => {
      window.removeEventListener("keydown", suppressReload, { capture: true });
    };
  });
</script>

{@render children()}

<Toaster
  richColors
  theme="dark"
  duration={10000}
  toastOptions={{
    classes: {
      title: "text-base font-bold!",
      description: "text-base",
    },
  }}
/>
