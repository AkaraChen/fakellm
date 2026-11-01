//! Snapshots of the control-plane state, for "back to the fixture state before every test" (fakes3 parity):
//! scripts with their hit counters, trajectories with their cursors, policies with their `state`, cassettes with
//! their consumed flags and the recording, the stored Responses API objects and the clock. The request log, other
//! snapshots and the startup configuration are not part of it. Snapshots live in memory and are not exportable.
//! See docs/m4-spec.md section 8.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::agent::trajectory::TrajectoryState;
use crate::cassette::CassetteState;
use crate::control::{Control, ControlError};
use crate::openai::responses_store::StoredResponse;
use crate::policy::PolicyState;
use crate::script::ScriptState;

/// Longest snapshot name.
pub const MAX_NAME: usize = 64;

/// Default for `Config::max_snapshots`: most snapshots held at once; each pins the state it captured
/// (recording, stored responses) in memory.
pub const MAX_SNAPSHOTS: usize = 100;

/// What `POST/GET /_fakellm/snapshots` report about one snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub name: String,
    /// RFC 3339 (server clock) when it was taken.
    pub created: String,
    pub scripts: usize,
    pub trajectories: usize,
    pub policies: usize,
    pub cassettes: usize,
    pub responses: usize,
}

/// The captured state.
pub(crate) struct Snapshot {
    pub(crate) info: SnapshotInfo,
    pub(crate) scripts: Vec<ScriptState>,
    pub(crate) trajectories: Vec<TrajectoryState>,
    pub(crate) policies: Vec<PolicyState>,
    pub(crate) cassettes: CassetteState,
    pub(crate) responses: Vec<Arc<StoredResponse>>,
    pub(crate) clock: i64,
    /// `Control`'s id counter, so ids after a restore repeat those that followed the snapshot.
    pub(crate) counter: u64,
}

/// The named snapshots of one server.
pub struct Snapshots {
    limit: usize,
    inner: Mutex<BTreeMap<String, Arc<Snapshot>>>,
    /// For names made up when `POST` gives none: `snapshot-1`, `snapshot-2`, ...
    counter: AtomicU64,
}

/// 1 to [`MAX_NAME`] characters of `A-Za-z0-9._-`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Default for Snapshots {
    fn default() -> Self {
        Self {
            limit: MAX_SNAPSHOTS,
            inner: Default::default(),
            counter: Default::default(),
        }
    }
}

impl Snapshots {
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// An empty store that refuses a snapshot beyond `limit`.
    pub fn with_limit(limit: usize) -> Self {
        Self { limit, ..Self::default() }
    }

    /// Infos, oldest name first (alphabetical).
    pub fn list(&self) -> Vec<SnapshotInfo> {
        self.inner.lock().values().map(|s| s.info.clone()).collect()
    }

    pub fn info(&self, name: &str) -> Option<SnapshotInfo> {
        self.inner.lock().get(name).map(|s| s.info.clone())
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<Snapshot>> {
        self.inner.lock().get(name).cloned()
    }

    /// Stores `snapshot` under its info's name; `ControlError::Conflict` when the name is taken.
    pub(crate) fn insert(&self, snapshot: Snapshot) -> Result<(), ControlError> {
        let mut inner = self.inner.lock();
        let name = snapshot.info.name.clone();
        if inner.contains_key(&name) {
            return Err(ControlError::Conflict(format!("a snapshot named {name:?} already exists")));
        }
        if inner.len() >= self.limit {
            return Err(ControlError::Conflict(format!(
                "too many snapshots ({}): delete some first, each one holds the state it captured (--max-snapshots raises the limit)",
                self.limit
            )));
        }
        inner.insert(name, Arc::new(snapshot));
        Ok(())
    }

    /// The next free `snapshot-<n>`.
    pub(crate) fn next_name(&self) -> String {
        loop {
            let name = format!("snapshot-{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
            if !self.inner.lock().contains_key(&name) {
                return name;
            }
        }
    }

    /// `false` when there was none by that name.
    pub fn delete(&self, name: &str) -> bool {
        self.inner.lock().remove(name).is_some()
    }

    pub fn clear(&self) {
        self.inner.lock().clear();
    }
}

impl Control {
    /// Captures the current state under `name` (`snapshot-<n>` when `None`). Errors: an invalid name
    /// (`ControlError::BadRequest`), a taken name (`ControlError::Conflict`). Not synchronized with requests in
    /// flight: take snapshots between tests.
    pub async fn snapshot(&self, name: Option<&str>) -> Result<SnapshotInfo, ControlError> {
        let _admin = self.exclusive().await;
        let name = match name {
            Some(n) if valid_name(n) => n.to_string(),
            Some(n) => {
                return Err(ControlError::BadRequest(format!(
                    "invalid snapshot name {n:?}: use 1 to {MAX_NAME} characters of A-Za-z0-9._-"
                )));
            }
            None => self.snapshots.next_name(),
        };
        let scripts = self.engine.scripts.export_state();
        let trajectories = self.engine.trajectories.export_state();
        let policies = self.engine.policies.export_state();
        let cassettes = self.engine.cassettes.export_state();
        let responses = self.responses.export();
        let info = SnapshotInfo {
            name,
            created: time::OffsetDateTime::from(self.clock.now())
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into()),
            scripts: scripts.len(),
            trajectories: trajectories.len(),
            policies: policies.len(),
            cassettes: cassettes.cassettes.len(),
            responses: responses.len(),
        };
        self.snapshots.insert(Snapshot {
            info: info.clone(),
            scripts,
            trajectories,
            policies,
            cassettes,
            responses,
            clock: self.clock.export(),
            counter: self.counter_value(),
        })?;
        Ok(info)
    }

    /// Puts the state of snapshot `name` back (the snapshot stays): scripts, trajectories, policies (fresh
    /// runtimes with the captured `state`), cassettes with their consumed flags and the recording, stored
    /// responses and the clock. `ControlError::NotFound` for an unknown name. If a policy cannot be started again
    /// the error is returned and nothing else is changed (policies are restored first; the imports after it cannot
    /// fail on state that was exported from a running server, so a restore is all or nothing in practice). The id
    /// counter is restored too.
    pub async fn restore(&self, name: &str) -> Result<SnapshotInfo, ControlError> {
        let _admin = self.exclusive().await;
        let snap = self
            .snapshots
            .get(name)
            .ok_or_else(|| ControlError::NotFound(format!("no snapshot named {name:?}")))?;
        self.engine.policies.import_state(snap.policies.clone()).await?;
        self.engine.scripts.import_state(snap.scripts.clone())?;
        self.engine.trajectories.import_state(snap.trajectories.clone())?;
        self.engine.cassettes.import_state(snap.cassettes.clone())?;
        self.responses.import(snap.responses.clone());
        self.clock.import(snap.clock);
        self.set_counter(snap.counter);
        Ok(snap.info.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("seeded") && valid_name("a.b_c-1") && valid_name(&"x".repeat(MAX_NAME)));
        assert!(!valid_name("") && !valid_name("a b") && !valid_name("a/b") && !valid_name(&"x".repeat(MAX_NAME + 1)));
    }

    #[test]
    fn made_up_names_skip_taken_ones() {
        let s = Snapshots::default();
        assert_eq!(s.next_name(), "snapshot-1");
        assert_eq!(s.next_name(), "snapshot-2");
    }

    fn control() -> Control {
        Control::new(&crate::Config::default()).unwrap()
    }

    fn rules(text: &str) -> Vec<crate::Rule> {
        crate::Rule::parse_many(text, crate::ScriptFormat::Yaml).unwrap()
    }

    #[tokio::test]
    async fn restore_puts_scripts_and_clock_back() {
        let c = control();
        c.engine
            .scripts
            .replace(rules("- reply: {text: one}\n- reply: {text: two}\n"))
            .unwrap();
        c.clock
            .set(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_900_000_000))
            .unwrap();
        let info = c.snapshot(Some("base")).await.unwrap();
        assert_eq!((info.name.as_str(), info.scripts), ("base", 2));

        c.engine.scripts.replace(rules("- reply: {text: three}\n")).unwrap();
        c.clock.reset();
        c.restore("base").await.unwrap();
        assert_eq!(c.engine.scripts.len(), 2);
        assert!(c.clock.is_virtual());
        // The snapshot stays, so a second restore works.
        c.engine.scripts.clear();
        c.restore("base").await.unwrap();
        assert_eq!(c.engine.scripts.len(), 2);
    }

    #[tokio::test]
    async fn names_are_checked() {
        let c = control();
        assert!(matches!(c.snapshot(Some("a b")).await, Err(ControlError::BadRequest(_))));
        c.snapshot(Some("x")).await.unwrap();
        assert!(matches!(c.snapshot(Some("x")).await, Err(ControlError::Conflict(_))));
        assert!(matches!(c.restore("nope").await, Err(ControlError::NotFound(_))));
        assert_eq!(c.snapshot(None).await.unwrap().name, "snapshot-1");
    }
}
