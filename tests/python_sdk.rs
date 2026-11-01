//! Runs the official Python `openai` and `anthropic` client suite in `sdk/python` (via `uv run pytest`)
//! against the freshly built binary. Skipped when `uv` is not installed and there is no
//! `sdk/python/.venv`; with `CI=true` a missing prerequisite is a failure.

use std::path::Path;
use std::process::Command;

fn skip(why: &str) {
    assert!(std::env::var("CI").as_deref() != Ok("true"), "CI=true but {why}");
    eprintln!("skip: {why}");
}

#[test]
fn official_python_sdks() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/python");
    let has_venv = dir.join(".venv").exists();
    let has_uv = Command::new("uv").arg("--version").output().is_ok();
    if !has_uv && !has_venv {
        skip("neither uv nor sdk/python/.venv is available");
        return;
    }
    let mut cmd = if has_uv {
        let mut c = Command::new("uv");
        c.args(["run", "--frozen", "pytest", "-q"]);
        c
    } else {
        let mut c = Command::new(dir.join(".venv/bin/python"));
        c.args(["-m", "pytest", "-q"]);
        c
    };
    let out = cmd
        .current_dir(&dir)
        .env("FAKELLM_BIN", env!("CARGO_BIN_EXE_fakellm"))
        .output()
        .expect("spawn pytest");
    assert!(
        out.status.success(),
        "pytest failed\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
