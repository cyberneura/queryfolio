#!/usr/bin/env bash
# GitHub Actions の Release ワークフローを起動し、完了まで watch する。
# `pnpm release [patch|minor|major]` から呼ばれる (省略時は patch)。
#
# 処理の流れ:
#   1. 作業ツリーがクリーン かつ HEAD == origin/main であることを検証
#   2. tauri.conf.json の version を bump 種別に応じて採番
#   3. tauri.conf.json / package.json の version を書き換えて commit & push
#   4. workflow をトリガーして watch
#
# version を毎回インクリメントするのは、公開済みバージョンと同じ version で
# workflow を再実行すると tauri-action が draft 状態の不一致でエラーになるため。
# 採番を自動化することで「bump し忘れて落ちる」事故を構造的に無くす。
#
# gh CLI (認証済み) が必要。
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

# gh の存在と認証を、何かを書き換える前に確認する。push した後で gh が使えないと、
# bump コミットだけが main に載って workflow が起動されず、次回実行が別 version を
# 採番してしまう (公開されない version が main に取り残される)。
if ! command -v gh >/dev/null 2>&1; then
  echo "Error: gh CLI not found. Install it and run 'gh auth login'." >&2
  exit 1
fi
if ! gh auth status >/dev/null 2>&1; then
  echo "Error: gh is not authenticated. Run 'gh auth login'." >&2
  exit 1
fi

# 採番は main のクリーンな状態からのみ行う。ローカルの未コミット変更が紛れ込んだり、
# origin/main とズレたままビルドするのを防ぐ (ビルドは origin/main の内容で走るため)。
if [ "$(git branch --show-current)" != "main" ]; then
  echo "Error: not on the 'main' branch. Switch to main first." >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "Error: working tree is not clean. Commit or stash your changes first." >&2
  exit 1
fi
# refspec を明示して origin/main を確実に更新する。`git fetch origin main` でも
# remote-tracking ref は更新されるが (git 1.8.4 以降の opportunistic update)、
# 明示しておけば remote の fetch 設定に依存しない。先頭の + は clone 既定の refspec と
# 同じ強制更新で、force push 後も fetch 自体は成功させ、ズレは下の比較で検出する。
git fetch origin +main:refs/remotes/origin/main
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
  echo "Error: local HEAD does not match origin/main. Push (or pull) first." >&2
  exit 1
fi

# 現行 version を読み、bump 種別に応じて次の version を計算する。厳密な X.Y.Z
# だけを受け付ける (Number.isNaN(undefined) は false のため、"1.2" や "1.2.3.4"
# のような不正な値を弾くには正規表現で全体を検証する必要がある)。
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

# tauri.conf.json / package.json の version を更新する。両ファイルを先に読んで
# 置換に成功することを確認してから書き込む (片方だけ更新されて中途半端な状態で
# 終わるのを避ける)。JSON をパースしてトップレベルの version 値を特定し、その値を
# ピンポイントで置換する (ファイル全体を再整形せず、別位置の version キーの誤爆も防ぐ)。
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

echo "Triggering release build for v${VERSION} ..."

# workflow_dispatch は run ID を返さないため、起動後にポーリングして拾う。
# 「最新の run」ではなく「今 push した bump コミットを head に持つ run」を探す:
# ポーリング中に別の dispatch が挟まっても、他人の run を watch してしまわない。
# version は毎回インクリメントされるので、この SHA を持つ run は今回の 1 つだけ。
RELEASE_SHA=$(git rev-parse HEAD)

if ! gh workflow run release.yml --ref main; then
  echo "Error: failed to trigger the workflow. v${VERSION} is already pushed to main." >&2
  echo "  Retry with: gh workflow run release.yml --ref main" >&2
  exit 1
fi

# `|| true` が無いと、GitHub API の一時エラーで set -e がリトライループごと殺す
# (X=$(failing-cmd) は set -e で即 exit する)。ここは「まだ run が出てこない」状態を
# 待つループなので、失敗は空文字として扱う。
# 60 回 x 2 秒 = 最大 2 分。run 一覧 API は反映が遅れることがあり、短いと誤判定する。
RUN_ID=""
for _ in $(seq 1 60); do
  sleep 2
  RUN_ID=$(gh run list --workflow=release.yml --branch main --limit 20 \
    --json databaseId,headSha \
    --jq "[.[] | select(.headSha == \"${RELEASE_SHA}\")] | .[0].databaseId // \"\"" \
    2>/dev/null || true)
  if [ -n "${RUN_ID}" ]; then
    break
  fi
done
if [ -z "${RUN_ID}" ]; then
  # 見つからないだけで、run 自体は動いている可能性が高い (watch できないだけ)。
  echo "Error: could not find the triggered workflow run within 2 minutes." >&2
  echo "  The build may still be running. Check it with:" >&2
  echo "    gh run list --workflow=release.yml" >&2
  exit 1
fi
echo "Watching run ${RUN_ID} ..."
gh run watch "${RUN_ID}" --exit-status

echo "Done: https://github.com/cyberneura/queryfolio/releases/tag/v${VERSION}"
