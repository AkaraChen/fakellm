//! Cassettes: recordings of real model calls (`--upstream` + `--record`) or imported conversations, replayed by
//! semantic match. The registry lives in the engine; a request that no script rule answers is looked up here before
//! the upstream (if any) and the default reply. See docs/m4-spec.md section 3.
//!
//! - [`format`]: the document, its JSON / YAML / JSONL forms and the trajectory conversion;
//! - [`normalize`]: the ignore list and regex normalizers;
//! - [`replay`]: the comparable form of a request and the similarity measure.

pub mod format;
pub mod normalize;
pub mod replay;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::cassette::format::Cassette;
use crate::cassette::format::{CassetteOutcome, Exchange};
use crate::cassette::replay::{Key, Matcher};
use crate::ir::{Protocol, Request};

/// The name of the in-memory cassette that `--upstream` appends every recorded exchange to. It is exported by
/// `GET /_fakellm/cassettes/recording` and never used for replay.
pub const RECORDING: &str = "recording";

/// A cassette could not be parsed, compiled or found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CassetteError(pub String);

impl std::fmt::Display for CassetteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CassetteError {}

/// What the registry says about one cassette.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CassetteInfo {
    pub name: String,
    pub exchanges: usize,
    pub once: bool,
    pub fuzzy: bool,
    /// Requests this cassette answered (exact and fuzzy).
    pub hits: u64,
    /// Exchanges already spent (`once` cassettes only; 0 otherwise).
    pub consumed: usize,
    /// `true` for the in-memory recording.
    pub recording: bool,
}

/// A recorded exchange chosen for a request.
#[derive(Clone, Debug, PartialEq)]
pub struct Replay {
    pub cassette: String,
    /// Index into the cassette's `exchanges`; goes into `Source::Cassette`.
    pub index: usize,
    pub outcome: CassetteOutcome,
}

/// Which cassettes are loaded, in registration order, with the state replay needs.
#[derive(Default)]
pub struct Cassettes {
    inner: Mutex<State>,
}

#[derive(Default)]
struct State {
    cassettes: Vec<Compiled>,
    /// The `recording` cassette; created on the first [`Cassettes::record`].
    recording: Option<Arc<Cassette>>,
    /// The `upstream` the recording gets when it is created.
    upstream: Option<String>,
}

/// What compiling a cassette produced; immutable, so snapshots share it instead of copying and recompiling.
struct Prepared {
    cassette: Cassette,
    matcher: Matcher,
    /// One key per exchange, computed at registration.
    keys: Vec<Key>,
    /// Exchange indexes (ascending) by key, for the exchanges without an `ignore` of their own.
    index: HashMap<Key, Vec<usize>>,
    /// The exchanges with their own `ignore`: their key depends on the live request, so they are scanned.
    custom: Vec<usize>,
    /// Exchange indexes (ascending) by normalized last user text, to narrow a fuzzy lookup to its candidates.
    by_last_user: HashMap<Option<String>, Vec<usize>>,
}

/// Exchanges the in-memory `recording` keeps; older ones are dropped (a `--record` file is not trimmed).
pub const MAX_RECORDING: usize = 10_000;

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Prepared({:?})", self.cassette.name)
    }
}

struct Compiled {
    prep: Arc<Prepared>,
    consumed: Vec<bool>,
    hits: u64,
}

impl std::ops::Deref for Compiled {
    type Target = Prepared;
    fn deref(&self) -> &Prepared {
        &self.prep
    }
}

/// Everything a snapshot keeps of the registry (docs/m4-spec.md section 8).
#[derive(Clone, Debug, Default)]
pub struct CassetteState {
    pub cassettes: Vec<CassetteEntryState>,
    pub recording: Option<Arc<Cassette>>,
}

#[derive(Clone, Debug)]
pub struct CassetteEntryState {
    prep: Arc<Prepared>,
    pub consumed: Vec<bool>,
    pub hits: u64,
}

impl Compiled {
    fn build(cassette: Cassette) -> Result<Self, CassetteError> {
        cassette.validate()?;
        let matcher = Matcher::compile(&cassette)?;
        let keys: Vec<Key> = cassette.exchanges.iter().map(|e| matcher.key(&e.request, &e.ignore)).collect();
        let mut index: HashMap<Key, Vec<usize>> = HashMap::new();
        let mut custom = Vec::new();
        for (i, e) in cassette.exchanges.iter().enumerate() {
            if e.ignore.is_empty() {
                index.entry(keys[i].clone()).or_default().push(i);
            } else {
                custom.push(i);
            }
        }
        let mut by_last_user: HashMap<Option<String>, Vec<usize>> = HashMap::new();
        if cassette.fuzzy {
            for (i, e) in cassette.exchanges.iter().enumerate() {
                by_last_user.entry(matcher.last_user(&e.request)).or_default().push(i);
            }
        }
        let consumed = vec![false; cassette.exchanges.len()];
        Ok(Self {
            prep: Arc::new(Prepared {
                cassette,
                matcher,
                keys,
                index,
                custom,
                by_last_user,
            }),
            consumed,
            hits: 0,
        })
    }
}

fn build_all(items: Vec<Cassette>) -> Result<Vec<Compiled>, CassetteError> {
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = items.iter().find(|c| !seen.insert(c.name.as_str())) {
        return Err(CassetteError(format!("duplicate cassette name {:?}", dup.name)));
    }
    items.into_iter().map(Compiled::build).collect()
}

impl Cassettes {
    /// Replaces all registered cassettes (counters restart; the recording is untouched). Fails without changing
    /// anything if one is invalid or two share a name.
    pub fn replace(&self, items: Vec<Cassette>) -> Result<(), CassetteError> {
        let compiled = build_all(items)?;
        self.inner.lock().cassettes = compiled;
        Ok(())
    }

    /// Adds cassettes, replacing a registered one of the same name in place (its counters restart).
    pub fn upsert(&self, items: Vec<Cassette>) -> Result<(), CassetteError> {
        let compiled = build_all(items)?;
        let mut state = self.inner.lock();
        for item in compiled {
            match state.cassettes.iter_mut().find(|c| c.cassette.name == item.cassette.name) {
                Some(slot) => *slot = item,
                None => state.cassettes.push(item),
            }
        }
        Ok(())
    }

    /// Removes one cassette (or, for `recording`, empties the recording); `false` when there was none by that name.
    pub fn remove(&self, name: &str) -> bool {
        let mut state = self.inner.lock();
        if name == RECORDING {
            return state.recording.take().is_some();
        }
        let before = state.cassettes.len();
        state.cassettes.retain(|c| c.cassette.name != name);
        state.cassettes.len() != before
    }

    /// Removes all registered cassettes; the recording stays (see [`Cassettes::clear_all`]).
    pub fn clear(&self) {
        self.inner.lock().cassettes.clear();
    }

    /// Removes the registered cassettes and the recording; what `reset` does.
    pub fn clear_all(&self) {
        let mut state = self.inner.lock();
        state.cassettes.clear();
        state.recording = None;
    }

    /// Registered cassettes, not counting the recording.
    pub fn len(&self) -> usize {
        self.inner.lock().cassettes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A registered cassette, or the recording for the name `recording` (`None` until something was recorded).
    pub fn get(&self, name: &str) -> Option<Cassette> {
        let state = self.inner.lock();
        if name == RECORDING {
            return state.recording.as_deref().cloned();
        }
        state.cassettes.iter().find(|c| c.cassette.name == name).map(|c| c.cassette.clone())
    }

    /// Registered cassettes in order, then the recording if it exists.
    pub fn list(&self) -> Vec<CassetteInfo> {
        let state = self.inner.lock();
        let registered = state.cassettes.iter().map(|c| CassetteInfo {
            name: c.cassette.name.clone(),
            exchanges: c.cassette.exchanges.len(),
            once: c.cassette.once,
            fuzzy: c.cassette.fuzzy,
            hits: c.hits,
            consumed: c.consumed.iter().filter(|&&spent| spent).count(),
            recording: false,
        });
        let recording = state.recording.iter().map(|r| CassetteInfo {
            name: RECORDING.into(),
            exchanges: r.exchanges.len(),
            once: false,
            fuzzy: false,
            hits: 0,
            consumed: 0,
            recording: true,
        });
        registered.chain(recording).collect()
    }

    /// The exchange that answers `req`, per docs/m4-spec.md section 3.4, or `None` (the request then falls through
    /// to the upstream, the default reply or strict mode). Counts the hit and, for a `once` cassette, spends the
    /// exchange. Embeddings requests are never answered from a cassette. Returns at once when no cassette is
    /// registered.
    pub fn lookup(&self, req: &Request) -> Option<Replay> {
        if req.protocol == Protocol::OpenaiEmbeddings {
            return None;
        }
        let mut state = self.inner.lock();
        if state.cassettes.is_empty() {
            return None;
        }
        let mut found: Option<(usize, usize)> = None;
        for (ci, c) in state.cassettes.iter().enumerate() {
            if !c.matcher.accepts_model(&req.model) {
                continue;
            }
            let free = |ei: &usize| !(c.cassette.once && c.consumed[*ei]);
            // The first unspent exchange with an equal key: from the index for the plain ones, by scan for those with
            // their own ignore list; the earlier of the two wins, as in a scan of all exchanges in order.
            let plain = c.index.get(&c.matcher.key(req, &[])).and_then(|v| v.iter().copied().find(free));
            let own = c
                .custom
                .iter()
                .copied()
                .filter(free)
                .take_while(|ei| plain.is_none_or(|p| *ei < p))
                .find(|&ei| c.matcher.key(req, &c.cassette.exchanges[ei].ignore) == c.keys[ei]);
            if let Some(ei) = own.or(plain) {
                found = Some((ci, ei));
                break;
            }
        }
        if found.is_none() {
            let mut best: Option<(usize, usize, usize)> = None;
            for (ci, c) in state.cassettes.iter().enumerate() {
                if !c.cassette.fuzzy || !c.matcher.accepts_model(&req.model) {
                    continue;
                }
                let Some(candidates) = c.by_last_user.get(&c.matcher.last_user(req)) else {
                    continue;
                };
                for &ei in candidates {
                    if c.consumed[ei] && c.cassette.once {
                        continue;
                    }
                    let Some(score) = c.matcher.similarity_same_last(req, &c.cassette.exchanges[ei].request) else {
                        continue;
                    };
                    if best.is_none_or(|(s, _, _)| score > s) {
                        best = Some((score, ci, ei));
                    }
                }
            }
            found = best.map(|(_, ci, ei)| (ci, ei));
        }
        let (ci, ei) = found?;
        let c = &mut state.cassettes[ci];
        c.hits += 1;
        if c.cassette.once {
            c.consumed[ei] = true;
        }
        Some(Replay {
            cassette: c.cassette.name.clone(),
            index: ei,
            outcome: c.cassette.exchanges[ei].outcome.clone(),
        })
    }

    /// Appends an exchange to the `recording` cassette, creating it (`recorded_at` now on the host clock, `upstream` as
    /// given to [`Cassettes::start_recording`]) when absent.
    pub fn record(&self, exchange: Exchange) {
        let mut state = self.inner.lock();
        let upstream = state.upstream.clone();
        let recording = state.recording.get_or_insert_with(|| {
            let mut c = Cassette::new(RECORDING);
            c.recorded_at = Some(crate::control::fmt_time(std::time::SystemTime::now()));
            c.upstream = upstream;
            Arc::new(c)
        });
        let recording = Arc::make_mut(recording);
        recording.exchanges.push(exchange);
        if recording.exchanges.len() > MAX_RECORDING {
            // The oldest go first; the `--record` file keeps everything.
            let excess = recording.exchanges.len() - MAX_RECORDING;
            recording.exchanges.drain(..excess);
        }
    }

    /// Sets the metadata (`upstream` URL) the recording gets when it is created; called once by the server at start.
    pub fn start_recording(&self, upstream: Option<String>) {
        self.inner.lock().upstream = upstream;
    }

    /// The recording so far (empty, named `recording`, when nothing was recorded).
    pub fn recording(&self) -> Cassette {
        self.inner
            .lock()
            .recording
            .as_deref()
            .cloned()
            .unwrap_or_else(|| Cassette::new(RECORDING))
    }

    /// For snapshots: the registered cassettes with their counters, and the recording.
    pub fn export_state(&self) -> CassetteState {
        let state = self.inner.lock();
        CassetteState {
            cassettes: state
                .cassettes
                .iter()
                .map(|c| CassetteEntryState {
                    prep: c.prep.clone(),
                    consumed: c.consumed.clone(),
                    hits: c.hits,
                })
                .collect(),
            recording: state.recording.clone(),
        }
    }

    /// For snapshots: replaces the registry (sharing the compiled cassettes) and the recording.
    pub fn import_state(&self, state: CassetteState) -> Result<(), CassetteError> {
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = state.cassettes.iter().find(|e| !seen.insert(e.prep.cassette.name.as_str())) {
            return Err(CassetteError(format!("duplicate cassette name {:?}", dup.prep.cassette.name)));
        }
        let compiled = state
            .cassettes
            .into_iter()
            .map(|entry| {
                let mut consumed = vec![false; entry.prep.cassette.exchanges.len()];
                for (slot, spent) in consumed.iter_mut().zip(entry.consumed) {
                    *slot = spent;
                }
                Ok(Compiled {
                    prep: entry.prep,
                    consumed,
                    hits: entry.hits,
                })
            })
            .collect::<Result<Vec<_>, CassetteError>>()?;
        let mut inner = self.inner.lock();
        inner.cassettes = compiled;
        inner.recording = state.recording;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassette::format::{CassetteFormat, ExchangeMeta, ToTrajectory};
    use crate::ir::{ApiError, Message, Reply, Role, ToolCall};
    use serde_json::{Map, json};

    fn request(texts: &[&str]) -> Request {
        let messages = texts
            .iter()
            .enumerate()
            .map(|(i, t)| Message::text(if i % 2 == 0 { Role::User } else { Role::Assistant }, *t))
            .collect();
        Request::new("gpt-4o", messages)
    }

    fn reply_exchange(texts: &[&str], answer: &str) -> Exchange {
        Exchange {
            request: request(texts),
            outcome: CassetteOutcome::Reply {
                reply: Reply {
                    text: Some(answer.into()),
                    ..Reply::default()
                },
            },
            protocol: Protocol::OpenaiChat,
            meta: ExchangeMeta::default(),
            ignore: Vec::new(),
        }
    }

    fn text_of(replay: &Replay) -> String {
        match &replay.outcome {
            CassetteOutcome::Reply { reply } => reply.text.clone().unwrap_or_default(),
            CassetteOutcome::Error { error } => error.message.clone(),
        }
    }

    fn cassette(name: &str, exchanges: Vec<Exchange>) -> Cassette {
        let mut c = Cassette::new(name);
        c.exchanges = exchanges;
        c
    }

    #[test]
    fn exact_lookup_ignores_sampling_and_counts_hits() {
        let reg = Cassettes::default();
        reg.replace(vec![cassette(
            "a",
            vec![reply_exchange(&["hi"], "yo"), reply_exchange(&["bye"], "ciao")],
        )])
        .unwrap();
        let mut req = request(&["bye"]);
        req.temperature = Some(0.9);
        req.model = "other".into();
        let hit = reg.lookup(&req).unwrap();
        assert_eq!((hit.cassette.as_str(), hit.index, text_of(&hit).as_str()), ("a", 1, "ciao"));
        assert!(reg.lookup(&request(&["unknown"])).is_none());
        reg.lookup(&req).unwrap();
        let info = &reg.list()[0];
        assert_eq!((info.hits, info.consumed), (2, 0));
        let mut emb = req.clone();
        emb.protocol = Protocol::OpenaiEmbeddings;
        assert!(reg.lookup(&emb).is_none());
    }

    #[test]
    fn once_spends_exchanges_in_recording_order() {
        let reg = Cassettes::default();
        let mut c = cassette("a", vec![reply_exchange(&["hi"], "first"), reply_exchange(&["hi"], "second")]);
        c.once = true;
        reg.replace(vec![c]).unwrap();
        let texts: Vec<Option<String>> = (0..3).map(|_| reg.lookup(&request(&["hi"])).map(|r| text_of(&r))).collect();
        assert_eq!(texts, vec![Some("first".into()), Some("second".into()), None]);
        assert_eq!(reg.list()[0].consumed, 2);
        let mut plain = cassette("p", vec![reply_exchange(&["hi"], "first"), reply_exchange(&["hi"], "second")]);
        plain.once = false;
        reg.replace(vec![plain]).unwrap();
        for _ in 0..3 {
            assert_eq!(text_of(&reg.lookup(&request(&["hi"])).unwrap()), "first");
        }
    }

    #[test]
    fn fuzzy_picks_the_closest_and_needs_the_flag() {
        let exchanges = vec![reply_exchange(&["a", "x", "go"], "far"), reply_exchange(&["a", "b", "go"], "near")];
        let reg = Cassettes::default();
        let mut c = cassette("f", exchanges.clone());
        reg.replace(vec![c.clone()]).unwrap();
        assert!(reg.lookup(&request(&["a", "b", "go"])).is_some());
        assert!(reg.lookup(&request(&["a", "b2", "go"])).is_none());
        c.fuzzy = true;
        reg.replace(vec![c]).unwrap();
        let hit = reg.lookup(&request(&["z", "b", "go"])).unwrap();
        assert_eq!(text_of(&hit), "near");
        assert!(reg.lookup(&request(&["a", "b", "stop"])).is_none());
        assert!(reg.lookup(&request(&["go"])).is_none());
    }

    #[test]
    fn model_glob_and_cassette_order() {
        let reg = Cassettes::default();
        let mut first = cassette("first", vec![reply_exchange(&["hi"], "one")]);
        first.model_glob = "claude-*".into();
        let second = cassette("second", vec![reply_exchange(&["hi"], "two")]);
        reg.replace(vec![first, second]).unwrap();
        assert_eq!(reg.lookup(&request(&["hi"])).unwrap().cassette, "second");
        let mut req = request(&["hi"]);
        req.model = "claude-3".into();
        assert_eq!(reg.lookup(&req).unwrap().cassette, "first");
    }

    #[test]
    fn exchange_ignore_applies_to_that_exchange_only() {
        let reg = Cassettes::default();
        let mut e = reply_exchange(&["hi"], "yo");
        e.request.stop = vec!["END".into()];
        e.ignore = vec!["stop".into()];
        reg.replace(vec![cassette("a", vec![e])]).unwrap();
        assert!(reg.lookup(&request(&["hi"])).is_some());
    }

    #[test]
    fn registration_is_all_or_nothing() {
        let reg = Cassettes::default();
        reg.replace(vec![cassette("keep", vec![])]).unwrap();
        let mut bad = cassette("bad", vec![reply_exchange(&["hi"], "x")]);
        bad.normalize = vec![crate::cassette::format::Normalizer {
            pattern: "(".into(),
            replace: String::new(),
        }];
        assert!(reg.replace(vec![cassette("new", vec![]), bad]).is_err());
        assert!(reg.replace(vec![cassette("d", vec![]), cassette("d", vec![])]).is_err());
        assert!(reg.upsert(vec![cassette("recording", vec![])]).is_err());
        assert!(reg.upsert(vec![cassette("x/y", vec![])]).is_err());
        let mut bad_ignore = cassette("i", vec![]);
        bad_ignore.ignore = vec!["model".into()];
        assert!(reg.upsert(vec![bad_ignore]).is_err());
        assert_eq!(reg.list().iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["keep"]);
        let mut again = cassette("keep", vec![reply_exchange(&["hi"], "x")]);
        again.once = true;
        reg.upsert(vec![again, cassette("more", vec![])]).unwrap();
        assert_eq!(reg.list().len(), 2);
        assert_eq!(reg.list()[0].exchanges, 1);
        let mut empty = cassette("e", vec![reply_exchange(&["hi"], "x")]);
        empty.exchanges[0].request.messages.clear();
        assert!(empty.validate().unwrap_err().0.contains("exchange 0"));
    }

    #[test]
    fn state_round_trip_keeps_counters() {
        let reg = Cassettes::default();
        let mut c = cassette("a", vec![reply_exchange(&["hi"], "one"), reply_exchange(&["hi"], "two")]);
        c.once = true;
        reg.replace(vec![c]).unwrap();
        reg.lookup(&request(&["hi"])).unwrap();
        reg.record(reply_exchange(&["rec"], "r"));
        let state = reg.export_state();
        reg.clear_all();
        assert!(reg.is_empty());
        reg.import_state(state).unwrap();
        assert_eq!(text_of(&reg.lookup(&request(&["hi"])).unwrap()), "two");
        assert_eq!(reg.recording().exchanges.len(), 1);
        assert_eq!(reg.list()[0].hits, 2);
    }

    #[test]
    fn parses_json_jsonl_and_rejects_trajectories() {
        let mut c = cassette("a", vec![reply_exchange(&["hi"], "yo")]);
        c.exchanges[0].meta.model = "gpt-4o".into();
        let two = cassette("b", vec![reply_exchange(&["x"], "y")]);
        for format in [CassetteFormat::Json, CassetteFormat::Auto] {
            assert_eq!(Cassette::parse_many(&c.to_json().to_string(), format).unwrap(), vec![c.clone()]);
        }
        let jsonl = format!(
            "{}
{}",
            c.to_jsonl(),
            two.to_jsonl()
        );
        let mut expected_two = two.clone();
        expected_two.exchanges[0].meta.model = "gpt-4o".into();
        for format in [CassetteFormat::Jsonl, CassetteFormat::Auto] {
            assert_eq!(Cassette::parse_many(&jsonl, format).unwrap(), vec![c.clone(), expected_two.clone()]);
        }
        let wrapped = json!({"cassettes": [c.to_json()]}).to_string();
        assert_eq!(Cassette::parse_many(&wrapped, CassetteFormat::Auto).unwrap().len(), 1);
        let yaml = "name: y\nexchanges:\n  - request: {model: m, messages: [{role: user, content: [{type: text, text: hi}]}]}\n    outcome: {outcome: reply, reply: {text: yo, finish: stop, usage: {prompt_tokens: 1, completion_tokens: 1}}}\n";
        let parsed = Cassette::parse_many(yaml, CassetteFormat::Auto).unwrap();
        assert_eq!(parsed[0].exchanges[0].meta.model, "m");
        let bare = "{\"kind\":\"cassette\",\"name\":\"n\"}\n\n{\"request\":{\"model\":\"m\",\"protocol\":\"anthropic.messages\",\"messages\":[]},\"outcome\":{\"outcome\":\"error\",\"error\":{\"status\":500,\"kind\":\"server_error\",\"message\":\"x\"}}}\n";
        let parsed = Cassette::parse_many(bare, CassetteFormat::Jsonl).unwrap();
        assert_eq!(parsed[0].exchanges[0].protocol, Protocol::AnthropicMessages);
        let traj = "{\"kind\":\"trajectory\",\"name\":\"t\",\"steps\":[]}";
        assert!(
            Cassette::parse_many(traj, CassetteFormat::Auto)
                .unwrap_err()
                .0
                .contains("trajectory")
        );
        let orphan = "{\"kind\":\"cassette\"}\n{\"request\":{}}";
        assert!(Cassette::parse_many(orphan, CassetteFormat::Jsonl).is_err());
        assert!(Cassette::parse_many("{\"kind\":\"cassette\",\"name\":\"n\",\"bogus\":1}", CassetteFormat::Json).is_err());
    }

    fn tool_exchange(texts: &[&str], call: Option<&str>, answer: &str) -> Exchange {
        let mut e = reply_exchange(texts, answer);
        if let (Some(id), CassetteOutcome::Reply { reply }) = (call, &mut e.outcome) {
            reply.tool_calls = vec![ToolCall {
                id: id.into(),
                name: "ls".into(),
                arguments: "{}".into(),
            }];
        }
        e
    }

    #[test]
    fn to_trajectory_keeps_one_exchange_per_turn() {
        let first = tool_exchange(&["fix"], Some("c1"), "");
        let mut second = reply_exchange(&["fix", "", "out"], "done");
        second.request.messages[1].tool_calls = vec![ToolCall {
            id: "c1".into(),
            name: "ls".into(),
            arguments: "{}".into(),
        }];
        second.request.messages[2].role = Role::Tool;
        second.request.messages[2].tool_call_id = Some("c1".into());
        let title = reply_exchange(&["write a title"], "Title");
        let retry = tool_exchange(&["fix"], Some("c1"), "");
        let mut c = cassette("rec", vec![first, title, retry, second]);
        c.upstream = Some("http://up".into());
        let t = c
            .to_trajectory(&ToTrajectory {
                with_expect: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t.name, "rec");
        assert_eq!(t.steps.len(), 2);
        assert_eq!(t.steps[0].reply.tool_calls.as_ref().unwrap()[0].id.as_deref(), Some("c1"));
        assert!(t.steps[0].expect.is_none());
        let expect = t.steps[1].expect.as_ref().unwrap();
        assert_eq!(
            (expect.tool_results[0].name.as_deref(), expect.tool_results[0].id.as_deref()),
            (Some("ls"), Some("c1"))
        );
        assert_eq!(t.meta["source"], "cassette");
        assert_eq!(t.meta["upstream"], "http://up");
        t.validate().unwrap();
        let plain = c
            .to_trajectory(&ToTrajectory {
                name: Some("n".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(plain.steps[1].expect.is_none() && plain.name == "n");
        let none = c.to_trajectory(&ToTrajectory {
            model: Some("claude*".into()),
            ..Default::default()
        });
        assert!(none.unwrap_err().0.contains("starts a conversation"));
    }

    #[test]
    fn to_trajectory_errors_only_when_asked() {
        let mut err = reply_exchange(&["hi"], "");
        err.outcome = CassetteOutcome::Error {
            error: ApiError::new(429, "rate_limit_error", "slow"),
        };
        let ok = reply_exchange(&["hi"], "yo");
        let c = cassette("e", vec![err, ok]);
        let t = c.to_trajectory(&ToTrajectory::default()).unwrap();
        assert_eq!(t.steps[0].reply.text.as_deref(), Some("yo"));
        let t = c
            .to_trajectory(&ToTrajectory {
                keep_errors: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(t.steps.len(), 1);
        assert_eq!(t.steps[0].reply.error.as_ref().unwrap().status, 429);
        let _ = Map::new();
    }
}
