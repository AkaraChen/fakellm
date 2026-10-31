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
