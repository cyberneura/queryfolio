fn main() {
    emit_app_version();
    tauri_build::build()
}

/// Reads the number that `--version` prints from `tauri.conf.json` and embeds it as
/// `QUERYFOLIO_VERSION`.
///
/// **The Cargo.toml version cannot be used.** The release version is determined by `version`
/// in `src-tauri/tauri.conf.json` (`.github/workflows/release.yml` reads it to create the tag
/// and the Release), and Cargo.toml does not follow it. Printing `CARGO_PKG_VERSION` would make
/// `--version` answer 0.1.0 even when the distributed build is 0.1.4.
///
/// If it cannot be read, **the build fails** (so a wrong number is never embedded silently).
fn emit_app_version() {
    println!("cargo:rerun-if-changed=tauri.conf.json");

    let text = std::fs::read_to_string("tauri.conf.json")
        .expect("src-tauri/tauri.conf.json should be readable");
    let conf: serde_json::Value =
        serde_json::from_str(&text).expect("src-tauri/tauri.conf.json should be valid JSON");
    let version = conf
        .get("version")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .expect("src-tauri/tauri.conf.json should have a non-empty string \"version\"");

    println!("cargo:rustc-env=QUERYFOLIO_VERSION={version}");
}
