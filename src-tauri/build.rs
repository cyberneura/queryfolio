fn main() {
    emit_app_version();
    tauri_build::build()
}

/// `--version` が出す番号を `tauri.conf.json` から取り、`QUERYFOLIO_VERSION` として埋め込む。
///
/// **Cargo.toml の version は使えない。** リリースの版番号は
/// `src-tauri/tauri.conf.json` の `version` で決まり (`.github/workflows/release.yml` が
/// そこを読んでタグと Release を作る)、Cargo.toml の方は追随していない。
/// `CARGO_PKG_VERSION` を出すと、配布物が 0.1.4 でも `--version` は 0.1.0 と答える。
///
/// 読めなければ**ビルドを失敗させる** (取り違えた番号を黙って埋め込まないため)。
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
