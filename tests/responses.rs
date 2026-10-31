//! OpenAI Responses API over raw HTTP: object shape, SSE sequence, function calls, chaining, store,
//! failed / incomplete terminals and GET / DELETE.

use serde_json::{Value, json};

mod common;
use common::*;

fn body(input: Value) -> Value {
    json!({"model": "gpt-5.5", "input": input})
}

fn types(ev: &[SseEvent]) -> Vec<String> {
    ev.iter().map(|e| e.json()["type"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn sync_object_shape() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "hello"}}])).await;
    let r = post_json(&s, "/v1/responses", &body(json!("hi"))).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let j = r.json();
    assert_eq!(j["object"], "response");
    assert_eq!(j["status"], "completed");
    assert!(j["id"].as_str().unwrap().starts_with("resp_"));
    assert_eq!(j["output"][0]["type"], "message");
    assert_eq!(j["output"][0]["role"], "assistant");
    assert_eq!(j["output"][0]["content"][0]["type"], "output_text");
    assert_eq!(j["output"][0]["content"][0]["text"], "hello");
    assert!(j["usage"]["input_tokens"].as_u64().unwrap() > 0);
    assert!(j["usage"]["output_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn sse_sequence_numbers_and_no_done() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "one two three"}}])).await;
    let mut b = body(json!("hi"));
    b["stream"] = json!(true);
    let ev = open_sse(&s, "/v1/responses", &[], &b).await.events().await;
    let t = types(&ev);
    assert_eq!(t[0], "response.created");
    assert_eq!(t.last().unwrap(), "response.completed");
    for w in [
        "response.output_item.added",
        "response.content_part.added",
        "response.output_text.delta",
        "response.output_text.done",
        "response.output_item.done",
    ] {
        assert!(t.iter().any(|x| x == w), "missing {w}: {t:?}");
    }
    for (i, e) in ev.iter().enumerate() {
        assert_eq!(e.json()["sequence_number"], i as u64);
        assert_eq!(e.event.as_deref(), e.json()["type"].as_str());
        assert_ne!(e.data, "[DONE]");
    }
    let text: String = ev
        .iter()
        .filter(|e| e.event.as_deref() == Some("response.output_text.delta"))
        .map(|e| e.json()["delta"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(text, "one two three");
}

#[tokio::test]
async fn function_call_item_and_argument_deltas() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"tool_calls": [{"name": "shell", "arguments": {"command": ["ls", "-la"]}}]}}]),
    )
    .await;
    let tools = json!([{"type": "function", "name": "shell", "parameters": {"type": "object"}}]);
    let mut b = body(json!("run"));
    b["tools"] = tools.clone();
    let r = post_json(&s, "/v1/responses", &b).await.json();
    assert_eq!(r["output"][0]["type"], "function_call");
    assert_eq!(r["output"][0]["name"], "shell");
    assert!(r["output"][0]["call_id"].as_str().is_some());

    b["stream"] = json!(true);
    let ev = open_sse(&s, "/v1/responses", &[], &b).await.events().await;
    let added = ev
        .iter()
        .find(|e| e.event.as_deref() == Some("response.output_item.added"))
        .unwrap()
        .json();
    assert_eq!(added["item"]["type"], "function_call");
    let frags: Vec<String> = ev
        .iter()
        .filter(|e| e.event.as_deref() == Some("response.function_call_arguments.delta"))
        .map(|e| e.json()["delta"].as_str().unwrap().to_owned())
        .collect();
    assert!(frags.len() > 1);
    let args: Value = serde_json::from_str(&frags.concat()).unwrap();
    assert_eq!(args, json!({"command": ["ls", "-la"]}));
    let done = ev
        .iter()
        .find(|e| e.event.as_deref() == Some("response.function_call_arguments.done"))
        .unwrap()
        .json();
    assert_eq!(serde_json::from_str::<Value>(done["arguments"].as_str().unwrap()).unwrap(), args);
}

#[tokio::test]
async fn previous_response_id_chaining_grows_prompt() {
    let s = start().await;
    put_rules(&s, json!([{"reply": {"text": "first answer"}}])).await;
    let r1 = post_json(&s, "/v1/responses", &body(json!("question one"))).await.json();
    let id = r1["id"].as_str().unwrap().to_owned();
    let mut b2 = body(json!("question two"));
    b2["previous_response_id"] = json!(id);
    let r2 = post_json(&s, "/v1/responses", &b2).await.json();
    assert!(r2["usage"]["input_tokens"].as_u64() > r1["usage"]["input_tokens"].as_u64());
    assert_eq!(r2["previous_response_id"], id);

    let log = get(&s, "/_fakellm/requests?full=true").await.json();
    let recs = log["requests"].as_array().unwrap();
    let ir = recs.last().unwrap()["request"].to_string();
    assert!(
        ir.contains("question one") && ir.contains("first answer") && ir.contains("question two"),
        "{ir}"
    );
    let first = recs[0]["request"].to_string();
    assert!(!first.contains("first answer"));
}

#[tokio::test]
async fn store_false_then_previous_is_404() {
    let s = start().await;
    let mut b = body(json!("hi"));
    b["store"] = json!(false);
    let r1 = post_json(&s, "/v1/responses", &b).await.json();
    let id = r1["id"].as_str().unwrap();
    assert_eq!(get(&s, &format!("/v1/responses/{id}")).await.status, 404);
    let mut b2 = body(json!("again"));
    b2["previous_response_id"] = json!(id);
    let r2 = post_json(&s, "/v1/responses", &b2).await;
    assert_eq!(r2.status, 404, "{}", r2.body);
    assert_eq!(r2.json()["error"]["code"], "previous_response_not_found");
}

#[tokio::test]
async fn failed_via_in_stream_error() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"error": {"status": 400, "code": "context_length_exceeded", "message": "too long", "in_stream": true}}}]),
    )
    .await;
    let mut b = body(json!("hi"));
    b["stream"] = json!(true);
    let ev = open_sse(&s, "/v1/responses", &[], &b).await.events().await;
    let t = types(&ev);
    assert_eq!(t.last().unwrap(), "response.failed", "{t:?}");
    let f = ev.last().unwrap().json();
    assert_eq!(f["response"]["status"], "failed");
    assert_eq!(f["response"]["error"]["code"], "context_length_exceeded");
}

#[tokio::test]
async fn incomplete_via_max_output_tokens() {
    let s = start().await;
    put_rules(
        &s,
        json!([{"reply": {"text": "a long answer with many many words in it to cut off, and then it keeps going on and on for quite a while longer than sixteen tokens"}}]),
    )
    .await;
    let mut b = body(json!("hi"));
    b["max_output_tokens"] = json!(16);
    let r = post_json(&s, "/v1/responses", &b).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let j = r.json();
    assert_eq!(j["status"], "incomplete", "{j}");
    assert_eq!(j["incomplete_details"]["reason"], "max_output_tokens");
    b["stream"] = json!(true);
    let ev = open_sse(&s, "/v1/responses", &[], &b).await.events().await;
    assert_eq!(types(&ev).last().unwrap(), "response.incomplete");
}

#[tokio::test]
async fn get_and_delete() {
    let s = start().await;
    let id = post_json(&s, "/v1/responses", &body(json!("hi"))).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let g = get(&s, &format!("/v1/responses/{id}")).await;
    assert_eq!(g.status, 200);
    assert_eq!(g.json()["id"], id);
    assert!(get(&s, "/_fakellm/responses").await.json().to_string().contains(&id));
    let d = send(&s, "DELETE", &format!("/v1/responses/{id}"), &[], "").await;
    assert_eq!(d.status, 200, "{}", d.body);
    let dj = d.json();
    assert_eq!(dj["deleted"], true);
    assert_eq!(dj["id"], id);
    assert_eq!(get(&s, &format!("/v1/responses/{id}")).await.status, 404);
    assert_eq!(send(&s, "DELETE", &format!("/v1/responses/{id}"), &[], "").await.status, 404);
}
