/// Guard that disables the "reload page" shortcuts the WebView has by default
/// (CYBERNEURA-DEV-648).
///
/// Queryfolio is a SPA and keeps connections, running queries, editor tabs and result tables
/// all in memory. If the page is reloaded, the whole app returns to its initial state, and
/// the query being edited and the fetched results silently vanish. Unlike a browser page,
/// there is no way to go back, so this feature is disabled altogether.
///
/// The main cause of the app being reset by Cmd+R was not this, but the CmdOrCtrl+R accelerator
/// on the native menu item `Reload config file`, which has been removed together with its
/// accelerator on the lib.rs side. Native menu keys do not reach the WebView, so
/// preventDefault cannot stop them.
/// This guard is effective against the WebView's own reload
/// (WebView2 on Windows has Ctrl+R / F5 as default accelerators). It takes a different path
/// from the menu-side fix, so both are needed.

/// Determines whether a key press would trigger a reload.
///
/// Besides Cmd+R on macOS / Ctrl+R on Windows and Linux, this also covers F5, which is
/// likewise bound to reload, and cache-bypassing reloads (with Shift).
/// Queryfolio uses none of them, so there is no reason to let them through.
export function isReloadShortcut(e: KeyboardEvent): boolean {
  // Keys with Alt are different operations, so leave them alone (avoids confusing them with plain F5)
  if (e.altKey) {
    return false;
  }
  if (e.key === "F5") {
    return true;
  }
  // e.key is "r" / "R" depending on Shift, so compare after lowercasing.
  // Look at only one of Cmd and Ctrl (pressing both at once is treated as a different operation)
  return (e.metaKey !== e.ctrlKey) && e.key.toLowerCase() === "r";
}
