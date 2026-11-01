//! Hot reload of the files given at startup (`--scripts`, `--trajectory`, `--policy`, `--cassette`): the files'
//! modification times are polled every 500 ms (no filesystem-notification dependency) and a changed kind is
//! re-read and its registry replaced, hit counters starting over. See docs/m4-spec.md section 9.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;

use crate::agent::trajectory::{Trajectory, TrajectoryFormat};
use crate::cassette::format::{Cassette, CassetteFormat};
use crate::control::Control;
use crate::policy::PolicySpec;
use crate::script::{Rule, ScriptFormat};

/// How often the files are checked.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The files to watch, by what they hold. `Config::watch` is `None` for no watching at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchConfig {
    pub scripts: Option<PathBuf>,
    pub trajectories: Vec<PathBuf>,
    pub policies: Vec<PathBuf>,
    pub cassettes: Vec<PathBuf>,
}

impl WatchConfig {
    pub fn is_empty(&self) -> bool {
        self.scripts.is_none() && self.trajectories.is_empty() && self.policies.is_empty() && self.cassettes.is_empty()
    }
}

/// Starts the polling task. The files' current stamps (mtime and length) are the baseline, so only later changes
/// reload. A kind reloads when any of its files changed: all of its files are read again and the registry is replaced
/// (`Scripts::replace`, `Trajectories::replace`, `Policies::replace`, `Cassettes::replace`; policies restart). A file
/// that cannot be read or parsed leaves the registry as it was and logs a warning; the same broken content is not
/// retried until the file changes again. Every successful reload logs one line at `info` level on target
/// `fakellm::watch` (`reloaded scripts from rules.yaml: 3 rules`). The task ends when aborted (the server aborts it on
/// shutdown).
pub(crate) fn spawn(control: Arc<Control>, config: WatchConfig) -> JoinHandle<()> {
    let mut watcher = Watcher::new(&config);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            watcher.tick(&control).await;
        }
    })
}

/// Modification time and length; `None` when the file cannot be stat'ed (missing, or being replaced).
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// The files of one kind and the stamps last seen (or last tried, when the content was bad).
struct Group {
    files: Vec<PathBuf>,
    seen: Vec<Stamp>,
    /// Whether a read error was already logged for the current attempt (a retry every tick must not log every tick).
    warned: bool,
}

impl Group {
    fn new(files: Vec<PathBuf>) -> Self {
        let seen = files.iter().map(|f| stamp(f)).collect();
        Self {
            files,
            seen,
            warned: false,
        }
    }

    /// The new stamps when something changed and all files can be stat'ed; `None` otherwise (an unreadable file is
    /// tried again on the next tick).
    fn changed(&self) -> Option<Vec<Stamp>> {
        let now: Vec<Stamp> = self.files.iter().map(|f| stamp(f)).collect();
        (now != self.seen && now.iter().all(Option::is_some)).then_some(now)
    }

    fn names(&self) -> String {
        self.files
            .iter()
            .map(|f| {
                f.file_name()
                    .map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

struct Watcher {
    scripts: Group,
    trajectories: Group,
    policies: Group,
    cassettes: Group,
}

/// What reading a kind's files gave. `Retry`: an I/O error, the stamp is not recorded.
enum Failure {
    Retry(String),
    Bad(String),
}

fn ext(path: &Path) -> Option<&str> {
    path.extension().and_then(|e| e.to_str())
}

async fn read(path: &Path) -> Result<String, Failure> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| Failure::Retry(format!("{}: {e}", path.display())))?;
    // A truncate-then-write save passes through an empty file; wait for the content.
    if text.trim().is_empty() {
        return Err(Failure::Retry(format!("{}: file is empty", path.display())));
    }
    Ok(text)
}

fn bad<E: std::fmt::Display>(path: &Path) -> impl Fn(E) -> Failure + '_ {
    move |e| Failure::Bad(format!("{}: {e}", path.display()))
}

async fn load_scripts(files: &[PathBuf]) -> Result<Vec<Rule>, Failure> {
    let mut out = Vec::new();
    for path in files {
        let format = match ext(path) {
            Some("json") => ScriptFormat::Json,
            Some("yaml" | "yml") => ScriptFormat::Yaml,
            _ => ScriptFormat::Auto,
        };
        out.extend(Rule::parse_many(&read(path).await?, format).map_err(bad(path))?);
    }
    Ok(out)
}

async fn load_trajectories(files: &[PathBuf]) -> Result<Vec<Trajectory>, Failure> {
    let mut out = Vec::new();
    for path in files {
        let format = match ext(path) {
            Some("json") => TrajectoryFormat::Json,
            Some("jsonl" | "ndjson") => TrajectoryFormat::Jsonl,
            Some("yaml" | "yml") => TrajectoryFormat::Yaml,
            _ => TrajectoryFormat::Auto,
        };
        out.extend(Trajectory::parse_many(&read(path).await?, format).map_err(bad(path))?);
    }
    Ok(out)
}

async fn load_cassettes(files: &[PathBuf]) -> Result<Vec<Cassette>, Failure> {
    let mut out = Vec::new();
    for path in files {
        let format = match ext(path) {
            Some("json") => CassetteFormat::Json,
            Some("jsonl" | "ndjson") => CassetteFormat::Jsonl,
            Some("yaml" | "yml") => CassetteFormat::Yaml,
            _ => CassetteFormat::Auto,
        };
        out.extend(Cassette::parse_many(&read(path).await?, format).map_err(bad(path))?);
    }
    Ok(out)
}

async fn load_policies(files: &[PathBuf]) -> Result<Vec<PolicySpec>, Failure> {
    files
        .iter()
        .map(|p| PolicySpec::from_path(p).map_err(|e| Failure::Retry(format!("{}: {e}", p.display()))))
        .collect()
}

/// Logs a failure and says whether the new stamps should be recorded.
fn failed(kind: &str, group: &mut Group, stamps: Vec<Stamp>, f: Failure) {
    match f {
        Failure::Retry(m) => {
            if !std::mem::replace(&mut group.warned, true) {
                tracing::warn!(target: "fakellm::watch", "cannot reload {kind}: {m} (retrying)");
            }
        }
        Failure::Bad(m) => {
            group.warned = false;
            tracing::warn!(target: "fakellm::watch", "not reloading {kind}, keeping the previous {kind}: {m}");
            group.seen = stamps;
        }
    }
}

fn committed<E: std::fmt::Display>(kind: &str, group: &mut Group, stamps: Vec<Stamp>, n: usize, unit: &str, r: Result<(), E>) {
    match r {
        Ok(()) => {
            tracing::info!(target: "fakellm::watch", "reloaded {kind} from {}: {n} {unit}", group.names());
            group.seen = stamps;
            group.warned = false;
        }
        Err(e) => failed(kind, group, stamps, Failure::Bad(e.to_string())),
    }
}

impl Watcher {
    fn new(config: &WatchConfig) -> Self {
        Self {
            scripts: Group::new(config.scripts.iter().cloned().collect()),
            trajectories: Group::new(config.trajectories.clone()),
            policies: Group::new(config.policies.clone()),
            cassettes: Group::new(config.cassettes.clone()),
        }
    }

    /// One poll. Holds the admin lock from the first read to the last swap, so no control-plane write or restore
    /// interleaves; the swaps themselves happen back to back with nothing awaited in between.
    async fn tick(&mut self, control: &Control) {
        let s = self.scripts.changed();
        let t = self.trajectories.changed();
        let p = self.policies.changed();
        let c = self.cassettes.changed();
        if s.is_none() && t.is_none() && p.is_none() && c.is_none() {
            return;
        }
        let _admin = control.exclusive().await;
        let engine = &control.engine;
        // Stat again under the lock: the stamps recorded for these reads are the ones from just before them, so a
        // write that landed while waiting for the lock is part of this reload, not a second one.
        let (s, t, p, c) = (
            s.and(self.scripts.changed()),
            t.and(self.trajectories.changed()),
            p.and(self.policies.changed()),
            c.and(self.cassettes.changed()),
        );

        let mut scripts = None;
        if let Some(stamps) = s {
            match load_scripts(&self.scripts.files).await {
                Ok(v) => scripts = Some((stamps, v)),
                Err(f) => failed("scripts", &mut self.scripts, stamps, f),
            }
        }
        let mut trajectories = None;
        if let Some(stamps) = t {
            match load_trajectories(&self.trajectories.files).await {
                Ok(v) => trajectories = Some((stamps, v)),
                Err(f) => failed("trajectories", &mut self.trajectories, stamps, f),
            }
        }
        let mut cassettes = None;
        if let Some(stamps) = c {
            match load_cassettes(&self.cassettes.files).await {
                Ok(v) => cassettes = Some((stamps, v)),
                Err(f) => failed("cassettes", &mut self.cassettes, stamps, f),
            }
        }
        let mut policies = None;
        if let Some(stamps) = p {
            let prepared = match load_policies(&self.policies.files).await {
                Ok(specs) => {
                    let n = specs.len();
                    engine
                        .policies
                        .prepare(specs)
                        .await
                        .map(|p| (n, p))
                        .map_err(|e| Failure::Bad(e.to_string()))
                }
                Err(f) => Err(f),
            };
            match prepared {
                Ok((n, prepared)) => policies = Some((stamps, n, prepared)),
                Err(f) => failed("policies", &mut self.policies, stamps, f),
            }
        }

        if let Some((stamps, v)) = scripts {
            let n = v.len();
            committed("scripts", &mut self.scripts, stamps, n, "rules", engine.scripts.replace(v));
        }
        if let Some((stamps, v)) = trajectories {
            let n = v.len();
            committed(
                "trajectories",
                &mut self.trajectories,
                stamps,
                n,
                "trajectories",
                engine.trajectories.replace(v),
            );
        }
        if let Some((stamps, v)) = cassettes {
            let n = v.len();
            committed(
                "cassettes",
                &mut self.cassettes,
                stamps,
                n,
                "cassettes",
                engine.cassettes.replace(v),
            );
        }
        if let Some((stamps, n, prepared)) = policies {
            engine.policies.commit(prepared);
            committed::<std::convert::Infallible>("policies", &mut self.policies, stamps, n, "policies", Ok(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fakellm-watch-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("rules.yaml")
    }

    async fn until(mut ok: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[tokio::test]
    async fn reloads_changed_scripts_and_keeps_the_old_ones_on_a_bad_file() {
        let file = temp("scripts");
        std::fs::write(&file, "- reply: {text: one}\n").unwrap();
        let rules = crate::Rule::parse_many("- reply: {text: one}\n", crate::ScriptFormat::Yaml).unwrap();
        let server = crate::Server::start(crate::Config {
            scripts: rules,
            watch: Some(WatchConfig {
                scripts: Some(file.clone()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .unwrap();
        let scripts = &server.control().engine.scripts;
        assert_eq!(scripts.len(), 1);

        // A longer file, so the length differs even when the mtime granularity is coarse.
        std::fs::write(&file, "- reply: {text: one}\n- reply: {text: two}\n").unwrap();
        assert!(until(|| scripts.len() == 2).await, "not reloaded");

        std::fs::write(&file, "- reply: [this is not a rule, and it is long enough to change the length]\n").unwrap();
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        assert_eq!(scripts.len(), 2);

        std::fs::write(&file, "- reply: {text: three}\n").unwrap();
        assert!(until(|| scripts.len() == 1).await, "not reloaded after the bad file");
        server.shutdown().await;
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn without_watch_config_nothing_reloads() {
        let file = temp("none");
        std::fs::write(&file, "- reply: {text: one}\n").unwrap();
        let rules = crate::Rule::parse_many("- reply: {text: one}\n", crate::ScriptFormat::Yaml).unwrap();
        let server = crate::Server::start(crate::Config {
            scripts: rules,
            ..Default::default()
        })
        .await
        .unwrap();
        std::fs::write(&file, "- reply: {text: a}\n- reply: {text: b}\n").unwrap();
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        assert_eq!(server.control().engine.scripts.len(), 1);
        server.shutdown().await;
        std::fs::remove_dir_all(file.parent().unwrap()).unwrap();
    }
}
