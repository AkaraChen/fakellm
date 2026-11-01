//! Hugging Face import: the canonical fixtures (docs/m4-spec.md section 6.5) through the converters and the CLI, outputs
//! driving a server, and `fetch_rows` against a local datasets-server.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fakellm::import::hf::{self, Source};
use fakellm::import::{BuildOptions, Conversation, Format, convert_rows, detect, to_cassette, to_trajectories};
use fakellm::ir::Role;
use fakellm::{Cassette, CassetteFormat, HttpClient, Trajectory, TrajectoryFormat};
use serde_json::{Value, json};

mod common;
use common::*;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hf").join(name)
}

fn rows(name: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(fixture(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    let rows: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 3, "{name} holds exactly three rows");
    assert!(
        rows.iter().all(|r| r["_comment"] == "synthetic"),
        "{name}: rows are marked synthetic"
    );
    rows
}

fn opts(name: &str) -> BuildOptions {
    BuildOptions {
        name: name.into(),
        source: format!("file:{name}"),
        model: "imported".into(),
    }
}

fn roles(c: &Conversation) -> Vec<Role> {
    c.messages.iter().map(|m| m.role).collect()
}

fn fakellm(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fakellm")).args(args).output().unwrap()
}

/// Runs `fakellm import file FIXTURE --format F --as AS -o OUT` and returns the output file's text and stderr.
fn import_file(fixture_name: &str, format: &str, output: &str) -> (String, String) {
    let out = temp_path("out.jsonl");
    let res = fakellm(&[
        "import",
        "file",
        fixture(fixture_name).to_str().unwrap(),
        "--format",
        format,
        "--as",
        output,
        "-o",
        out.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&res.stderr).into_owned();
    assert!(res.status.success(), "{fixture_name} {format} {output}: {stderr}");
    (std::fs::read_to_string(&out).unwrap(), stderr)
}

fn cassette_of(text: &str) -> Cassette {
    let mut all = Cassette::parse_many(text, CassetteFormat::Auto).unwrap();
    assert_eq!(all.len(), 1);
    let c = all.remove(0);
    c.validate().unwrap();
    c
}

fn trajectories_of(text: &str) -> Vec<Trajectory> {
    let all = Trajectory::parse_many(text, TrajectoryFormat::Auto).unwrap();
    for t in &all {
        t.validate().unwrap();
    }
    all
}

// ---- converters on the fixtures ----

#[test]
fn sharegpt() {
    let conv = convert_rows(&rows("sharegpt.jsonl"), Format::ShareGpt);
    assert_eq!((conv.conversations.len(), conv.skipped), (2, 1));
    assert_eq!(conv.reasons.get("no assistant turn"), Some(&1));
    let c = &conv.conversations[0];
    assert_eq!(c.id, "sg-1");
    assert_eq!(roles(c), [Role::System, Role::User, Role::Assistant]);
    assert_eq!(c.messages[0].text_content(), "Be brief.");
    assert_eq!(c.messages[1].text_content(), "Hi");
    assert_eq!(c.messages[2].text_content(), "Hello!");
    assert_eq!(conv.conversations[1].id, "sg-2");
    assert_eq!(
        roles(&conv.conversations[1]),
        [Role::User, Role::Assistant, Role::User, Role::Assistant]
    );
    assert_eq!(convert_rows(&rows("sharegpt.jsonl"), Format::Auto).conversations.len(), 2);
}

#[test]
fn wildchat() {
    let conv = convert_rows(&rows("wildchat.jsonl"), Format::WildChat);
    assert_eq!((conv.conversations.len(), conv.skipped), (2, 1));
    let c = &conv.conversations[0];
    assert_eq!(c.id, "abc123");
    assert_eq!(c.model.as_deref(), Some("gpt-4"));
    assert_eq!(roles(c), [Role::User, Role::Assistant]);
    assert_eq!(c.meta["language"], "English");
    assert_eq!(c.meta["toxic"], false);
    assert_eq!(c.meta["redacted"], false);
    assert!(c.meta.contains_key("timestamp"));
    assert_eq!(conv.conversations[1].id, "abc124");
    assert_eq!(convert_rows(&rows("wildchat.jsonl"), Format::Auto).conversations.len(), 2);
}

#[test]
fn hermes_tool_calls_pair_with_their_results() {
    let r = rows("hermes.jsonl");
    assert_eq!(detect(&r[0]), Some(Format::HermesFc));
    let conv = convert_rows(&r, Format::HermesFc);
    assert_eq!((conv.conversations.len(), conv.skipped), (2, 1));
    let c = &conv.conversations[0];
    assert_eq!(roles(c), [Role::System, Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
    assert_eq!(c.messages[0].text_content(), "You are a function calling AI.");
    let call = &c.messages[2].tool_calls[0];
    assert_eq!((call.id.as_str(), call.name.as_str()), ("call_1", "get_weather"));
    assert_eq!(serde_json::from_str::<Value>(&call.arguments).unwrap(), json!({"city": "Paris"}));
    assert_eq!(c.messages[2].text_content(), "");
    assert_eq!(c.messages[3].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(c.messages[3].text_content(), "{\"temp\": 18}");
    assert_eq!(c.messages[4].text_content(), "It is 18 C in Paris.");
    assert_eq!(c.tools.len(), 1);
    assert_eq!(c.tools[0].name, "get_weather");
    assert_eq!(c.meta["category"], "Weather");

    // Row 2: Python-dict quoting and a <think> block.
    let c2 = &conv.conversations[1];
    assert_eq!(c2.id, "h2");
    let call = &c2.messages.iter().find(|m| !m.tool_calls.is_empty()).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&call.tool_calls[0].arguments).unwrap(),
        json!({"city": "Rome"})
    );
    assert_eq!(call.reasoning.as_deref(), Some("plan"));
    assert_eq!(convert_rows(&r, Format::Auto).conversations.len(), 2);
}

#[test]
fn swe_agent_nebius_and_swe_smith() {
    let r = rows("swe_agent.jsonl");
    let conv = convert_rows(&r, Format::SweAgent);
    assert_eq!((conv.conversations.len(), conv.skipped), (2, 1));
    let c = &conv.conversations[0];
    assert_eq!(c.id, "o__r-1");
    assert_eq!(roles(c), [Role::System, Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
    assert_eq!(c.messages[2].text_content(), "Let me look.");
    let first = &c.messages[2].tool_calls[0];
    assert_eq!((first.id.as_str(), first.name.as_str()), ("call_1", "bash"));
    assert_eq!(serde_json::from_str::<Value>(&first.arguments).unwrap(), json!({"command": "ls"}));
    assert_eq!(c.messages[3].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(c.messages[3].text_content(), "a.py b.py");
    assert_eq!(c.messages[4].text_content(), "Done.");
    assert_eq!(c.messages[4].tool_calls[0].id, "call_2");
    assert_eq!(c.meta["instance_id"], "o__r-1");
    assert_eq!(c.meta["model_name"], "swe-agent-llama-70b");
    assert_eq!(c.meta["exit_status"], "submitted");
    assert_eq!(c.tool_map, fakellm::import::swe_agent::default_tool_map());

    // SWE-smith: ids kept.
    let smith = &conv.conversations[1];
    assert_eq!(smith.messages[2].tool_calls[0].id, "c1");
    assert_eq!(smith.messages[3].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(smith.messages[1].text_content(), "Fix it");
    assert_eq!(convert_rows(&r, Format::Auto).conversations.len(), 2);
}

#[test]
fn openhands() {
    let r = rows("openhands.jsonl");
    let conv = convert_rows(&r, Format::OpenHands);
    assert_eq!((conv.conversations.len(), conv.skipped), (2, 1));
    let c = &conv.conversations[0];
    assert_eq!(c.id, "oh-1");
    assert_eq!(roles(c), [Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
    assert_eq!(c.messages[0].text_content(), "List files");
    let call = &c.messages[1].tool_calls[0];
    assert_eq!((call.id.as_str(), call.name.as_str()), ("call_1", "execute_bash"));
    assert_eq!(serde_json::from_str::<Value>(&call.arguments).unwrap(), json!({"command": "ls"}));
    assert_eq!(c.messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(c.messages[2].text_content(), "a.py");
    assert_eq!(c.messages[3].text_content(), "Done");
    assert_eq!(convert_rows(&r, Format::Auto).conversations.len(), 2);
}

#[test]
fn a_row_of_no_known_shape_is_skipped_with_a_reason() {
    let conv = convert_rows(&[json!({"foo": 1})], Format::Auto);
    assert_eq!((conv.conversations.len(), conv.skipped), (0, 1));
    assert_eq!(conv.reasons.get("unrecognized row shape"), Some(&1));
}

// ---- through the CLI ----

#[test]
fn cli_cassettes_have_the_expected_exchange_counts() {
    for (file, format, exchanges) in [
        ("sharegpt.jsonl", "sharegpt", 3),
        ("wildchat.jsonl", "wildchat", 3),
        ("hermes.jsonl", "hermes-fc", 4),
        ("swe_agent.jsonl", "swe-agent", 3),
        ("openhands.jsonl", "openhands", 3),
        ("sharegpt.jsonl", "auto", 3),
    ] {
        let (text, stderr) = import_file(file, format, "cassette");
        let c = cassette_of(&text);
        assert_eq!(c.exchanges.len(), exchanges, "{file} {format}");
        assert!(stderr.contains("skipped 1"), "{file}: {stderr}");
        assert!(stderr.contains("no assistant turn") || stderr.contains("skipped"), "{stderr}");
    }
    let (text, _) = import_file("hermes.jsonl", "hermes-fc", "cassette");
    let c = cassette_of(&text);
    let first = &c.exchanges[0];
    assert_eq!(first.meta.model, "imported");
    assert_eq!(first.meta.extra["row"], "h1");
    let reply = match &first.outcome {
        fakellm::CassetteOutcome::Reply { reply } => reply,
        other => panic!("{other:?}"),
    };
    assert_eq!(reply.tool_calls[0].name, "get_weather");
    assert_eq!(reply.finish, fakellm::FinishReason::ToolCalls);
}

#[test]
fn cli_trajectories_and_default_outputs() {
    let (text, _) = import_file("openhands.jsonl", "openhands", "trajectory");
    let t = trajectories_of(&text);
    assert_eq!(t.len(), 2);
    assert!(t.iter().all(|t| !t.steps.is_empty()));
    // The default output of the agent formats is a trajectory file, of chat formats a cassette.
    let out = temp_path("default.jsonl");
    let res = fakellm(&[
        "import",
        "file",
        fixture("swe_agent.jsonl").to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(res.status.success(), "{}", String::from_utf8_lossy(&res.stderr));
    let t = trajectories_of(&std::fs::read_to_string(&out).unwrap());
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].steps.len(), 2);
    // `-o -` writes to stdout.
    let res = fakellm(&[
        "import",
        "file",
        fixture("sharegpt.jsonl").to_str().unwrap(),
        "--name",
        "tape",
        "-o",
        "-",
    ]);
    assert!(res.status.success());
    let c = cassette_of(&String::from_utf8(res.stdout).unwrap());
    assert_eq!(c.name, "tape");
}

#[test]
fn cli_errors_exit_one() {
    let out = temp_path("none.jsonl");
    let empty = temp_path("empty.jsonl");
    std::fs::write(&empty, "{\"foo\": 1}\n").unwrap();
    let res = fakellm(&["import", "file", empty.to_str().unwrap(), "-o", out.to_str().unwrap()]);
    assert_eq!(res.status.code(), Some(1), "nothing converted");
    let res = fakellm(&["import", "file", "/nonexistent/rows.jsonl", "-o", out.to_str().unwrap()]);
    assert_eq!(res.status.code(), Some(1));
    let res = fakellm(&[
        "import",
        "file",
        empty.to_str().unwrap(),
        "--format",
        "nope",
        "-o",
        out.to_str().unwrap(),
    ]);
    assert_eq!(res.status.code(), Some(2), "clap usage error");
}

// ---- outputs drive a server ----

#[tokio::test]
async fn a_converted_cassette_replays() {
    let (text, _) = import_file("wildchat.jsonl", "wildchat", "cassette");
    let s = start().await;
    let r = send(&s, "PUT", "/_fakellm/cassettes", &[("content-type", "application/x-ndjson")], &text).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(chat_text(&chat(&s, &chat_body("gpt-4", "Hi")).await), "Hello!");
    assert_eq!(log(&s, "source=cassette").await.len(), 1);
}

#[tokio::test]
async fn a_converted_trajectory_drives_a_request_chain() {
    let conv = convert_rows(&rows("hermes.jsonl"), Format::HermesFc);
    let trajs = to_trajectories(&conv.conversations, &opts("hermes"));
    let first = &trajs[0];
    let s = start().await;
    let r = send(
        &s,
        "PUT",
        "/_fakellm/trajectories",
        &[("content-type", "application/x-ndjson")],
        &first.to_jsonl(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    put_rules(&s, json!([{"reply": {"trajectory": first.name}}])).await;
    let tools = json!([{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}]);
    let mut b = chat_body("gpt-4o", "Weather in Paris?");
    b["tools"] = tools.clone();
    let r = chat(&s, &b).await.json();
    let call = &r["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(
        serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
        json!({"city": "Paris"})
    );
    let id = call["id"].as_str().unwrap().to_owned();
    let body = json!({"model": "gpt-4o", "tools": tools, "messages": [
        {"role": "user", "content": "Weather in Paris?"},
        {"role": "assistant", "content": null, "tool_calls": [call]},
        {"role": "tool", "tool_call_id": id, "content": "{\"temp\": 18}"},
    ]});
    assert_eq!(chat_text(&chat(&s, &body).await), "It is 18 C in Paris.");
}

#[test]
fn library_outputs_validate() {
    for (file, format) in [
        ("sharegpt.jsonl", Format::ShareGpt),
        ("wildchat.jsonl", Format::WildChat),
        ("hermes.jsonl", Format::HermesFc),
        ("swe_agent.jsonl", Format::SweAgent),
        ("openhands.jsonl", Format::OpenHands),
    ] {
        let conv = convert_rows(&rows(file), format);
        to_cassette(&conv.conversations, &opts("x")).validate().unwrap();
        for t in to_trajectories(&conv.conversations, &opts("x")) {
            t.validate().unwrap();
        }
    }
}

// ---- fetch_rows against a local datasets-server ----

type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

/// One-connection-per-request HTTP server on a thread: `handler(path_and_query, authorization) -> (status, body)`.
struct Hf {
    endpoint: String,
    seen: Seen,
}

fn hf_server(handler: impl Fn(&str, Option<&str>) -> (u16, String) + Send + 'static) -> Hf {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let text = String::from_utf8_lossy(&buf).into_owned();
            let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let auth = text.lines().find_map(|l| {
                l.to_ascii_lowercase()
                    .starts_with("authorization:")
                    .then(|| l[14..].trim().to_owned())
            });
            let (status, body) = handler(&path, auth.as_deref());
            log.lock().unwrap().push((path, auth));
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    Hf { endpoint, seen }
}

fn query_of(path: &str, key: &str) -> Option<String> {
    path.split_once('?')?
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_owned))
}

fn rows_page(path: &str, total: usize, truncated_idx: &[usize]) -> String {
    let offset: usize = query_of(path, "offset").unwrap().parse().unwrap();
    let length: usize = query_of(path, "length").unwrap().parse().unwrap();
    let rows: Vec<Value> = (offset..total.min(offset + length))
        .map(|i| {
            json!({"row_idx": i, "row": {"id": format!("r{i}"), "conversations": [{"from": "human", "value": "q"}, {"from": "gpt", "value": "a"}]},
                   "truncated_cells": if truncated_idx.contains(&i) { json!(["conversations"]) } else { json!([]) }})
        })
        .collect();
    json!({"rows": rows, "num_rows_total": total}).to_string()
}

fn source(limit: usize) -> Source {
    Source {
        dataset: "owner/data".into(),
        config: Some("default".into()),
        split: "train".into(),
        offset: 0,
        limit,
    }
}

#[tokio::test]
async fn fetch_rows_pages_by_100_and_drops_truncated_rows() {
    let hf = hf_server(|path, _| {
        assert!(path.starts_with("/rows?"), "{path}");
        (200, rows_page(path, 250, &[5]))
    });
    let got = hf::fetch_rows(&HttpClient::new(), &hf.endpoint, &source(150), None).await.unwrap();
    assert_eq!(got.rows.len(), 149);
    assert_eq!(got.truncated, 1);
    assert_eq!(got.total, Some(250));
    let seen = hf.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(query_of(&seen[0].0, "length").as_deref(), Some("100"));
    assert_eq!(query_of(&seen[1].0, "offset").as_deref(), Some("100"));
    assert_eq!(query_of(&seen[1].0, "length").as_deref(), Some("50"));
    for key in ["dataset=owner/data", "config=default", "split=train"] {
        let (k, v) = key.split_once('=').unwrap();
        assert_eq!(query_of(&seen[0].0, k).map(|s| s.replace("%2F", "/")).as_deref(), Some(v), "{key}");
    }
    assert!(seen.iter().all(|(_, auth)| auth.is_none()));

    // The split ends before the limit: stop at the empty page.
    let got = hf::fetch_rows(&HttpClient::new(), &hf.endpoint, &source(1000), None).await.unwrap();
    assert_eq!(got.rows.len(), 249);
}

#[tokio::test]
async fn fetch_rows_sends_the_token_and_explains_gated_and_missing_datasets() {
    let hf = hf_server(|path, auth| {
        if path.contains("gated") {
            if auth == Some("Bearer secret") {
                return (200, rows_page(path, 3, &[]));
            }
            return (401, json!({"error": "The dataset is gated"}).to_string());
        }
        if path.starts_with("/splits") {
            return (
                200,
                json!({"splits": [{"dataset": "owner/missing", "config": "default", "split": "train"}]}).to_string(),
            );
        }
        (404, json!({"error": "not found"}).to_string())
    });
    let client = HttpClient::new();
    let mut src = source(10);
    src.dataset = "owner/gated".into();
    let err = hf::fetch_rows(&client, &hf.endpoint, &src, None).await.unwrap_err();
    assert!(err.0.contains("gated") && err.0.contains("HF_TOKEN"), "{err}");
    let ok = hf::fetch_rows(&client, &hf.endpoint, &src, Some("secret")).await.unwrap();
    assert_eq!(ok.rows.len(), 3);
    assert!(hf.seen.lock().unwrap().iter().any(|(_, a)| a.as_deref() == Some("Bearer secret")));

    let mut src = source(10);
    src.dataset = "owner/missing".into();
    let err = hf::fetch_rows(&client, &hf.endpoint, &src, None).await.unwrap_err();
    assert!(err.0.contains("not found"), "{err}");
}

#[tokio::test]
async fn cli_import_hf_uses_the_endpoint_variable_the_token_and_the_first_config() {
    let hf = hf_server(|path, auth| {
        if path.starts_with("/splits") {
            return (200, json!({"splits": [{"dataset": "o/d", "config": "cfg", "split": "test"}, {"dataset": "o/d", "config": "cfg", "split": "train"}]}).to_string());
        }
        assert_eq!(auth, Some("Bearer tok"));
        assert_eq!(query_of(path, "config").as_deref(), Some("cfg"));
        (200, rows_page(path, 4, &[1]))
    });
    let out = temp_path("hf.jsonl");
    let endpoint = hf.endpoint.clone();
    let out_path = out.to_str().unwrap().to_owned();
    let res = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_fakellm"))
            .args(["import", "hf", "o/d", "--limit", "10", "--format", "sharegpt", "-o", &out_path])
            .env("FAKELLM_HF_ENDPOINT", endpoint)
            .env("HF_TOKEN", "tok")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(res.status.success(), "{stderr}");
    assert!(stderr.contains("1 truncated"), "{stderr}");
    let c = cassette_of(&std::fs::read_to_string(&out).unwrap());
    assert_eq!(c.name, "o-d");
    assert_eq!(c.exchanges.len(), 3);
}

#[tokio::test]
async fn real_wildchat_fetch_when_asked() {
    if std::env::var("FAKELLM_HF_TEST").as_deref() != Ok("1") {
        eprintln!("skip: FAKELLM_HF_TEST=1 fetches 5 rows of allenai/WildChat-1M from the real datasets-server");
        return;
    }
    let src = Source {
        dataset: "allenai/WildChat-1M".into(),
        config: None,
        split: "train".into(),
        offset: 0,
        limit: 5,
    };
    let token = std::env::var("HF_TOKEN").ok().filter(|t| !t.is_empty());
    let got = hf::fetch_rows(&HttpClient::new(), &hf::endpoint(), &src, token.as_deref())
        .await
        .unwrap();
    assert!(!got.rows.is_empty());
    let conv = convert_rows(&got.rows, Format::Auto);
    assert!(!conv.conversations.is_empty(), "{:?}", conv.reasons);
    to_cassette(&conv.conversations, &opts("wildchat")).validate().unwrap();
}

// ---- rows copied (truncated, anonymized) from the real datasets; see docs/import.md ----

fn real_rows(name: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(fixture(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn real_dataset_rows_convert_with_auto_detection() {
    for (file, format, conversations, units) in [
        ("real_wildchat.jsonl", Format::WildChat, 3, 4),
        ("real_sharegpt.jsonl", Format::ShareGpt, 3, 4),
        ("real_hermes.jsonl", Format::HermesFc, 2, 4),
        ("real_swe_nebius.jsonl", Format::SweAgent, 1, 3),
        ("real_swe_smith_tool.jsonl", Format::SweAgent, 1, 3),
        ("real_swe_smith_xml.jsonl", Format::SweAgent, 1, 3),
        ("real_swe_smith_ticks.jsonl", Format::SweAgent, 1, 3),
        ("real_openhands.jsonl", Format::SweAgent, 2, 8),
        ("xlam.jsonl", Format::Xlam, 2, 2),
    ] {
        let rows = real_rows(file);
        assert!(
            rows[0]["_comment"]
                .as_str()
                .is_some_and(|c| c.starts_with("real") || c.starts_with("synthetic"))
        );
        assert_eq!(detect(&rows[0]), Some(format), "{file}");
        let conv = convert_rows(&rows, Format::Auto);
        assert_eq!(conv.conversations.len(), conversations, "{file}: {:?}", conv.reasons);
        let assistant: usize = conv
            .conversations
            .iter()
            .map(|c| c.messages.iter().filter(|m| m.role == Role::Assistant).count())
            .sum();
        assert_eq!(assistant, units, "{file}");
        to_cassette(&conv.conversations, &opts("x")).validate().unwrap();
        for t in to_trajectories(&conv.conversations, &opts("x")) {
            t.validate().unwrap();
        }
    }
}

#[test]
fn smith_xml_and_ticks_splits_become_tool_calls() {
    for file in ["real_swe_smith_xml.jsonl", "real_swe_smith_ticks.jsonl"] {
        let conv = convert_rows(&real_rows(file), Format::Auto);
        let c = &conv.conversations[0];
        let calls: Vec<_> = c.messages.iter().flat_map(|m| m.tool_calls.iter()).collect();
        assert!(!calls.is_empty(), "{file}");
        assert!(calls.iter().all(|t| t.name == "bash" || t.name == "str_replace_editor"), "{file}");
        let results = c.messages.iter().filter(|m| m.role == Role::Tool).count();
        assert!(results > 0, "{file}: observations are tool results");
        assert!(
            !c.messages
                .iter()
                .any(|m| m.role == Role::User && m.text_content().starts_with("OBSERVATION:")),
            "{file}: observations are not user turns"
        );
    }
}
