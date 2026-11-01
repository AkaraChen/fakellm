//! `fakellm ctl`: a command-line client for the control-plane HTTP API (docs/design.md).
//!
//! Commands: `status`, `reset [--keep-clock] [--keep-requests] [--keep-scripts] [--keep-responses]`,
//! `responses {list, clear}`,
//! `scripts {list, put FILE [--keep-hits], add FILE, clear}` (FILE is JSON or YAML by extension),
//! `clock {get, freeze [AT], unfreeze, set AT, advance BY, reset}`,
//! `requests {list [--model] [--has-tool] [--stream] [--protocol] [--status] [--source] [--rule] [--path]
//! [--since] [--after] [--limit] [--full] [--count], clear, config [--enable|--disable] [--capacity N]}`.
//! `--endpoint` (env `FAKELLM_ENDPOINT`, default `http://127.0.0.1:4010`) and `--json` are global.
//! Human output is a compact text rendering; `--json` prints the server's JSON.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bytes::Bytes;
use clap::{Args, Subcommand};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpStream;

#[derive(Args)]
pub struct Ctl {
    /// Server to control.
    #[arg(long, global = true, default_value = "http://127.0.0.1:4010", env = "FAKELLM_ENDPOINT")]
    endpoint: String,

    /// Print the server's JSON response instead of formatted text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show clock, scripts and request-log state.
    Status,
    /// Clear scripts, the request log and the clock.
    Reset {
        /// Leave the clock as it is.
        #[arg(long)]
        keep_clock: bool,
        /// Leave the request log as it is.
        #[arg(long)]
        keep_requests: bool,
        /// Leave the scripts as they are.
        #[arg(long)]
        keep_scripts: bool,
        /// Leave the stored Responses API objects as they are.
        #[arg(long)]
        keep_responses: bool,
    },
    /// Manage scripted replies.
    #[command(subcommand)]
    Scripts(Scripts),
    /// Read or move the server's time.
    #[command(subcommand)]
    Clock(Clock),
    /// Inspect the request log.
    #[command(subcommand)]
    Requests(Requests),
    /// Inspect or clear the stored Responses API objects.
    #[command(subcommand)]
    Responses(Responses),
}

#[derive(Subcommand)]
enum Responses {
    /// List stored response ids.
    List,
    /// Forget all stored responses.
    Clear,
}

#[derive(Subcommand)]
enum Scripts {
    /// List rules in match order.
    List,
    /// Replace all rules with those in a JSON or YAML file.
    Put {
        file: PathBuf,
        /// Keep the hit counts of rules that stay at the same position.
        #[arg(long)]
        keep_hits: bool,
    },
    /// Append the rules in a JSON or YAML file.
    Add { file: PathBuf },
    /// Delete all rules.
    Clear,
}

#[derive(Subcommand)]
enum Clock {
    /// Show the server time.
    Get,
    /// Stop time, at the given RFC 3339 time or at the current server time.
    Freeze { at: Option<String> },
    /// Let time run again from the current server time.
    Unfreeze,
    /// Jump to an RFC 3339 time, keeping the frozen or running state.
    Set { at: String },
    /// Move time by a duration such as 30d, 1h30m, 500ms or -2w.
    Advance {
        #[arg(allow_hyphen_values = true)]
        by: String,
    },
    /// Go back to the host clock.
    Reset,
}

#[allow(clippy::large_enum_variant)] // parsed once per process
#[derive(Subcommand)]
enum Requests {
    /// List logged requests, oldest first.
    List {
        #[arg(long)]
        model: Option<String>,
        /// Only requests that offered a tool with this name.
        #[arg(long)]
        has_tool: Option<String>,
        /// Only streaming (true) or non-streaming (false) requests.
        #[arg(long)]
        stream: Option<bool>,
        /// openai.chat, openai.responses or anthropic.messages.
        #[arg(long)]
        protocol: Option<String>,
        /// 200, 4xx, 5xx or error (400 and above).
        #[arg(long)]
        status: Option<String>,
        /// Where the reply came from, e.g. script or fallback.
        #[arg(long)]
        source: Option<String>,
        /// Only replies produced by the rule with this name.
        #[arg(long)]
        rule: Option<String>,
        #[arg(long)]
        path: Option<String>,
        /// Only requests at or after this RFC 3339 time.
        #[arg(long)]
        since: Option<String>,
        /// Only requests with a sequence number greater than this.
        #[arg(long)]
        after: Option<u64>,
        /// Keep only the most recent N matches.
        #[arg(long)]
        limit: Option<usize>,
        /// Include headers and raw bodies.
        #[arg(long)]
        full: bool,
        /// Print only the number of matches.
        #[arg(long)]
        count: bool,
    },
    /// Empty the request log.
    Clear,
    /// Show or change the log settings.
    Config {
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        /// Number of requests kept.
        #[arg(long)]
        capacity: Option<usize>,
    },
}

pub async fn run(ctl: Ctl) -> ExitCode {
    match execute(&ctl).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fakellm ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

type Result<T> = std::result::Result<T, String>;

struct Body {
    bytes: Vec<u8>,
    content_type: &'static str,
}

impl From<Value> for Body {
    fn from(v: Value) -> Self {
        Self {
            bytes: v.to_string().into_bytes(),
            content_type: "application/json",
        }
    }
}

type Call = (&'static str, String, Option<Body>);

fn script_content_type(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("yaml" | "yml") => "application/yaml",
        _ => "application/json",
    }
}

fn script_body(file: &Path) -> Result<Body> {
    let bytes = std::fs::read(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    Ok(Body {
        bytes,
        content_type: script_content_type(file),
    })
}

fn requests_query(r: &Requests) -> String {
    let Requests::List {
        model,
        has_tool,
        stream,
        protocol,
        status,
        source,
        rule,
        path,
        since,
        after,
        limit,
        full,
        ..
    } = r
    else {
        return String::new();
    };
    let mut q = form_urlencoded::Serializer::new(String::new());
    let strs = [
        ("model", model),
        ("has_tool", has_tool),
        ("protocol", protocol),
        ("status", status),
        ("source", source),
        ("rule", rule),
        ("path", path),
        ("since", since),
    ];
    for (k, v) in strs {
        if let Some(v) = v {
            q.append_pair(k, v);
        }
    }
    if let Some(s) = stream {
        q.append_pair("stream", &s.to_string());
    }
    if let Some(a) = after {
        q.append_pair("after", &a.to_string());
    }
    if let Some(l) = limit {
        q.append_pair("limit", &l.to_string());
    }
    if *full {
        q.append_pair("full", "true");
    }
    q.finish()
}

fn plan(command: &Command) -> Result<Call> {
    Ok(match command {
        Command::Status => ("GET", "/_fakellm".to_owned(), None),
        Command::Reset {
            keep_clock,
            keep_requests,
            keep_scripts,
            keep_responses,
        } => (
            "POST",
            format!(
                "/_fakellm/reset?clock={}&requests={}&scripts={}&responses={}",
                !keep_clock, !keep_requests, !keep_scripts, !keep_responses
            ),
            None,
        ),
        Command::Scripts(s) => match s {
            Scripts::List => ("GET", "/_fakellm/scripts".into(), None),
            Scripts::Put { file, keep_hits } => (
                "PUT",
                format!("/_fakellm/scripts{}", if *keep_hits { "?keep_hits=true" } else { "" }),
                Some(script_body(file)?),
            ),
            Scripts::Add { file } => ("POST", "/_fakellm/scripts".into(), Some(script_body(file)?)),
            Scripts::Clear => ("DELETE", "/_fakellm/scripts".into(), None),
        },
        Command::Clock(c) => match c {
            Clock::Get => ("GET", "/_fakellm/clock".into(), None),
            Clock::Freeze { at } => ("POST", "/_fakellm/clock/freeze".into(), Some(json!({ "at": at }).into())),
            Clock::Unfreeze => ("POST", "/_fakellm/clock/unfreeze".into(), None),
            Clock::Set { at } => ("POST", "/_fakellm/clock/set".into(), Some(json!({ "at": at }).into())),
            Clock::Advance { by } => ("POST", "/_fakellm/clock/advance".into(), Some(json!({ "by": by }).into())),
            Clock::Reset => ("POST", "/_fakellm/clock/reset".into(), None),
        },
        Command::Responses(r) => match r {
            Responses::List => ("GET", "/_fakellm/responses".into(), None),
            Responses::Clear => ("DELETE", "/_fakellm/responses".into(), None),
        },
        Command::Requests(r) => match r {
            Requests::List { .. } => ("GET", format!("/_fakellm/requests?{}", requests_query(r)), None),
            Requests::Clear => ("DELETE", "/_fakellm/requests".into(), None),
            Requests::Config { enable, disable, capacity } => {
                if !enable && !disable && capacity.is_none() {
                    ("GET", "/_fakellm/requests/config".into(), None)
                } else {
                    let mut b = serde_json::Map::new();
                    if *enable || *disable {
                        b.insert("enabled".into(), json!(*enable));
                    }
                    if let Some(c) = capacity {
                        b.insert("capacity".into(), json!(c));
                    }
                    ("PUT", "/_fakellm/requests/config".into(), Some(Value::Object(b).into()))
                }
            }
        },
    })
}

async fn execute(ctl: &Ctl) -> Result<()> {
    let api = Api::new(&ctl.endpoint)?;
    let (method, path, body) = plan(&ctl.command)?;
    let resp = api.call(method, &path, body).await?;
    if ctl.json {
        println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_default());
    } else {
        print_human(&ctl.command, &resp);
    }
    Ok(())
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("-")
}

fn print_human(command: &Command, v: &Value) {
    match command {
        Command::Status => {
            println!("version        {}", text(v, "version"));
            print_clock(&v["clock"]);
            let n = v["scripts"]["count"].as_u64().map_or_else(|| "-".to_owned(), |n| n.to_string());
            println!("scripts        {n} rules");
            if let Some(n) = v["responses"]["count"].as_u64() {
                println!("responses      {n} stored");
            }
            if !v["strict"].is_null() {
                println!("strict         {}", v["strict"]);
            }
            print_log_config(&v["request_log"]);
        }
        Command::Reset { .. } => println!("reset"),
        Command::Scripts(Scripts::List) => {
            let rules = v["rules"].as_array().map(Vec::as_slice).unwrap_or_default();
            for r in rules {
                println!("{}", rule_line(r));
            }
            if rules.is_empty() {
                println!("no rules");
            }
        }
        Command::Scripts(Scripts::Put { .. } | Scripts::Add { .. }) => match (v["added"].as_u64(), v["count"].as_u64()) {
            (Some(n), Some(total)) => println!("{n} rules loaded ({total} total)"),
            (Some(n), None) => println!("{n} rules loaded"),
            _ => println!("ok"),
        },
        Command::Scripts(Scripts::Clear) => println!("cleared"),
        Command::Clock(_) => print_clock(v),
        Command::Requests(Requests::List { count: true, .. }) => println!("{}", v["total"]),
        Command::Requests(Requests::List { .. }) => {
            for r in v["requests"].as_array().into_iter().flatten() {
                println!("{}", request_line(r));
            }
            let (shown, total) = (v["requests"].as_array().map_or(0, Vec::len), v["total"].as_u64().unwrap_or(0));
            if total > shown as u64 {
                eprintln!("showing {shown} of {total} matching requests");
            }
        }
        Command::Responses(Responses::List) => {
            for r in v["responses"].as_array().into_iter().flatten() {
                println!("{}  {}", text(r, "id"), text(r, "created_at"));
            }
            println!("{} stored", v["count"]);
        }
        Command::Responses(Responses::Clear) => println!("deleted {}", v["deleted"]),
        Command::Requests(Requests::Clear) => println!("cleared"),
        Command::Requests(Requests::Config { .. }) => print_log_config(v),
    }
}

const MATCH_FIELDS: &[&str] = &[
    "model",
    "last_user",
    "system",
    "any_message",
    "has_tool",
    "no_tools",
    "has_tool_result",
    "last_tool_result",
    "tool_result_contains",
    "stream",
    "turn",
    "any",
    "protocol",
    "probability",
    "every_nth",
];

/// `#<index> <name or -> hits/<times or inf> [exhausted] <matcher> -> <reply>`, from one
/// `RuleInfo` JSON object.
fn rule_line(r: &Value) -> String {
    let times = r["times"].as_u64().map_or_else(|| "inf".to_owned(), |t| t.to_string());
    let exhausted = if r["exhausted"] == true { " exhausted" } else { "" };
    let mut conds = Vec::new();
    for k in MATCH_FIELDS {
        if let Some(v) = r["match"].get(*k) {
            let shown = match v {
                Value::String(s) if matches!(*k, "last_user" | "system" | "any_message" | "tool_result_contains") => {
                    format!("/{}/", s.trim_matches('/'))
                }
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            conds.push(format!("{k}={shown}"));
        }
    }
    let matcher = if conds.is_empty() { "any".to_owned() } else { conds.join(" ") };
    format!(
        "#{} {} {}/{times}{exhausted} {matcher} -> {}",
        r["index"],
        r["name"].as_str().unwrap_or("-"),
        r["hits"],
        reply_kind(&r["reply"]),
    )
}

fn reply_kind(reply: &Value) -> String {
    if reply["hang"] == true || !reply["hang_ms"].is_null() {
        return "hang".into();
    }
    if let Some(status) = reply["error"]["status"].as_u64() {
        return format!("error {status}");
    }
    if reply["tool_calls"].as_array().is_some_and(|t| !t.is_empty()) {
        return "tool_calls".into();
    }
    "text".into()
}

fn request_line(r: &Value) -> String {
    let source = r["source"]["kind"].as_str().unwrap_or("-");
    let origin = match r["rule"].as_str() {
        Some(rule) => format!("{source}/{rule}"),
        None => source.to_owned(),
    };
    format!(
        "{:>6}  {}  {:<4} {:<6} {}  {}  stream={}  {}  {}",
        r["seq"],
        text(r, "time"),
        if r["hung"] == true {
            "hung".to_owned()
        } else {
            r["status"].to_string()
        },
        text(r, "method"),
        text(r, "path"),
        text(r, "model"),
        r["stream"] == true,
        origin,
        reply_summary(r),
    )
}

fn reply_summary(r: &Value) -> String {
    let reply = &r["reply"];
    if let Some(e) = reply.get("error").filter(|e| !e.is_null()) {
        let msg = e["message"].as_str().map_or_else(|| e.to_string(), str::to_owned);
        return format!("error: {}", truncate(&msg, 60));
    }
    if reply.is_null() {
        return "-".into();
    }
    let finish = text(reply, "finish");
    let tools: Vec<&str> = reply["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str())
        .collect();
    if !tools.is_empty() {
        return format!("{finish} tools=[{}]", tools.join(","));
    }
    format!("{finish} {:?}", truncate(reply["text"].as_str().unwrap_or(""), 60))
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_owned(),
    }
}

fn print_clock(c: &Value) {
    let mode = if c["frozen"] == true {
        "frozen"
    } else if c["shifted"] == true {
        "running, offset from host clock"
    } else {
        "running, host clock"
    };
    println!("clock          {} ({mode})", text(c, "now"));
}

fn print_log_config(l: &Value) {
    let state = if l["enabled"] == true { "enabled" } else { "disabled" };
    println!(
        "request log    {state}, {} of {} stored, {} dropped",
        l["stored"], l["capacity"], l["dropped"]
    );
}

struct Api {
    authority: String,
}

impl Api {
    fn new(endpoint: &str) -> Result<Self> {
        let rest = endpoint
            .strip_prefix("http://")
            .ok_or_else(|| format!("unsupported endpoint {endpoint:?}: only http:// is supported"))?;
        let authority = rest.trim_end_matches('/');
        if authority.is_empty() || authority.contains('/') {
            return Err(format!("invalid endpoint {endpoint:?}, expected http://host:port"));
        }
        Ok(Self {
            authority: authority.to_owned(),
        })
    }

    async fn call(&self, method: &str, path: &str, body: Option<Body>) -> Result<Value> {
        let stream = TcpStream::connect(&self.authority)
            .await
            .map_err(|e| format!("cannot connect to {}: {e}", self.authority))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        let (payload, content_type) = body.map_or((Vec::new(), "application/json"), |b| (b.bytes, b.content_type));
        let req = http::Request::builder()
            .method(method)
            .uri(path)
            .header(http::header::HOST, &self.authority)
            .header(http::header::CONTENT_TYPE, content_type)
            .body(Full::new(Bytes::from(payload)))
            .map_err(|e| e.to_string())?;
        let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        let json: Value = serde_json::from_slice(&bytes)
            .map_err(|_| format!("{} did not answer like a fakellm control plane (HTTP {status})", self.authority))?;
        if status.is_success() {
            return Ok(json);
        }
        let msg = json["error"]["message"].as_str().unwrap_or("request failed");
        Err(format!("{msg} (HTTP {status})"))
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        ctl: Ctl,
    }

    #[test]
    fn request_line_shows_source_kind() {
        let r = json!({"seq": 1, "status": 200, "method": "POST", "path": "/v1/chat/completions",
            "source": {"kind": "script", "index": 0, "name": "greet"}, "rule": "greet"});
        assert!(request_line(&r).contains("script/greet"));
        let r = json!({"seq": 2, "source": {"kind": "fallback"}, "rule": null});
        assert!(request_line(&r).contains("fallback"));
    }

    #[test]
    fn request_line_marks_hung() {
        let hung = request_line(&json!({"seq": 1, "status": 0, "hung": true, "method": "POST", "path": "/v1/messages"}));
        assert!(hung.contains(" hung "), "{hung}");
        let ok = request_line(&json!({"seq": 2, "status": 200, "hung": false, "method": "POST", "path": "/v1/messages"}));
        assert!(ok.contains(" 200 ") && !ok.contains("hung"), "{ok}");
    }

    fn plan_of(args: &[&str]) -> Call {
        let cli = Cli::try_parse_from(std::iter::once("ctl").chain(args.iter().copied())).unwrap();
        plan(&cli.ctl.command).unwrap()
    }

    #[test]
    fn reset_flags() {
        assert_eq!(
            plan_of(&["reset", "--keep-clock"]).1,
            "/_fakellm/reset?clock=false&requests=true&scripts=true&responses=true"
        );
        assert_eq!(
            plan_of(&["reset", "--keep-responses"]).1,
            "/_fakellm/reset?clock=true&requests=true&scripts=true&responses=false"
        );
    }

    #[test]
    fn rule_lines() {
        let r = json!({"index": 0, "name": "greet", "hits": 2, "exhausted": false,
            "match": {"model": "gpt-*", "last_user": "^hi", "stream": true}, "reply": {"text": "yo"}});
        assert_eq!(rule_line(&r), "#0 greet 2/inf model=gpt-* last_user=/^hi/ stream=true -> text");
        let r = json!({"index": 3, "hits": 1, "exhausted": true, "times": 1, "match": {},
            "reply": {"error": {"status": 429}}});
        assert_eq!(rule_line(&r), "#3 - 1/1 exhausted any -> error 429");
        let r = json!({"index": 4, "hits": 0, "times": 5, "match": {"protocol": "anthropic.messages", "every_nth": 3},
            "reply": {"hang": true}});
        assert_eq!(rule_line(&r), "#4 - 0/5 protocol=anthropic.messages every_nth=3 -> hang");
        let r = json!({"index": 5, "hits": 0, "match": {}, "reply": {"tool_calls": [{"name": "a"}]}});
        assert_eq!(rule_line(&r), "#5 - 0/inf any -> tool_calls");
    }

    #[test]
    fn responses_and_keep_hits() {
        let (m, p, _) = plan_of(&["responses", "list"]);
        assert_eq!((m, p.as_str()), ("GET", "/_fakellm/responses"));
        assert_eq!(plan_of(&["responses", "clear"]).0, "DELETE");
        let dir = std::env::temp_dir().join(format!("fakellm-ctl-kh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let j = dir.join("a.json");
        std::fs::write(&j, "[]").unwrap();
        assert_eq!(
            plan_of(&["scripts", "put", j.to_str().unwrap(), "--keep-hits"]).1,
            "/_fakellm/scripts?keep_hits=true"
        );
        std::fs::remove_dir_all(dir).ok();
        assert!(
            plan_of(&["requests", "list", "--protocol", "openai.responses"])
                .1
                .contains("protocol=openai.responses")
        );
    }

    #[test]
    fn requests_query_mapping() {
        let (m, p, _) = plan_of(&["requests", "list", "--model", "gpt 4", "--stream", "true", "--full", "--limit", "3"]);
        assert_eq!(m, "GET");
        assert_eq!(p, "/_fakellm/requests?model=gpt+4&stream=true&limit=3&full=true");
    }

    #[test]
    fn scripts_file_content_type() {
        let dir = std::env::temp_dir().join(format!("fakellm-ctl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let y = dir.join("a.YML");
        std::fs::write(&y, "- reply: hi\n").unwrap();
        let j = dir.join("a.json");
        std::fs::write(&j, "[]").unwrap();
        let (m, _, b) = plan_of(&["scripts", "put", y.to_str().unwrap()]);
        let b = b.unwrap();
        assert_eq!(
            (m, b.content_type, b.bytes.as_slice()),
            ("PUT", "application/yaml", b"- reply: hi\n".as_slice())
        );
        let (m, p, b) = plan_of(&["scripts", "add", j.to_str().unwrap()]);
        assert_eq!(
            (m, p.as_str(), b.unwrap().content_type),
            ("POST", "/_fakellm/scripts", "application/json")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn config_and_clock() {
        let (m, _, b) = plan_of(&["requests", "config", "--disable", "--capacity", "5"]);
        assert_eq!(m, "PUT");
        assert_eq!(String::from_utf8(b.unwrap().bytes).unwrap(), r#"{"enabled":false,"capacity":5}"#);
        assert_eq!(plan_of(&["requests", "config"]).0, "GET");
        assert_eq!(plan_of(&["clock", "advance", "-2w"]).1, "/_fakellm/clock/advance");
    }

    #[test]
    fn summaries() {
        let r = json!({"reply": {"finish": "stop", "text": "hello"}});
        assert_eq!(reply_summary(&r), "stop \"hello\"");
        let r = json!({"reply": {"finish": "tool_calls", "tool_calls": [{"name": "a"}, {"name": "b"}]}});
        assert_eq!(reply_summary(&r), "tool_calls tools=[a,b]");
        assert_eq!(truncate(&"é".repeat(70), 60).chars().count(), 63);
    }
}
