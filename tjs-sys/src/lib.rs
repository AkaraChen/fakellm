//! txiki.js embedded as a library: a JavaScript runtime (QuickJS + libuv + the `tjs` standard library) that runs a fakellm policy module
//! in-process.
//!
//! The contract of the module is the one in `docs/agent.md`: it default-exports `decide(request, ctx)`, where `ctx` is `{state, hits, log}`,
//! and returns a reply object (optionally with a `state` field), `null`/`undefined` or `{decline: true}`. This crate moves JSON strings in
//! and out and does not interpret them.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The raw C API of `csrc/fakellm_tjs.h`.
#[allow(non_camel_case_types)]
pub mod ffi {
    use super::{c_char, c_int, c_void};

    #[repr(C)]
    pub struct fakellm_tjs {
        _private: [u8; 0],
    }

    pub const FAKELLM_TJS_OK: c_int = 0;
    pub const FAKELLM_TJS_DECLINED: c_int = 1;
    pub const FAKELLM_TJS_ERROR: c_int = 2;
    pub const FAKELLM_TJS_TIMEOUT: c_int = 3;
    pub const FAKELLM_TJS_CANCELLED: c_int = 4;

    pub type fakellm_tjs_log_fn = unsafe extern "C" fn(user: *mut c_void, line: *const c_char, len: usize);

    unsafe extern "C" {
        pub fn fakellm_tjs_new(mem_limit: c_int, stack_size: usize) -> *mut fakellm_tjs;
        pub fn fakellm_tjs_set_log(h: *mut fakellm_tjs, f: Option<fakellm_tjs_log_fn>, user: *mut c_void);
        pub fn fakellm_tjs_load(
            h: *mut fakellm_tjs,
            dir: *const c_char,
            entry: *const c_char,
            timeout_ms: c_int,
            err_out: *mut *mut c_char,
        ) -> c_int;
        pub fn fakellm_tjs_decide(
            h: *mut fakellm_tjs,
            request_json: *const c_char,
            state_json: *const c_char,
            hits: i64,
            timeout_ms: c_int,
            reply_out: *mut *mut c_char,
            state_out: *mut *mut c_char,
            err_out: *mut *mut c_char,
        ) -> c_int;
        pub fn fakellm_tjs_cancel(h: *mut fakellm_tjs);
        pub fn fakellm_tjs_free(h: *mut fakellm_tjs);
        pub fn fakellm_tjs_string_free(s: *mut c_char);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The runtime could not be created.
    Create,
    /// The script failed: it threw, the module did not load, or the reply was invalid. Carries the script's own message.
    Script(String),
    /// The deadline passed. The runtime is [broken](Runtime::is_broken) afterwards: drop it and create a new one.
    Timeout,
    /// [`Canceller::cancel`] was called; the runtime is [broken](Runtime::is_broken) for good.
    Cancelled,
    /// An argument contained a NUL byte.
    Argument(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Create => f.write_str("cannot create the JavaScript runtime"),
            Error::Script(message) => f.write_str(message),
            Error::Timeout => f.write_str("the script timed out"),
            Error::Cancelled => f.write_str("the script was cancelled"),
            Error::Argument(what) => write!(f, "{what} contains a NUL byte"),
        }
    }
}

impl std::error::Error for Error {}

/// What the policy decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A reply: the JSON of the returned object without its `state` field, and the JSON of `state` if the policy returned one.
    Reply { reply: String, state: Option<String> },
    /// `null`, `undefined` or `{decline: true}`; `state` is the JSON of `{decline: true, state}`'s state, if any.
    Declined { state: Option<String> },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Memory limit of the JS heap in bytes; 0 means unlimited.
    pub mem_limit: usize,
    /// JS stack limit in bytes; 0 means the txiki.js default (1 or 2 MiB). Calls must run on a thread with a bigger stack.
    pub stack_size: usize,
}

/// Where the C glue delivers log lines. A struct of its own (not a bare `Vec`) so it can be boxed: the box keeps its address while the
/// `Runtime` moves.
#[derive(Default)]
struct LogSink {
    lines: Vec<String>,
}

/// One JavaScript runtime holding one policy module.
///
/// QuickJS runtimes are single-threaded: nothing inside may run concurrently, and libuv's loop is not thread safe either. So a `Runtime` is
/// `Send` (all its state lives in the C heap and the C glue re-anchors QuickJS's stack check on every call, which is what makes moving it
/// between threads sound) but not `Sync`, and every method takes `&mut self`. Distinct runtimes are fully independent and may run on
/// different threads at the same time.
pub struct Runtime {
    raw: NonNull<ffi::fakellm_tjs>,
    cancel: Arc<CancelState>,
    /// Log lines; the C glue pushes into it while a method runs. Boxed so its address survives moves of the `Runtime`.
    logs: Box<LogSink>,
    broken: bool,
}

/// What a [`Canceller`] shares with its [`Runtime`].
struct CancelState {
    /// `cancel` was called; checked without the lock so a caller can ask before it starts a call.
    requested: AtomicBool,
    /// The handle while the runtime lives. Held across `fakellm_tjs_cancel` and cleared by `Drop` under the same lock, which is what makes
    /// cancelling from another thread safe against the handle being freed.
    handle: Mutex<Option<NonNull<ffi::fakellm_tjs>>>,
}

// SAFETY: the pointer is only dereferenced by the C glue under the mutex, and `fakellm_tjs_cancel` is the one C function that is thread safe.
unsafe impl Send for CancelState {}
unsafe impl Sync for CancelState {}

/// Interrupts the calls of a [`Runtime`] from any thread, even while it is busy. Cheap to clone.
#[derive(Clone)]
pub struct Canceller(Arc<CancelState>);

impl Canceller {
    /// Ends the running call (a runaway loop or a pending await) and makes every later call fail with [`Error::Cancelled`]. Permanent: drop
    /// the runtime afterwards. Does nothing once the runtime is gone.
    pub fn cancel(&self) {
        self.0.requested.store(true, Ordering::SeqCst);
        if let Some(handle) = *self.0.handle.lock().unwrap_or_else(|e| e.into_inner()) {
            // SAFETY: the handle is alive: `Runtime::drop` clears it under this lock before freeing.
            unsafe { ffi::fakellm_tjs_cancel(handle.as_ptr()) };
        }
    }
}

// SAFETY: see the type's documentation. The raw handle is owned exclusively, and it is only touched through `&mut self`.
unsafe impl Send for Runtime {}

unsafe extern "C" fn collect_log(user: *mut c_void, line: *const c_char, len: usize) {
    // SAFETY: `user` is the `LogSink` boxed in the Runtime that registered this callback and is only invoked inside its `&mut self` methods;
    // `line` points to `len` bytes.
    let (logs, bytes) = unsafe { (&mut *(user as *mut LogSink), std::slice::from_raw_parts(line as *const u8, len)) };
    logs.lines.push(String::from_utf8_lossy(bytes).into_owned());
}

fn timeout_ms(timeout: Option<Duration>) -> c_int {
    // 0 means "no deadline" to the C side, so a zero duration still gets 1 ms.
    timeout.map_or(0, |d| d.as_millis().clamp(1, c_int::MAX as u128) as c_int)
}

/// Takes ownership of a string the C glue allocated.
fn take(s: *mut c_char) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: `s` is a NUL-terminated string from the glue, released right after the copy.
    let owned = unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned();
    unsafe { ffi::fakellm_tjs_string_free(s) };
    Some(owned)
}

fn cstring(s: &str, what: &'static str) -> Result<CString, Error> {
    CString::new(s).map_err(|_| Error::Argument(what))
}

impl Runtime {
    pub fn new(options: Options) -> Result<Runtime, Error> {
        let mem_limit = c_int::try_from(options.mem_limit).unwrap_or(c_int::MAX);
        // SAFETY: plain constructor.
        let raw = NonNull::new(unsafe { ffi::fakellm_tjs_new(mem_limit, options.stack_size) }).ok_or(Error::Create)?;
        let mut logs = Box::<LogSink>::default();
        // SAFETY: the callback only dereferences the sink, which outlives the handle (the handle is freed first in `Drop`).
        unsafe { ffi::fakellm_tjs_set_log(raw.as_ptr(), Some(collect_log), &mut *logs as *mut LogSink as *mut c_void) };
        let cancel = Arc::new(CancelState {
            requested: AtomicBool::new(false),
            handle: Mutex::new(Some(raw)),
        });
        Ok(Runtime {
            raw,
            cancel,
            logs,
            broken: false,
        })
    }

    /// Loads the ES module `dir/entry` (`dir` is made absolute) and keeps its default export as the policy.
    pub fn load(&mut self, dir: &Path, entry: &str, timeout: Option<Duration>) -> Result<(), Error> {
        let dir = std::path::absolute(dir).map_err(|e| Error::Script(format!("cannot resolve {}: {e}", dir.display())))?;
        let dir = cstring(&dir.to_string_lossy(), "dir")?;
        let entry = cstring(entry, "entry")?;
        let mut err = ptr::null_mut();
        // SAFETY: valid handle and NUL-terminated strings; `err` is an out parameter.
        let status = unsafe { ffi::fakellm_tjs_load(self.raw.as_ptr(), dir.as_ptr(), entry.as_ptr(), timeout_ms(timeout), &mut err) };
        self.finish(status, err).map(|_| ())
    }

    /// Calls `decide(request, {state, hits, log})`. `request_json` and `state_json` are JSON documents.
    pub fn decide(&mut self, request_json: &str, state_json: &str, hits: u64, timeout: Option<Duration>) -> Result<Decision, Error> {
        let request = cstring(request_json, "request")?;
        let state = cstring(state_json, "state")?;
        let (mut reply, mut next_state, mut err) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        // SAFETY: valid handle and strings; the three out parameters are written by the callee.
        let status = unsafe {
            ffi::fakellm_tjs_decide(
                self.raw.as_ptr(),
                request.as_ptr(),
                state.as_ptr(),
                hits.min(i64::MAX as u64) as i64,
                timeout_ms(timeout),
                &mut reply,
                &mut next_state,
                &mut err,
            )
        };
        let (reply, next_state) = (take(reply), take(next_state));
        match self.finish(status, err)? {
            ffi::FAKELLM_TJS_DECLINED => Ok(Decision::Declined { state: next_state }),
            _ => Ok(Decision::Reply {
                reply: reply.unwrap_or_default(),
                state: next_state,
            }),
        }
    }

    /// The log lines (`ctx.log` and `console.*`) written since the last call.
    pub fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.logs.lines)
    }

    /// True once a call timed out or the runtime was cancelled: it may be stuck in an abandoned computation and refuses further calls.
    pub fn is_broken(&self) -> bool {
        self.broken || self.cancel.requested.load(Ordering::SeqCst)
    }

    /// A handle that interrupts this runtime from another thread.
    pub fn canceller(&self) -> Canceller {
        Canceller(self.cancel.clone())
    }

    fn finish(&mut self, status: c_int, err: *mut c_char) -> Result<c_int, Error> {
        let message = take(err);
        match status {
            ffi::FAKELLM_TJS_OK | ffi::FAKELLM_TJS_DECLINED => Ok(status),
            ffi::FAKELLM_TJS_TIMEOUT => {
                self.broken = true;
                Err(Error::Timeout)
            }
            ffi::FAKELLM_TJS_CANCELLED => {
                self.broken = true;
                Err(Error::Cancelled)
            }
            _ => Err(Error::Script(message.unwrap_or_else(|| "unknown error".to_string()))),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Under the lock: a `Canceller` on another thread is either done with the handle or sees `None`.
        self.cancel.handle.lock().unwrap_or_else(|e| e.into_inner()).take();
        // SAFETY: the handle is valid and never used again.
        unsafe { ffi::fakellm_tjs_free(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests;
