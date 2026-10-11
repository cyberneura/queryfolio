<script lang="ts">
  import "../app.css";
  import { onMount } from "svelte";
  import { Toaster } from "svelte-sonner";
  import { isReloadShortcut } from "$lib/reloadGuard";
  import { isExternalFileDrag } from "$lib/fileDrag";

  let { children } = $props();

  /// Disable the reload shortcuts (CYBERNEURA-DEV-648).
  ///
  /// Attach to window in the capture phase. Unlike the bubble-phase
  /// `<svelte:window onkeydown>` (+page.svelte's in-app shortcuts), this always runs first
  /// even if an intermediate component calls stopPropagation, so it is not missed
  /// when a modal or CodeMirror has focus.
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

  /// Disable dropping of files brought in from the OS (CYBERNEURA-DEV-977).
  ///
  /// To make FILES -> CONNECTIONS drag & drop work, tauri.conf.json sets
  /// `dragDropEnabled: false` (if left true, Tauri's native handler steals the drop and
  /// HTML drop events do not fire on macOS).
  /// As a result, dropping an external file falls through to the WebView's default behavior
  /// (navigating to that file), so we stop it here. Stopping it in the capture phase also keeps
  /// it from reaching the path where CodeMirror inserts the file contents (as before, "nothing happens").
  onMount(() => {
    const blockExternalFileDrop = (e: DragEvent) => {
      if (!isExternalFileDrag(e.dataTransfer)) {
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      if (e.dataTransfer) {
        e.dataTransfer.dropEffect = "none";
      }
    };
    for (const type of ["dragover", "drop"] as const) {
      window.addEventListener(type, blockExternalFileDrop, { capture: true });
    }
    return () => {
      for (const type of ["dragover", "drop"] as const) {
        window.removeEventListener(type, blockExternalFileDrop, {
          capture: true,
        });
      }
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
