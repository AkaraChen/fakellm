//! Bounded in-memory log of API requests, for tests that assert on what the client sent: the
//! parsed IR, the raw JSON body, the headers, and what the server answered. Control-plane
//! requests are not logged.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use serde_json::Value;

use crate::ir::{Outcome, Protocol, Request, Source};

/// One recorded request.
#[derive(Clone, Debug)]
pub struct RequestRecord {
    /// Position in the order requests arrived, starting at 1; survives `clear`.
    pub seq: u64,
    /// Server clock time when the request arrived.
    pub time: SystemTime,
    pub method: http::Method,
    /// Path and query exactly as sent.
    pub path: String,
    pub status: u16,
    /// Parked by a `hang` rule right now; `status` is 0 and `latency` zero until it is released
    /// or the client gives up.
    pub hung: bool,
    /// From arrival until the response head was ready; a streamed body is not included.
    pub latency: Duration,
    /// Request headers, lower-cased names, in order. `authorization` and `x-api-key` values are
    /// kept (it is a test server; asserting on the key is a valid test).
    pub headers: Vec<(String, String)>,
    /// The JSON body as sent, `None` when it was not valid JSON (see `raw_text`).
    pub raw: Option<Arc<Value>>,
    /// The body as text when it was not JSON, truncated to 64 KiB.
    pub raw_text: Option<String>,
    /// Set when the body parsed into the IR.
    pub request: Option<Arc<Request>>,
    /// Set for every request that reached a decision, including rejections.
    pub outcome: Option<Arc<Outcome>>,
}

impl RequestRecord {
    pub fn protocol(&self) -> Option<Protocol> {
        self.request.as_ref().map(|r| r.protocol)
    }

    pub fn model(&self) -> Option<&str> {
        self.request.as_ref().map(|r| r.model.as_str())
    }

    pub fn stream(&self) -> bool {
        self.request.as_ref().is_some_and(|r| r.stream)
    }
}

/// What to keep when reading the log. Unset fields match everything.
#[derive(Clone, Debug, Default)]
pub struct RequestFilter {
    /// Glob over the model name.
    pub model: Option<String>,
    /// Requests that offered a tool with this name.
    pub has_tool: Option<String>,
    /// Streaming (`true`) or sync (`false`) requests only.
    pub stream: Option<bool>,
    pub status: Option<StatusFilter>,
    /// `script`, `fallback`, `strict` or `rejected`.
    pub source: Option<String>,
    /// Only requests that came in on this API.
    pub protocol: Option<Protocol>,
    /// Name of the rule that answered.
    pub rule: Option<String>,
    /// Path prefix, e.g. `/v1/chat/completions`.
    pub path_prefix: Option<String>,
    /// Only requests at or after this server time.
    pub since: Option<SystemTime>,
    /// Only requests with a `seq` greater than this.
    pub after: Option<u64>,
    /// Keep only the most recent `limit` matches.
    pub limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusFilter {
    Exact(u16),
    /// `2xx`, `4xx`, ...
    Class(u16),
    /// 400 and above.
    Error,
}

impl std::str::FromStr for StatusFilter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if s == "error" {
            return Ok(Self::Error);
        }
        let bad = || format!("invalid status {s:?} (use 404, 4xx or error)");
        if let Some(c) = s.strip_suffix("xx") {
            return c.parse().ok().filter(|c| (1..=5).contains(c)).map(Self::Class).ok_or_else(bad);
        }
        s.parse().map(Self::Exact).map_err(|_| bad())
    }
}

impl StatusFilter {
    pub fn matches(self, status: u16) -> bool {
        match self {
            Self::Exact(s) => s == status,
            Self::Class(c) => status / 100 == c,
            Self::Error => status >= 400,
        }
    }
}

impl RequestFilter {
    pub fn matches(&self, r: &RequestRecord) -> bool {
        if let Some(p) = &self.model
            && !r.model().is_some_and(|m| glob_match(p, m))
        {
            return false;
        }
        if let Some(t) = &self.has_tool
            && !r.request.as_ref().is_some_and(|q| q.has_tool(t))
        {
            return false;
        }
        if self.stream.is_some_and(|s| s != r.stream()) {
            return false;
        }
        if self.status.is_some_and(|s| !s.matches(r.status)) {
            return false;
        }
        if let Some(want) = &self.source {
            let kind = match r.outcome.as_deref().map(Outcome::source) {
                Some(Source::Script { .. } | Source::Trajectory { .. } | Source::Policy { .. }) => "script",
                Some(Source::Fallback) => "fallback",
                Some(Source::Strict) => "strict",
                Some(Source::Rejected) => "rejected",
                None => return false,
            };
            if kind != want {
                return false;
            }
        }
        if self.protocol.is_some_and(|p| r.protocol() != Some(p)) {
            return false;
        }
        if let Some(want) = &self.rule {
            match r.outcome.as_deref().map(Outcome::source).and_then(Source::rule) {
                Some((_, Some(n))) if n == want => {}
                _ => return false,
            }
        }
        if let Some(p) = &self.path_prefix
            && !r.path.starts_with(p.as_str())
        {
            return false;
        }
        if self.since.is_some_and(|t| r.time < t) {
            return false;
        }
        self.after.is_none_or(|a| r.seq > a)
    }
}

pub const DEFAULT_CAPACITY: usize = 10_000;

pub struct RequestLog {
    enabled: AtomicBool,
    redact_auth: AtomicBool,
    next_seq: AtomicU64,
    inner: Mutex<Inner>,
}

struct Inner {
    capacity: usize,
    records: VecDeque<RequestRecord>,
    /// Records pushed out of the buffer because it was full.
    dropped: u64,
}

impl RequestLog {
    pub fn new(enabled: bool, capacity: usize) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            redact_auth: AtomicBool::new(false),
            next_seq: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                capacity: capacity.max(1),
                records: VecDeque::new(),
                dropped: 0,
            }),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Replace the values of credential headers by `<redacted>` in records stored from now on.
    pub fn set_redact_auth(&self, on: bool) {
        self.redact_auth.store(on, Ordering::Relaxed);
    }

    pub fn capacity(&self) -> usize {
        self.inner.lock().capacity
    }

    /// Resizes the buffer, dropping the oldest records if it shrinks.
    pub fn set_capacity(&self, capacity: usize) {
        let mut inner = self.inner.lock();
        inner.capacity = capacity.max(1);
        while inner.records.len() > inner.capacity {
            inner.records.pop_front();
            inner.dropped += 1;
        }
    }

    /// Stores `record` (its `seq` is assigned here) and returns the seq. No-op returning 0 when
    /// logging is off.
    pub fn record(&self, mut record: RequestRecord) -> u64 {
        if !self.is_enabled() {
            return 0;
        }
        if self.redact_auth.load(Ordering::Relaxed) {
            for (name, value) in &mut record.headers {
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "authorization" | "proxy-authorization" | "x-api-key"
                ) {
                    *value = "<redacted>".into();
                }
            }
        }
        // Assigned under the lock so that buffer order and seq order agree (`list` relies on it).
        let mut inner = self.inner.lock();
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        if inner.records.len() >= inner.capacity {
            inner.records.pop_front();
            inner.dropped += 1;
        }
        inner.records.push_back(RequestRecord { seq, ..record });
        seq
    }

    /// Edits the record with this `seq` in place; a no-op when it was dropped or cleared.
    pub fn update(&self, seq: u64, edit: impl FnOnce(&mut RequestRecord)) {
        let mut inner = self.inner.lock();
        let start = inner.records.partition_point(|r| r.seq < seq);
        if let Some(r) = inner.records.get_mut(start).filter(|r| r.seq == seq) {
            edit(r);
        }
    }

    /// Matching records, oldest first, plus the number of matches before `limit` was applied.
    pub fn list(&self, filter: &RequestFilter) -> (Vec<RequestRecord>, usize) {
        let inner = self.inner.lock();
        let start = filter.after.map_or(0, |a| inner.records.partition_point(|r| r.seq <= a));
        let mut matched: Vec<_> = inner.records.iter().skip(start).filter(|r| filter.matches(r)).cloned().collect();
        let total = matched.len();
        if let Some(limit) = filter.limit
            && total > limit
        {
            matched.drain(..total - limit);
        }
        (matched, total)
    }

    /// Records evicted because the buffer was full since the last `clear`.
    pub fn dropped(&self) -> u64 {
        self.inner.lock().dropped
    }

    pub fn len(&self) -> usize {
        self.inner.lock().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.records.clear();
        inner.dropped = 0;
    }
}

/// Glob match with `*` (any run) and `?` (one char), case-sensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    // Where the last `*` was and which text position it currently swallows up to.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) && p[pi] != '*' {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ApiError, Delivery, Message, Reply, Role, ToolDef};

    #[test]
    fn redacts_credential_headers() {
        let mut r = rec(Some("m"), "/v1/x", 200, Source::Fallback);
        r.headers = vec![
            ("Authorization".into(), "Bearer sk-1".into()),
            ("x-api-key".into(), "k".into()),
            ("proxy-authorization".into(), "p".into()),
            ("user-agent".into(), "ua".into()),
        ];
        let log = RequestLog::new(true, 10);
        log.record(r.clone());
        log.set_redact_auth(true);
        log.record(r);
        let (rows, _) = log.list(&RequestFilter::default());
        assert_eq!(rows[0].headers[0].1, "Bearer sk-1");
        assert_eq!(
            rows[1].headers,
            vec![
                ("Authorization".to_owned(), "<redacted>".to_owned()),
                ("x-api-key".into(), "<redacted>".into()),
                ("proxy-authorization".into(), "<redacted>".into()),
                ("user-agent".into(), "ua".into()),
            ]
        );
    }

    #[test]
    fn glob() {
        assert!(glob_match("gpt-*", "gpt-4o"));
        assert!(glob_match("*", ""));
        assert!(glob_match("g?t-4*o", "gpt-4o"));
        assert!(glob_match("*4*", "gpt-4o-mini"));
        assert!(glob_match("a*b*c", "aXbYbZc"));
        assert!(!glob_match("gpt-?", "gpt-4o"));
        assert!(!glob_match("gpt", "gpt-4o"));
        assert!(!glob_match("a*b", "acd"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "a"));
    }

    fn rec(model: Option<&str>, path: &str, status: u16, source: Source) -> RequestRecord {
        let request = model.map(|m| {
            let mut r = Request::new(m, vec![Message::text(Role::User, "hi")]);
            r.stream = m.ends_with("-s");
            r.tools = vec![ToolDef {
                name: "get_weather".into(),
                description: None,
                parameters: None,
                strict: None,
            }];
            Arc::new(r)
        });
        let outcome = match &source {
            Source::Rejected => Outcome::rejected(ApiError::invalid_request("bad")),
            _ => Outcome::Reply {
                reply: Reply::default(),
                delivery: Delivery::default(),
                source,
            },
        };
        RequestRecord {
            seq: 0,
            time: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
            method: http::Method::POST,
            path: path.into(),
            status,
            hung: false,
            latency: Duration::ZERO,
            headers: Vec::new(),
            raw: None,
            raw_text: None,
            request,
            outcome: Some(Arc::new(outcome)),
        }
    }

    fn script(name: Option<&str>) -> Source {
        Source::Script {
            index: 0,
            name: name.map(Into::into),
        }
    }

    fn seeded() -> RequestLog {
        let log = RequestLog::new(true, 100);
        log.record(rec(Some("gpt-4o"), "/v1/chat/completions", 200, script(Some("greet"))));
        log.record(rec(Some("gpt-4o-s"), "/v1/chat/completions", 200, Source::Fallback));
        log.record(rec(Some("o3"), "/v1/chat/completions", 429, script(None)));
        log.record(rec(None, "/v1/other", 400, Source::Rejected));
        log
    }

    #[test]
    fn filters() {
        let log = seeded();
        let n = |f: RequestFilter| log.list(&f).1;
        assert_eq!(n(RequestFilter::default()), 4);
        assert_eq!(
            n(RequestFilter {
                model: Some("gpt-*".into()),
                ..Default::default()
            }),
            2
        );
        assert_eq!(
            n(RequestFilter {
                has_tool: Some("get_weather".into()),
                ..Default::default()
            }),
            3
        );
        assert_eq!(
            n(RequestFilter {
                has_tool: Some("nope".into()),
                ..Default::default()
            }),
            0
        );
        assert_eq!(
            n(RequestFilter {
                stream: Some(true),
                ..Default::default()
            }),
            1
        );
        assert_eq!(
            n(RequestFilter {
                stream: Some(false),
                ..Default::default()
            }),
            3
        );
        assert_eq!(
            n(RequestFilter {
                status: Some(StatusFilter::Error),
                ..Default::default()
            }),
            2
        );
        assert_eq!(
            n(RequestFilter {
                source: Some("script".into()),
                ..Default::default()
            }),
            2
        );
        assert_eq!(
            n(RequestFilter {
                source: Some("rejected".into()),
                ..Default::default()
            }),
            1
        );
        assert_eq!(
            n(RequestFilter {
                rule: Some("greet".into()),
                ..Default::default()
            }),
            1
        );
        assert_eq!(
            n(RequestFilter {
                path_prefix: Some("/v1/other".into()),
                ..Default::default()
            }),
            1
        );
        let late = SystemTime::UNIX_EPOCH + Duration::from_secs(101);
        assert_eq!(
            n(RequestFilter {
                since: Some(late),
                ..Default::default()
            }),
            0
        );
    }

    #[test]
    fn after_and_limit() {
        let log = seeded();
        let (rows, total) = log.list(&RequestFilter {
            after: Some(1),
            limit: Some(2),
            ..Default::default()
        });
        assert_eq!(total, 3);
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [3, 4]);
    }

    #[test]
    fn ring_buffer_drops_oldest() {
        let log = RequestLog::new(true, 2);
        for _ in 0..5 {
            log.record(rec(Some("m"), "/p", 200, Source::Fallback));
        }
        assert_eq!(log.len(), 2);
        assert_eq!(log.dropped(), 3);
        let (rows, _) = log.list(&RequestFilter::default());
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [4, 5]);
        log.set_capacity(1);
        assert_eq!((log.len(), log.dropped()), (1, 4));
        log.clear();
        assert_eq!(log.record(rec(Some("m"), "/p", 200, Source::Fallback)), 6);
    }

    #[test]
    fn disabled_records_nothing() {
        let log = RequestLog::new(false, 10);
        assert_eq!(log.record(rec(Some("m"), "/p", 200, Source::Fallback)), 0);
        assert!(log.is_empty());
        log.set_enabled(true);
        assert_eq!(log.record(rec(Some("m"), "/p", 200, Source::Fallback)), 1);
    }
}
