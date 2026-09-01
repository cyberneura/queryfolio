/// WebView が既定で持つ「ページのリロード」ショートカットを潰すためのガード
/// (CYBERNEURA-DEV-648)。
///
/// QueryFolio は SPA で、接続・実行中のクエリ・エディタタブ・結果テーブルを
/// すべてメモリ上に持っている。ページが再読込されるとアプリ全体が初期状態へ
/// 戻り、編集中のクエリと取得済みの結果が黙って消える。ブラウザのページと
/// 違って「戻る」手段が無いので、この機能ごと無効化する。
///
/// Cmd+R でアプリが初期化されていた主因はこれではなく、ネイティブメニューの
/// `Reload config file` に付いていた CmdOrCtrl+R アクセラレータで、そちらは
/// lib.rs 側でアクセラレータごと外してある。ネイティブメニューのキーは
/// WebView まで届かないため、preventDefault では止められない。
/// このガードが効くのは WebView 自身が持つリロード
/// (Windows の WebView2 は Ctrl+R / F5 を既定のアクセラレータとして持つ) の方で、
/// メニュー側の対処とは経路が別なので両方が要る。

/// リロードを起こすキー操作か判定する。
///
/// macOS の Cmd+R / Windows・Linux の Ctrl+R に加え、同じくリロードに割り当て
/// られている F5 と、キャッシュ無視のリロード (Shift 併用) も対象にする。
/// いずれも QueryFolio 側では何にも使っていないので、素通しする理由が無い。
export function isReloadShortcut(e: KeyboardEvent): boolean {
  // Alt が付くものは別の操作なので触らない (F5 単体との取り違えを避ける)
  if (e.altKey) {
    return false;
  }
  if (e.key === "F5") {
    return true;
  }
  // e.key は Shift の有無で "r" / "R" になるため小文字化して比べる。
  // Cmd と Ctrl はどちらか一方だけを見る (両方同時押しは別の操作扱い)
  return (e.metaKey !== e.ctrlKey) && e.key.toLowerCase() === "r";
}
