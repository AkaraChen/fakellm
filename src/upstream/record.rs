//! Recording what the upstream proxy saw: the JSONL record file (`--record FILE`) and the finishing step that
//! runs when a relayed response has been fully sent to the client.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use parking_lot::Mutex;
use serde_json::json;

use crate::cassette::format::{CassetteOutcome, Exchange, ExchangeMeta};
use crate::control::Control;
use crate::ir::{ApiError, Delivery, Outcome, Protocol, Request, Source};
use crate::openai::responses::{ResponseMeta, to_stored};
use crate::reqlog::RequestRecord;
use crate::upstream::Completed;

/// The `--record` file: a cassette in JSONL form that exchanges are appended to as they complete.
pub struct Recorder {
    path: PathBuf,
    /// The file and its length, tracked here so an append needs no `metadata()` call.
    file: Mutex<(std::fs::File, u64)>,
}

impl Recorder {
    /// Opens `path` for appending, creating it. A new or empty file gets the header line
    /// `{"kind":"cassette","name":<file stem>,"recorded_at":<now>,"upstream":<url>}` first; a file that already has
    /// content is appended to as it is (its header stays), so a recording can be continued across runs.
    pub fn open(path: &Path, upstream: &str) -> std::io::Result<Self> {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let mut len = file.metadata()?.len();
        if len > 0 && !ends_with_newline(path)? {
            // A previous run died mid-line: end that line so the next exchange does not glue onto it.
            file.write_all(b"\n")?;
            file.flush()?;
            len += 1;
        }
        if len == 0 {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("recording");
            let header = json!({
                "kind": "cassette",
                "name": name,
                "recorded_at": crate::control::fmt_time(SystemTime::now()),
                "upstream": upstream,
            });
            let line = format!("{header}\n");
            file.write_all(line.as_bytes())?;
            file.flush()?;
            len += line.len() as u64;
        }
        Ok(Self {
            path: path.to_owned(),
            file: Mutex::new((file, len)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one exchange as a line, flushed before returning.
    pub fn append(&self, exchange: &Exchange) -> std::io::Result<()> {
        let mut line = serde_json::to_string(exchange).map_err(std::io::Error::other)?;
        line.push('\n');
        // Blocking I/O: on a multi-threaded runtime tell it, so the worker's other tasks move away meanwhile.
        let write = || {
            let mut guard = self.file.lock();
            let (file, len) = &mut *guard;
            let written = file.write_all(line.as_bytes()).and_then(|()| file.flush());
            match &written {
                Ok(()) => *len += line.len() as u64,
                // Do not leave a partial line behind for the next append to glue onto.
                Err(_) => {
                    let _ = file.set_len(*len);
                }
            }
            written
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(write),
            _ => write(),
        }
    }
}

/// Body fields that look like credentials (some SDKs and gateways pass `api_key` in the body) are not kept in a
/// recorded request; matching ignores `extra.*` by default, so nothing needs the value.
fn redacted(mut request: Request) -> Request {
    const WORDS: [&str; 6] = ["api_key", "apikey", "authorization", "token", "secret", "password"];
    for (key, value) in request.extra.iter_mut() {
        let lower = key.to_ascii_lowercase().replace('-', "_");
        if WORDS.iter().any(|w| lower.contains(w)) {
            *value = serde_json::Value::String("<redacted>".into());
        }
    }
    request
}

fn ends_with_newline(path: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// What the proxy hands over when a relayed response is done, see [`finish`].
pub struct Finish {
    pub control: Arc<Control>,
    /// The request as parsed into the IR (for Responses with an unknown `previous_response_id`, without the history).
    pub request: Arc<Request>,
    /// The log record as it was at arrival (headers, raw body, IR request); `None` when the log is off.
    pub head: Option<RequestRecord>,
    /// `Control::generation()` at arrival; a reset since means nothing is logged.
    pub generation: u64,
    pub started: Instant,
    /// Whether the Responses API object, if one was reassembled, should be stored (`store` was not `false`).
    pub store_response: bool,
}

/// Runs synchronously, from the response body, before the client sees the end of the stream (so a test that read
/// the whole body finds the log record and the exchange in place):
///
/// 1. (unless the status is a transient or credential error, see `Upstream::records_status`) builds the `Exchange` (request, outcome, protocol, `meta` with the model, duration and upstream status) from a
///    successfully reassembled outcome (embeddings: the reply holds usage only) and appends it to the in-memory
///    recording (`Cassettes::record`) and to the record file;
/// 2. stores the reassembled Responses object in `Control::responses` under the upstream's id;
/// 3. writes the request-log record with `Source::Upstream`: the reassembled outcome, or, when `completed.outcome` is
///    `None`, an error outcome (`ApiError::upstream_incomplete`) and nothing recorded.
pub fn finish(f: Finish, completed: Completed) {
    let control = &f.control;
    let request = &f.request;
    let status = match &completed.outcome {
        Some(CassetteOutcome::Error { error }) => error.status,
        _ => completed.status,
    };
    let recordable = control.upstream.as_ref().is_none_or(|u| u.records_status(status));
    if recordable && let Some(outcome) = &completed.outcome {
        let exchange = Exchange {
            request: redacted((*f.request).clone()),
            outcome: outcome.clone(),
            protocol: request.protocol,
            meta: ExchangeMeta {
                model: request.model.clone(),
                duration_ms: Some(completed.duration.as_millis() as u64),
                upstream_status: Some(completed.status),
                extra: Default::default(),
            },
            ignore: Vec::new(),
        };
        control.engine.cassettes.record(exchange.clone());
        if let Some(recorder) = control.upstream.as_ref().and_then(|u| u.recorder())
            && let Err(e) = recorder.append(&exchange)
        {
            tracing::warn!(target: "fakellm::upstream", "cannot write {}: {e}", recorder.path().display());
        }
    }
    if request.protocol == Protocol::OpenaiResponses
        && f.store_response
        && let (Some(CassetteOutcome::Reply { reply }), Some(obj)) = (&completed.outcome, &completed.response)
        && let Some(id) = obj["id"].as_str()
    {
        let meta = ResponseMeta {
            id: id.to_owned(),
            created: obj["created_at"].as_i64().unwrap_or_else(|| control.clock.unix_secs()),
            model: request.model.clone(),
            seed: 0,
        };
        let mut stored = to_stored(request, reply, &meta);
        // Serve the upstream's own object on `GET /v1/responses/{id}`.
        stored.response = obj.clone();
        control.responses.insert(stored);
    }
    if let Some(head) = f.head
        && control.generation() == f.generation
    {
        let outcome = match completed.outcome {
            Some(o) => o.into_outcome(Delivery::default(), Source::Upstream),
            None => Outcome::Error {
                error: ApiError::upstream_incomplete(completed.problem.as_deref().unwrap_or("unknown")),
                delivery: Delivery::default(),
                source: Source::Upstream,
            },
        };
        control.log.record(RequestRecord {
            status: completed.status,
            latency: f.started.elapsed(),
            outcome: Some(Arc::new(outcome)),
            ..head
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Message, Reply, Role};

    #[test]
    fn record_file_gets_a_header_once_and_one_line_per_exchange() {
        let dir = std::env::temp_dir().join(format!("fakellm-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mine.jsonl");
        let _ = std::fs::remove_file(&path);
        let ex = Exchange {
            request: Request::new("gpt-4o", vec![Message::text(Role::User, "hi")]),
            outcome: CassetteOutcome::Reply {
                reply: Reply {
                    text: Some("yo".into()),
                    ..Reply::default()
                },
            },
            protocol: Protocol::OpenaiChat,
            meta: ExchangeMeta::default(),
            ignore: vec![],
        };
        Recorder::open(&path, "http://u").unwrap().append(&ex).unwrap();
        Recorder::open(&path, "http://other").unwrap().append(&ex).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            (header["name"].as_str(), header["upstream"].as_str()),
            (Some("mine"), Some("http://u"))
        );
        let back: Exchange = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(back, ex);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn appending_to_a_file_without_a_trailing_newline_starts_a_new_line() {
        let dir = std::env::temp_dir().join(format!("fakellm-rec-nl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cut.jsonl");
        std::fs::write(&path, "{\"kind\":\"cassette\",\"name\":\"x\"}\n{\"partial").unwrap();
        let ex = Exchange {
            request: Request::new("gpt-4o", vec![Message::text(Role::User, "hi")]),
            outcome: CassetteOutcome::Reply { reply: Reply::default() },
            protocol: Protocol::OpenaiChat,
            meta: ExchangeMeta::default(),
            ignore: vec![],
        };
        Recorder::open(&path, "http://u").unwrap().append(&ex).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "{\"partial");
        assert!(serde_json::from_str::<Exchange>(lines[2]).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
