//! Runs the official-SDK suite in `sdk/node` (openai, Vercel AI SDK) against the freshly built binary.
//! Skipped when `node_modules` is missing or `node` is not installed.

use std::path::Path;
use std::process::Command;

#[test]
fn official_node_sdks() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/node");
    if !dir.join("node_modules").exists() {
        eprintln!("skip: sdk/node/node_modules missing (run `npm ci` in sdk/node)");
        return;
    }
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skip: node not on PATH");
        return;
    }
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let out = Command::new(npm)
        .args(["test"])
        .current_dir(&dir)
        .env("FAKELLM_BIN", env!("CARGO_BIN_EXE_fakellm"))
        .output()
        .expect("spawn npm");
    assert!(
        out.status.success(),
        "npm test failed\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
