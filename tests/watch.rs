//! Hot reload of the startup files (docs/m4-spec.md section 9 and 11).

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fakellm::{Cassette, Config, PolicySpec, Rule, Server, WatchConfig};
use serde_json::{Value, json};

mod common;
use common::*;

fn rules_json(text: &str) -> String {
    json!([{"name": "r", "reply": {"text": text}}]).to_string()
}

fn rules_of(text: &str) -> Vec<Rule> {
    serde_json::from_str(text).unwrap()
}

async fn watched(path: &Path, initial: &str) -> Server {
    std::fs::write(path, initial).unwrap();
    start_with(Config {
        scripts: rules_of(initial),
        watch: Some(WatchConfig {
            scripts: Some(path.to_owned()),
            ..WatchConfig::default()
        }),
        ..Config::default()
    })
    .await
}

async fn says(s: &Server) -> String {
    chat_text(&chat(s, &chat_body("gpt-4o", "hi")).await)
}

async fn wait_for_text(s: &Server, want: &str, secs: f64) -> bool {
    eventually(secs, || async { (says(s).await == want).then_some(()) }).await.is_some()
}

#[tokio::test]
async fn editing_the_scripts_file_shows_the_new_rule_within_two_seconds() {
    let path = temp_path("rules.json");
    let s = watched(&path, &rules_json("old")).await;
    assert_eq!(says(&s).await, "old");
    // Let the baseline be taken, then edit (a different length also changes the stamp on coarse clocks).
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&path, rules_json("a new and longer answer")).unwrap();
    assert!(wait_for_text(&s, "a new and longer answer", 2.0).await, "no reload within 2 s");
    let listed = get(&s, "/_fakellm/scripts").await.json();
    assert_eq!(listed["rules"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_broken_rewrite_keeps_the_old_rule_and_a_fix_is_picked_up() {
    let path = temp_path("rules.json");
    let s = watched(&path, &rules_json("old")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&path, "[{\"reply\": {\"text\": ").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(says(&s).await, "old");
    std::fs::write(&path, rules_json("fixed after a mistake")).unwrap();
    assert!(wait_for_text(&s, "fixed after a mistake", 2.5).await);
}

#[tokio::test]
async fn without_watch_config_nothing_reloads() {
    let path = temp_path("rules.json");
    std::fs::write(&path, rules_json("old")).unwrap();
    let s = start_with(Config {
        scripts: rules_of(&rules_json("old")),
        watch: None,
        ..Config::default()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&path, rules_json("changed on disk")).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(says(&s).await, "old");
}

#[tokio::test]
async fn a_cassette_file_reloads() {
    let path = temp_path("tape.jsonl");
    let write = |text: &str| {
        let doc = format!("{}\n{}\n", json!({"kind": "cassette", "name": "tape"}), exchange("hi", text));
        std::fs::write(&path, doc).unwrap();
        doc_cassette(&path)
    };
    let c = write("first");
    let s = start_with(Config {
        cassettes: vec![c],
        watch: Some(WatchConfig {
            cassettes: vec![path.clone()],
            ..WatchConfig::default()
        }),
        ..Config::default()
    })
    .await;
    assert_eq!(says(&s).await, "first");
    tokio::time::sleep(Duration::from_millis(100)).await;
    write("second, rewritten");
    assert!(wait_for_text(&s, "second, rewritten", 2.5).await);
}

fn doc_cassette(path: &Path) -> Cassette {
    Cassette::parse_many(&std::fs::read_to_string(path).unwrap(), fakellm::CassetteFormat::Jsonl)
        .unwrap()
        .remove(0)
}

#[tokio::test]
async fn requests_during_a_reload_see_the_old_or_the_new_answer_never_an_error() {
    let rules_path = temp_path("rules.json");
    let s = watched(&rules_path, &rules_json("version-0")).await;
    let stop = Arc::new(AtomicBool::new(false));
    let addr_server = &s;
    let flag = stop.clone();
    let hammer = async move {
        let mut seen = Vec::new();
        while !flag.load(Ordering::Relaxed) {
            let r = chat(addr_server, &chat_body("gpt-4o", "hi")).await;
            assert_eq!(r.status, 200, "a request failed during reload: {}", r.body);
            seen.push(chat_text(&r));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        seen
    };
    let editor = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        for v in 1..=3 {
            std::fs::write(&rules_path, rules_json(&format!("version-{v}"))).unwrap();
            tokio::time::sleep(Duration::from_millis(900)).await;
        }
        stop.store(true, Ordering::Relaxed);
    };
    let (seen, ()) = tokio::join!(hammer, editor);
    assert!(seen.len() > 20, "only {} requests", seen.len());
    for text in &seen {
        assert!(
            ["version-0", "version-1", "version-2", "version-3"].contains(&text.as_str()),
            "{text}"
        );
    }
    assert_eq!(seen.last().map(String::as_str), Some("version-3"));
    // Versions only move forward.
    let versions: Vec<u32> = seen.iter().map(|t| t.trim_start_matches("version-").parse().unwrap()).collect();
    assert!(versions.windows(2).all(|w| w[0] <= w[1]), "{versions:?}");
}

#[tokio::test]
async fn a_control_plane_put_racing_a_reload_ends_in_one_state() {
    let path = temp_path("rules.json");
    let s = watched(&path, &rules_json("file-0")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&path, json!([{"name": "from-file", "reply": {"text": "from file"}}]).to_string()).unwrap();
    // Hammer PUTs while the poller picks the file up.
    for _ in 0..30 {
        put_rules(&s, json!([{"name": "from-put", "reply": {"text": "from put"}}])).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let names: Vec<Value> = get(&s, "/_fakellm/scripts").await.json()["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].clone())
        .collect();
    assert_eq!(names.len(), 1, "a merge of both: {names:?}");
    assert!(names[0] == "from-file" || names[0] == "from-put", "{names:?}");
}

#[tokio::test]
async fn a_policy_reload_fails_only_the_in_flight_decisions() {
    if !cfg!(feature = "tjs") && Command::new("node").arg("--version").output().is_err() {
        eprintln!("skip: node not on PATH");
        return;
    }
    let dir = temp_path("policies");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("slow.mjs");
    let slow = "export default async function decide() { await new Promise((r) => setTimeout(r, 2500)); return { text: 'slow' }; }";
    let quick = "export default function decide() { return { text: 'quick' }; }";
    std::fs::write(&path, slow).unwrap();
    let spec = PolicySpec::from_path(&path).unwrap();
    let s = start_with(Config {
        policies: vec![spec],
        policy_timeout_ms: 10_000,
        scripts: rules_of(&json!([{"reply": {"policy": "slow"}}]).to_string()),
        watch: Some(WatchConfig {
            policies: vec![path.clone()],
            ..WatchConfig::default()
        }),
        ..Config::default()
    })
    .await;
    let body = chat_body("gpt-4o", "hi");
    let in_flight = chat(&s, &body);
    let rewrite = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(&path, quick).unwrap();
    };
    let (r, ()) = tokio::join!(in_flight, rewrite);
    assert_eq!(r.status, 500, "{}", r.body);
    assert_eq!(r.json()["error"]["code"], "fakellm_policy_crashed");
    assert!(wait_for_text(&s, "quick", 3.0).await, "the new policy never answered");
}

// ---- the binary's flags ----

struct Kill(std::process::Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_with_scripts(path: &Path, extra: &[&str]) -> (Kill, String) {
    let mut child = Kill(
        Command::new(env!("CARGO_BIN_EXE_fakellm"))
            .args(["--port", "0", "--scripts", path.to_str().unwrap()])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(child.0.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    (child, line.trim().to_owned())
}

async fn ask_endpoint(endpoint: &str) -> String {
    let url = endpoint.trim_end_matches('/').to_owned();
    let port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
    let body = chat_body("gpt-4o", "hi").to_string();
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("host", "x")
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let bytes = http_body_util::BodyExt::collect(resp.into_body()).await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn the_binary_reloads_scripts_unless_no_watch() {
    for (flags, reloads) in [(vec![], true), (vec!["--no-watch"], false)] {
        let path = temp_path("rules.json");
        std::fs::write(&path, rules_json("old")).unwrap();
        let (_child, endpoint) = spawn_with_scripts(&path, &flags);
        assert_eq!(ask_endpoint(&endpoint).await, "old");
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&path, rules_json("brand new answer")).unwrap();
        let got = eventually(if reloads { 3.0 } else { 1.5 }, || async {
            (ask_endpoint(&endpoint).await == "brand new answer").then_some(())
        })
        .await;
        assert_eq!(got.is_some(), reloads, "flags {flags:?}");
    }
}
