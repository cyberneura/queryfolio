---
name: publish-macos-release
description: Queryfolio をビルドして GitHub Release として公開 (サイトで配布) する手順。「mac 版をリリース」「アプリを公開」「新しいバージョンを配布」「release the mac app」「publish a new build」と言及された時に使う。`pnpm release` で version 採番 → GitHub Actions で macOS dmg (署名 + 公証) と Windows インストーラをビルド → 全プラットフォーム成功時に自動公開 → 公開された成果物の署名・公証を検証してダウンロード導線を確認する、一連の作業を扱う。
---

# Queryfolio のリリース手順

macOS 版 (universal dmg) と Windows 版 (NSIS インストーラ) を GitHub Actions でビルドし、
GitHub Release としてサイト (ダウンロードページ) で公開するまでの runbook。
ビルドは `.github/workflows/release.yml`。**main の `src-tauri/tauri.conf.json` の
version を変えて push すると始まる** (`plan` ジョブが「その version がリリース済みか」を
訊き、未リリースの時だけビルドへ進む)。version の採番と push は `scripts/release.sh`
(`pnpm release` / `fab release`)。構成の設計判断はグローバルスキル
`tauri-github-actions-release` に書いてある。

## 前提

- `gh` が `ytyng` アカウントで認証済みで、リポジトリは `cyberneura/queryfolio`
  (`ytyng` は cyberneura org の admin なので、この 1 アカウントで release まで通る)。
- 署名・公証用の GitHub Secrets が 6 つとも設定済み (下記「初回セットアップ」)。
  **1 つでも未設定なら macOS ジョブは "Check macOS signing secrets" ステップで失敗する**
  (ad-hoc 署名や公証なしの dmg が黙って公開されるのを防ぐための preflight。フォールバックは無い)。
- `main` がクリーンで `origin/main` と一致していること (スクリプトが検証して弾く)。

## リリース手順

### 1. version 採番 → ビルド → 公開 (1 コマンド)

```shell
pnpm release           # 0.1.0 -> 0.1.1 (patch)
pnpm release minor     # 0.1.0 -> 0.2.0
pnpm release major     # 0.1.0 -> 1.0.0
```

`scripts/release.sh` が以下を行う:

1. `gh` の認証、`main` ブランチ・クリーンツリー・`HEAD == origin/main` を検証
2. `src-tauri/tauri.conf.json` の version を bump し、`package.json` にも同期
3. `chore: release v<version>` として commit → `origin/main` へ push
4. その push で始まった run を探し、`gh run watch --exit-status` で追う
   (リリースを始めるのは push であってスクリプトではない)

ワークフローは `draft` ジョブで `v<version>` の **draft** Release を 1 つ用意し、matrix で
macOS (universal dmg / Developer ID 署名 + 公証 + staple) と Windows (NSIS exe / 署名なし) を
並列ビルドしてその draft にアップロードする。全プラットフォームが成功すると `publish` ジョブが
公開直前にリリース判定をやり直し、draft を公開する (tag はその run の commit に作られる)。
所要 15〜25 分程度。

> どれかのプラットフォームが失敗した場合、Release は **draft のまま残る**。原因を直して
> main に push すれば、同じ version のまま次の run がその draft を再利用して公開する
> (version は未公開のままなので上げなくてよい)。version を上げて出し直すなら、残った
> draft は消しておく: `gh release delete v<version> --yes`

### 2. 成果物を検証する (公開後 / 初回は必ず)

DMG を落として署名・公証・universal を実機確認する:

```shell
VERSION=$(node -p "require('./src-tauri/tauri.conf.json').version")
gh release download "v${VERSION}" --pattern '*.dmg' --dir /tmp/qf-release --clobber
hdiutil attach -nobrowse -quiet /tmp/qf-release/Queryfolio_${VERSION}_universal.dmg
APP=/Volumes/Queryfolio/Queryfolio.app
codesign -dv --verbose=2 "$APP"      # Authority=Developer ID Application: Cyberneura K.K. (2YN5TLNQ9J) / flags=...runtime
spctl -a -vvv "$APP"                 # → accepted / source=Notarized Developer ID
xcrun stapler validate "$APP"        # → The validate action worked!
# バンドル内の実行バイナリは crate 名のまま小文字。tauri は mainBinaryName を
# 指定しない限り cargo の出力バイナリ名をそのまま使うので、productName
# (Queryfolio) ではない。ここを productName に揃えるとファイルが無くて落ちる
# (CYBERNEURA-DEV-805 の時点で、元から大文字で書かれていて落ちる状態だった)。
lipo -archs "$APP/Contents/MacOS/queryfolio"   # → x86_64 arm64
hdiutil detach -quiet /Volumes/Queryfolio
```

`source=Notarized Developer ID` と staple 成功が出れば、ユーザーがダウンロードして開いても
Gatekeeper 警告は出ない。ここが `Developer ID` 止まり (Notarized でない) なら公証が
効いていないので、`APPLE_ID` / `APPLE_PASSWORD` (app 用パスワード) / `APPLE_TEAM_ID` を疑う。

### 3. 配布導線を確認・更新する

```shell
gh release view "v${VERSION}" --json assets --jq '.assets[].name'
```

- `https://github.com/cyberneura/queryfolio/releases/latest` が新バージョンを指していること。
- dmg / exe の公開 URL が匿名 (未ログイン) で落とせること。
- README の Download セクションは `releases/latest` を指す固定リンクなので、通常は更新不要。
- 告知が必要なら ytyng-blog (`ytyng-blog-cli` スキル) に BlogPost / Achievement を作る。

## 初回セットアップ (署名・公証 Secrets)

ローカルの署名 ID を CI に持ち込むため、`.p12` を書き出して Secrets に登録する。1 回だけでよい。
公証には Apple ID と app 用パスワードも要る。

```shell
security find-identity -v -p codesigning | grep "Developer ID Application: Cyberneura"
# → キーチェーンアクセスで該当の証明書 + 秘密鍵を .p12 として書き出す (例: queryfolio-signing.p12)

base64 -i queryfolio-signing.p12 | gh secret set APPLE_CERTIFICATE
gh secret set APPLE_CERTIFICATE_PASSWORD   # .p12 のパスワードを入力
printf 'Developer ID Application: Cyberneura K.K. (2YN5TLNQ9J)' | gh secret set APPLE_SIGNING_IDENTITY
gh secret set APPLE_ID                     # Apple Developer のメール
gh secret set APPLE_PASSWORD               # app 用パスワード (appleid.apple.com で発行。通常のパスワードは不可)
printf '2YN5TLNQ9J' | gh secret set APPLE_TEAM_ID
```

登録確認: `gh secret list` (上記 6 つ)。keychain のパスワードはワークフロー内で
`openssl rand` から生成するので Secret は不要 (旧構成の `KEYCHAIN_PASSWORD` は廃止)。

## トラブルシューティング

- **`draft` ジョブが "found N draft releases" で失敗する** → 同じ tag の draft が複数残っている
  (この構成になる前の run の残骸)。どれに上げるか決められないので止めている。
  `gh api repos/cyberneura/queryfolio/releases --jq '.[] | select(.draft) | [.id, .tag_name, .created_at] | @tsv'`
  で確認し、不要な方を `gh api -X DELETE repos/cyberneura/queryfolio/releases/<id>` で消して再実行する。
- **push したのにリリースされない (plan が release=false)** → その version が公開済みか、
  公開中の最新より古い。plan ジョブのログに理由が出る。
- **`spctl` が `source=Developer ID` (Notarized でない)** → 公証が走っていない。macOS ジョブのログで
  tauri-action の notarize ステップを確認し、`APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID` を疑う。
  app 用パスワードは通常の Apple ID パスワードでは代用できない。
- **macOS ジョブが署名で失敗する** → `APPLE_CERTIFICATE` / `APPLE_CERTIFICATE_PASSWORD` 未設定か、
  `.p12` に秘密鍵が入っていない。"Import Apple Developer certificate" ステップの
  `security find-identity` の出力に Developer ID が出ているか見る。
- **Windows ジョブだけ失敗して Release が draft のまま** → Release は公開されない (仕様)。
  原因を直して push (または Actions の "Re-run failed jobs") すれば同じ draft に上げ直して公開する。
  急ぐなら手で `gh release edit v<version> --draft=false --latest` で macOS 分だけ公開できる。
- **`pnpm release` が "working tree is not clean" / "does not match origin/main" で止まる** →
  ビルドは origin/main の内容で走るため、ローカルとズレたままリリースさせない安全弁。
  commit / push してから再実行する。
