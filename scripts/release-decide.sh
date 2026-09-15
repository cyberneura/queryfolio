#!/usr/bin/env bash
# VERSION をいまリリースしてよいかを判定し、stdout に true / false を 1 行で出す。
# 判定できない時 (API 障害・想定外の形) は exit 1。理由は stderr に出す。
#
# release.yml の plan (ビルドするか) と publish (公開する直前) の両方から呼ぶ。
# publish でもう一度訊くのは、Actions の「Re-run failed jobs」が成功済みの plan を
# 再実行せず、その時の判定結果を使い回すため。plan から publish までの間に新しい
# version が公開されていると、そのまま --latest で最新を巻き戻してしまう。
#
# 必要な env: GH_TOKEN, GH_REPO (owner/repo), VERSION (X.Y.Z)
set -euo pipefail

: "${GH_REPO:?GH_REPO is required}"
: "${VERSION:?VERSION is required}"

# 新旧比較を X.Y.Z 同士の sort -V に頼るので、それ以外の形は判定しない。
semver='^[0-9]+\.[0-9]+\.[0-9]+$'
if ! [[ "$VERSION" =~ $semver ]]; then
  echo "::error::version is not X.Y.Z: '${VERSION}'" >&2
  exit 1
fi

http_status() {
  gh api "$1" --silent --include 2>/dev/null | head -n 1 | awk '{print $2}' || true
}

# 訊きたいのは「公開済みか」。draft は未リリースとして扱う: 失敗した run が残した
# draft は同じ version で再実行して埋め直すし、publish はビルドが作った draft を
# 前にしてこの判定を呼ぶ。このエンドポイントは draft に 404 を返す (ドキュメントは
# "Get a published release"、実測でもそう) が、200 で draft が返ってきた場合に
# 公開済みと読むと publish が永久に止まるので、200 の時は draft かどうかも見る。
#
# 404 だけを「見つからない」と読む。rate limit や障害をそう読むと、公開済みの
# version をもう一度ビルドして二重に公開しにいく。
status=$(http_status "repos/${GH_REPO}/releases/tags/v${VERSION}")
case "$status" in
  404) ;;
  200)
    # 代入で受ける: `[ "$(gh ...)" ]` の中だと gh の失敗が空文字になり、set -e が効かない。
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

# 未リリースでも、公開中の最新より古い version は出さない。publish は --latest を
# 付けるので、出すと最新が巻き戻り、Homebrew tap もダウングレードする。
# 起こり得るのは、version を上げた commit の revert (一度も出していない 0.2.0 等に
# 戻る) と、pending の run が push 順と逆に消化された時。後者で飛ばされた version は
# それより新しい version に含まれているので、改めて出す必要は無い。
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
