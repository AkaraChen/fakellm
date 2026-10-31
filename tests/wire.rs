//! `fakellm::wire`, library level: every target, endpoint handling, formats. The TOML is checked
//! with string asserts because no TOML crate is a dependency.

use fakellm::wire::{self, Format, TARGETS};
use serde_json::Value;

const E: &str = "http://127.0.0.1:4010";

#[test]
fn every_target_builds_and_mentions_the_endpoint() {
    for t in TARGETS {
        let out = wire::wire(t, "http://10.0.0.5:9/v1/").unwrap();
        let all = format!("{:?}{:?}{}", out.env, out.files, out.snippet);
        assert!(all.contains("http://10.0.0.5:9"), "{t}");
        assert!(!all.contains("/v1/v1"), "{t}");
        for f in [Format::Json, Format::Shell] {
            wire::format(&out, f).unwrap();
        }
    }
}

#[test]
fn codex_toml_has_the_wire_settings() {
    let toml = wire::format(&wire::wire("codex", E).unwrap(), Format::Toml).unwrap();
    for line in [
        "model_provider = \"fakellm\"",
        "[model_providers.fakellm]",
        "base_url = \"http://127.0.0.1:4010/v1\"",
        "wire_api = \"responses\"",
        "request_max_retries = 0",
        "stream_max_retries = 0",
        "stream_idle_timeout_ms = 5000",
    ] {
        assert!(toml.lines().any(|l| l == line), "missing {line}");
    }
}

#[test]
fn json_format_round_trips() {
    let out = wire::wire("opencode", E).unwrap();
    let v: Value = serde_json::from_str(&wire::format(&out, Format::Json).unwrap()).unwrap();
    let file: Value = serde_json::from_str(v["files"]["opencode.json"].as_str().unwrap()).unwrap();
    assert_eq!(file["provider"]["fakellm"]["options"]["baseURL"], "http://127.0.0.1:4010/v1");
    assert!(v["env"]["OPENCODE_CONFIG_CONTENT"].is_string());
}

#[test]
fn errors() {
    assert!(wire::wire("emacs", E).unwrap_err().contains("claude"));
    assert!(wire::wire("claude", "127.0.0.1:4010").is_err());
    assert!(wire::format(&wire::wire("claude", E).unwrap(), Format::Toml).is_err());
    assert!("xml".parse::<Format>().is_err());
}
