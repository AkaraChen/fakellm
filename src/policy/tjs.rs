//! The in-process policy runtime: txiki.js (QuickJS + libuv) linked into the binary through `tjs-sys`. One dedicated OS thread per policy
//! owns its JavaScript runtime; the async side sends it decisions over a channel and awaits a `oneshot` reply, so a slow or busy policy never
//! blocks a Tokio worker. Stopping a policy cancels whatever the thread is running and joins it (bounded).
//!
//! Without the `tjs` cargo feature this module is a stub whose `Host::start` fails with a message that names the feature.

// The reply types are only constructed by the real host; the stub build still names them in `Policies::decide`.
#![cfg_attr(not(feature = "tjs"), allow(dead_code))]

use std::sync::Arc;

use parking_lot::Mutex;

use crate::policy::PolicyError;
use crate::policy::runtime::PolicyDir;

/// Why a decision produced no answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The policy threw, returned an unusable value or rejected: the message is its own. The runtime is fine.
    Script(String),
    /// No answer within the deadline; the runtime was dropped and the next decision builds a new one.
    Timeout,
    /// The thread died, the runtime panicked or the decision was cancelled (the policy was removed or the request went away).
    Crashed,
    /// The next decision found the runtime gone and could not build a new one.
    Load(String),
}

/// What a decision came back with.
pub struct Reply {
    pub result: Result<Decision, Failure>,
    /// A new runtime was built for this decision (after a timeout, cancel or panic).
    pub restarted: bool,
}

/// `tjs_sys::Decision` without the dependency, for the stub build.
pub enum Decision {
    Reply { reply: String, state: Option<String> },
    Declined { state: Option<String> },
}

/// Cancels the decision a [`Host`] is running; cheap to clone, and safe to call after the host is gone.
#[derive(Clone, Default)]
pub struct CancelHandle(Arc<Mutex<Option<Canceller>>>);

impl CancelHandle {
    pub fn cancel(&self) {
        if let Some(c) = &*self.0.lock() {
            c.cancel();
        }
    }
}

#[cfg(feature = "tjs")]
type Canceller = tjs_sys::Canceller;
#[cfg(not(feature = "tjs"))]
struct Canceller;
#[cfg(not(feature = "tjs"))]
impl Canceller {
    fn cancel(&self) {}
}

#[cfg(feature = "tjs")]
pub use real::Host;
#[cfg(not(feature = "tjs"))]
pub use stub::Host;

#[cfg(not(feature = "tjs"))]
mod stub {
    use super::*;

    pub struct Host;

    impl Host {
        pub async fn start(
            _dir: PolicyDir,
            _entry: &str,
            _timeout: std::time::Duration,
            _on_log: impl Fn(String) + Send + 'static,
        ) -> Result<Host, PolicyError> {
            Err(PolicyError(
                "this build of fakellm has no embedded txiki.js (cargo feature `tjs`): use runtime node or bun".into(),
            ))
        }

        pub async fn decide(&self, _request: String, _state: String, _hits: u64, _timeout: std::time::Duration) -> Reply {
            unreachable!("no Host exists without the `tjs` feature")
        }

        pub fn cancel_handle(&self) -> CancelHandle {
            CancelHandle::default()
        }

        pub fn running(&self) -> bool {
            false
        }

        pub fn exited(&self) -> bool {
            true
        }
    }
}

#[cfg(feature = "tjs")]
mod real {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use tjs_sys::{Error, Options};
    use tokio::sync::oneshot;

    use super::*;

    /// JS heap limit per policy; a runaway allocation is a script error instead of an out-of-memory kill of the server.
    const MEM_LIMIT: usize = 512 << 20;
    /// JS stack limit, and the thread stack that holds it (virtual memory, committed on use).
    const JS_STACK: usize = 8 << 20;
    const THREAD_STACK: usize = 16 << 20;
    /// How long past the policy timeout the async side waits for a thread that does not answer before it gives up on the thread.
    const GRACE: Duration = Duration::from_secs(2);
    /// How long dropping a host waits for its thread.
    const JOIN_TIMEOUT: Duration = Duration::from_secs(3);

    /// Most log lines forwarded per decision; the glue already caps what a policy can queue, this bounds the host side as well.
    const MAX_FLUSH_LINES: usize = 1001;

    enum Command {
        Decide(Box<Job>),
        Stop,
    }

    struct Job {
        request: String,
        state: String,
        hits: u64,
        timeout: Duration,
        reply: oneshot::Sender<Reply>,
    }

    /// One policy's runtime thread, as the async code sees it.
    pub struct Host {
        commands: mpsc::Sender<Command>,
        thread: Option<JoinHandle<()>>,
        cancel: CancelHandle,
        /// The thread holds a usable runtime.
        running: Arc<AtomicBool>,
        /// Last: removed after the thread is gone. The module and its helpers stay for the life of the host, since a policy may `import()`
        /// lazily and the runtime is rebuilt from them after a timeout.
        /// Sticky: set by `stop`, read by the thread before and after it builds a runtime.
        stopping: Arc<AtomicBool>,
        /// Moved to the joiner on drop so it is removed only once the thread is gone.
        dir: Option<PolicyDir>,
    }

    /// What the thread needs to build a runtime again.
    struct Spec {
        dir: PathBuf,
        entry: String,
        load_timeout: Duration,
        on_log: Box<dyn Fn(String) + Send>,
        cancel: CancelHandle,
        running: Arc<AtomicBool>,
        stopping: Arc<AtomicBool>,
    }

    impl Spec {
        /// A runtime with the module loaded. The log lines of the load go to `on_log`, failure included.
        fn build(&self) -> Result<tjs_sys::Runtime, String> {
            if self.stopping.load(Ordering::SeqCst) {
                return Err("the policy is being stopped".into());
            }
            let mut rt = tjs_sys::Runtime::new(Options {
                mem_limit: MEM_LIMIT,
                stack_size: JS_STACK,
            })
            .map_err(|e| e.to_string())?;
            // Published before the load, so cancelling also ends a module that never finishes loading.
            let canceller = rt.canceller();
            *self.cancel.0.lock() = Some(canceller);
            // `stop` may have set the flag and cancelled while no canceller was published yet: cancel again now that there is one.
            if self.stopping.load(Ordering::SeqCst) {
                self.cancel.cancel();
            }
            let loaded = rt.load(&self.dir, &self.entry, Some(self.load_timeout));
            self.flush(&mut rt);
            match loaded {
                Ok(()) => Ok(rt),
                Err(Error::Timeout) => Err("timed out waiting for the policy to load".into()),
                Err(e) => Err(e.to_string()),
            }
        }

        fn flush(&self, rt: &mut tjs_sys::Runtime) {
            for line in rt.take_logs().into_iter().take(MAX_FLUSH_LINES) {
                (self.on_log)(line);
            }
        }

        fn release(&self) {
            *self.cancel.0.lock() = None;
            self.running.store(false, Ordering::SeqCst);
        }
    }

    fn thread_main(spec: Spec, commands: mpsc::Receiver<Command>, ready: oneshot::Sender<Result<(), String>>) {
        let mut slot = match spec.build() {
            Ok(rt) => Some(rt),
            Err(message) => {
                spec.release();
                let _ = ready.send(Err(message));
                return;
            }
        };
        spec.running.store(true, Ordering::SeqCst);
        let _ = ready.send(Ok(()));
        while let Ok(Command::Decide(job)) = commands.recv() {
            if job.reply.is_closed() {
                continue; // the request went away while it waited
            }
            let reply = decide(&spec, &mut slot, &job);
            let _ = job.reply.send(reply);
        }
        spec.release();
        drop(slot);
    }

    fn decide(spec: &Spec, slot: &mut Option<tjs_sys::Runtime>, job: &Job) -> Reply {
        let mut restarted = false;
        if slot.as_ref().is_none_or(|rt| rt.is_broken()) {
            *slot = None;
            match spec.build() {
                Ok(rt) => {
                    *slot = Some(rt);
                    restarted = true;
                    spec.running.store(true, Ordering::SeqCst);
                }
                Err(message) => {
                    spec.release();
                    return Reply {
                        result: Err(Failure::Load(message)),
                        restarted: false,
                    };
                }
            }
        }
        let rt = slot.as_mut().expect("built above");
        let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let called = rt.decide(&job.request, &job.state, job.hits, Some(job.timeout));
            spec.flush(rt);
            called
        }));
        let result = match called {
            Ok(Ok(tjs_sys::Decision::Reply { reply, state })) => Ok(Decision::Reply { reply, state }),
            Ok(Ok(tjs_sys::Decision::Declined { state })) => Ok(Decision::Declined { state }),
            Ok(Err(Error::Script(message))) => Err(Failure::Script(message)),
            Ok(Err(Error::Timeout)) => Err(Failure::Timeout),
            Ok(Err(Error::Cancelled)) => Err(Failure::Crashed),
            Ok(Err(e)) => Err(Failure::Script(e.to_string())),
            Err(_) => {
                // A Rust panic mid-call leaves the C heap in an unknown state: leak it rather than free it.
                std::mem::forget(slot.take());
                Err(Failure::Crashed)
            }
        };
        if result.is_err() && !matches!(result, Err(Failure::Script(_))) {
            *slot = None; // timed out, cancelled or panicked: the next decision starts from a fresh module
            spec.release();
        }
        Reply { result, restarted }
    }

    impl Host {
        /// Starts the thread, builds the runtime and loads `dir/entry` (`dir` holds the policy files and is deleted with the host, also when
        /// this fails). `Err` carries the module's own load error.
        pub async fn start(
            dir: PolicyDir,
            entry: &str,
            timeout: Duration,
            on_log: impl Fn(String) + Send + 'static,
        ) -> Result<Host, PolicyError> {
            let (commands, inbox) = mpsc::channel();
            let (ready, ready_rx) = oneshot::channel();
            let cancel = CancelHandle::default();
            let running = Arc::new(AtomicBool::new(false));
            let stopping = Arc::new(AtomicBool::new(false));
            let spec = Spec {
                dir: dir.0.clone(),
                entry: entry.to_owned(),
                load_timeout: timeout,
                on_log: Box::new(on_log),
                cancel: cancel.clone(),
                running: running.clone(),
                stopping: stopping.clone(),
            };
            let thread = std::thread::Builder::new()
                .name("fakellm-policy".into())
                .stack_size(THREAD_STACK)
                .spawn(move || thread_main(spec, inbox, ready))
                .map_err(|e| PolicyError(format!("cannot start the policy thread: {e}")))?;
            let mut host = Host {
                commands,
                thread: Some(thread),
                cancel,
                running,
                stopping,
                dir: Some(dir),
            };
            match tokio::time::timeout(timeout + GRACE, ready_rx).await {
                Ok(Ok(Ok(()))) => Ok(host),
                Ok(Ok(Err(message))) => {
                    host.stop();
                    Err(PolicyError(message))
                }
                Ok(Err(_)) => {
                    host.stop();
                    Err(PolicyError("the policy thread ended before it was ready".into()))
                }
                Err(_) => {
                    host.stop();
                    Err(PolicyError("timed out waiting for the policy to load".into()))
                }
            }
        }

        /// Runs `decide` on the policy's thread. The glue enforces `timeout` itself; the extra wait here only covers a thread that is stuck
        /// somewhere the interrupt cannot reach, which is then given up on.
        pub async fn decide(&self, request: String, state: String, hits: u64, timeout: Duration) -> Reply {
            let (reply, answer) = oneshot::channel();
            let job = Job {
                request,
                state,
                hits,
                timeout,
                reply,
            };
            let crashed = || Reply {
                result: Err(Failure::Crashed),
                restarted: false,
            };
            if self.commands.send(Command::Decide(Box::new(job))).is_err() {
                return crashed();
            }
            match tokio::time::timeout(timeout + GRACE, answer).await {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) => crashed(),
                Err(_) => {
                    self.cancel.cancel();
                    Reply {
                        result: Err(Failure::Timeout),
                        restarted: false,
                    }
                }
            }
        }

        pub fn cancel_handle(&self) -> CancelHandle {
            self.cancel.clone()
        }

        /// The thread holds a usable runtime.
        pub fn running(&self) -> bool {
            self.running.load(Ordering::SeqCst) && !self.exited()
        }

        /// The thread has ended (it never does by itself once started, so this means it died).
        pub fn exited(&self) -> bool {
            self.thread.as_ref().is_none_or(|t| t.is_finished())
        }

        /// Cancels the running decision and tells the thread to end. The wait for it happens on a detached thread, so dropping a host never
        /// blocks an async worker; the policy directory goes with that thread, once the policy thread is really gone.
        fn stop(&mut self) {
            self.stopping.store(true, Ordering::SeqCst);
            self.cancel.cancel();
            let _ = self.commands.send(Command::Stop);
            let thread = self.thread.take();
            let dir = self.dir.take();
            let Some(thread) = thread else { return };
            let joiner = move || {
                let started = Instant::now();
                let mut warned = false;
                while !thread.is_finished() {
                    if !warned && started.elapsed() >= JOIN_TIMEOUT {
                        warned = true;
                        tracing::warn!(target: "fakellm::policy", "a policy thread did not stop within {JOIN_TIMEOUT:?}; still waiting");
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                let _ = thread.join();
                drop(dir);
            };
            if let Err(e) = std::thread::Builder::new().name("fakellm-policy-join".into()).spawn(joiner) {
                tracing::warn!(target: "fakellm::policy", "cannot start the policy joiner: {e}");
            }
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            self.stop();
        }
    }
}
