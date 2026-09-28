//! 配布物に同梱している依存ライブラリのライセンス一覧。
//!
//! リポジトリ直下の THIRD-PARTY-NOTICES.txt (`scripts/generate-third-party-notices.sh`
//! = `pnpm notices` の生成物) をビルド時に埋め込む。`--license` とメニューの
//! Third-Party Licenses が表示する。
//!
//! テストは「依存を変えたのに `pnpm notices` を流し忘れた」ことを検出する。

pub const NOTICES: &str = include_str!("../../THIRD-PARTY-NOTICES.txt");

#[cfg(test)]
mod tests {
    use super::NOTICES;

    const RUST_HEADING: &str = "# Rust crates";
    const NATIVE_HEADING: &str = "# Native libraries";

    /// Windows の checkout は autocrlf で CRLF になるので、行末を揃えてから読む
    /// (`"\nimporters:\n"` のような改行込みの検索が外れる)。
    fn lf(text: &str) -> String {
        text.replace("\r\n", "\n")
    }

    /// 配布物に入る直接依存の crate 名を Cargo.toml から拾う。`[dependencies]` と
    /// `[target.'cfg(..)'.dependencies]` の `name = ...` で始まる行だけを見る
    /// (`[dependencies.foo]` 形式は拾えない)。複数行にまたがる値の続きの行は
    /// `name =` の形をしていないので拾わない。build / dev 依存は配布物に入らないので除く。
    /// 配布しないターゲット (Linux) 専用の target 依存を足すと、about.toml の targets の
    /// 外なので notices に載らず、このテストが落ちる。その時はここで除外する。
    fn direct_rust_dependencies(manifest: &str) -> Vec<String> {
        let mut section = String::new();
        let mut names = Vec::new();
        for line in manifest.lines() {
            if line.starts_with('[') {
                section = line.trim().to_string();
                continue;
            }
            let shipped = section == "[dependencies]"
                || (section.starts_with("[target.") && section.ends_with(".dependencies]"));
            // 値の続きの行 (features の列挙等) はインデントされている
            if !shipped || line.starts_with(|c: char| c.is_whitespace() || c == '#') {
                continue;
            }
            if let Some((name, _)) = line.split_once('=') {
                let name = name.trim();
                if !name.is_empty() && !name.contains(|c: char| c == '"' || c == ']') {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    /// package.json の dependencies (webview に bundle される runtime 依存) の名前。
    fn direct_npm_dependencies() -> Vec<String> {
        let package: serde_json::Value =
            serde_json::from_str(include_str!("../../package.json")).expect("package.json parses");
        package["dependencies"]
            .as_object()
            .expect("package.json has dependencies")
            .keys()
            .cloned()
            .collect()
    }

    /// notices の節 (npm / rust / native) の本文。見出しの行で分ける。
    fn section<'a>(notices: &'a str, name: &str) -> &'a str {
        let (npm, rest) = notices
            .split_once(RUST_HEADING)
            .expect("the notices file has a Rust crates heading");
        let (rust, native) = rest
            .split_once(NATIVE_HEADING)
            .expect("the notices file has a Native libraries heading");
        match name {
            "npm" => npm,
            "rust" => rust,
            "native" => native,
            _ => panic!("unknown section {name}"),
        }
    }

    /// THIRD-PARTY-NOTICES.txt の "Used by:" ブロックに並ぶ行 (先頭の 2 空白を除いたもの)。
    /// 生成物のエントリは区切り線 → `License: ...` → 空行 → "Used by:" の並びなので、
    /// その並びだけをブロックとして読む (ライセンス本文に同じ字面があっても数えない)。
    fn used_by_lines(text: &str) -> Vec<String> {
        let separator = "=".repeat(80);
        let mut lines = Vec::new();
        let mut in_block = false;
        let mut after_separator = false;
        let mut after_license = false;
        for line in text.lines() {
            if in_block {
                match line.strip_prefix("  ") {
                    Some(rest) if !rest.is_empty() => lines.push(rest.to_string()),
                    _ => in_block = false,
                }
                continue;
            }
            in_block = after_license && line == "Used by:";
            after_license = (after_separator && line.starts_with("License: "))
                || (after_license && line.is_empty());
            after_separator = line == separator;
        }
        lines
    }

    /// npm / rust 節の (package 名, version)。行は `name version (repository)`。
    fn packages_in_notices(notices: &str, name: &str) -> Vec<(String, String)> {
        used_by_lines(section(notices, name))
            .iter()
            .map(|line| {
                let mut words = line.split(' ');
                let package = words.next().unwrap_or_default().to_string();
                let version = words.next().unwrap_or_default().to_string();
                (package, version)
            })
            .collect()
    }

    /// native 節の (crate 名, version)。行は `<library> (bundled by <crate> <version>)`。
    fn native_crates_in_notices(notices: &str) -> Vec<(String, String)> {
        used_by_lines(section(notices, "native"))
            .iter()
            .map(|line| {
                let inner = line
                    .split_once("(bundled by ")
                    .and_then(|(_, rest)| rest.strip_suffix(')'))
                    .unwrap_or_else(|| panic!("unexpected native entry: {line}"));
                let (name, version) = inner.split_once(' ').expect("crate and version");
                (name.to_string(), version.to_string())
            })
            .collect()
    }

    /// Cargo.lock の [[package]] ブロック。(name, version, dependencies の行)。
    fn locked_rust_packages(lock: &str) -> Vec<(String, String, Vec<String>)> {
        lock.split("[[package]]")
            .skip(1)
            .map(|block| {
                let field = |key: &str| {
                    block
                        .lines()
                        .find_map(|line| line.strip_prefix(key))
                        .map(|rest| rest.trim().trim_matches('"').to_string())
                        .unwrap_or_default()
                };
                let deps = block
                    .lines()
                    .filter_map(|line| line.strip_prefix(" \""))
                    .map(|line| line.trim_end_matches("\",").to_string())
                    .collect();
                (field("name = "), field("version = "), deps)
            })
            .collect()
    }

    /// Cargo.lock が queryfolio の直接依存 `name` に選んだ version。同じ crate が 2 つ以上の
    /// version で入っている時は、ルートの dependencies に `name version` の形で書かれる。
    fn resolved_rust_version(lock: &[(String, String, Vec<String>)], name: &str) -> String {
        let root = lock
            .iter()
            .find(|(crate_name, _, _)| crate_name == "queryfolio")
            .expect("Cargo.lock has the queryfolio package");
        let entry = root
            .2
            .iter()
            .find(|dep| *dep == name || dep.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} is not a dependency of queryfolio in Cargo.lock"));
        // 同名同 version で source が違う時は `name version (source)` になるので 2 語目だけ
        match entry.split(' ').nth(1) {
            Some(version) => version.to_string(),
            None => {
                let mut versions = lock
                    .iter()
                    .filter(|(crate_name, _, _)| crate_name == name)
                    .map(|(_, version, _)| version.clone());
                let version = versions.next().expect("the crate is in Cargo.lock");
                assert!(
                    versions.next().is_none(),
                    "{name} has several versions in Cargo.lock"
                );
                version
            }
        }
    }

    /// pnpm-lock.yaml の importers の `.` (このプロジェクト) が `group`
    /// (`dependencies` / `devDependencies`) に選んだ (name, version)。
    /// `name:` → `specifier:` → `version:` の 3 行で並ぶ。
    fn resolved_npm_versions(lock: &str, group: &str) -> Vec<(String, String)> {
        let header = format!("    {group}:");
        let importer = lock
            .split("\nimporters:\n")
            .nth(1)
            .expect("pnpm-lock.yaml has an importers section")
            .split("\npackages:\n")
            .next()
            .expect("importers come before packages");
        let mut resolved = Vec::new();
        let mut name = String::new();
        let mut in_dependencies = false;
        for line in importer.lines() {
            if line.starts_with("    ") && !line.starts_with("     ") {
                in_dependencies = line == header;
                continue;
            }
            if !in_dependencies {
                continue;
            }
            if let Some(key) = line
                .strip_prefix("      ")
                .filter(|rest| !rest.starts_with(' '))
            {
                name = key.trim_end_matches(':').trim_matches('\'').to_string();
            } else if let Some(version) = line.strip_prefix("        version: ") {
                // peer 依存の括弧は notices の version には無い
                let version = version.split('(').next().unwrap_or(version).trim();
                resolved.push((name.clone(), version.to_string()));
            }
        }
        resolved
    }

    /// pnpm-lock.yaml の packages 節に並ぶ全 (name, version)。キーは `name@version:`
    /// (scoped は引用符付き)。推移依存と、bundle される devDependencies の照合に使う。
    fn locked_npm_packages(lock: &str) -> Vec<(String, String)> {
        let packages = lock
            .split("\npackages:\n")
            .nth(1)
            .expect("pnpm-lock.yaml has a packages section")
            .split("\nsnapshots:\n")
            .next()
            .expect("packages come before snapshots");
        packages
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .filter(|rest| !rest.starts_with(' ') && rest.ends_with(':'))
            .filter_map(|key| {
                let key = key.trim_end_matches(':').trim_matches('\'');
                // scoped の `@scope/name@1.0.0` は先頭の @ を飛ばして区切る
                let at = key[1..].find('@')? + 1;
                Some((key[..at].to_string(), key[at + 1..].to_string()))
            })
            .collect()
    }

    /// `--license` とメニューはこの文字列をそのまま出すので、見出しと 3 つの節があること。
    #[test]
    fn notices_have_the_heading_and_every_section() {
        let notices = lf(NOTICES);
        assert!(notices.starts_with("THIRD-PARTY NOTICES\n"));
        for heading in ["# JavaScript packages", RUST_HEADING, NATIVE_HEADING] {
            assert!(notices.contains(heading), "missing {heading}");
        }
    }

    /// 直接依存が、Cargo.lock が選んだ version で載っているか。名前だけだと、上げた依存の
    /// 旧 version が推移依存として残っている時に通ってしまう。
    #[test]
    fn third_party_notices_list_every_direct_rust_dependency() {
        // Arrange
        let deps = direct_rust_dependencies(&lf(include_str!("../Cargo.toml")));
        assert!(deps.contains(&"tauri".to_string()), "parsed deps: {deps:?}");
        assert!(deps.contains(&"sqlx".to_string()), "parsed deps: {deps:?}");
        assert!(
            !deps.contains(&"tempfile".to_string()),
            "parsed deps: {deps:?}"
        );
        let lock = locked_rust_packages(&lf(include_str!("../Cargo.lock")));
        let listed = packages_in_notices(&lf(NOTICES), "rust");
        assert!(listed.len() > 100, "parsed notices: {listed:?}");

        // Act
        let missing: Vec<(String, String)> = deps
            .iter()
            .map(|name| (name.clone(), resolved_rust_version(&lock, name)))
            .filter(|entry| !listed.contains(entry))
            .collect();

        // Assert
        assert!(
            missing.is_empty(),
            "not in THIRD-PARTY-NOTICES.txt (run `pnpm notices`): {missing:?}"
        );
    }

    /// 載っている crate の version が Cargo.lock と食い違えば、依存を上げたのに
    /// `pnpm notices` を流していない。native 節の crate も同じ。
    #[test]
    fn third_party_notices_match_cargo_lock_versions() {
        // Arrange
        let lock = locked_rust_packages(&lf(include_str!("../Cargo.lock")));
        let notices = lf(NOTICES);
        let mut listed = packages_in_notices(&notices, "rust");
        assert!(listed.len() > 100, "parsed notices: {listed:?}");
        let native = native_crates_in_notices(&notices);
        assert!(native.len() >= 3, "parsed native entries: {native:?}");
        listed.extend(native);

        // Act
        let stale: Vec<&(String, String)> = listed
            .iter()
            .filter(|(name, version)| !lock.iter().any(|(n, v, _)| n == name && v == version))
            .collect();

        // Assert
        assert!(
            stale.is_empty(),
            "not in Cargo.lock (run `pnpm notices`): {stale:?}"
        );
    }

    /// 直接依存が pnpm-lock.yaml の選んだ version で載っており、載っている全 package が
    /// lock にあるか。notices の npm 側は node_modules の package.json から書くので、
    /// lock を更新して install と再生成を忘れると古い version のまま残る。
    #[test]
    fn third_party_notices_match_pnpm_lock() {
        // Arrange
        let deps = direct_npm_dependencies();
        assert!(
            deps.contains(&"@codemirror/view".to_string()),
            "parsed deps: {deps:?}"
        );
        let lock = lf(include_str!("../../pnpm-lock.yaml"));
        let resolved = resolved_npm_versions(&lock, "dependencies");
        assert_eq!(
            resolved.len(),
            deps.len(),
            "parsed lock importer: {resolved:?}"
        );
        let locked = locked_npm_packages(&lock);
        assert!(locked.len() > 100, "parsed lock packages: {locked:?}");
        let dev = resolved_npm_versions(&lock, "devDependencies");
        assert!(
            dev.iter().any(|(name, _)| name == "svelte"),
            "parsed lock importer: {dev:?}"
        );
        let listed = packages_in_notices(&lf(NOTICES), "npm");

        // Act
        let missing: Vec<&(String, String)> = resolved
            .iter()
            .filter(|entry| !listed.contains(entry))
            .collect();
        // bundle される devDependencies (svelte 等) は、旧 version が別の依存経路で lock に
        // 残っていても通ってしまうので、直接依存としての解決 version と突き合わせる
        let stale_dev: Vec<&(String, String)> = listed
            .iter()
            .filter(|(name, version)| dev.iter().any(|(n, v)| n == name && v != version))
            .collect();
        let unknown: Vec<&(String, String)> = listed
            .iter()
            .filter(|entry| !locked.contains(entry))
            .collect();

        // Assert
        assert!(
            missing.is_empty(),
            "not in THIRD-PARTY-NOTICES.txt (run `pnpm notices`): {missing:?}"
        );
        assert!(
            unknown.is_empty(),
            "not in pnpm-lock.yaml (run `pnpm notices`): {unknown:?}"
        );
        assert!(
            stale_dev.is_empty(),
            "older than the devDependencies in pnpm-lock.yaml (run `pnpm notices`): {stale_dev:?}"
        );
    }

    /// Windows の checkout (CRLF) でも同じ結果になること。
    #[test]
    fn notices_parsers_accept_crlf() {
        // Arrange
        let entry = |used_by: &str| {
            "=".repeat(80) + "\r\nLicense: MIT\r\n\r\nUsed by:\r\n  " + used_by + "\r\n\r\ntext\r\n"
        };
        let notices = "x\r\n".to_string()
            + &entry("yaml 2.9.0 (u)")
            + "# Rust crates\r\n\r\n"
            + &entry("serde 1.0.0 (u)")
            + "# Native libraries\r\n\r\n"
            + &entry("zlib (bundled by libz-sys 1.1.29)");
        let lock = "lockfileVersion: '9.0'\r\nimporters:\r\n  .:\r\n    dependencies:\r\n      yaml:\r\n        specifier: ^2\r\n        version: 2.9.0\r\npackages:\r\n  yaml@2.9.0:\r\n    x: y\r\n  '@a/b@1.0.0':\r\n    x: y\r\nsnapshots:\r\n";

        // Act
        let notices = lf(&notices);
        let lock = lf(lock);

        // Assert
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            packages_in_notices(&notices, "npm"),
            vec![pair("yaml", "2.9.0")]
        );
        assert_eq!(
            packages_in_notices(&notices, "rust"),
            vec![pair("serde", "1.0.0")]
        );
        assert_eq!(
            native_crates_in_notices(&notices),
            vec![pair("libz-sys", "1.1.29")]
        );
        assert_eq!(
            resolved_npm_versions(&lock, "dependencies"),
            vec![pair("yaml", "2.9.0")]
        );
        assert_eq!(
            locked_npm_packages(&lock),
            vec![pair("yaml", "2.9.0"), pair("@a/b", "1.0.0")]
        );
    }

    /// Cargo.toml の複数行にまたがる値 (sqlx の features 等) を依存と取り違えない。
    #[test]
    fn direct_rust_dependencies_skip_continuation_lines() {
        // Arrange
        let manifest = "[package]\nname = \"x\"\n\n[dependencies]\nsqlx = { version = \"0.8\", features = [\n  \"mysql\",\n] }\n# comment = 1\nserde = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n\n[target.'cfg(windows)'.dependencies]\nwindows = \"0.1\"\n";

        // Act
        let deps = direct_rust_dependencies(manifest);

        // Assert
        assert_eq!(deps, vec!["sqlx", "serde", "windows"]);
    }
}
