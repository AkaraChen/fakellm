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

fn http_post(endpoint: &str, path: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let addr = endpoint.trim_start_matches("http://");
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    write!(
        conn,
        "POST {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    conn.read_to_string(&mut out).unwrap();
    out
}

fn spawn_with(args: &[&str]) -> (Kill, String) {
    let mut child = Kill(
        Command::new(env!("CARGO_BIN_EXE_fakellm"))
            .args(["--port", "0"])
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(child.0.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    (child, line.trim().to_owned())
}

#[test]
fn policy_and_trajectory_files_load_at_start() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skip: node not on PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("fakellm-bin-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let policy = dir.join("greeter.mjs");
    std::fs::write(&policy, "export default () => ({ text: 'hello from policy file' });").unwrap();
    let traj = dir.join("walk.json");
    std::fs::write(
        &traj,
        r#"{"name": "walk", "steps": [{"reply": {"text": "hello from trajectory file"}}]}"#,
    )
    .unwrap();
    let rules = dir.join("rules.json");
    std::fs::write(
        &rules,
        r#"[{"match": {"last_user": "^traj"}, "reply": {"trajectory": "walk"}}, {"reply": {"policy": "greeter"}}]"#,
    )
    .unwrap();

    let (_child, endpoint) = spawn_with(&[
        "--policy",
        policy.to_str().unwrap(),
        "--policy-runtime",
        "node",
        "--trajectory",
        traj.to_str().unwrap(),
        "--scripts",
        rules.to_str().unwrap(),
    ]);
    let body = |u: &str| format!(r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"{u}"}}]}}"#);
    let r = http_post(&endpoint, "/v1/chat/completions", &body("anything"));
    assert!(r.contains("hello from policy file"), "{r}");
    let r = http_post(&endpoint, "/v1/chat/completions", &body("traj please"));
    assert!(r.contains("hello from trajectory file"), "{r}");

    let status = Command::new(env!("CARGO_BIN_EXE_fakellm"))
        .args(["ctl", "--endpoint", &endpoint, "--json", "status"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        (v["policies"]["count"].as_u64(), v["trajectories"]["count"].as_u64()),
        (Some(1), Some(1))
    );

    let broken = dir.join("broken.mjs");
    std::fs::write(&broken, "export default (").unwrap();
    let out = run(&["--port", "0", "--policy", broken.to_str().unwrap(), "--policy-runtime", "node"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("broken"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
