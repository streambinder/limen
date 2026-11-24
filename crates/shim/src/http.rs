//! The HTTP layer: the OpenAI-compatible surface in front of the
//! gadget client. Direct port of the Python `Handler`.
//!
//! Streaming answers are produced by a background task that writes
//! SSE frames (or a single failure) into a channel; the handler
//! waits for the first message before answering, so a failure before
//! any chunk is still a 502 JSON response, exactly like the Python
//! shim, which sends its headers lazily on the first chunk.

use bytes::Bytes;
use futures_core::Stream;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode, header};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::config::{Config, MAX_WAIT_S, log};
use crate::gadget::{Gadget, GadgetError, SocketItem};
use crate::json::{is_truthy, py_str, python_dumps};
use crate::model::{model_list, resolve_model};
use crate::prompt::{build_prompt, extract_text, json_fallback};
use crate::replies::{ReplyWait, reply_footer};
use crate::translator::StreamTranslator;

/// The response body type: JSON bodies and SSE streams alike.
pub type Body = BoxBody<Bytes, Infallible>;
pub type HttpResponse = Response<Body>;

/// Constant-time byte comparison, like `secrets.compare_digest`.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The request is authorized when no keys are configured, or the
/// `Authorization: Bearer <key>` header matches a configured key.
pub fn authorized(api_keys: &[String], authorization: Option<&str>) -> bool {
    if api_keys.is_empty() {
        return true;
    }
    let Some(header) = authorization else {
        return false;
    };
    let Some(token) = header.strip_prefix("Bearer ") else {
        return false;
    };
    api_keys
        .iter()
        .any(|key| constant_time_eq(token.as_bytes(), key.as_bytes()))
}

fn json_response(status: StatusCode, obj: &Value) -> HttpResponse {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(python_dumps(obj))).boxed())
        .unwrap()
}

fn not_found() -> HttpResponse {
    json_response(
        StatusCode::NOT_FOUND,
        &json!({"error": {"message": "not found"}}),
    )
}

fn unsupported_method() -> HttpResponse {
    json_response(
        StatusCode::NOT_IMPLEMENTED,
        &json!({"error": {"message": "unsupported method"}}),
    )
}

fn deny() -> HttpResponse {
    json_response(
        StatusCode::FORBIDDEN,
        &json!({"error": {
            "message": "invalid api key",
            "type": "invalid_request_error",
            "code": "invalid_api_key",
        }}),
    )
}

/// The non-streaming completion object.
pub fn completion_object(model: &str, content: &str) -> Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    json!({
        "id": format!("chatcmpl-gadget-{now}"),
        "object": "chat.completion",
        "created": now,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
    })
}

/// The `error` (else the `status`) of a failed turn, for log lines.
fn error_or_status(result: &Value) -> String {
    if let Some(error) = result.get("error")
        && is_truthy(error)
    {
        return py_str(error);
    }
    if let Some(status) = result.get("status")
        && is_truthy(status)
    {
        return py_str(status);
    }
    "None".to_string()
}

/// The `error` of a failed turn, else the whole reply, for 502 bodies.
fn error_or_reply(result: &Value) -> String {
    if let Some(error) = result.get("error")
        && is_truthy(error)
    {
        return py_str(error);
    }
    python_dumps(result)
}

/// The finished text of a successful wait reply, with the outcome
/// logged: the `reply` field when it is non-blank, else "". Used by
/// both the non-streaming handler and the hybrid stream.
fn wait_reply_text(reply: &Value, elapsed: f32) -> String {
    let mut content = String::new();
    if let Some(Value::String(waited)) = reply.get("reply")
        && !waited.trim().is_empty()
    {
        content = waited.clone();
    }
    if is_truthy(reply.get("partial").unwrap_or(&Value::Null)) {
        log(&format!(
            "wait reply partial after {elapsed:.1}s: {}",
            py_str(reply.get("detail").unwrap_or(&Value::Null))
        ));
    } else if !content.is_empty() {
        log(&format!("wait reply after {elapsed:.1}s"));
    } else {
        log(&format!(
            "wait returned no reply: {}",
            py_str(reply.get("detail").unwrap_or(&Value::Null))
        ));
    }
    content
}

/// What the streaming background task sends the handler: an SSE
/// frame, or the single failure that becomes a 502 (only before any
/// frame was sent).
enum StreamMsg {
    Frame(Bytes),
    Fail(String),
}

/// The SSE sink: builds chunks and tracks what the Python shim kept
/// in its `state` dict (headers sent, first delta sent, appends).
struct SseSink {
    comp_id: String,
    created: u64,
    model: String,
    sent: bool,
    frames: usize,
    appends: usize,
    tx: mpsc::UnboundedSender<StreamMsg>,
    started: Instant,
}

impl SseSink {
    fn headers_sent(&self) -> bool {
        self.frames > 0
    }

    async fn send_chunk(&mut self, delta: Value, finish: Option<&str>) {
        let chunk = json!({
            "id": self.comp_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }],
        });
        let frame = format!("data: {}\n\n", python_dumps(&chunk));
        self.frames += 1;
        let _ = self.tx.send(StreamMsg::Frame(Bytes::from(frame)));
    }

    async fn send_delta(&mut self, text: &str) {
        let delta = if self.sent {
            json!({"content": text})
        } else {
            self.sent = true;
            json!({"role": "assistant", "content": text})
        };
        self.send_chunk(delta, None).await;
    }

    async fn finish(&mut self, translator: &StreamTranslator) {
        if !translator.done_text.is_empty() && translator.done_text != translator.accumulated {
            log(&format!(
                "stream done text differs: deltas_chars={} done_chars={}",
                translator.accumulated.chars().count(),
                translator.done_text.chars().count()
            ));
        }
        self.send_chunk(json!({}), Some("stop")).await;
        self.frames += 1;
        let _ = self
            .tx
            .send(StreamMsg::Frame(Bytes::from("data: [DONE]\n\n")));
        log(&format!(
            "stream ok in {:.1}s appends={} response_chars={}",
            self.started.elapsed().as_secs_f32(),
            self.appends,
            translator.accumulated.chars().count()
        ));
    }
}

/// The parameters of one streaming request, bundled so the flow
/// functions stay under clippy's argument limit.
struct StreamRequest {
    prompt: String,
    forwarded: String,
    session_id: Option<String>,
    model: String,
    cid: String,
    wait: bool,
    listen: bool,
    started: Instant,
}

enum HybridOutcome {
    Done,
    Answered,
    Fallback,
}

/// The shim: configuration, gadget client and reply drop.
pub struct Shim<G: Gadget, R: ReplyWait> {
    pub config: Arc<Config>,
    pub gadget: Arc<G>,
    pub replies: Arc<R>,
}

// Manual Clone: the derived one would demand G: Clone and R: Clone,
// but only the Arcs need cloning.
impl<G: Gadget, R: ReplyWait> Clone for Shim<G, R> {
    fn clone(&self) -> Self {
        Shim {
            config: self.config.clone(),
            gadget: self.gadget.clone(),
            replies: self.replies.clone(),
        }
    }
}

impl<G: Gadget + 'static, R: ReplyWait + 'static> Shim<G, R> {
    pub fn new(config: Config, gadget: Arc<G>, replies: Arc<R>) -> Self {
        Shim {
            config: Arc::new(config),
            gadget,
            replies,
        }
    }

    /// Dispatch on the raw request target, like the Python handler's
    /// exact `self.path` comparisons (query strings included).
    pub async fn handle(&self, req: Request<Incoming>) -> HttpResponse {
        let method = req.method().clone();
        let target = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| req.uri().path().to_string());
        match method {
            Method::GET => match target.as_str() {
                "/v1/models" => self.get_models(&req),
                "/healthz" => self.get_healthz(),
                _ => not_found(),
            },
            Method::POST => match target.as_str() {
                "/v1/chat/completions" => self.post_completions(req).await,
                _ => not_found(),
            },
            _ => unsupported_method(),
        }
    }

    fn get_models(&self, req: &Request<Incoming>) -> HttpResponse {
        if !authorized(
            &self.config.api_keys,
            req.headers()
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok()),
        ) {
            return deny();
        }
        json_response(
            StatusCode::OK,
            &json!({"object": "list", "data": model_list(&self.config.model_aliases, self.config.context_window)}),
        )
    }

    fn get_healthz(&self) -> HttpResponse {
        let ok = std::path::Path::new(&self.config.socket_path).exists();
        json_response(
            if ok {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            &json!({"ok": ok, "socket": self.config.socket_path}),
        )
    }

    async fn post_completions(&self, req: Request<Incoming>) -> HttpResponse {
        let started = Instant::now();
        if !authorized(
            &self.config.api_keys,
            req.headers()
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok()),
        ) {
            return deny();
        }
        let session_header = req
            .headers()
            .get("X-Session-Id")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let body_bytes = match req.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return error_400_invalid_json(),
        };
        let body: Value = if body_bytes.is_empty() {
            Value::Object(Default::default())
        } else {
            match serde_json::from_slice(&body_bytes) {
                Ok(body) => body,
                Err(_) => return error_400_invalid_json(),
            }
        };
        let Some(body) = body.as_object() else {
            return error_400_invalid_json();
        };
        let listen_only = body.get("listen_only") == Some(&Value::Bool(true));
        if listen_only && !is_truthy(body.get("stream").unwrap_or(&Value::Null)) {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": "listen_only requires stream"}}),
            );
        }
        let prompt = build_prompt(body.get("messages").unwrap_or(&Value::Null));
        if prompt.is_empty() && !listen_only {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": "no message content"}}),
            );
        }
        let model_raw = body
            .get("model")
            .map(|v| {
                if is_truthy(v) {
                    py_str(v)
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        let model = model_raw.trim().to_string();
        if model.is_empty() {
            return json_response(
                StatusCode::BAD_REQUEST,
                &json!({"error": {"message": "no model specified"}}),
            );
        }
        // The session source is the X-Session-Id header, then the
        // session_id body field, then the user body field.
        let session_source: Option<String> =
            session_header.filter(|h| !h.is_empty()).or_else(|| {
                [body.get("session_id"), body.get("user")]
                    .into_iter()
                    .flatten()
                    .find(|v| is_truthy(v))
                    .map(py_str)
            });
        let route = match resolve_model(
            &model,
            session_source.as_deref(),
            &self.config.model_aliases,
        ) {
            Ok(route) => route,
            Err(err) => {
                return json_response(
                    StatusCode::from_u16(err.status).unwrap(),
                    &json!({"error": {
                        "message": err.message,
                        "type": "invalid_request_error",
                        "param": "model",
                        "code": err.code,
                    }}),
                );
            }
        };
        let session_id = route.session_id;
        let cid = Uuid::new_v4().simple().to_string();
        let wait = !listen_only
            && self.config.reply_wait_s > 0
            && body.get("wait_reply") != Some(&Value::Bool(false));
        let forwarded = if wait {
            format!("{prompt}{}", reply_footer(&self.config.replies_dir, &cid))
        } else {
            prompt.clone()
        };
        log(&format!(
            "chat model={} session={} prompt_chars={} wait={}",
            model,
            session_id.as_deref().unwrap_or("main"),
            prompt.chars().count(),
            if wait { "True" } else { "False" }
        ));
        if is_truthy(body.get("stream").unwrap_or(&Value::Null)) {
            return self
                .stream_response(StreamRequest {
                    prompt,
                    forwarded,
                    session_id,
                    model,
                    cid,
                    wait,
                    listen: listen_only,
                    started,
                })
                .await;
        }
        let reply = if wait {
            self.gadget
                .ask(
                    prompt,
                    session_id,
                    Some(std::cmp::min(self.config.reply_wait_s, MAX_WAIT_S)),
                )
                .await
        } else {
            self.gadget.ask(forwarded, session_id, None).await
        };
        let elapsed = started.elapsed().as_secs_f32();
        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                log(&format!("chat failed after {elapsed:.1}s: {e}"));
                return json_response(
                    StatusCode::BAD_GATEWAY,
                    &json!({"error": {"message": format!("musegadget unreachable: {e}")}}),
                );
            }
        };
        if !is_truthy(reply.get("ok").unwrap_or(&Value::Null)) {
            log(&format!(
                "chat not ok after {elapsed:.1}s: {}",
                error_or_status(&reply)
            ));
            return json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": format!("Muse turn failed: {}", error_or_reply(&reply))}}),
            );
        }
        let mut content = if wait {
            wait_reply_text(&reply, elapsed)
        } else {
            String::new()
        };
        if content.is_empty() {
            content = extract_text(reply.get("response").unwrap_or(&Value::Null));
        }
        if content.is_empty() {
            content = json_fallback(reply.get("response").unwrap_or(&Value::Null));
        }
        log(&format!(
            "chat ok in {elapsed:.1}s response_chars={}",
            content.chars().count()
        ));
        json_response(StatusCode::OK, &completion_object(&model, &content))
    }

    /// Start the streaming flow in the background and answer from its
    /// first message: an SSE stream, or a 502 when it failed before
    /// any chunk.
    async fn stream_response(&self, request: StreamRequest) -> HttpResponse {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamMsg>();
        let shim = self.clone();
        tokio::spawn(async move {
            shim.run_stream_flow(request, tx).await;
        });
        match rx.recv().await {
            Some(StreamMsg::Frame(first)) => {
                let body = StreamBody::new(FrameStream {
                    rx,
                    first: Some(first),
                });
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header("X-Accel-Buffering", "no")
                    .header(header::CONNECTION, "close")
                    .body(body.boxed())
                    .unwrap()
            }
            Some(StreamMsg::Fail(message)) => json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": message}}),
            ),
            None => json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": {"message": "musegadget unreachable: stream ended"}}),
            ),
        }
    }

    /// The streaming flow: listen-only, hybrid, or legacy, then the
    /// reply-drop fallback. Emits SSE frames (or one failure) on `tx`.
    async fn run_stream_flow(&self, request: StreamRequest, tx: mpsc::UnboundedSender<StreamMsg>) {
        let StreamRequest {
            prompt,
            forwarded,
            session_id,
            model,
            cid,
            wait,
            listen,
            started,
        } = request;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut sink = SseSink {
            comp_id: format!("chatcmpl-gadget-{now}"),
            created: now,
            model,
            sent: false,
            frames: 0,
            appends: 0,
            tx: tx.clone(),
            started,
        };
        let mut translator = StreamTranslator::with_follow(listen);
        if listen {
            let rx = self.gadget.listen(session_id.as_deref(), None, None);
            let (result, error) = drain(rx, &mut translator, &mut sink, started).await;
            if drain_outcome(&result, error, &sink, &tx).await {
                return;
            }
            sink.finish(&translator).await;
            return;
        }
        if wait {
            match self
                .stream_hybrid(
                    &prompt,
                    session_id.as_deref(),
                    &mut translator,
                    &mut sink,
                    started,
                    &tx,
                )
                .await
            {
                HybridOutcome::Done => {
                    sink.finish(&translator).await;
                    return;
                }
                HybridOutcome::Answered => return,
                HybridOutcome::Fallback => {
                    translator = StreamTranslator::new();
                    sink.appends = 0;
                }
            }
        }
        let rx = self.gadget.stream(&forwarded, session_id.as_deref());
        let (result, error) = drain(rx, &mut translator, &mut sink, started).await;
        if drain_outcome(&result, error, &sink, &tx).await {
            return;
        }
        if translator.accumulated.is_empty() {
            let mut content = String::new();
            if wait {
                content = self
                    .replies
                    .wait_for_reply(cid, Duration::from_secs(self.config.reply_wait_s))
                    .await;
                if !content.is_empty() {
                    log(&format!(
                        "stream fell back to the reply drop after {:.1}s",
                        started.elapsed().as_secs_f32()
                    ));
                }
            }
            if content.is_empty()
                && let Some(result) = &result
            {
                content = extract_text(result.get("response").unwrap_or(&Value::Null));
            }
            if !content.is_empty() {
                translator.accumulated = content.clone();
                sink.send_delta(&content).await;
            }
        }
        sink.finish(&translator).await;
    }

    /// The hybrid path: send the turn with `wait` on a worker while
    /// draining the listen subscription. Returns `Done` when the
    /// stream is complete, `Answered` when the 502 was already sent,
    /// `Fallback` when the wait send was rejected and the legacy
    /// path (send without wait, subscription, drop) must run.
    async fn stream_hybrid(
        &self,
        prompt: &str,
        session_id: Option<&str>,
        translator: &mut StreamTranslator,
        sink: &mut SseSink,
        started: Instant,
        tx: &mpsc::UnboundedSender<StreamMsg>,
    ) -> HybridOutcome {
        let (ready_tx, ready_rx) = watch::channel(false);
        let (stop_tx, stop_rx) = watch::channel(false);
        let slot: Arc<std::sync::Mutex<HybridSlot>> =
            Arc::new(std::sync::Mutex::new(HybridSlot::default()));
        let gadget = self.gadget.clone();
        let prompt_owned = prompt.to_string();
        let session_owned = session_id.map(str::to_string);
        let sender_session = session_owned.clone();
        let wait_s = std::cmp::min(self.config.reply_wait_s, MAX_WAIT_S);
        let slot_sender = slot.clone();
        let ready_rx_sender = ready_rx;
        let stop_rx_sender = stop_rx.clone();
        let stop_tx_sender = stop_tx.clone();
        let sender = tokio::spawn(async move {
            // Wait for the subscription to open, polling every 100ms,
            // and give up without sending once the drain ends.
            loop {
                if *ready_rx_sender.borrow() {
                    break;
                }
                if *stop_rx_sender.borrow() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let outcome = gadget.ask(prompt_owned, sender_session, Some(wait_s)).await;
            let mut slot = slot_sender.lock().unwrap();
            match outcome {
                Ok(reply) => slot.reply = Some(reply),
                Err(e) => slot.error = Some(e.to_string()),
            }
            let _ = stop_tx_sender.send(true);
        });
        let rx = self
            .gadget
            .listen(session_id, Some(ready_tx), Some(stop_rx));
        let (_result, error) = drain(rx, translator, sink, started).await;
        let _ = stop_tx.send(true);
        let _ = sender.await;
        let (reply, slot_error) = {
            let slot = slot.lock().unwrap();
            (slot.reply.clone(), slot.error.clone())
        };
        if let Some(reply) = reply {
            if is_truthy(reply.get("ok").unwrap_or(&Value::Null)) {
                if !translator.accumulated.is_empty() {
                    if let Some(Value::String(waited)) = reply.get("reply")
                        && !waited.trim().is_empty()
                        && waited != &translator.accumulated
                    {
                        log(&format!(
                            "stream wait text differs: deltas_chars={} wait_chars={}",
                            translator.accumulated.chars().count(),
                            waited.chars().count()
                        ));
                    }
                    return HybridOutcome::Done;
                }
                let elapsed = started.elapsed().as_secs_f32();
                let mut content = wait_reply_text(&reply, elapsed);
                if content.is_empty() {
                    content = extract_text(reply.get("response").unwrap_or(&Value::Null));
                }
                if !content.is_empty() {
                    translator.accumulated = content.clone();
                    sink.send_delta(&content).await;
                }
                return HybridOutcome::Done;
            }
            log(&format!(
                "stream wait rejected ({}); falling back to the drop path",
                error_or_status(&reply)
            ));
            return HybridOutcome::Fallback;
        }
        if let Some(error) = error
            && !sink.headers_sent()
        {
            let _ = tx.send(StreamMsg::Fail(format!("musegadget unreachable: {error}")));
            return HybridOutcome::Answered;
        }
        if !sink.headers_sent() {
            let detail = slot_error.unwrap_or_else(|| "send failed".to_string());
            let _ = tx.send(StreamMsg::Fail(format!("musegadget unreachable: {detail}")));
            return HybridOutcome::Answered;
        }
        HybridOutcome::Done
    }
}

#[derive(Default)]
struct HybridSlot {
    reply: Option<Value>,
    error: Option<String>,
}

fn error_400_invalid_json() -> HttpResponse {
    json_response(
        StatusCode::BAD_REQUEST,
        &json!({"error": {"message": "invalid JSON body"}}),
    )
}

/// Drain one socket line stream into the translator, forwarding live
/// deltas. Returns the final result line, if any, and the socket
/// error, if the stream failed.
async fn drain(
    mut rx: mpsc::UnboundedReceiver<Result<SocketItem, GadgetError>>,
    translator: &mut StreamTranslator,
    sink: &mut SseSink,
    started: Instant,
) -> (Option<Value>, Option<GadgetError>) {
    let mut result = None;
    while let Some(item) = rx.recv().await {
        match item {
            Err(e) => {
                log(&format!(
                    "stream failed after {:.1}s: {e}",
                    started.elapsed().as_secs_f32()
                ));
                return (result, Some(e));
            }
            Ok(SocketItem::Result(value)) => {
                result = Some(value);
                break;
            }
            Ok(SocketItem::Event(event)) => {
                let name = py_str(event.get("event").unwrap_or(&Value::Null));
                if name == "delta.text_append" {
                    sink.appends += 1;
                } else {
                    log(&format!("stream event {name}"));
                }
                let delta = translator.feed(&event);
                if !delta.is_empty() {
                    sink.send_delta(&delta).await;
                }
            }
        }
    }
    (result, None)
}

/// Handle a drained stream's outcome: a socket error before any
/// chunk is a 502, and a failed turn is a 502 before any chunk (or a
/// finish once chunks went out). Returns true when the caller must
/// stop; the result stays with the caller for the receipt fallback.
async fn drain_outcome(
    result: &Option<Value>,
    error: Option<GadgetError>,
    sink: &SseSink,
    tx: &mpsc::UnboundedSender<StreamMsg>,
) -> bool {
    if let Some(error) = error
        && !sink.headers_sent()
    {
        let _ = tx.send(StreamMsg::Fail(format!("musegadget unreachable: {error}")));
        return true;
    }
    if let Some(result) = result
        && fail_turn(result, sink, tx).await
    {
        return true;
    }
    false
}

/// A failed turn: log it, and send the 502 when no chunk went out
/// yet. Returns true when the caller must stop (the 502 was sent).
async fn fail_turn(result: &Value, sink: &SseSink, tx: &mpsc::UnboundedSender<StreamMsg>) -> bool {
    if is_truthy(result.get("ok").unwrap_or(&Value::Null)) {
        return false;
    }
    log(&format!("stream turn not ok: {}", error_or_status(result)));
    if !sink.headers_sent() {
        let _ = tx.send(StreamMsg::Fail(format!(
            "Muse turn failed: {}",
            error_or_reply(result)
        )));
        return true;
    }
    false
}

/// The SSE body: the first frame the handler already received, then
/// the rest of the channel until it closes.
struct FrameStream {
    rx: mpsc::UnboundedReceiver<StreamMsg>,
    first: Option<Bytes>,
}

impl Stream for FrameStream {
    type Item = Result<Frame<Bytes>, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(first) = self.first.take() {
            return Poll::Ready(Some(Ok(Frame::data(first))));
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(StreamMsg::Frame(bytes))) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            Poll::Ready(Some(StreamMsg::Fail(_))) => Poll::Ready(None),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::gadget::SocketItem;
    use crate::gadget::fake::{FakeGadget, io_error};
    use crate::replies::fake::FakeReplyWait;
    use crate::server::bind_and_serve;
    use crate::test_utils::event;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn ev(item: Value) -> Result<SocketItem, GadgetError> {
        Ok(SocketItem::Event(item))
    }

    fn res(item: Value) -> Result<SocketItem, GadgetError> {
        Ok(SocketItem::Result(item))
    }

    struct TestServer<G: Gadget, R: ReplyWait> {
        port: u16,
        #[allow(dead_code)]
        shim: Shim<G, R>,
    }

    async fn serve_with<G: Gadget + 'static, R: ReplyWait + 'static>(
        config: Config,
        gadget: Arc<G>,
        replies: Arc<R>,
    ) -> TestServer<G, R> {
        let shim = Shim::new(config, gadget, replies);
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        let port = bind_and_serve(shim.clone(), addr).await.unwrap().port();
        TestServer { port, shim }
    }

    /// A raw HTTP/1.1 request over a plain socket, like the Python
    /// tests' `http.client`: returns (status, content-type, body).
    async fn http_request(
        port: u16,
        method: &str,
        path: &str,
        body: Option<&str>,
        headers: &[(&str, &str)],
    ) -> (u16, Option<String>, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        if let Some(body) = body {
            request.push_str(&format!(
                "Content-Length: {}\r\nContent-Type: application/json\r\n",
                body.len()
            ));
        }
        for (key, value) in headers {
            request.push_str(&format!("{key}: {value}\r\n"));
        }
        request.push_str("\r\n");
        if let Some(body) = body {
            request.push_str(body);
        }
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 65536];
        // Read until the header block is complete.
        let header_end = loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break 0;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
        };
        if header_end == 0 {
            return (0, None, buf);
        }
        let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let mut lines = header_text.lines();
        let status_line = lines.next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut content_type = None;
        let mut content_length: Option<usize> = None;
        let mut chunked = false;
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                match key.trim().to_ascii_lowercase().as_str() {
                    "content-type" => content_type = Some(value.trim().to_string()),
                    "content-length" => content_length = value.trim().parse().ok(),
                    "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                    _ => {}
                }
            }
        }
        let mut body = buf[header_end..].to_vec();
        if chunked {
            // De-chunk: read until the terminating 0-chunk.
            loop {
                while !body_contains_chunk_end(&body) {
                    let n = stream.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                match dechunk(&body) {
                    Some((decoded, _)) => return (status, content_type, decoded),
                    None => {
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return (status, content_type, body);
                        }
                        body.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        } else if let Some(len) = content_length {
            while body.len() < len {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            body.truncate(len);
            (status, content_type, body)
        } else {
            // Close-delimited: read until EOF.
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            (status, content_type, body)
        }
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn body_contains_chunk_end(body: &[u8]) -> bool {
        find_subslice(body, b"\r\n0\r\n\r\n").is_some()
            || body.ends_with(b"\r\n0\r\n\r\n")
            || find_subslice(body, b"\n0\n\n").is_some()
    }

    fn dechunk(body: &[u8]) -> Option<(Vec<u8>, usize)> {
        let mut out = Vec::new();
        let mut pos = 0;
        loop {
            let line_end = find_subslice(&body[pos..], b"\r\n").map(|i| pos + i)?;
            let size_line = std::str::from_utf8(&body[pos..line_end]).ok()?;
            let size = usize::from_str_radix(size_line.trim(), 16).ok()?;
            if size == 0 {
                return Some((out, line_end + 4));
            }
            let start = line_end + 2;
            if body.len() < start + size + 2 {
                return None;
            }
            out.extend_from_slice(&body[start..start + size]);
            pos = start + size + 2;
        }
    }

    fn json_body(raw: &[u8]) -> Value {
        serde_json::from_slice(raw).unwrap()
    }

    fn config_open() -> Config {
        Config {
            api_keys: vec![],
            ..Config::test_default()
        }
    }

    async fn post(
        port: u16,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (u16, Option<String>, Vec<u8>) {
        http_request(port, "POST", "/v1/chat/completions", Some(body), headers).await
    }

    #[test]
    fn authorized_without_keys_is_open() {
        assert!(authorized(&[], None));
        assert!(authorized(&[], Some("Bearer whatever")));
    }

    #[test]
    fn authorized_with_keys() {
        let keys = vec!["k1".to_string(), "k2".to_string()];
        assert!(!authorized(&keys, None));
        assert!(!authorized(&keys, Some("Token k1")));
        assert!(!authorized(&keys, Some("Bearer wrong")));
        assert!(authorized(&keys, Some("Bearer k2")));
    }

    #[test]
    fn completion_object_shape() {
        let obj = completion_object("model-x", "body");
        assert_eq!(obj["object"], "chat.completion");
        assert_eq!(obj["model"], "model-x");
        assert_eq!(
            obj["choices"][0]["message"],
            json!({"role": "assistant", "content": "body"})
        );
        assert_eq!(obj["choices"][0]["finish_reason"], "stop");
        assert_eq!(obj["usage"]["total_tokens"], 0);
    }

    #[tokio::test]
    async fn get_models_open() {
        let gadget = Arc::new(FakeGadget::new());
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, raw) = http_request(server.port, "GET", "/v1/models", None, &[]).await;
        assert_eq!(status, 200);
        let body = json_body(&raw);
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["muse"]);
    }

    #[tokio::test]
    async fn get_models_lists_configured_aliases() {
        let config = Config {
            model_aliases: vec!["muse".to_string(), "other".to_string()],
            ..config_open()
        };
        let gadget = Arc::new(FakeGadget::new());
        let server = serve_with(config, gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, raw) = http_request(server.port, "GET", "/v1/models", None, &[]).await;
        assert_eq!(status, 200);
        let body = json_body(&raw);
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["muse", "other"]);
    }

    #[tokio::test]
    async fn get_models_auth() {
        let config = Config {
            api_keys: vec!["k1".to_string(), "k2".to_string()],
            ..config_open()
        };
        let gadget = Arc::new(FakeGadget::new());
        let server = serve_with(config, gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, raw) = http_request(server.port, "GET", "/v1/models", None, &[]).await;
        assert_eq!(status, 403);
        assert_eq!(json_body(&raw)["error"]["code"], "invalid_api_key");
        let (status, _, _) = http_request(
            server.port,
            "GET",
            "/v1/models",
            None,
            &[("Authorization", "Bearer wrong")],
        )
        .await;
        assert_eq!(status, 403);
        let (status, _, _) = http_request(
            server.port,
            "GET",
            "/v1/models",
            None,
            &[("Authorization", "Bearer k2")],
        )
        .await;
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn healthz_reflects_socket_presence() {
        let dir = std::env::temp_dir().join(format!("limen-shim-healthz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("present.sock");
        std::fs::write(&sock, "").unwrap();
        let config = Config {
            socket_path: sock.to_string_lossy().to_string(),
            ..config_open()
        };
        let gadget = Arc::new(FakeGadget::new());
        let server = serve_with(config, gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, raw) = http_request(server.port, "GET", "/healthz", None, &[]).await;
        assert_eq!(status, 200);
        assert_eq!(
            json_body(&raw),
            json!({"ok": true, "socket": sock.to_string_lossy()})
        );
        let config = Config {
            socket_path: dir.join("absent.sock").to_string_lossy().to_string(),
            ..config_open()
        };
        let server = serve_with(
            config,
            Arc::new(FakeGadget::new()),
            Arc::new(FakeReplyWait::new()),
        )
        .await;
        let (status, _, raw) = http_request(server.port, "GET", "/healthz", None, &[]).await;
        assert_eq!(status, 503);
        assert_eq!(json_body(&raw)["ok"], false);
    }

    #[tokio::test]
    async fn unknown_paths_are_404() {
        let gadget = Arc::new(FakeGadget::new());
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        assert_eq!(
            http_request(server.port, "GET", "/nope", None, &[]).await.0,
            404
        );
        assert_eq!(
            http_request(server.port, "POST", "/nope", Some("{}"), &[])
                .await
                .0,
            404
        );
    }

    #[tokio::test]
    async fn post_invalid_json_is_400() {
        let gadget = Arc::new(FakeGadget::new());
        let (status, _, raw) = completion_case(gadget, "{not json").await;
        assert_eq!(status, 400);
        assert_error_contains(&raw, "invalid JSON");
    }

    #[tokio::test]
    async fn post_without_content_is_400() {
        let gadget = Arc::new(FakeGadget::new());
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, _) = post(server.port, r#"{"messages": []}"#, &[]).await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn post_denied_without_key() {
        let config = Config {
            api_keys: vec!["k1".to_string()],
            ..config_open()
        };
        let gadget = Arc::new(FakeGadget::new());
        let server = serve_with(config, gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, _) = post(server.port, r#"{"messages": [{"content": "hi"}]}"#, &[]).await;
        assert_eq!(status, 403);
    }

    fn gadget_with_reply(reply: Value) -> Arc<FakeGadget> {
        Arc::new(FakeGadget::new().with_ask_replies(vec![Ok(reply)]))
    }

    #[tokio::test]
    async fn post_completion_uses_response_text() {
        let gadget = gadget_with_reply(json!({"ok": true, "response": "plain answer"}));
        let server = serve_with(
            config_open(),
            gadget.clone(),
            Arc::new(FakeReplyWait::new()),
        )
        .await;
        let (status, _, raw) = post(
            server.port,
            r#"{"messages": [{"role": "user", "content": "hi"}], "wait_reply": false, "model": "muse"}"#,
            &[("X-Session-Id", "My Session!")],
        )
        .await;
        assert_eq!(status, 200);
        let body = json_body(&raw);
        assert_eq!(body["model"], "muse");
        assert_eq!(body["choices"][0]["message"]["content"], "plain answer");
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![("user: hi".to_string(), Some("My-Session".to_string()), None)]
        );
    }

    #[tokio::test]
    async fn post_completion_uses_wait_reply_text() {
        // Non-streaming takes the reply from the service wait path: no
        // footer in the prompt and no reply drop involved.
        let gadget =
            gadget_with_reply(json!({"ok": true, "reply": "waited text", "response": "receipt"}));
        let replies = Arc::new(FakeReplyWait::panicking());
        let (status, _, raw) = completion_case_with_replies(
            gadget.clone(),
            replies,
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse", "user": "bob"}"#,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            json_body(&raw)["choices"][0]["message"]["content"],
            "waited text"
        );
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, Some("bob".to_string()));
        assert!(!calls[0].0.contains("[limen]"));
        assert_eq!(calls[0].2, Some(30));
    }

    #[tokio::test]
    async fn post_completion_accepts_partial_wait_reply() {
        let gadget = gadget_with_reply(json!({
            "ok": true,
            "reply": "partial text",
            "partial": true,
            "detail": "no answer within 30 seconds",
            "response": "receipt",
        }));
        let (status, _, raw) = completion_case(
            gadget,
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse"}"#,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            json_body(&raw)["choices"][0]["message"]["content"],
            "partial text"
        );
    }

    #[tokio::test]
    async fn post_completion_falls_back_when_wait_empty() {
        let gadget = gadget_with_reply(json!({
            "ok": true,
            "reply": null,
            "detail": "no answer within 30 seconds",
            "response": {"choices": [{"message": {"content": "nested"}}]},
        }));
        let replies = Arc::new(FakeReplyWait::panicking());
        let server = serve_with(config_open(), gadget, replies).await;
        let (status, _, raw) = post(
            server.port,
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse"}"#,
            &[],
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            json_body(&raw)["choices"][0]["message"]["content"],
            "nested"
        );
    }

    #[tokio::test]
    async fn post_model_muse_with_header_keeps_header_session() {
        let gadget = gadget_with_reply(json!({"ok": true, "reply": "text", "response": "r"}));
        let (status, _, raw) = completion_case_with_headers(
            gadget.clone(),
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse"}"#,
            &[("X-Session-Id", "sess-9")],
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(json_body(&raw)["model"], "muse");
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![("user: hi".to_string(), Some("sess-9".to_string()), Some(30))]
        );
    }

    #[tokio::test]
    async fn post_header_wins_over_body_session_fields() {
        let gadget = gadget_with_reply(json!({"ok": true, "reply": "text", "response": "r"}));
        let (status, _, _) = completion_case_with_headers(
            gadget.clone(),
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse", "session_id": "sess-body", "user": "bob"}"#,
            &[("X-Session-Id", "sess-header")],
        )
        .await;
        assert_eq!(status, 200);
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![(
                "user: hi".to_string(),
                Some("sess-header".to_string()),
                Some(30)
            )]
        );
    }

    #[tokio::test]
    async fn post_model_slash_uuid_wins_over_header() {
        let gadget = gadget_with_reply(json!({"ok": true, "reply": "text", "response": "r"}));
        let server = serve_with(
            config_open(),
            gadget.clone(),
            Arc::new(FakeReplyWait::new()),
        )
        .await;
        let (status, _, _) = post(
            server.port,
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse/12345678-1234-1234-1234-1234567890ab"}"#,
            &[("X-Session-Id", "sess-9")],
        )
        .await;
        assert_eq!(status, 200);
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![(
                "user: hi".to_string(),
                Some("12345678-1234-1234-1234-1234567890ab".to_string()),
                Some(30)
            )]
        );
    }

    #[tokio::test]
    async fn post_bare_model_without_header_goes_to_main() {
        let gadget = gadget_with_reply(json!({"ok": true, "reply": "text", "response": "r"}));
        let (status, _, _) = completion_case(
            gadget.clone(),
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse"}"#,
        )
        .await;
        assert_eq!(status, 200);
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(*calls, vec![("user: hi".to_string(), None, Some(30))]);
    }

    #[tokio::test]
    async fn post_missing_model_is_400() {
        let gadget = Arc::new(FakeGadget::new());
        let (status, _, raw) = completion_case(
            gadget,
            r#"{"messages": [{"role": "user", "content": "hi"}]}"#,
        )
        .await;
        assert_eq!(status, 400);
        assert_error_contains(&raw, "no model specified");
    }

    #[tokio::test]
    async fn post_unknown_model_is_404() {
        let gadget = gadget_with_reply(json!({"ok": true, "reply": "text"}));
        let (status, _, raw) = completion_case(
            gadget.clone(),
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "nope"}"#,
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(json_body(&raw)["error"]["code"], "model_not_found");
        assert!(gadget.ask_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn post_bad_model_conversation_is_400() {
        let gadget = gadget_with_reply(json!({"ok": true, "reply": "text"}));
        let (status, _, raw) = completion_case(
            gadget.clone(),
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse/not-a-uuid"}"#,
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(json_body(&raw)["error"]["code"], "invalid_model");
        assert!(gadget.ask_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn post_completion_unextractable_response_is_json_dumped() {
        let gadget = gadget_with_reply(json!({"ok": true, "response": null}));
        let (status, _, raw) = completion_case(gadget, r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse", "wait_reply": false}"#).await;
        assert_eq!(status, 200);
        assert_eq!(json_body(&raw)["choices"][0]["message"]["content"], "null");
    }

    #[tokio::test]
    async fn post_completion_socket_failure_is_502() {
        let gadget = Arc::new(FakeGadget::new().with_ask_replies(vec![Err(io_error("no socket"))]));
        let (status, _, raw) = completion_case(
            gadget,
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse"}"#,
        )
        .await;
        assert_eq!(status, 502);
        assert_error_contains(&raw, "unreachable");
    }

    #[tokio::test]
    async fn post_completion_failed_turn_is_502() {
        let gadget = gadget_with_reply(json!({"ok": false, "error": "bad turn"}));
        let (status, _, raw) = completion_case(
            gadget,
            r#"{"messages": [{"role": "user", "content": "hi"}], "model": "muse"}"#,
        )
        .await;
        assert_eq!(status, 502);
        assert_error_contains(&raw, "bad turn");
    }

    fn stream_body(extra: &str) -> String {
        format!(
            r#"{{"messages": [{{"role": "user", "content": "hi"}}], "stream": true, "model": "muse"{extra}}}"#
        )
    }

    /// Serve a fake gadget with default replies and post a streaming
    /// completion; returns what `post` returns.
    async fn stream_case(gadget: Arc<FakeGadget>, extra: &str) -> (u16, Option<String>, Vec<u8>) {
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        post(server.port, &stream_body(extra), &[]).await
    }

    /// The same, with scripted replies (e.g. a panicking drop).
    async fn stream_case_with_replies(
        gadget: Arc<FakeGadget>,
        replies: Arc<FakeReplyWait>,
        extra: &str,
    ) -> (u16, Option<String>, Vec<u8>) {
        let server = serve_with(config_open(), gadget, replies).await;
        post(server.port, &stream_body(extra), &[]).await
    }

    /// Serve a fake gadget with default replies and post a completion;
    /// returns what `post` returns.
    async fn completion_case(
        gadget: Arc<FakeGadget>,
        body: &str,
    ) -> (u16, Option<String>, Vec<u8>) {
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        post(server.port, body, &[]).await
    }

    /// Serve a fake gadget with scripted replies and post a completion.
    async fn completion_case_with_replies(
        gadget: Arc<FakeGadget>,
        replies: Arc<FakeReplyWait>,
        body: &str,
    ) -> (u16, Option<String>, Vec<u8>) {
        let server = serve_with(config_open(), gadget, replies).await;
        post(server.port, body, &[]).await
    }

    /// Serve a fake gadget and post a completion with extra headers.
    async fn completion_case_with_headers(
        gadget: Arc<FakeGadget>,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (u16, Option<String>, Vec<u8>) {
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        post(server.port, body, headers).await
    }

    /// Assert an error body names the expected failure.
    fn assert_error_contains(raw: &[u8], needle: &str) {
        let body = json_body(raw);
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(needle),
            "error {message:?} lacks {needle:?}"
        );
    }

    #[tokio::test]
    async fn stream_completion_emits_sse_deltas() {
        // Hybrid: deltas stream live from the subscription and the
        // finished wait text is the closing confirmation only, never
        // re-emitted.
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(
                    json!({"ok": true, "reply": "World", "response": "receipt"}),
                )])
                .with_listen_items(vec![
                    ev(event("message.user", "n1", "")),
                    ev(event("delta.text_append", "a1", "Wor")),
                    ev(event("delta.text_append", "a1", "ld")),
                    ev(event("delta.message_done", "a1", "World")),
                ]),
        );
        let replies = Arc::new(FakeReplyWait::panicking());
        let server = serve_with(config_open(), gadget.clone(), replies).await;
        let (status, content_type, raw) = post(server.port, &stream_body(""), &[]).await;
        assert_eq!(status, 200);
        assert_eq!(content_type.as_deref(), Some("text/event-stream"));
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(r#""content": "Wor""#));
        assert!(text.contains(r#""content": "ld""#));
        assert!(!text.contains(r#""content": "World""#));
        assert!(text.contains(r#""finish_reason": "stop""#));
        assert!(text.contains("data: [DONE]"));
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, None);
        assert!(!calls[0].0.contains("[limen]"));
        assert_eq!(calls[0].2, Some(30));
    }

    #[tokio::test]
    async fn stream_hybrid_emits_wait_text_when_no_deltas() {
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(json!({
                    "ok": true, "reply": "waited full text", "response": "receipt"
                }))])
                .with_listen_items(vec![ev(event("message.user", "n1", ""))]),
        );
        let (status, _, raw) =
            stream_case_with_replies(gadget.clone(), Arc::new(FakeReplyWait::panicking()), "")
                .await;
        assert_eq!(status, 200);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(r#""content": "waited full text""#));
        assert!(text.contains(r#""finish_reason": "stop""#));
        assert!(text.contains("data: [DONE]"));
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, None);
        assert!(!calls[0].0.contains("[limen]"));
        assert_eq!(calls[0].2, Some(30));
    }

    #[tokio::test]
    async fn stream_hybrid_accepts_partial_wait_reply() {
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(json!({
                    "ok": true,
                    "reply": "partial text",
                    "partial": true,
                    "detail": "no answer within 30 seconds",
                    "response": "receipt",
                }))])
                .with_listen_items(vec![ev(event("message.user", "n1", ""))]),
        );
        let (status, _, raw) =
            stream_case_with_replies(gadget, Arc::new(FakeReplyWait::panicking()), "").await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&raw).contains(r#""content": "partial text""#));
    }

    #[tokio::test]
    async fn stream_result_failure_before_headers_is_502() {
        // The wait send is rejected, so the legacy path runs and its
        // failing result still answers 502 before any chunk.
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(json!({"ok": false, "error": "wait rejected"}))])
                .with_listen_items(vec![ev(event("message.user", "n1", ""))])
                .with_stream_items(vec![res(json!({"ok": false, "error": "nope"}))]),
        );
        let (status, _, raw) = stream_case(gadget, "").await;
        assert_eq!(status, 502);
        assert_error_contains(&raw, "nope");
    }

    #[tokio::test]
    async fn stream_hybrid_wait_text_differs_keeps_streamed_text() {
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(json!({
                    "ok": true, "reply": "different", "response": "receipt"
                }))])
                .with_listen_items(vec![
                    ev(event("message.user", "n1", "")),
                    ev(event("delta.text_append", "a1", "abc")),
                    ev(event("delta.message_done", "a1", "abc")),
                ]),
        );
        let (status, _, raw) = stream_case(gadget, "").await;
        assert_eq!(status, 200);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(r#""content": "abc""#));
        assert!(!text.contains(r#""content": "different""#));
        assert!(text.contains("data: [DONE]"));
    }

    #[tokio::test]
    async fn stream_hybrid_empty_wait_falls_back_to_receipt() {
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(json!({
                    "ok": true,
                    "reply": null,
                    "detail": "no answer within 30 seconds",
                    "response": "receipt text",
                }))])
                .with_listen_items(vec![ev(event("message.user", "n1", ""))]),
        );
        let (status, _, raw) = stream_case(gadget, "").await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&raw).contains(r#""content": "receipt text""#));
    }

    #[tokio::test]
    async fn stream_hybrid_send_failure_after_deltas_finishes() {
        // Deltas streamed but the wait send failed: the stream still
        // finishes with what streamed instead of failing the request.
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Err(io_error("down"))])
                .with_listen_items(vec![
                    ev(event("message.user", "n1", "")),
                    ev(event("delta.text_append", "a1", "abc")),
                    ev(event("delta.message_done", "a1", "abc")),
                ]),
        );
        let (status, _, raw) = stream_case(gadget.clone(), "").await;
        assert_eq!(status, 200);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(r#""content": "abc""#));
        assert!(text.contains("data: [DONE]"));
        assert_eq!(gadget.ask_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stream_hybrid_subscription_never_opens_is_502() {
        // The subscription never opens, so the sender gives up without
        // sending and the request fails instead of hanging.
        let mut gadget = FakeGadget::new().with_ask_replies(vec![Ok(json!({
            "ok": true, "reply": "never sent"
        }))]);
        gadget.listen_sets_ready = false;
        let gadget = Arc::new(gadget);
        let server = serve_with(
            config_open(),
            gadget.clone(),
            Arc::new(FakeReplyWait::new()),
        )
        .await;
        let (status, _, raw) = post(server.port, &stream_body(""), &[]).await;
        assert_eq!(status, 502);
        assert_error_contains(&raw, "unreachable");
        assert!(gadget.ask_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stream_socket_failure_before_headers_is_502() {
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Err(io_error("down"))])
                .with_listen_items(vec![Err(io_error("down"))]),
        );
        let (status, _, raw) = stream_case(gadget, "").await;
        assert_eq!(status, 502);
        assert_error_contains(&raw, "unreachable");
    }

    #[tokio::test]
    async fn stream_falls_back_to_reply_drop() {
        // The wait send is rejected (concurrent wait on the same
        // session), so the legacy path runs: send without wait,
        // subscription, drop.
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(json!({"ok": false, "error": "wait locked"}))])
                .with_listen_items(vec![ev(event("message.user", "n1", ""))])
                .with_stream_items(vec![
                    ev(event("message.user", "n1", "")),
                    res(json!({"ok": true})),
                ]),
        );
        let replies = Arc::new(FakeReplyWait::with_text("fallback text"));
        let server = serve_with(config_open(), gadget.clone(), replies).await;
        let (status, _, raw) = post(server.port, &stream_body(""), &[]).await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&raw).contains(r#""content": "fallback text""#));
        let calls = gadget.ask_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, None);
        assert!(!calls[0].0.contains("[limen]"));
        assert_eq!(calls[0].2, Some(30));
        let stream_calls = gadget.stream_calls.lock().unwrap();
        assert_eq!(stream_calls.len(), 1);
        assert!(stream_calls[0].0.contains("[limen]"));
    }

    #[tokio::test]
    async fn stream_no_drop_and_no_result_text() {
        let gadget = Arc::new(FakeGadget::new().with_stream_items(vec![
            ev(event("message.user", "n1", "")),
            res(json!({"ok": true})),
        ]));
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, raw) =
            post(server.port, &stream_body(r#", "wait_reply": false"#), &[]).await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&raw).contains("data: [DONE]"));
    }

    #[tokio::test]
    async fn stream_done_text_mismatch_still_completes() {
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(json!({
                    "ok": true, "reply": "abc", "response": "receipt"
                }))])
                .with_listen_items(vec![
                    ev(event("message.user", "n1", "")),
                    ev(event("delta.text_append", "a1", "abc")),
                    ev(event("delta.message_done", "a1", "abX")),
                ]),
        );
        let (status, _, raw) =
            stream_case_with_replies(gadget, Arc::new(FakeReplyWait::panicking()), "").await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&raw).contains(r#""content": "abc""#));
    }

    #[tokio::test]
    async fn stream_failure_after_headers_still_finishes() {
        // The wait send is rejected, so the legacy path runs; its late
        // result failure still finishes the stream with what streamed.
        let gadget = Arc::new(
            FakeGadget::new()
                .with_ask_replies(vec![Ok(json!({"ok": false, "error": "wait rejected"}))])
                .with_listen_items(vec![ev(event("message.user", "n1", ""))])
                .with_stream_items(vec![
                    ev(event("message.user", "n1", "")),
                    ev(event("delta.text_append", "a1", "partial")),
                    res(json!({"ok": false, "error": "late failure"})),
                ]),
        );
        let (status, _, raw) = stream_case(gadget, "").await;
        assert_eq!(status, 200);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(r#""content": "partial""#));
        assert!(text.contains("data: [DONE]"));
    }

    #[tokio::test]
    async fn stream_extracts_result_text_when_no_deltas() {
        let gadget = Arc::new(FakeGadget::new().with_stream_items(vec![
            ev(event("message.user", "n1", "")),
            res(json!({"ok": true, "response": "whole reply"})),
        ]));
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, raw) =
            post(server.port, &stream_body(r#", "wait_reply": false"#), &[]).await;
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&raw).contains(r#""content": "whole reply""#));
    }

    #[tokio::test]
    async fn stream_uses_resolved_session_from_model() {
        let gadget = Arc::new(FakeGadget::new().with_stream_items(vec![
            ev(event("message.user", "n1", "")),
            ev(event("delta.text_append", "a1", "hi")),
            res(json!({"ok": true})),
        ]));
        let (status, _, raw) = stream_case(
            gadget.clone(),
            r#", "wait_reply": false, "model": "muse/12345678-1234-1234-1234-1234567890ab""#,
        )
        .await;
        assert_eq!(status, 200);
        let stream_calls = gadget.stream_calls.lock().unwrap();
        assert_eq!(
            *stream_calls,
            vec![(
                "user: hi".to_string(),
                Some("12345678-1234-1234-1234-1234567890ab".to_string())
            )]
        );
        assert!(String::from_utf8_lossy(&raw).contains(r#""content": "hi""#));
    }

    #[tokio::test]
    async fn stream_legacy_socket_failure_is_502() {
        // wait_reply False skips the hybrid; the legacy socket failure
        // is a 502.
        let gadget = Arc::new(FakeGadget::new().with_stream_items(vec![Err(io_error("down"))]));
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, raw) =
            post(server.port, &stream_body(r#", "wait_reply": false"#), &[]).await;
        assert_eq!(status, 502);
        assert_error_contains(&raw, "unreachable");
    }

    #[tokio::test]
    async fn stream_listen_socket_failure_is_502() {
        let gadget = Arc::new(FakeGadget::new().with_listen_items(vec![Err(io_error("down"))]));
        let (status, _, raw) = completion_case(
            gadget,
            r#"{"stream": true, "listen_only": true, "model": "muse", "session_id": "sess-1"}"#,
        )
        .await;
        assert_eq!(status, 502);
        assert_error_contains(&raw, "unreachable");
    }

    #[tokio::test]
    async fn stream_listen_result_failure_is_502() {
        let gadget = Arc::new(
            FakeGadget::new().with_listen_items(vec![res(json!({"ok": false, "error": "nope"}))]),
        );
        let (status, _, raw) = completion_case(
            gadget,
            r#"{"stream": true, "listen_only": true, "model": "muse", "session_id": "sess-1"}"#,
        )
        .await;
        assert_eq!(status, 502);
        assert_error_contains(&raw, "nope");
    }

    #[tokio::test]
    async fn stream_listen_only_observes_live_turn_without_drop() {
        let gadget = Arc::new(FakeGadget::new().with_listen_items(vec![
            ev(event("message.user", "n0", "")),
            ev(event("delta.text_append", "a0", "old")),
            ev(event("message.user", "n1", "")),
            ev(event("delta.text_append", "a1", "live")),
            ev(event("delta.message_done", "a1", "live")),
            res(json!({"ok": true})),
        ]));
        let replies = Arc::new(FakeReplyWait::panicking());
        let server = serve_with(config_open(), gadget, replies).await;
        let (status, _, raw) = post(
            server.port,
            r#"{"stream": true, "listen_only": true, "model": "muse", "session_id": "sess-1"}"#,
            &[],
        )
        .await;
        assert_eq!(status, 200);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains(r#""content": "live""#));
        assert!(text.contains("data: [DONE]"));
    }

    #[tokio::test]
    async fn stream_response_disables_proxy_buffering() {
        let gadget = Arc::new(FakeGadget::new().with_listen_items(vec![
            ev(event("message.user", "n1", "")),
            ev(event("delta.text_append", "a1", "live")),
            ev(event("delta.message_done", "a1", "live")),
            res(json!({"ok": true})),
        ]));
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        let mut stream = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .unwrap();
        let body =
            r#"{"stream": true, "listen_only": true, "model": "muse", "session_id": "sess-1"}"#;
        let request = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        // Read the header block only.
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 65536];
        let header_end = loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break 0;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
        assert!(header_text.contains("HTTP/1.1 200"));
        assert!(
            header_text
                .to_ascii_lowercase()
                .contains("x-accel-buffering: no")
        );
        // Drain the rest; the first read may already hold body bytes.
        let mut rest = buf[header_end..].to_vec();
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            rest.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&rest);
        assert!(text.contains(r#""content": "live""#));
    }

    #[tokio::test]
    async fn listen_only_without_stream_is_400() {
        let gadget = Arc::new(FakeGadget::new());
        let server = serve_with(config_open(), gadget, Arc::new(FakeReplyWait::new())).await;
        let (status, _, raw) = post(server.port, r#"{"listen_only": true}"#, &[]).await;
        assert_eq!(status, 400);
        assert!(String::from_utf8_lossy(&raw).contains("listen_only requires stream"));
    }
}
