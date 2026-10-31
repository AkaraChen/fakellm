//! Server-sent events for every protocol, paced by the server clock. Headers go out immediately;
//! the body task waits on `Clock::wait_until` for each frame, so a frozen clock holds the stream
//! and `advance` releases the next frame at once, with no real waiting in tests.
//!
//! Frontends build a `Vec<Frame>` for the whole reply up front; [`respond`] only schedules and
//! writes them. That keeps ttft, token interval and `cut_after_chunks` identical for Chat,
//! Responses and Messages. See docs/m1-spec.md ("Shared SSE writer").

use std::convert::Infallible;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::Response;
use crate::clock::Clock;
use crate::ir::Delivery;

/// How a frame is paced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// Every frame before the first `Content` frame (role chunk, `message_start`, `ping`,
    /// `response.created`, the first `content_block_start`, ...). Sent at `ttft` together with
    /// the first content frame, never cut.
    Head,
    /// A frame that carries generated tokens (text, reasoning, tool-argument fragment). Frames
    /// of this kind are `token_interval * chunk_tokens` apart; the first goes out with the head.
    Content,
    /// Every non-content frame after the first content frame (block stops, later block starts,
    /// finish chunk, usage chunk, `message_delta`, `message_stop`, `response.completed`, ...).
    /// Sent right after the previous frame, no extra delay. That includes block boundaries between
    /// two content frames: the next block's start goes out one interval before its first delta.
    Trailer,
}

/// One SSE event. `data` is the already-serialized payload (compact JSON, or the literal
/// sentinel text); `event` becomes the `event:` line when set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub event: Option<String>,
    pub data: String,
}

impl Frame {
    /// Serializes `value` compactly. `event` is the SSE event name (`None` for Chat, which only
    /// uses `data:` lines).
    pub fn json(kind: FrameKind, event: Option<&str>, value: &Value) -> Self {
        Self {
            kind,
            event: event.map(str::to_owned),
            data: serde_json::to_string(value).unwrap_or_default(),
        }
    }

    /// The wire bytes: `event: <e>\ndata: <data>\n\n`, or `data: <data>\n\n` without an event.
    pub fn encode(&self) -> Bytes {
        Bytes::from(match &self.event {
            Some(e) => format!("event: {e}\ndata: {}\n\n", self.data),
            None => format!("data: {}\n\n", self.data),
        })
    }
}

/// Builds the `text/event-stream` response and spawns the task that writes the frames.
///
/// Timing: the first frame at `now + delivery.ttft` (all leading `Head` frames at once); each
/// `Content` frame after the first `delivery.token_interval * delivery.chunk_tokens` after the
/// previous content frame; `Trailer` frames immediately after the previous frame. Then, when
/// `done_sentinel` is `Some(s)`, a final `data: <s>\n\n` frame (Chat passes `Some("[DONE]")`;
/// Messages and Responses pass `None`).
///
/// `delivery.cut_after_chunks == Some(n)`: after `n` content frames have been written, stop at
/// the next non-`Head` frame: no more frames, no sentinel, a clean end of the chunked body (not a
/// socket reset). If the frames hold fewer than `n` content frames the cut never fires and
/// everything including the sentinel is sent; with exactly `n`, the frames after the last content
/// frame (finish chunk, sentinel) are dropped. Same rule as the old `openai::stream::respond`.
///
/// Writing stops as soon as the client disconnects, even while parked on the clock. Deadlines are
/// absolute server-clock times fixed when this is called.
///
/// Response headers: `content-type: text/event-stream; charset=utf-8`, `cache-control: no-cache`,
/// then `headers` (e.g. `x-request-id`), chunked transfer (no `content-length`).
pub fn respond(
    clock: Arc<Clock>,
    delivery: Delivery,
    frames: Vec<Frame>,
    headers: Vec<(String, String)>,
    done_sentinel: Option<&'static str>,
) -> Response {
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    let start = clock.now();
    tokio::spawn(async move {
        tokio::select! {
            () = tx.closed() => {}
            () = run(&clock, start, &delivery, &frames, done_sentinel, |bytes| tx.send(bytes).is_ok()) => {}
        }
    });
    let stream = futures::stream::poll_fn(move |cx| {
        rx.poll_recv(cx)
            .map(|o| o.map(|b| Ok::<_, Infallible>(hyper::body::Frame::data(b))))
    });
    let mut builder = http::Response::builder()
        .status(200)
        .header(http::header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(http::header::CACHE_CONTROL, "no-cache");
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    builder
        .body(StreamBody::new(stream).boxed())
        .expect("static response parts are valid")
}

/// Walks the frames on the clock, handing each to `send`; stops when `send` returns false.
async fn run(
    clock: &Clock,
    start: SystemTime,
    delivery: &Delivery,
    frames: &[Frame],
    done_sentinel: Option<&str>,
    mut send: impl FnMut(Bytes) -> bool,
) {
    // The deadline of the previous content frame, or the first deadline until one was sent.
    let mut deadline = start.checked_add(delivery.ttft).unwrap_or(start);
    clock.wait_until(deadline).await;
    let cut = delivery.cut_after_chunks;
    let mut content_sent = 0usize;
    for frame in frames {
        if frame.kind != FrameKind::Head && cut.is_some_and(|n| content_sent >= n) {
            return;
        }
        if frame.kind == FrameKind::Content {
            if content_sent > 0 {
                deadline = deadline
                    .checked_add(delivery.token_interval.saturating_mul(delivery.chunk_tokens.max(1) as u32))
                    .unwrap_or(deadline);
                clock.wait_until(deadline).await;
            }
            content_sent += 1;
        }
        if !send(frame.encode()) {
            return;
        }
    }
    if let Some(s) = done_sentinel {
        send(Bytes::from(format!("data: {s}\n\n")));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    fn chunks(contents: usize) -> Vec<Frame> {
        let mut v = vec![Frame::json(FrameKind::Head, None, &json!({"i": 0}))];
        v.extend((1..=contents).map(|i| Frame::json(FrameKind::Content, None, &json!({"i": i}))));
        v.push(Frame::json(FrameKind::Trailer, None, &json!({"i": "fin"})));
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

    fn go(clock: Arc<Clock>, d: Delivery, frames: Vec<Frame>) -> crate::Body {
        respond(clock, d, frames, vec![("x-request-id".into(), "r".into())], Some("[DONE]")).into_body()
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
        let f = Frame::json(FrameKind::Content, None, &json!({"a": [1, "x"]}));
        assert_eq!(&f.encode()[..], b"data: {\"a\":[1,\"x\"]}\n\n");
    }

    #[test]
    fn event_line_precedes_data() {
        let f = Frame::json(FrameKind::Head, Some("message_start"), &json!({"type": "message_start"}));
        assert_eq!(&f.encode()[..], b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n");
    }

    #[tokio::test]
    async fn headers() {
        let res = respond(
            Arc::default(),
            delivery(0, 0, None),
            chunks(1),
            vec![("x-request-id".into(), "req_1".into())],
            None,
        );
        assert_eq!(res.headers()["content-type"], "text/event-stream; charset=utf-8");
        assert_eq!(res.headers()["cache-control"], "no-cache");
        assert_eq!(res.headers()["x-request-id"], "req_1");
        assert!(res.headers().get("content-length").is_none());
    }

    #[tokio::test]
    async fn frozen_clock_holds_each_deadline() {
        let clock = Arc::new(Clock::default());
        clock.freeze(None).unwrap();
        let mut body = go(clock.clone(), delivery(100, 10, None), chunks(3));
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
    async fn event_frames_are_paced_like_data_frames() {
        let clock = Arc::new(Clock::default());
        clock.freeze(None).unwrap();
        let frames = vec![
            Frame::json(FrameKind::Head, Some("message_start"), &json!(1)),
            Frame::json(FrameKind::Head, Some("ping"), &json!(2)),
            Frame::json(FrameKind::Content, Some("delta"), &json!(3)),
            Frame::json(FrameKind::Content, Some("delta"), &json!(4)),
            Frame::json(FrameKind::Trailer, Some("message_stop"), &json!(5)),
        ];
        let mut body = respond(clock.clone(), delivery(50, 20, None), frames, vec![], None).into_body();
        settle().await;
        assert_eq!(next(&mut body).await, None);
        clock.advance(50_000_000).unwrap();
        assert_eq!(next(&mut body).await.unwrap(), "event: message_start\ndata: 1\n\n");
        assert_eq!(next(&mut body).await.unwrap(), "event: ping\ndata: 2\n\n");
        assert_eq!(next(&mut body).await.unwrap(), "event: delta\ndata: 3\n\n");
        assert_eq!(next(&mut body).await, None);
        clock.advance(20_000_000).unwrap();
        assert_eq!(next(&mut body).await.unwrap(), "event: delta\ndata: 4\n\n");
        assert_eq!(next(&mut body).await.unwrap(), "event: message_stop\ndata: 5\n\n");
        assert!(body.frame().await.is_none(), "no sentinel without done_sentinel");
    }

    #[tokio::test]
    async fn disconnect_ends_task_on_frozen_clock() {
        let clock = Arc::new(Clock::default());
        clock.freeze(None).unwrap();
        let body = go(clock.clone(), delivery(100, 10, None), chunks(3));
        settle().await;
        assert_eq!(Arc::strong_count(&clock), 2);
        drop(body);
        settle().await;
        assert_eq!(Arc::strong_count(&clock), 1);
    }

    #[tokio::test]
    async fn cut_ends_without_finish_or_done() {
        let mut body = go(Arc::new(Clock::default()), delivery(0, 0, Some(2)), chunks(3));
        let mut got = Vec::new();
        while let Some(f) = next(&mut body).await {
            got.push(f);
        }
        assert_eq!(got, ["data: {\"i\":0}\n\n", "data: {\"i\":1}\n\n", "data: {\"i\":2}\n\n"]);
    }

    #[tokio::test]
    async fn cut_beyond_content_still_finishes() {
        let mut body = go(Arc::new(Clock::default()), delivery(0, 0, Some(9)), chunks(1));
        let mut last = None;
        while let Some(f) = next(&mut body).await {
            last = Some(f);
        }
        assert_eq!(last.unwrap(), "data: [DONE]\n\n");
    }

    #[tokio::test]
    async fn cut_equal_to_content_count_drops_finish_and_done() {
        let mut body = go(Arc::new(Clock::default()), delivery(0, 0, Some(1)), chunks(1));
        let mut got = vec![];
        while let Some(f) = next(&mut body).await {
            got.push(f);
        }
        assert_eq!(got, ["data: {\"i\":0}\n\n", "data: {\"i\":1}\n\n"]);
    }
}
