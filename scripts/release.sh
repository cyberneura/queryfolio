#!/usr/bin/env bash
# Assign a version number, land it on main, and watch the Release workflow that starts from it.
# Called from `pnpm release [patch|minor|major]` (defaults to patch).
#
# Flow:
#   1. Verify the working tree is clean and HEAD == origin/main
#   2. Compute the next version in tauri.conf.json according to the bump type
#   3. Rewrite the version in tauri.conf.json / package.json, then commit & push
#   4. Find the run started by that push and watch it
#
# It is the push, not this script, that starts the release (the workflow's `on: push`).
# The build still runs if this script fails, and pushing the same version again is harmless:
# the workflow sees it is already released and does nothing.
#
# The version is incremented every time because re-running the workflow with the same version as
# an already published one makes tauri-action fail on a draft state mismatch.
# Automating the numbering structurally eliminates the "forgot to bump and it failed" accident.
#
# Requires the gh CLI (authenticated).
set -euo pipefail

cd "$(dirname "$0")/.."

BUMP="${1:-patch}"
case "${BUMP}" in
  patch | minor | major) ;;
  *)
    echo "Usage: pnpm release [patch|minor|major]  (default: patch)" >&2
    exit 1
    ;;
esac

# Check that gh exists and is authenticated before rewriting anything. The release itself starts
# from the push, so the build runs even without gh, but then this script cannot watch it.
# Better to say so and stop up front than to end without knowing whether it started.
if ! command -v gh >/dev/null 2>&1; then
  echo "Error: gh CLI not found. Install it and run 'gh auth login'." >&2
  exit 1
fi
if ! gh auth status >/dev/null 2>&1; then
  echo "Error: gh is not authenticated. Run 'gh auth login'." >&2
  exit 1
fi

# Only number from a clean main. This prevents uncommitted local changes from slipping in, or
# building while out of sync with origin/main (the build runs on the contents of origin/main).
if [ "$(git branch --show-current)" != "main" ]; then
  echo "Error: not on the 'main' branch. Switch to main first." >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "Error: working tree is not clean. Commit or stash your changes first." >&2
  exit 1
fi
# Specify the refspec explicitly so origin/main is reliably updated. `git fetch origin main` also
# updates the remote-tracking ref (opportunistic update since git 1.8.4), but being explicit
# means we do not depend on the remote's fetch config. The leading + is the same forced update as
# the clone's default refspec; fetching still succeeds after a force push, and the mismatch is
# caught by the comparison below.
git fetch origin +main:refs/remotes/origin/main
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
  echo "Error: local HEAD does not match origin/main. Push (or pull) first." >&2
  exit 1
fi

# Read the current version and compute the next one according to the bump type. Only a strict X.Y.Z
# is accepted (Number.isNaN(undefined) is false, so rejecting bad values such as "1.2" or "1.2.3.4"
# requires validating the whole string with a regex).
CURRENT=$(node -p "require('./src-tauri/tauri.conf.json').version")
VERSION=$(node -e '
  const cur = process.argv[1];
  const bump = process.argv[2];
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(cur)) {
    console.error("Error: current version is not X.Y.Z: " + cur);
    process.exit(1);
  }
  const [maj, min, pat] = cur.split(".").map(Number);
  const next = bump === "major" ? [maj + 1, 0, 0]
    : bump === "minor" ? [maj, min + 1, 0]
    : [maj, min, pat + 1];
  process.stdout.write(next.join("."));
' "${CURRENT}" "${BUMP}")

echo "Bumping version: ${CURRENT} -> ${VERSION} (${BUMP})"

# Update the version in tauri.conf.json / package.json. Read both files first and confirm the
# replacement succeeds before writing (to avoid ending up half-updated with only one of them
# changed). Parse the JSON to locate the top-level version value and replace just that value
# (without reformatting the whole file, and without hitting a version key at another position).
node -e '
  const fs = require("fs");
  const version = process.argv[1];
  const files = ["src-tauri/tauri.conf.json", "package.json"];
  const edits = files.map((file) => {
    const text = fs.readFileSync(file, "utf8");
    const old = JSON.parse(text).version;
    if (typeof old !== "string") {
      throw new Error("no top-level string version in " + file);
    }
    const esc = old.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const needle = new RegExp("(\"version\"\\s*:\\s*\")" + esc + "(\")");
    const out = text.replace(needle, "$1" + version + "$2");
    if (out === text) throw new Error("version not replaced in " + file);
    return { file, out };
  });
  for (const e of edits) fs.writeFileSync(e.file, e.out);
' "${VERSION}"

git add src-tauri/tauri.conf.json package.json
git commit -m "chore: release v${VERSION}"
if ! git push origin HEAD:main; then
  echo "Error: push failed. The local release commit remains." >&2
  echo "  Undo it:  git reset --hard origin/main" >&2
  echo "  Or retry: git push origin HEAD:main" >&2
  exit 1
fi

echo "Waiting for the release build of v${VERSION} ..."

# A run started by the push shows up in the API after a short delay, so poll for it.
# Look for "the push run whose head is the bump commit we just pushed", not "the latest run":
# even if another push or dispatch lands while waiting, we never watch someone else's run.
RELEASE_SHA=$(git rev-parse HEAD)

# Without `|| true`, a transient GitHub API error makes set -e kill the whole retry loop
# (X=$(failing-cmd) exits immediately under set -e). This loop keeps waiting while no run
# has appeared yet, so a failure is treated as an empty string.
# 60 tries x 2 seconds = 2 minutes at most. The run-list API can lag, and a shorter wait misjudges.
RUN_ID=""
for _ in $(seq 1 60); do
  sleep 2
  RUN_ID=$(gh run list --workflow=release.yml --branch main --event push --limit 20 \
    --json databaseId,headSha \
    --jq "[.[] | select(.headSha == \"${RELEASE_SHA}\")] | .[0].databaseId // \"\"" \
    2>/dev/null || true)
  if [ -n "${RUN_ID}" ]; then
    break
  fi
done
if [ -z "${RUN_ID}" ]; then
  # Most likely it was just not found and the run itself is going (we simply cannot watch it).
  echo "Error: could not find the workflow run within 2 minutes." >&2
  echo "  The build may still be running. Check it with:" >&2
  echo "    gh run list --workflow=release.yml" >&2
  exit 1
fi
echo "Watching run ${RUN_ID} ..."
gh run watch "${RUN_ID}" --exit-status

# A successful run does not mean "published". A run where plan returned release=false (e.g. a
# newer version pushed later was published first) also ends in success, with build and later
# jobs skipped. Confirm a published (non-draft) Release exists before saying Done.
if [ "$(gh release view "v${VERSION}" --json isDraft --jq '.isDraft' 2>/dev/null || true)" != "false" ]; then
  echo "Error: the run succeeded but v${VERSION} is not published. See why in the plan job:" >&2
  echo "  gh run view ${RUN_ID} --log" >&2
  exit 1
fi

echo "Done: https://github.com/cyberneura/queryfolio/releases/tag/v${VERSION}"
