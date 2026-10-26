#!/bin/sh
# Build the .app, sign it with the Developer ID certificate and move it to /Applications.
set -eu

cd "$(dirname "$0")/.."

# Keep signing with the same certificate: macOS ties granted permissions (folder access,
# keychain items) to the signature, and treats a build signed otherwise as another app
SIGN_IDENTITY="${SIGN_IDENTITY:-Developer ID Application: Cyberneura K.K. (2YN5TLNQ9J)}"
DEST_DIR="${DEST_DIR:-/Applications}"

product_name="$(node -p "require('./src-tauri/tauri.conf.json').productName")"
bundle="src-tauri/target/release/bundle/macos/${product_name}.app"
dest="${DEST_DIR}/${product_name}.app"
staging="${dest}.new"
previous="${dest}.old"

# Two runs share the build output and the staging path, and one would delete what the other
# is about to move into place. The lock sits next to the destination, so every run deploying
# there shares it whichever checkout it comes from. It records its owner's pid: a run that was
# killed leaves it behind, and the next run reclaims it (and then restores the app below)
# instead of refusing until it is removed by hand
lock="${dest}.deploy-lock"
take_lock() {
    mkdir "$lock" 2>/dev/null || return 1
    echo $$ > "$lock/pid"
}
if ! take_lock; then
    owner="$(cat "$lock/pid" 2>/dev/null || true)"
    # No pid yet means its owner has only just created it
    if [ -z "$owner" ] || kill -0 "$owner" 2>/dev/null; then
        echo "Another local deploy is running (remove $lock if it is not)." >&2
        exit 1
    fi
    # Renaming is atomic, so of two runs reclaiming the same stale lock only one gets it
    stale="${lock}.stale.$$"
    if mv "$lock" "$stale" 2>/dev/null; then
        rm -rf "$stale"
    fi
    if ! take_lock; then
        echo "Another local deploy is running (remove $lock if it is not)." >&2
        exit 1
    fi
fi

# The swap below moves the installed version aside before putting the new one in place. If
# the script stops in between (an interrupt, or a run that was killed), the app would be
# missing with the old version left next to it, so it is put back on exit and on the next start
restore_previous() {
    if [ -e "$previous" ] && [ ! -e "$dest" ]; then
        mv "$previous" "$dest"
    fi
}
trap 'restore_previous; rm -rf "$lock"' EXIT
trap 'exit 130' INT TERM
restore_previous

rm -rf "$bundle"
# `pnpm exec tauri`, not `pnpm tauri`: the package.json script sets APPLE_SIGNING_IDENTITY, which
# makes Tauri sign with --timestamp and the Hardened Runtime. Both are there for notarization,
# not needed for a local copy, and the timestamp fails offline. The APPLE_* variables Tauri
# reads are also cleared, so one exported in the shell cannot bring the signing (or, with
# the Apple ID / API key ones, notarization) back. Tauri then signs ad hoc
# (bundle.macOS.signingIdentity "-") and the signature is replaced below
env -u APPLE_SIGNING_IDENTITY -u APPLE_CERTIFICATE -u APPLE_CERTIFICATE_PASSWORD \
    -u APPLE_ID -u APPLE_PASSWORD -u APPLE_TEAM_ID \
    -u APPLE_API_KEY -u APPLE_API_ISSUER -u APPLE_API_KEY_PATH \
    pnpm exec tauri build --bundles app

codesign --force --deep --timestamp=none --sign "$SIGN_IDENTITY" "$bundle"
codesign --verify --deep --strict "$bundle"

# The build output may be on another volume, and that move can fail halfway, so it goes to a
# staging path next to the destination first. The swap after it is two renames on one volume,
# and the old version is put back if the second one fails
rm -rf "$staging" "$previous"
mv "$bundle" "$staging"
if [ -e "$dest" ]; then
    mv "$dest" "$previous"
fi
if ! mv "$staging" "$dest"; then
    restore_previous
    echo "Could not move the new build into place; the previous version was kept." >&2
    exit 1
fi
rm -rf "$previous"

echo "Deployed: $dest"
codesign -dv --verbose=2 "$dest" 2>&1 | grep '^Authority=' | head -1
echo "If ${product_name} is running, restart it to use the new build."
