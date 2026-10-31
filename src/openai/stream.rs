//! Server-sent events paced by the server clock. Headers go out immediately; the body task waits
//! on `Clock::wait_until` for each chunk, so a frozen clock holds the stream and `advance`
//! releases the next chunk at once, with no real waiting in tests.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::Response;
use crate::clock::Clock;
use crate::ir::Delivery;
use crate::openai::chat::ChunkKind;

/// Builds the `text/event-stream` response and spawns the task that writes
/// `data: <json>\n\n` frames. Timing: the first chunk at `now + delivery.ttft`; each `Content`
/// chunk after the first `delivery.token_interval * delivery.chunk_tokens` after the previous content chunk; `Role` and
/// `Trailer` chunks right after the previous one. After the last chunk, `data: [DONE]\n\n`. When
/// `delivery.cut_after_chunks` is `Some(n)`, the body ends after `n` content chunks without the
/// finish chunks or `[DONE]` (a clean end of the chunked body, not a socket reset). Writing stops as soon as the
/// client disconnects, even while parked on the clock.
///
/// Deadlines are absolute server-clock times fixed here, so moving the clock backwards later
/// (`set`, `reset`) stretches the remaining wait and moving it forwards releases chunks at once.
///
/// Response headers: `content-type: text/event-stream; charset=utf-8`, `cache-control: no-cache`,
/// `x-request-id`, chunked transfer (no `content-length`).
pub fn respond(clock: Arc<Clock>, delivery: Delivery, chunks: Vec<(ChunkKind, Value)>, request_id: String) -> Response {
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    let start = clock.now();
    tokio::spawn(async move {
        tokio::select! {
            () = tx.closed() => {}
            () = run(&clock, start, &delivery, &chunks, |frame| tx.send(frame).is_ok()) => {}
        }
    });
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx).map(|o| o.map(|b| Ok::<_, Infallible>(Frame::data(b)))));
    http::Response::builder()
        .status(200)
        .header(http::header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(http::header::CACHE_CONTROL, "no-cache")
        .header("x-request-id", request_id)
        .body(StreamBody::new(stream).boxed())
        .expect("static response parts are valid")
}

/// Walks the chunks on the clock, handing each frame to `send`; stops when `send` returns false.
async fn run(clock: &Clock, start: SystemTime, delivery: &Delivery, chunks: &[(ChunkKind, Value)], mut send: impl FnMut(Bytes) -> bool) {
    // The deadline of the previous content chunk, or the first deadline until one was sent.
    let mut deadline = start.checked_add(delivery.ttft).unwrap_or(start);
    clock.wait_until(deadline).await;
    let mut content_sent = 0usize;
    for (kind, value) in chunks {
        match kind {
            ChunkKind::Role | ChunkKind::Trailer => {
                if *kind == ChunkKind::Trailer && delivery.cut_after_chunks.is_some_and(|n| content_sent >= n) {
                    return;
                }
            }
            ChunkKind::Content => {
                if delivery.cut_after_chunks.is_some_and(|n| content_sent >= n) {
                    return;
                }
                if content_sent > 0 {
                    deadline = deadline
                        .checked_add(delivery.token_interval.saturating_mul(delivery.chunk_tokens.max(1) as u32))
                        .unwrap_or(deadline);
                    clock.wait_until(deadline).await;
                }
                content_sent += 1;
            }
        }
        if !send(frame(value)) {
            return;
        }
    }
    if delivery.cut_after_chunks.is_some_and(|n| content_sent >= n) && chunks.iter().any(|(k, _)| *k == ChunkKind::Content) {
        return;
    }
    send(Bytes::from_static(b"data: [DONE]\n\n"));
}

fn frame(value: &Value) -> Bytes {
    let json = serde_json::to_string(value).unwrap_or_default();
    Bytes::from(format!("data: {json}\n\n"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    fn chunks(contents: usize) -> Vec<(ChunkKind, Value)> {
        let mut v = vec![(ChunkKind::Role, json!({"i": 0}))];
        v.extend((1..=contents).map(|i| (ChunkKind::Content, json!({"i": i}))));
        v.push((ChunkKind::Trailer, json!({"i": "fin"})));
        v
    }

    fn delivery(ttft_ms: u64, interval_ms: u64, cut: Option<usize>) -> Delivery {
        Delivery {
            ttft: Duration::from_millis(ttft_ms),
            token_interval: Duration::from_millis(interval_ms),
            cut_after_chunks: cut,
            ..Delivery::default()
        }
    }

    async fn next(body: &mut crate::Body) -> Option<String> {
        let frame = tokio::time::timeout(Duration::from_millis(50), body.frame()).await.ok()??.ok()?;
        Some(String::from_utf8(frame.into_data().ok()?.to_vec()).unwrap())
    }

    async fn settle() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn frames_are_compact_sse() {
        assert_eq!(&frame(&json!({"a": [1, "x"]}))[..], b"data: {\"a\":[1,\"x\"]}\n\n");
    }

    #[tokio::test]
    async fn headers() {
        let res = respond(Arc::default(), delivery(0, 0, None), chunks(1), "req_1".into());
        assert_eq!(res.headers()["content-type"], "text/event-stream; charset=utf-8");
        assert_eq!(res.headers()["cache-control"], "no-cache");
        assert_eq!(res.headers()["x-request-id"], "req_1");
        assert!(res.headers().get("content-length").is_none());
    }

    #[tokio::test]
    async fn frozen_clock_holds_each_deadline() {
        let clock = Arc::new(Clock::default());
        clock.freeze(None).unwrap();
        let mut body = respond(clock.clone(), delivery(100, 10, None), chunks(3), "r".into()).into_body();
        settle().await;
        assert_eq!(next(&mut body).await, None);

        clock.advance(100_000_000).unwrap();
        assert_eq!(next(&mut body).await.unwrap(), "data: {\"i\":0}\n\n");
        assert_eq!(next(&mut body).await.unwrap(), "data: {\"i\":1}\n\n");
        assert_eq!(next(&mut body).await, None);

        clock.advance(10_000_000).unwrap();
        assert_eq!(next(&mut body).await.unwrap(), "data: {\"i\":2}\n\n");
        assert_eq!(next(&mut body).await, None);

        clock.advance(10_000_000).unwrap();
        assert_eq!(next(&mut body).await.unwrap(), "data: {\"i\":3}\n\n");
        assert_eq!(next(&mut body).await.unwrap(), "data: {\"i\":\"fin\"}\n\n");
        assert_eq!(next(&mut body).await.unwrap(), "data: [DONE]\n\n");
        assert!(body.frame().await.is_none());
    }

    #[tokio::test]
    async fn disconnect_ends_task_on_frozen_clock() {
        let clock = Arc::new(Clock::default());
        clock.freeze(None).unwrap();
        let body = respond(clock.clone(), delivery(100, 10, None), chunks(3), "r".into()).into_body();
        settle().await;
        assert_eq!(Arc::strong_count(&clock), 2);
        drop(body);
        settle().await;
        assert_eq!(Arc::strong_count(&clock), 1);
    }

    #[tokio::test]
    async fn cut_ends_without_finish_or_done() {
        let clock = Arc::new(Clock::default());
        let mut body = respond(clock, delivery(0, 0, Some(2)), chunks(3), "r".into()).into_body();
        let mut got = Vec::new();
        while let Some(f) = next(&mut body).await {
            got.push(f);
        }
        assert_eq!(got, ["data: {\"i\":0}\n\n", "data: {\"i\":1}\n\n", "data: {\"i\":2}\n\n"]);
    }

    #[tokio::test]
    async fn cut_beyond_content_still_finishes() {
        let clock = Arc::new(Clock::default());
        let mut body = respond(clock, delivery(0, 0, Some(9)), chunks(1), "r".into()).into_body();
        let mut last = None;
        while let Some(f) = next(&mut body).await {
            last = Some(f);
        }
        assert_eq!(last.unwrap(), "data: [DONE]\n\n");
    }
}
