//! One conversation, three wire formats: builds requests and reads replies for Chat Completions, Responses and
//! Anthropic Messages so the same scenario can run on every protocol. Included with
//! `#[path = "common/protocols.rs"] mod protocols;`.
#![allow(dead_code)]

use serde_json::{Value, json};

use crate::common::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum P {
    Chat,
    Responses,
    Anthropic,
}

pub const ALL: [P; 3] = [P::Chat, P::Responses, P::Anthropic];

/// One finished exchange already in the history.
#[derive(Clone, Debug)]
pub enum Turn {
    /// Assistant tool call and the client's result.
    Call {
        id: String,
        name: String,
        args: Value,
        result: String,
    },
    /// A plain assistant message (what a client keeps after a failed attempt).
    Say(String),
}

pub fn call_turn(id: &str, name: &str, args: Value, result: &str) -> Turn {
    Turn::Call {
        id: id.into(),
        name: name.into(),
        args,
        result: result.into(),
    }
}

impl P {
    pub fn path(self) -> &'static str {
        match self {
            P::Chat => "/v1/chat/completions",
            P::Responses => "/v1/responses",
            P::Anthropic => "/v1/messages",
        }
    }

    pub fn headers(self) -> &'static [(&'static str, &'static str)] {
        match self {
            P::Anthropic => &[("anthropic-version", "2023-06-01")],
            _ => &[],
        }
    }

    pub fn model(self) -> &'static str {
        if self == P::Anthropic { "claude-sonnet-4" } else { "gpt-4o" }
    }

    /// The protocol name the request log uses.
    pub fn log_name(self) -> &'static str {
        match self {
            P::Chat => "openai.chat",
            P::Responses => "openai.responses",
            P::Anthropic => "anthropic.messages",
        }
    }

    pub fn body(self, user: &str, turns: &[Turn], tools: &[&str]) -> Value {
        match self {
            P::Chat => {
                let mut m = vec![json!({"role": "user", "content": user})];
                for t in turns {
                    match t {
                        Turn::Call { id, name, args, result } => {
                            m.push(json!({"role": "assistant", "content": null, "tool_calls": [
                                {"id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}));
                            m.push(json!({"role": "tool", "tool_call_id": id, "content": result}));
                        }
                        Turn::Say(t) => m.push(json!({"role": "assistant", "content": t})),
                    }
                }
                let mut b = json!({"model": "gpt-4o", "messages": m});
                if !tools.is_empty() {
                    b["tools"] = tools
                        .iter()
                        .map(|n| json!({"type": "function", "function": {"name": n, "parameters": {"type": "object"}}}))
                        .collect();
                }
                b
            }
            P::Responses => {
                let mut input = vec![json!({"role": "user", "content": user})];
                for t in turns {
                    match t {
                        Turn::Call { id, name, args, result } => {
                            input.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": args.to_string()}));
                            input.push(json!({"type": "function_call_output", "call_id": id, "output": result}));
                        }
                        Turn::Say(t) => input.push(json!({"role": "assistant", "content": t})),
                    }
                }
                let mut b = json!({"model": "gpt-4o", "input": input});
                if !tools.is_empty() {
                    b["tools"] = tools
                        .iter()
                        .map(|n| json!({"type": "function", "name": n, "parameters": {"type": "object"}}))
                        .collect();
                }
                b
            }
            P::Anthropic => {
                let mut m = vec![json!({"role": "user", "content": user})];
                for t in turns {
                    match t {
                        Turn::Call { id, name, args, result } => {
                            m.push(json!({"role": "assistant", "content": [
                                {"type": "tool_use", "id": id, "name": name, "input": args}]}));
                            m.push(json!({"role": "user", "content": [
                                {"type": "tool_result", "tool_use_id": id, "content": result}]}));
                        }
                        Turn::Say(t) => m.push(json!({"role": "assistant", "content": t})),
                    }
                }
                let mut b = json!({"model": "claude-sonnet-4", "max_tokens": 64, "messages": m});
                if !tools.is_empty() {
                    b["tools"] = tools
                        .iter()
                        .map(|n| json!({"name": n, "input_schema": {"type": "object"}}))
                        .collect();
                }
                b
            }
        }
    }

    pub async fn send(self, s: &fakellm::Server, user: &str, turns: &[Turn], tools: &[&str]) -> Reply {
        post_json_with(s, self.path(), self.headers(), &self.body(user, turns, tools)).await
    }

    /// Tool calls in the reply as (name, arguments, id).
    pub fn calls(self, r: &Reply) -> Vec<(String, Value, String)> {
        assert_eq!(r.status, 200, "{self:?}: {}", r.body);
        let v = r.json();
        match self {
            P::Chat => v["choices"][0]["message"]["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    (
                        c["function"]["name"].as_str().unwrap().to_owned(),
                        serde_json::from_str(c["function"]["arguments"].as_str().unwrap()).unwrap(),
                        c["id"].as_str().unwrap().to_owned(),
                    )
                })
                .collect(),
            P::Responses => v["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|o| o["type"] == "function_call")
                .map(|o| {
                    (
                        o["name"].as_str().unwrap().to_owned(),
                        serde_json::from_str(o["arguments"].as_str().unwrap()).unwrap(),
                        o["call_id"].as_str().unwrap().to_owned(),
                    )
                })
                .collect(),
            P::Anthropic => v["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| {
                    (
                        b["name"].as_str().unwrap().to_owned(),
                        b["input"].clone(),
                        b["id"].as_str().unwrap().to_owned(),
                    )
                })
                .collect(),
        }
    }

    pub fn text(self, r: &Reply) -> String {
        assert_eq!(r.status, 200, "{self:?}: {}", r.body);
        let v = r.json();
        match self {
            P::Chat => v["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_owned(),
            P::Responses => v["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|o| o["type"] == "message")
                .flat_map(|o| o["content"].as_array().cloned().unwrap_or_default())
                .filter_map(|c| c["text"].as_str().map(str::to_owned))
                .collect(),
            P::Anthropic => v["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str().map(str::to_owned))
                .collect(),
        }
    }

    /// The error message of a non-200 reply, whatever the protocol's error shape.
    pub fn error_message(self, r: &Reply) -> String {
        r.json()["error"]["message"].as_str().unwrap_or_default().to_owned()
    }
}
