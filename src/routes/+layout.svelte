<script lang="ts">
  import "../app.css";
  import { onMount } from "svelte";
  import { Toaster } from "svelte-sonner";
  import { isReloadShortcut } from "$lib/reloadGuard";
  import { isExternalFileDrag } from "$lib/fileDrag";

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

  /// OS から持ち込まれたファイルのドロップを無効化する (CYBERNEURA-DEV-977)。
  ///
  /// FILES → CONNECTIONS のドラッグ & ドロップを動かすため、tauri.conf.json で
  /// `dragDropEnabled: false` にしている (true のままだと Tauri のネイティブ
  /// ハンドラがドロップを奪い、macOS では HTML の drop イベントが発火しない)。
  /// その代わり外部ファイルのドロップは WebView の既定動作 (そのファイルへの
  /// 遷移) に流れるので、ここで止める。capture フェーズで止めて、CodeMirror が
  /// ファイルの中身を挿入する経路にも渡さない (以前と同じく「何も起きない」)。
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
