#!/usr/bin/env bash
# Decide whether VERSION may be released now, and print true / false on a single line to stdout.
# Exits 1 when it cannot decide (API failure, unexpected shape). The reason goes to stderr.
#
# Called from both plan (whether to build) and publish (just before publishing) in release.yml.
# Publish asks again because Actions' "Re-run failed jobs" does not re-run a plan that already
# succeeded and reuses its earlier result. If a newer version was published between plan and
# publish, --latest would roll the latest back.
#
# Required env: GH_TOKEN, GH_REPO (owner/repo), VERSION (X.Y.Z)
set -euo pipefail

: "${GH_REPO:?GH_REPO is required}"
: "${VERSION:?VERSION is required}"

# The newer/older comparison relies on sort -V between X.Y.Z values, so other shapes are not judged.
semver='^[0-9]+\.[0-9]+\.[0-9]+$'
if ! [[ "$VERSION" =~ $semver ]]; then
  echo "::error::version is not X.Y.Z: '${VERSION}'" >&2
  exit 1
fi

http_status() {
  gh api "$1" --silent --include 2>/dev/null | head -n 1 | awk '{print $2}' || true
}

# What we want to know is "is it published". A draft counts as unreleased: a draft left by a failed
# run is refilled by re-running the same version, and publish calls this decision with the draft the
# build created in front of it. This endpoint returns 404 for a draft (the docs say
# "Get a published release", and it does in practice too), but if a 200 with a draft came back and
# we read it as published, publish would stop forever, so on 200 we also check whether it is a draft.
#
# Only 404 is read as "not found". Reading a rate limit or an outage that way would rebuild an
# already published version and try to publish it twice.
status=$(http_status "repos/${GH_REPO}/releases/tags/v${VERSION}")
case "$status" in
  404) ;;
  200)
    # Capture via assignment: inside `[ "$(gh ...)" ]` a gh failure becomes an empty string and set -e does not fire.
    draft=$(gh api "repos/${GH_REPO}/releases/tags/v${VERSION}" --jq '.draft')
    if [ "$draft" != "true" ] && [ "$draft" != "false" ]; then
      echo "::error::could not read whether v${VERSION} is a draft (got '${draft}'). Not guessing." >&2
      exit 1
    fi
    if [ "$draft" = "false" ]; then
      echo "v${VERSION} is already released." >&2
      echo false
      exit 0
    fi
    ;;
  *)
    echo "::error::could not tell whether v${VERSION} exists (HTTP ${status:-none}). Not guessing." >&2
    exit 1
    ;;
esac

# Even if unreleased, never emit a version older than the latest published one. Publish passes
# --latest, so doing so would roll the latest back and downgrade the Homebrew tap too.
# This can happen when a version-bump commit is reverted (returning to e.g. 0.2.0, which was never
# released) and when pending runs are processed in the opposite order of the pushes. In the latter
# case the skipped version is contained in the newer version, so it need not be released again.
status=$(http_status "repos/${GH_REPO}/releases/latest")
case "$status" in
  404)
    echo "Releasing v${VERSION} (no release yet)." >&2
    echo true
    exit 0
    ;;
  200) ;;
  *)
    echo "::error::could not read the latest release (HTTP ${status:-none}). Not guessing." >&2
    exit 1
    ;;
esac

latest=$(gh api "repos/${GH_REPO}/releases/latest" --jq '.tag_name')
latest="${latest#v}"
if ! [[ "$latest" =~ $semver ]]; then
  echo "::error::latest release tag is not vX.Y.Z: 'v${latest}'. Not guessing." >&2
  exit 1
fi

if [ "$(printf '%s\n%s\n' "$latest" "$VERSION" | sort -V | tail -n 1)" != "$VERSION" ]; then
  echo "::warning::v${VERSION} is older than the latest release v${latest}; not releasing it." >&2
  echo false
  exit 0
fi

echo "Releasing v${VERSION} (latest is v${latest})." >&2
echo true
