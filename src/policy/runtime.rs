//! Which JavaScript runtime runs a policy and how its subprocess is started. All three runtimes run the
//! same protocol through a small embedded shim (`shim_node.js`, `shim_bun.js`, `shim_tjs.js`) that is
//! written, with the user's module, to a temp dir (`fakellm-policy-<pid>-<n>/`) and removed when the policy
//! is. The module path travels in the `FAKELLM_POLICY_MODULE` environment variable.

use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::policy::PolicyError;
use crate::policy::protocol::{self, Incoming};

pub const SHIM_NODE: &str = include_str!("shim_node.js");
pub const SHIM_BUN: &str = include_str!("shim_bun.js");
/// Tested against txiki.js v26.6.0 (`tests/policy_tjs.rs`, gated by `FAKELLM_TJS`).
pub const SHIM_TJS: &str = include_str!("shim_tjs.js");

/// A policy runtime, written `"tjs"`, `"node"`, `"bun"` or a path to an executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Runtime {
    Tjs,
    Node,
    Bun,
    /// Which shim it gets follows the file stem: `tjs` uses the txiki.js shim, `bun` the Bun one,
    /// anything else the Node one.
    Path(PathBuf),
}

impl FromStr for Runtime {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "" => Err("runtime must not be empty".into()),
            "tjs" => Ok(Self::Tjs),
            "node" => Ok(Self::Node),
            "bun" => Ok(Self::Bun),
            path => Ok(Self::Path(PathBuf::from(path))),
        }
    }
}

impl std::fmt::Display for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tjs => f.write_str("tjs"),
            Self::Node => f.write_str("node"),
            Self::Bun => f.write_str("bun"),
            Self::Path(p) => write!(f, "{}", p.display()),
        }
    }
}

impl Serialize for Runtime {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Runtime {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Runtime {
    /// The executable to start: the bare name (resolved through `PATH`) or the given path.
    pub fn program(&self) -> OsString {
        match self {
            Self::Tjs => "tjs".into(),
            Self::Node => "node".into(),
            Self::Bun => "bun".into(),
            Self::Path(p) => p.clone().into_os_string(),
        }
    }

    fn kind(&self) -> Kind {
        let stem = match self {
            Self::Tjs => "tjs",
            Self::Bun => "bun",
            Self::Node => "node",
            Self::Path(p) => p.file_stem().and_then(|s| s.to_str()).unwrap_or("node"),
        };
        match stem {
            "tjs" => Kind::Tjs,
            "bun" => Kind::Bun,
            _ => Kind::Node,
        }
    }

    /// The embedded shim source and the file name it is written under.
    pub fn shim(&self) -> (&'static str, &'static str) {
        match self.kind() {
            Kind::Tjs => (SHIM_TJS, "shim.js"),
            Kind::Bun => (SHIM_BUN, "shim.mjs"),
            Kind::Node => (SHIM_NODE, "shim.mjs"),
        }
    }

    /// Arguments before the shim path: `run` for tjs and bun, none for node.
    pub fn args(&self) -> Vec<OsString> {
        match self.kind() {
            Kind::Tjs | Kind::Bun => vec!["run".into()],
            Kind::Node => Vec::new(),
        }
    }
}

enum Kind {
    Tjs,
    Bun,
    Node,
}

/// The default runtime: `tjs` if an executable of that name is on `PATH`, else `node`, else `bun`.
pub fn detect_runtime() -> Option<Runtime> {
    let path = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let has = |name: &str| dirs.iter().any(|d| d.join(name).is_file());
    [("tjs", Runtime::Tjs), ("node", Runtime::Node), ("bun", Runtime::Bun)]
        .into_iter()
        .find(|(name, _)| has(name))
        .map(|(_, r)| r)
}

/// The temp dir holding a policy's shim and module; deleted on drop.
pub struct PolicyDir(pub PathBuf);

impl Drop for PolicyDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Longest stdout line read from a policy; a longer one is treated as a broken policy.
pub const MAX_LINE: u64 = 16 * 1024 * 1024;

/// `read_line` capped at [`MAX_LINE`]: `Ok(0)` is EOF, an unterminated line at the cap is an error.
pub async fn read_line_bounded(stdout: &mut BufReader<ChildStdout>, buf: &mut String) -> std::io::Result<usize> {
    let n = stdout.take(MAX_LINE).read_line(buf).await?;
    if n as u64 >= MAX_LINE && !buf.ends_with('\n') {
        return Err(std::io::Error::other("policy stdout line too long"));
    }
    Ok(n)
}

impl Process {
    /// Whether the child has already exited (killed, crashed or `process.exit` while idle).
    pub fn exited(&mut self) -> bool {
        !matches!(self._child.try_wait(), Ok(None))
    }
}

/// A running policy subprocess. Dropping it kills the child (`kill_on_drop`) without waiting for it to
/// exit, and only the immediate child: grandchildren a policy starts are not tracked.
pub struct Process {
    pub _child: Child,
    pub stdin: ChildStdin,
    pub stdout: BufReader<ChildStdout>,
    /// Last: removes the temp dir after the child is gone.
    pub _dir: PolicyDir,
}

/// Writes the shim and `source` into a fresh temp dir and starts `runtime` on the shim, then waits
/// (at most `timeout`, host clock) for `{"ready":true}`. Errors carry the module's own load error,
/// a missing-executable message, or "timed out waiting for the policy to load".
/// A `tokio` task forwards the child's stderr lines to `on_log`.
pub async fn spawn(
    runtime: &Runtime,
    source: &str,
    files: &std::collections::BTreeMap<String, String>,
    timeout: std::time::Duration,
    on_log: impl Fn(String) + Send + 'static,
) -> Result<Process, PolicyError> {
    static NEXT_DIR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "fakellm-policy-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let (shim, shim_name) = runtime.shim();
    let module = dir.join("policy.mjs");
    let io = |what: &str, e: std::io::Error| PolicyError(format!("{what}: {e}"));
    std::fs::create_dir_all(&dir).map_err(|e| io("cannot create the policy directory", e))?;
    let dir = PolicyDir(dir);
    std::fs::write(dir.0.join(shim_name), shim).map_err(|e| io("cannot write the policy shim", e))?;
    std::fs::write(&module, source).map_err(|e| io("cannot write the policy module", e))?;
    // `.js` helpers are ES modules too, unless the caller ships its own package.json.
    if !files.contains_key("package.json") {
        std::fs::write(dir.0.join("package.json"), r#"{"type":"module"}"#).map_err(|e| io("cannot write package.json", e))?;
    }
    for (path, content) in files {
        let target = dir.0.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io("cannot create a policy file directory", e))?;
        }
        std::fs::write(&target, content).map_err(|e| io(&format!("cannot write policy file {path:?}"), e))?;
    }

    let mut child = tokio::process::Command::new(runtime.program())
        .args(runtime.args())
        .arg(dir.0.join(shim_name))
        .env("FAKELLM_POLICY_MODULE", &module)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| PolicyError(format!("cannot start {}: {e}", runtime.program().to_string_lossy())))?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let stderr = child.stderr.take().expect("stderr is piped");
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            on_log(line);
        }
    });

    let mut process = Process {
        _child: child,
        stdin,
        stdout,
        _dir: dir,
    };
    let ready = async {
        let mut line = String::new();
        loop {
            line.clear();
            match read_line_bounded(&mut process.stdout, &mut line).await {
                Ok(0) | Err(_) => return Err("the policy process exited before it was ready".to_owned()),
                Ok(_) => {}
            }
            match protocol::parse(line.trim()) {
                Ok(Incoming::Ready) => return Ok(()),
                Ok(Incoming::LoadError(message)) => return Err(message),
                _ => tracing::warn!(target: "fakellm::policy", "ignoring unexpected stdout line while loading: {}", line.trim_end()),
            }
        }
    };
    match tokio::time::timeout(timeout, ready).await {
        Ok(Ok(())) => Ok(process),
        Ok(Err(message)) => Err(PolicyError(message)),
        Err(_) => Err(PolicyError("timed out waiting for the policy to load".into())),
    }
}
