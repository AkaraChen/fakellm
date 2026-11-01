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

#[cfg(feature = "tjs")]
fn http_get(endpoint: &str, path: &str) -> String {
    use std::io::{Read, Write};
    let addr = endpoint.trim_start_matches("http://");
    let mut conn = std::net::TcpStream::connect(addr).unwrap();
    write!(conn, "GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    conn.read_to_string(&mut out).unwrap();
    out
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
    spawn_with_env(args, &[])
}

fn spawn_with_env(args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> (Kill, String) {
    let mut child = Kill(
        Command::new(env!("CARGO_BIN_EXE_fakellm"))
            .args(["--port", "0"])
            .args(args)
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(child.0.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    (child, line.trim().to_owned())
}

fn have_node() -> bool {
    let ok = Command::new("node").arg("--version").output().is_ok();
    if !ok {
        eprintln!("skip: node not on PATH");
    }
    ok
}

/// With no `--policy-runtime`: the embedded runtime, or `node` in a build without the `tjs` feature.
#[test]
fn policy_and_trajectory_files_load_at_start() {
    if cfg!(feature = "tjs") || have_node() {
        policy_and_trajectory_files(None);
    }
}

#[test]
fn policy_and_trajectory_files_load_at_start_on_node() {
    if have_node() {
        policy_and_trajectory_files(Some("node"));
    }
}

fn policy_and_trajectory_files(runtime: Option<&str>) {
    let dir = std::env::temp_dir().join(format!("fakellm-bin-test-{}-{}", std::process::id(), runtime.unwrap_or("default")));
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

    let runtime_args: Vec<&str> = runtime.map(|r| vec!["--policy-runtime", r]).unwrap_or_default();
    let (_child, endpoint) = spawn_with(
        &[
            &["--policy", policy.to_str().unwrap()][..],
            &runtime_args,
            &["--trajectory", traj.to_str().unwrap(), "--scripts", rules.to_str().unwrap()],
        ]
        .concat(),
    );
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
    let out = run(&[&["--port", "0", "--policy", broken.to_str().unwrap()][..], &runtime_args].concat());
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("broken"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn ctl_stdin(endpoint: &str, args: &[&str], stdin: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_fakellm"))
        .args(["ctl", "--endpoint", endpoint])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn ctl_trajectories_policies_and_stdin() {
    let (_child, endpoint) = spawn_with(&[]);
    let ok = |out: std::process::Output| {
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    // Scripts from stdin, JSON and YAML.
    let out = ok(ctl_stdin(
        &endpoint,
        &["scripts", "put", "-"],
        r#"[{"reply": {"text": "from stdin"}}]"#,
    ));
    assert!(out.contains("1 rules loaded"), "{out}");
    let out = ok(ctl_stdin(&endpoint, &["scripts", "add", "-"], "- reply:\n    text: yaml too\n"));
    assert!(out.contains("(2 total)"), "{out}");
    // Trajectories: put/add from stdin, list, get, delete, clear.
    let out = ok(ctl_stdin(
        &endpoint,
        &["trajectories", "put", "-"],
        r#"{"name": "walk", "steps": [{"reply": {"text": "hi"}}]}"#,
    ));
    assert!(out.contains("1 trajectories loaded"), "{out}");
    ok(ctl_stdin(
        &endpoint,
        &["trajectories", "add", "-"],
        "{\"kind\":\"trajectory\",\"name\":\"two\"}\n{\"reply\":{\"text\":\"yo\"}}\n",
    ));
    let out = ok(ctl_stdin(&endpoint, &["trajectories", "list"], ""));
    assert!(out.contains("walk") && out.contains("two"), "{out}");
    let out = ok(ctl_stdin(&endpoint, &["trajectories", "get", "walk"], ""));
    assert!(out.contains("\"name\": \"walk\""), "{out}");
    let out = ok(ctl_stdin(&endpoint, &["trajectories", "get", "walk", "--jsonl"], ""));
    assert!(out.lines().count() >= 2, "{out}");
    ok(ctl_stdin(&endpoint, &["trajectories", "delete", "two"], ""));
    let out = ctl_stdin(&endpoint, &["trajectories", "delete", "two"], "");
    assert!(!out.status.success());
    ok(ctl_stdin(&endpoint, &["trajectories", "clear"], ""));
    assert!(ok(ctl_stdin(&endpoint, &["trajectories", "list"], "")).contains("no trajectories"));

    // Policies need a JS runtime: the embedded one, or node in a build without it.
    if !cfg!(feature = "tjs") && Command::new("node").arg("--version").output().is_err() {
        eprintln!("skip policies: node not on PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("fakellm-bin-ctl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let js = dir.join("greeter.mjs");
    std::fs::write(&js, "export default () => ({ text: 'hello from ctl policy' });").unwrap();
    let out = ok(ctl_stdin(&endpoint, &["policies", "put", js.to_str().unwrap()], ""));
    assert!(out.contains("1 policies loaded"), "{out}");
    ok(ctl_stdin(
        &endpoint,
        &["policies", "add", "-", "--name", "second"],
        "export default () => null;",
    ));
    let out = ok(ctl_stdin(&endpoint, &["policies", "list"], ""));
    assert!(out.contains("greeter") && out.contains("second"), "{out}");
    ok(ctl_stdin(
        &endpoint,
        &["scripts", "put", "-"],
        r#"[{"reply": {"policy": "greeter"}}]"#,
    ));
    let r = http_post(
        &endpoint,
        "/v1/chat/completions",
        r#"{"model":"gpt-4o","messages":[{"role":"user","content":"x"}]}"#,
    );
    assert!(r.contains("hello from ctl policy"), "{r}");
    let out = ok(ctl_stdin(&endpoint, &["policies", "get", "greeter"], ""));
    assert!(out.contains("\"hits\""), "{out}");
    let bad = ctl_stdin(&endpoint, &["policies", "add", "-", "--name", "broken"], "export default (");
    assert!(!bad.status.success());
    ok(ctl_stdin(&endpoint, &["policies", "delete", "second"], ""));
    ok(ctl_stdin(&endpoint, &["policies", "clear"], ""));
    assert!(ok(ctl_stdin(&endpoint, &["policies", "list"], "")).contains("no policies"));
    let help = ok(run(&["ctl", "--help"]));
    for word in ["trajectories", "policies", "cassettes", "snapshot"] {
        assert!(help.contains(word), "{word}: {help}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory that exists and holds nothing, as the whole of PATH.
fn empty_path() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fakellm-bin-empty-path-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A fresh directory for a test's files (`greeter.mjs` names the policy `greeter`).
fn files_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fakellm-bin-files-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(feature = "tjs")]
#[test]
fn policies_run_with_no_javascript_runtime_on_path() {
    let path = empty_path();
    let files = files_dir("nopath");
    let js = files.join("greeter.mjs");
    std::fs::write(
        &js,
        "export default (req, ctx) => ({ text: `no node here, hits=${ctx.hits}`, state: { seen: true } });",
    )
    .unwrap();
    let rules = files.join("rules.json");
    std::fs::write(&rules, r#"[{"reply": {"policy": "greeter"}}]"#).unwrap();
    // The binary is started by absolute path, and nothing it could exec is on PATH.
    for runtime_args in [&[][..], &["--policy-runtime", "tjs"]] {
        let args = [
            &["--policy", js.to_str().unwrap(), "--scripts", rules.to_str().unwrap()][..],
            runtime_args,
        ]
        .concat();
        let (_child, endpoint) = spawn_with_env(&args, &[("PATH", path.as_os_str())]);
        let body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let r = http_post(&endpoint, "/v1/chat/completions", body);
        assert!(r.contains("no node here, hits=0"), "{runtime_args:?}: {r}");
        let r = http_post(&endpoint, "/v1/chat/completions", body);
        assert!(r.contains("no node here, hits=1"), "{runtime_args:?}: {r}");
        let policies = http_get(&endpoint, "/_fakellm/policies");
        assert!(
            policies.contains(r#""runtime":"tjs""#) && policies.contains(r#""in_process":true"#),
            "{policies}"
        );
    }
    let _ = std::fs::remove_dir_all(&files);
}

#[cfg(feature = "tjs")]
#[test]
fn the_server_exits_promptly_on_sigterm_with_policies_running() {
    let files = files_dir("sigterm");
    let js = files.join("sleeper.mjs");
    std::fs::write(
        &js,
        "export default async () => { await new Promise((r) => setTimeout(r, 60000)); return { text: 'late' }; };",
    )
    .unwrap();
    let rules = files.join("rules.json");
    std::fs::write(&rules, r#"[{"reply": {"policy": "sleeper"}}]"#).unwrap();
    let (mut child, endpoint) = spawn_with(&["--policy", js.to_str().unwrap(), "--scripts", rules.to_str().unwrap()]);
    // A decision is in flight when the signal arrives.
    let addr = endpoint.clone();
    std::thread::spawn(move || {
        http_post(
            &addr,
            "/v1/chat/completions",
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let started = std::time::Instant::now();
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let status = child.0.wait().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(status.success() || status.code().is_none(), "{status:?}");
    let _ = std::fs::remove_dir_all(&files);
}

/// Naming a subprocess runtime that is not installed is an error that says so.
#[test]
fn a_subprocess_runtime_missing_from_path_fails_clearly() {
    let path = empty_path();
    let files = files_dir("missing");
    let js = files.join("p.mjs");
    std::fs::write(&js, "export default () => null;").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_fakellm"))
        .args(["--port", "0", "--policy", js.to_str().unwrap(), "--policy-runtime", "node"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot start node"), "{err}");
    let _ = std::fs::remove_dir_all(&files);
}

/// A build without the embedded runtime has nothing to default to when no node or bun is on PATH.
#[cfg(not(feature = "tjs"))]
#[test]
fn policy_without_runtime_fails_clearly() {
    let path = empty_path();
    let files = files_dir("noruntime");
    let js = files.join("q.mjs");
    std::fs::write(&js, "export default () => null;").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_fakellm"))
        .args(["--port", "0", "--policy", js.to_str().unwrap()])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("JavaScript runtime"), "{err}");
    let _ = std::fs::remove_dir_all(&files);
}
