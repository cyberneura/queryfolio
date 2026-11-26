---
name: publish-macos-release
description: The procedure for building Queryfolio and publishing it as a GitHub Release (distributed on the site). Use when someone mentions "release the mac app", "publish the app", "distribute a new version", "publish a new build" (or the Japanese phrasings 「mac 版をリリース」「アプリを公開」「新しいバージョンを配布」). Covers the whole sequence: assign the version with `pnpm release` → GitHub Actions builds the macOS dmg (signed + notarized) and the Windows installer → automatic publication when all platforms succeed → verify the signature and notarization of the published artifacts and check the download path.
---

# Queryfolio release procedure

A runbook for building the macOS version (universal dmg) and the Windows version (NSIS installer) with GitHub Actions
and publishing them as a GitHub Release on the site (download page).
The build is `.github/workflows/release.yml`. **It starts when the version in `src-tauri/tauri.conf.json` on main
is changed and pushed** (the `plan` job asks "has this version already been released?"
and proceeds to the build only when it has not). Version assignment and the push are done by `scripts/release.sh`
(`pnpm release` / `fab release`). The design decisions behind this setup are written in the global skill
`tauri-github-actions-release`.

## Prerequisites

- `gh` is authenticated with the `ytyng` account, and the repository is `cyberneura/queryfolio`
  (`ytyng` is an admin of the cyberneura org, so this one account gets all the way through the release).
- All 6 GitHub Secrets for signing and notarization are set (see "First-time setup" below).
  **If even one is missing, the macOS job fails at the "Check macOS signing secrets" step**
  (a preflight to prevent an ad-hoc signed or un-notarized dmg from being published silently. There is no fallback).
- `main` is clean and matches `origin/main` (the script checks this and refuses otherwise).

## Release steps

### 1. Assign the version → build → publish (one command)

```shell
pnpm release           # 0.1.0 -> 0.1.1 (patch)
pnpm release minor     # 0.1.0 -> 0.2.0
pnpm release major     # 0.1.0 -> 1.0.0
```

`scripts/release.sh` does the following:

1. Checks `gh` authentication, the `main` branch, a clean tree, and `HEAD == origin/main`
2. Bumps the version in `src-tauri/tauri.conf.json` and syncs it to `package.json` as well
3. Commits as `chore: release v<version>` → pushes to `origin/main`
4. Finds the run started by that push and follows it with `gh run watch --exit-status`
   (what starts the release is the push, not the script)

The workflow prepares one **draft** Release for `v<version>` in the `draft` job, builds in a matrix
macOS (universal dmg / both the .app and the dmg are Developer ID signed + notarized + stapled) and Windows (NSIS exe / unsigned)
in parallel, and uploads them to that draft. When all platforms succeed, the `publish` job
repeats the release decision right before publishing and publishes the draft (the tag is created on that run's commit).
It takes roughly 15–25 minutes.

> If any platform fails, the Release **stays a draft**. Fix the cause and
> push to main, and the next run reuses that draft with the same version and publishes it
> (the version is still unpublished, so you do not need to bump it). If you bump the version and release again instead, delete the leftover
> draft: `gh release delete v<version> --yes`

### 2. Verify the artifacts (after publication / always the first time)

Download the DMG and confirm the signature, notarization, and universal binary on a real machine:

```shell
VERSION=$(node -p "require('./src-tauri/tauri.conf.json').version")
gh release download "v${VERSION}" --pattern '*.dmg' --dir /tmp/qf-release --clobber
DMG=/tmp/qf-release/Queryfolio_${VERSION}_universal.dmg
spctl -a -vv -t open --context context:primary-signature "$DMG"   # → accepted / source=Notarized Developer ID
xcrun stapler validate "$DMG"        # → The validate action worked!
hdiutil attach -nobrowse -quiet "$DMG"
APP=/Volumes/Queryfolio/Queryfolio.app
codesign -dv --verbose=2 "$APP"      # Authority=Developer ID Application: Cyberneura K.K. (2YN5TLNQ9J) / flags=...runtime
spctl -a -vvv "$APP"                 # → accepted / source=Notarized Developer ID
xcrun stapler validate "$APP"        # → The validate action worked!
# The executable binary inside the bundle keeps the crate name in lowercase. tauri uses
# cargo's output binary name as-is unless mainBinaryName is specified, so it is not
# productName (Queryfolio). Aligning this with productName fails because the file does not exist
# (as of CYBERNEURA-DEV-805, it had been written in uppercase from the start and was failing).
lipo -archs "$APP/Contents/MacOS/queryfolio"   # → x86_64 arm64
hdiutil detach -quiet /Volumes/Queryfolio
```

If you see `source=Notarized Developer ID` and a successful staple, users will not get a
Gatekeeper warning when they download and open it. If it stops at `Developer ID` (not Notarized), notarization
is not taking effect, so suspect `APPLE_ID` / `APPLE_PASSWORD` (app-specific password) / `APPLE_TEAM_ID`.

### 3. Check and update the distribution path

```shell
gh release view "v${VERSION}" --json assets --jq '.assets[].name'
```

- `https://github.com/cyberneura/queryfolio/releases/latest` points at the new version.
- The public URLs of the dmg / exe can be downloaded anonymously (not logged in).
- The README's Download section is a fixed link pointing at `releases/latest`, so it normally needs no update.
- If an announcement is needed, create a BlogPost / Achievement on ytyng-blog (the `ytyng-blog-cli` skill).

## First-time setup (signing and notarization Secrets)

To bring the local signing identity into CI, export a `.p12` and register it in Secrets. This only needs to be done once.
Notarization also needs an Apple ID and an app-specific password.

```shell
security find-identity -v -p codesigning | grep "Developer ID Application: Cyberneura"
# → In Keychain Access, export the matching certificate + private key as a .p12 (e.g. queryfolio-signing.p12)

base64 -i queryfolio-signing.p12 | gh secret set APPLE_CERTIFICATE
gh secret set APPLE_CERTIFICATE_PASSWORD   # Enter the .p12 password
printf 'Developer ID Application: Cyberneura K.K. (2YN5TLNQ9J)' | gh secret set APPLE_SIGNING_IDENTITY
gh secret set APPLE_ID                     # The Apple Developer email
gh secret set APPLE_PASSWORD               # App-specific password (issued at appleid.apple.com; the regular password does not work)
printf '2YN5TLNQ9J' | gh secret set APPLE_TEAM_ID
```

Check the registration: `gh secret list` (the 6 above). The keychain password is generated inside the workflow
from `openssl rand`, so no Secret is needed for it (`KEYCHAIN_PASSWORD` from the old setup has been retired).

## Troubleshooting

- **The `draft` job fails with "found N draft releases"** → multiple drafts with the same tag are left over
  (remnants of runs from before this setup). It stops because it cannot decide which one to upload to.
  Check with `gh api repos/cyberneura/queryfolio/releases --jq '.[] | select(.draft) | [.id, .tag_name, .created_at] | @tsv'`,
  delete the unneeded ones with `gh api -X DELETE repos/cyberneura/queryfolio/releases/<id>`, and re-run.
- **Pushed but nothing was released (plan returned release=false)** → that version is already published, or it is
  older than the latest published one. The reason appears in the plan job's log.
- **`spctl` shows `source=Developer ID` / `Unnotarized Developer ID` (not Notarized)** → notarization did not run.
  For the .app, check the log of the macOS job's "Build" step (the tauri CLI's notarize); for the dmg, check the log of the "Notarize the dmg"
  step (the `notarytool` result, and `notarytool log` when Invalid), and
  suspect `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID`.
  The app-specific password cannot be substituted with the regular Apple ID password.
- **The macOS job fails at signing** → `APPLE_CERTIFICATE` / `APPLE_CERTIFICATE_PASSWORD` is not set, or
  the `.p12` does not contain the private key. Check whether the Developer ID appears in the output of
  `security find-identity` in the "Import Apple Developer certificate" step.
- **Only the Windows job fails and the Release stays a draft** → the Release is not published (by design).
  Fix the cause and push (or use "Re-run failed jobs" in Actions), and it uploads to the same draft again and publishes it.
  If you are in a hurry, you can publish just the macOS part by hand with `gh release edit v<version> --draft=false --latest`.
- **`pnpm release` stops with "working tree is not clean" / "does not match origin/main"** →
  the build runs on the contents of origin/main, so this is a safety valve that refuses to release while local is out of sync.
  Commit / push, then run it again.
