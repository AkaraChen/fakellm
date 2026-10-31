//! Runs the real binary: it must print its endpoint, and `ctl status` must reach it.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

struct Kill(std::process::Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn prints_endpoint_and_serves_ctl() {
    let exe = env!("CARGO_BIN_EXE_fakellm");
    let mut child = Kill(Command::new(exe).args(["--port", "0"]).stdout(Stdio::piped()).spawn().unwrap());
    let mut line = String::new();
    BufReader::new(child.0.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    let endpoint = line.trim();
    let port: u16 = endpoint
        .strip_prefix("http://127.0.0.1:")
        .unwrap_or_else(|| panic!("unexpected output {line:?}"))
        .parse()
        .unwrap();
    assert_ne!(port, 0);

    let out = Command::new(exe).args(["ctl", "--endpoint", endpoint, "status"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!out.stdout.is_empty());

    let json = Command::new(exe)
        .args(["ctl", "--endpoint", endpoint, "--json", "status"])
        .output()
        .unwrap();
    assert!(json.status.success(), "{}", String::from_utf8_lossy(&json.stderr));
    serde_json::from_slice::<serde_json::Value>(&json.stdout).unwrap();
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_fakellm")).args(args).output().unwrap()
}

#[test]
fn wire_codex_prints_exports_and_config() {
    let out = run(&["wire", "codex", "--endpoint", "http://127.0.0.1:4010/v1"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("export FAKELLM_API_KEY='fakellm'\n"), "{text}");
    assert!(text.contains("# config.toml\nmodel = \"gpt-5.5\""));
    assert!(text.contains("base_url = \"http://127.0.0.1:4010/v1\""));
    assert!(text.contains("-c model_provider=fakellm"));

    let out = run(&["wire", "codex", "--format", "env"]);
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "FAKELLM_API_KEY=fakellm\n");
    let out = run(&["wire", "pi", "--format", "json"]);
    serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap();
}

#[test]
fn wire_usage_errors_exit_2() {
    for args in [
        &["wire", "nope"][..],
        &["wire", "claude", "--endpoint", "nope"],
        &["wire", "claude", "--format", "toml"],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(!out.stderr.is_empty());
    }
}

#[test]
fn env_vars_and_flag_errors() {
    let exe = env!("CARGO_BIN_EXE_fakellm");
    let out = Command::new(exe)
        .args(["--port", "0"])
        .env("FAKELLM_STRICT", "1")
        .env("FAKELLM_SCRIPTS", "/nonexistent/x.json")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent/x.json"));
    let out = Command::new(exe).args(["--chunk-tokens", "0"]).output().unwrap();
    assert!(!out.status.success());
    let out = Command::new(exe)
        .args(["--port", "0", "--ttft", "1.5s"])
        .env("FAKELLM_STRICT", "yes")
        .env("FAKELLM_NO_REQUEST_LOG", "off")
        .env("FAKELLM_SCRIPTS", "/nonexistent/y.json")
        .output()
        .unwrap();
    // The failure is the missing scripts file, not a rejected boolean or duration.
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent/y.json"));
}
