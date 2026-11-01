use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A temp directory with policy files, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(files: &[(&str, &str)]) -> Dir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tjs-sys-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        Dir(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SECOND: Option<Duration> = Some(Duration::from_secs(5));

fn load(source: &str) -> (Dir, Runtime) {
    let dir = Dir::new(&[("policy.mjs", source)]);
    let mut rt = Runtime::new(Options::default()).unwrap();
    rt.load(&dir.0, "policy.mjs", SECOND).unwrap();
    (dir, rt)
}

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}

fn reply(decision: Decision) -> (serde_json::Value, Option<serde_json::Value>) {
    match decision {
        Decision::Reply { reply, state } => (json(&reply), state.map(|s| json(&s))),
        other => panic!("expected a reply, got {other:?}"),
    }
}

#[test]
fn returns_text_and_state() {
    let (_dir, mut rt) = load(
        r#"export default (request, ctx) => ({
            text: `${request.model} says hi, hit ${ctx.hits}, seen ${ctx.state.n ?? 0}, tools ${request.tools.length}`,
            state: { n: (ctx.state.n ?? 0) + 1 },
        });"#,
    );
    let (reply1, state) = reply(rt.decide(r#"{"model":"gpt"}"#, "{}", 3, SECOND).unwrap());
    assert_eq!(reply1, serde_json::json!({"text": "gpt says hi, hit 3, seen 0, tools 0"}));
    assert_eq!(state, Some(serde_json::json!({"n": 1})));
    // The caller owns the state: a second call with the state it got sees it.
    let (reply2, _) = reply(rt.decide(r#"{"model":"gpt"}"#, r#"{"n":1}"#, 4, SECOND).unwrap());
    assert_eq!(reply2["text"], "gpt says hi, hit 4, seen 1, tools 0");
}

#[test]
fn awaits_a_timer() {
    let (_dir, mut rt) = load(
        r#"const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
        export default async function decide() {
            const start = Date.now();
            await sleep(60);
            await Promise.resolve();
            return { text: `slept ${Date.now() - start >= 50}` };
        }"#,
    );
    let started = std::time::Instant::now();
    let (reply, state) = reply(rt.decide("{}", "{}", 0, SECOND).unwrap());
    assert_eq!(reply["text"], "slept true");
    assert_eq!(state, None);
    assert!(started.elapsed() >= Duration::from_millis(50));
}

#[test]
fn has_the_full_tjs_runtime() {
    let dir = Dir::new(&[
        ("data.txt", "hello from disk"),
        ("lib/helper.js", "export const shout = (s) => s.toUpperCase();"),
    ]);
    fs::write(
        dir.0.join("policy.mjs"),
        r#"import path from "tjs:path";
        import { shout } from "./lib/helper.js";
        const here = import.meta.dirname ?? new URL(".", import.meta.url).pathname;
        export default async function decide() {
            const bytes = await tjs.readFile(path.join(here, "data.txt"));
            const text = new TextDecoder().decode(bytes);
            const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
            return {
                text: shout(text),
                tjs: typeof tjs.version === "string",
                fetch: typeof fetch,
                url: new URL("https://example.com/a?b=1").searchParams.get("b"),
                digest: digest.byteLength,
                uuid: crypto.randomUUID().length,
                hasNode: typeof process,
            };
        }"#,
    )
    .unwrap();
    let mut rt = Runtime::new(Options::default()).unwrap();
    rt.load(&dir.0, "policy.mjs", SECOND).unwrap();
    let (reply, _) = reply(rt.decide("{}", "{}", 0, SECOND).unwrap());
    assert_eq!(reply["text"], "HELLO FROM DISK");
    assert_eq!(reply["tjs"], true);
    assert_eq!(reply["fetch"], "function");
    assert_eq!(reply["url"], "1");
    assert_eq!(reply["digest"], 32);
    assert_eq!(reply["uuid"], 36);
}

#[test]
fn times_out_a_busy_loop() {
    let (_dir, mut rt) = load("export default () => { while (true) {} };");
    let started = std::time::Instant::now();
    let err = rt.decide("{}", "{}", 0, Some(Duration::from_millis(200))).unwrap_err();
    assert_eq!(err, Error::Timeout);
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(rt.is_broken());
    // A broken runtime refuses further work instead of running on top of an abandoned computation.
    assert!(matches!(rt.decide("{}", "{}", 0, SECOND), Err(Error::Script(_))));
}

#[test]
fn times_out_a_busy_loop_after_an_await() {
    let (_dir, mut rt) = load("export default async () => { await null; for (;;) {} };");
    assert_eq!(
        rt.decide("{}", "{}", 0, Some(Duration::from_millis(200))).unwrap_err(),
        Error::Timeout
    );
}

#[test]
fn times_out_a_long_sleep() {
    let (_dir, mut rt) = load("export default async () => { await new Promise((r) => setTimeout(r, 60_000)); return { text: 'late' }; };");
    let started = std::time::Instant::now();
    assert_eq!(
        rt.decide("{}", "{}", 0, Some(Duration::from_millis(150))).unwrap_err(),
        Error::Timeout
    );
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
}

#[test]
fn a_promise_that_can_never_settle_is_an_error_not_a_hang() {
    let (_dir, mut rt) = load("export default async () => { await new Promise(() => {}); return { text: 'x' }; };");
    let err = rt.decide("{}", "{}", 0, None).unwrap_err();
    assert!(matches!(&err, Error::Script(m) if m.contains("never settle")), "{err:?}");
}

#[test]
fn declines() {
    for (body, state) in [
        ("null", None),
        ("undefined", None),
        ("({ decline: true })", None),
        ("({ decline: true, state: { seen: 1 } })", Some(serde_json::json!({"seen": 1}))),
    ] {
        let (_dir, mut rt) = load(&format!("export default () => {body};"));
        match rt.decide("{}", "{}", 0, SECOND).unwrap() {
            Decision::Declined { state: got } => assert_eq!(got.map(|s| json(&s)), state, "{body}"),
            other => panic!("{body}: {other:?}"),
        }
        assert!(!rt.is_broken());
    }
}

#[test]
fn bad_replies_and_throws_are_script_errors() {
    for (body, expected) in [
        ("{ throw new Error('boom'); }", "boom"),
        ("{ throw 'plain'; }", "plain"),
        ("{ return 7; }", "must return an object"),
        ("{ return [1]; }", "must return an object"),
        ("{ return Promise.reject(new TypeError('later')); }", "later"),
        ("{ return { text: 1n }; }", "BigInt"),
    ] {
        let (_dir, mut rt) = load(&format!("export default async function decide() {body}"));
        let err = rt.decide("{}", "{}", 0, SECOND).unwrap_err();
        assert!(matches!(&err, Error::Script(m) if m.contains(expected)), "{body}: {err:?}");
        // The runtime is still fine.
        assert!(!rt.is_broken());
    }
}

#[test]
fn an_unhandled_background_rejection_is_an_error() {
    let (_dir, mut rt) = load(
        "export default async () => { setTimeout(() => Promise.reject(new Error('background')), 5); await new Promise((r) => setTimeout(r, 200)); return { text: 'x' }; };",
    );
    let err = rt.decide("{}", "{}", 0, SECOND).unwrap_err();
    assert!(matches!(&err, Error::Script(m) if m.contains("background")), "{err:?}");
}

#[test]
fn load_errors() {
    let cases: &[(&str, &str, &str)] = &[
        ("policy.mjs", "export default (", "unexpected token"),
        ("policy.mjs", "export const x = 1;", "default function"),
        ("policy.mjs", "export default 5;", "default function"),
        ("policy.mjs", "throw new Error('at top level');", "at top level"),
        ("policy.mjs", "import './missing.mjs'; export default () => null;", "missing.mjs"),
        (
            "policy.mjs",
            "export default () => null; await Promise.reject(new Error('tla'));",
            "tla",
        ),
        ("other.mjs", "", "policy.mjs"),
    ];
    for (file, source, expected) in cases {
        let dir = Dir::new(&[(file, source)]);
        let mut rt = Runtime::new(Options::default()).unwrap();
        let err = rt.load(&dir.0, "policy.mjs", SECOND).unwrap_err();
        assert!(matches!(&err, Error::Script(m) if m.contains(expected)), "{source:?}: {err:?}");
    }
}

#[test]
fn top_level_await_in_the_module_is_awaited() {
    let (_dir, mut rt) = load(
        "const greeting = await new Promise((r) => setTimeout(() => r('late hello'), 30)); export default () => ({ text: greeting });",
    );
    assert_eq!(reply(rt.decide("{}", "{}", 0, SECOND).unwrap()).0["text"], "late hello");
}

#[test]
fn collects_logs_and_console_output() {
    let (_dir, mut rt) =
        load("export default (req, ctx) => { ctx.log('a', { b: 1 }); console.log('c'); console.error('d'); return { text: 'x' }; };");
    rt.decide("{}", "{}", 0, SECOND).unwrap();
    assert_eq!(rt.take_logs(), vec!["a {\"b\":1}", "c", "d"]);
    assert!(rt.take_logs().is_empty());
}

#[test]
fn a_big_request_goes_through() {
    let (_dir, mut rt) = load("export default (req) => ({ text: String(req.messages[0].content.length) });");
    let big = "x".repeat(300_000);
    let request = format!(r#"{{"messages":[{{"content":"{big}"}}]}}"#);
    assert_eq!(reply(rt.decide(&request, "{}", 0, SECOND).unwrap()).0["text"], "300000");
}

#[test]
fn nul_bytes_are_rejected() {
    let (_dir, mut rt) = load("export default () => null;");
    assert_eq!(rt.decide("{\0}", "{}", 0, SECOND).unwrap_err(), Error::Argument("request"));
}

#[test]
fn memory_limit_applies() {
    let dir = Dir::new(&[(
        "policy.mjs",
        "export default () => { const a = []; for (;;) a.push(new Array(100000).fill(1)); };",
    )]);
    let mut rt = Runtime::new(Options {
        mem_limit: 32 << 20,
        stack_size: 0,
    })
    .unwrap();
    rt.load(&dir.0, "policy.mjs", SECOND).unwrap();
    let err = rt.decide("{}", "{}", 0, SECOND).unwrap_err();
    assert!(matches!(&err, Error::Script(m) if m.contains("memory")), "{err:?}");
}

#[test]
fn deep_recursion_is_a_catchable_error_not_a_crash() {
    let (_dir, mut rt) =
        load("const f = (n) => f(n + 1) + 1; export default () => { try { f(0); } catch (e) { return { text: e.constructor.name }; } };");
    assert_eq!(reply(rt.decide("{}", "{}", 0, SECOND).unwrap()).0["text"], "RangeError");
}

#[test]
fn a_runtime_moves_between_threads() {
    let (_dir, mut rt) = load("let calls = 0; export default () => ({ text: `call ${++calls}` });");
    assert_eq!(reply(rt.decide("{}", "{}", 0, SECOND).unwrap()).0["text"], "call 1");
    let mut rt = std::thread::spawn(move || {
        assert_eq!(reply(rt.decide("{}", "{}", 0, SECOND).unwrap()).0["text"], "call 2");
        rt
    })
    .join()
    .unwrap();
    assert_eq!(reply(rt.decide("{}", "{}", 0, SECOND).unwrap()).0["text"], "call 3");
}

#[test]
fn runtimes_run_in_parallel() {
    let handles: Vec<_> = (0..4)
        .map(|i| {
            std::thread::spawn(move || {
                let (_dir, mut rt) = load(&format!(
                    "export default async () => {{ await new Promise((r) => setTimeout(r, 20)); return {{ text: 'p{i}' }}; }};"
                ));
                for _ in 0..5 {
                    assert_eq!(reply(rt.decide("{}", "{}", 0, SECOND).unwrap()).0["text"], format!("p{i}"));
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn create_and_drop_many() {
    for _ in 0..20 {
        let (_dir, mut rt) = load("export default () => ({ text: 'x' });");
        rt.decide("{}", "{}", 0, SECOND).unwrap();
    }
}

#[test]
fn cancel_ends_a_busy_loop_and_a_pending_await_from_another_thread() {
    for source in [
        "export default () => { while (true) {} };",
        "export default async () => { await new Promise((r) => setTimeout(r, 60_000)); };",
    ] {
        let (_dir, mut rt) = load(source);
        let canceller = rt.canceller();
        let started = std::time::Instant::now();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel();
        });
        assert_eq!(rt.decide("{}", "{}", 0, None).unwrap_err(), Error::Cancelled, "{source}");
        handle.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        assert!(rt.is_broken());
        assert_eq!(rt.decide("{}", "{}", 0, SECOND).unwrap_err(), Error::Cancelled, "sticky");
    }
}

#[test]
fn cancelling_a_runtime_that_is_gone_or_idle_is_harmless() {
    let (_dir, rt) = load("export default () => ({ text: 'x' });");
    let canceller = rt.canceller();
    drop(rt);
    canceller.cancel();
    let (_dir, mut rt) = load("export default () => ({ text: 'x' });");
    rt.canceller().cancel();
    assert!(rt.is_broken());
    assert_eq!(rt.decide("{}", "{}", 0, SECOND).unwrap_err(), Error::Cancelled);
}
