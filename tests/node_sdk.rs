//! Runs the official-SDK suite in `sdk/node` (openai, Anthropic and Vercel AI SDK suites: chat, Responses, Messages) against the freshly built binary.
//! Skipped when `node_modules` or the built `sdk/js/dist` is missing, or `node` is not installed; under `CI=true`
//! (CI runs `npm ci` first) any of those is a failure instead.

use std::path::Path;
use std::process::Command;

/// Skips locally, fails in CI: a green run there must not mean the suite did not run.
fn unavailable(why: &str) {
    assert!(!std::env::var("CI").is_ok_and(|v| v == "true"), "CI=true and {why}");
    eprintln!("skip: {why}");
}

#[test]
fn official_node_sdks() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/node");
    if !dir.join("node_modules").exists() {
        unavailable("sdk/node/node_modules missing (run `npm ci` in sdk/node)");
        return;
    }
    if !Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/js/dist/index.js").exists() {
        unavailable("sdk/js/dist missing (run `npm ci && npm run build` in sdk/js)");
        return;
    }
    if Command::new("node").arg("--version").output().is_err() {
        unavailable("node not on PATH");
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
