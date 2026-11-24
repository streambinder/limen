//! The gadget service over its Unix socket: one JSON line per
//! request, one-line replies for `ask_muse`, line streams for the
//! subscription. Direct port of the Python socket helpers.
//!
//! The `Gadget` trait is the seam the tests use: the real
//! implementation talks to the socket, the fake one (tests only)
//! returns scripted replies and records the calls, like the Python
//! tests' monkeypatched `ask_muse` / `iter_muse_stream` /
//! `iter_muse_listen`.

use serde_json::{Value, json};
use std::fmt;
use std::io;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio::time;

use crate::config::{Config, log};
use crate::json::{is_truthy, py_str, python_dumps};

#[derive(Debug)]
pub enum GadgetError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for GadgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GadgetError::Io(e) => write!(f, "{e}"),
            GadgetError::Json(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GadgetError {}

/// One item of a socket line stream: a subscribe `event`, or the
/// final `result` line (after which the stream ends).
#[derive(Debug, Clone)]
pub enum SocketItem {
    Event(Value),
    Result(Value),
}

/// The gadget client: one-shot requests and line streams.
pub trait Gadget: Send + Sync {
    /// Send one request and read its one-line reply.
    ///
    /// The `impl Future + Send` form (instead of `async fn`) is what
    /// lets callers spawn the wait send on a worker task. Arguments
    /// are owned so the future carries no borrowed lifetimes.
    fn raw(
        &self,
        payload: Value,
    ) -> impl std::future::Future<Output = Result<Value, GadgetError>> + Send + '_;

    /// Send a turn; `wait_s` asks the service to wait for the
    /// finished reply. The session is omitted for the main chat.
    fn ask(
        &self,
        prompt: String,
        session_id: Option<String>,
        wait_s: Option<u64>,
    ) -> impl std::future::Future<Output = Result<Value, GadgetError>> + Send + '_ {
        async move {
            let mut payload = serde_json::Map::new();
            payload.insert("message".to_string(), Value::String(prompt));
            if let Some(session_id) = session_id {
                payload.insert("session_id".to_string(), Value::String(session_id));
            }
            if let Some(wait_s) = wait_s {
                payload.insert("wait".to_string(), json!(wait_s));
            }
            self.raw(Value::Object(payload)).await
        }
    }

    /// Open a subscribe-only connection for a turn. `ready` is set
    /// once the request is on the wire; `stop` ends the stream
    /// promptly instead of waiting for the service to close it.
    fn listen(
        &self,
        session_id: Option<&str>,
        ready: Option<watch::Sender<bool>>,
        stop: Option<watch::Receiver<bool>>,
    ) -> mpsc::UnboundedReceiver<Result<SocketItem, GadgetError>>;

    /// Open a subscribe connection that also sends the turn.
    fn stream(
        &self,
        prompt: &str,
        session_id: Option<&str>,
    ) -> mpsc::UnboundedReceiver<Result<SocketItem, GadgetError>>;
}

/// One request on a fresh Unix socket, one line back. Reads until
/// the buffer ends with a newline (or the socket closes).
async fn socket_line(
    socket_path: &str,
    request: &[u8],
    timeout: Duration,
) -> Result<Value, GadgetError> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .map_err(GadgetError::Io)?;
    stream.write_all(request).await.map_err(GadgetError::Io)?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    while !buf.ends_with(b"\n") {
        let read = time::timeout(timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| GadgetError::Io(io::Error::new(io::ErrorKind::TimedOut, "timed out")))?;
        let n = read.map_err(GadgetError::Io)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice::<Value>(&buf).map_err(GadgetError::Json)
}

/// Read a socket line stream on a background task, forwarding parsed
/// lines to `tx`. A line that is an object with a dict `event` is an
/// event; any other line is the result and ends the stream.
fn spawn_line_reader(
    socket_path: String,
    request: Vec<u8>,
    timeout: Duration,
    ready: Option<watch::Sender<bool>>,
    stop: Option<watch::Receiver<bool>>,
) -> mpsc::UnboundedReceiver<Result<SocketItem, GadgetError>> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stream = match UnixStream::connect(&socket_path).await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = tx.send(Err(GadgetError::Io(e)));
                return;
            }
        };
        if stream.write_all(&request).await.is_err() {
            let _ = tx.send(Err(GadgetError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "failed to write request",
            ))));
            return;
        }
        if let Some(ready) = ready {
            let _ = ready.send(true);
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            let read = stream.read(&mut chunk);
            let n = match &stop {
                Some(stop) => match time::timeout(Duration::from_millis(500), read).await {
                    Ok(read) => read,
                    Err(_) => {
                        if *stop.borrow() {
                            return;
                        }
                        continue;
                    }
                },
                None => match time::timeout(timeout, read).await {
                    Ok(read) => read,
                    Err(_) => {
                        let _ = tx.send(Err(GadgetError::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out",
                        ))));
                        return;
                    }
                },
            };
            let n = match n {
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.send(Err(GadgetError::Io(e)));
                    return;
                }
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                if line
                    .iter()
                    .all(|&b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
                {
                    continue;
                }
                let parsed = match serde_json::from_slice::<Value>(&line) {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        let _ = tx.send(Err(GadgetError::Json(e)));
                        return;
                    }
                };
                if let Some(event) = parsed.get("event")
                    && event.is_object()
                {
                    if tx.send(Ok(SocketItem::Event(event.clone()))).is_err() {
                        return;
                    }
                    continue;
                }
                let _ = tx.send(Ok(SocketItem::Result(parsed)));
                return;
            }
        }
    });
    rx
}

/// The real client: the gadget service on its Unix socket.
pub struct UnixGadget {
    pub socket_path: String,
    pub timeout: Duration,
}

impl UnixGadget {
    pub fn new(config: &Config) -> Self {
        UnixGadget {
            socket_path: config.socket_path.clone(),
            timeout: config.timeout,
        }
    }
}

impl Gadget for UnixGadget {
    async fn raw(&self, payload: Value) -> Result<Value, GadgetError> {
        let mut bytes = python_dumps(&payload).into_bytes();
        bytes.push(b'\n');
        socket_line(&self.socket_path, &bytes, self.timeout).await
    }

    fn listen(
        &self,
        session_id: Option<&str>,
        ready: Option<watch::Sender<bool>>,
        stop: Option<watch::Receiver<bool>>,
    ) -> mpsc::UnboundedReceiver<Result<SocketItem, GadgetError>> {
        let mut payload = serde_json::Map::new();
        payload.insert("subscribe".to_string(), Value::Bool(true));
        payload.insert("listen".to_string(), Value::Bool(true));
        if let Some(session_id) = session_id {
            payload.insert(
                "session_id".to_string(),
                Value::String(session_id.to_string()),
            );
        }
        let mut bytes = python_dumps(&Value::Object(payload)).into_bytes();
        bytes.push(b'\n');
        spawn_line_reader(self.socket_path.clone(), bytes, self.timeout, ready, stop)
    }

    fn stream(
        &self,
        prompt: &str,
        session_id: Option<&str>,
    ) -> mpsc::UnboundedReceiver<Result<SocketItem, GadgetError>> {
        let mut payload = serde_json::Map::new();
        payload.insert("message".to_string(), Value::String(prompt.to_string()));
        payload.insert("subscribe".to_string(), Value::Bool(true));
        if let Some(session_id) = session_id {
            payload.insert(
                "session_id".to_string(),
                Value::String(session_id.to_string()),
            );
        }
        let mut bytes = python_dumps(&Value::Object(payload)).into_bytes();
        bytes.push(b'\n');
        spawn_line_reader(self.socket_path.clone(), bytes, self.timeout, None, None)
    }
}

/// One identity query: `{"op": "identity"}` on a fresh socket.
/// Returns the name, or (None, reason) when it cannot be learned.
pub async fn query_identity_once<G: Gadget>(gadget: &G) -> (Option<String>, Option<String>) {
    let reply = match gadget.raw(json!({"op": "identity"})).await {
        Ok(reply) => reply,
        Err(e) => return (None, Some(e.to_string())),
    };
    if !reply.is_object() || !is_truthy(reply.get("ok").unwrap_or(&Value::Null)) {
        let error = match reply.as_object() {
            Some(map) => map.get("error").map_or("None".to_string(), py_str),
            None => "bad reply".to_string(),
        };
        return (None, Some(error));
    }
    let name = reply
        .get("identity")
        .and_then(Value::as_object)
        .and_then(|identity| identity.get("name"))
        .and_then(Value::as_str);
    match name {
        Some(name) if !name.trim().is_empty() => (Some(name.trim().to_string()), None),
        _ => (None, Some("no name in identity".to_string())),
    }
}

/// Ask the service for its identity until it answers or `wait`
/// elapses, polling every `poll`. Returns the name, or None.
pub async fn discover_muse_name<G: Gadget>(
    gadget: &G,
    wait: Duration,
    poll: Duration,
) -> Option<String> {
    let deadline = Instant::now() + wait;
    let mut reason: Option<String>;
    loop {
        let (name, error) = query_identity_once(gadget).await;
        if let Some(name) = name {
            return Some(name);
        }
        reason = error;
        if Instant::now() >= deadline {
            log(&format!(
                "identity discovery failed after {}s: {}",
                wait.as_secs(),
                reason.as_deref().unwrap_or("")
            ));
            return None;
        }
        time::sleep(poll).await;
    }
}

#[cfg(test)]
pub mod fake {
    //! The fake gadget for tests: scripted replies, recorded calls.
    //! Mirrors the Python tests' monkeypatched socket functions.

    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    pub fn io_error(msg: &str) -> GadgetError {
        GadgetError::Io(io::Error::other(msg.to_string()))
    }

    /// One recorded `ask` call: (prompt, session id, wait seconds).
    pub type AskCall = (String, Option<String>, Option<u64>);

    pub struct FakeGadget {
        pub raw_replies: Mutex<VecDeque<Result<Value, GadgetError>>>,
        pub raw_sent: Mutex<Vec<Value>>,
        pub ask_replies: Mutex<VecDeque<Result<Value, GadgetError>>>,
        pub ask_calls: Mutex<Vec<AskCall>>,
        pub listen_items: Mutex<VecDeque<Result<SocketItem, GadgetError>>>,
        pub listen_calls: Mutex<Vec<Option<String>>>,
        pub listen_sets_ready: bool,
        pub stream_items: Mutex<VecDeque<Result<SocketItem, GadgetError>>>,
        pub stream_calls: Mutex<Vec<(String, Option<String>)>>,
    }

    impl FakeGadget {
        pub fn new() -> Self {
            FakeGadget {
                raw_replies: Mutex::new(VecDeque::new()),
                raw_sent: Mutex::new(Vec::new()),
                ask_replies: Mutex::new(VecDeque::new()),
                ask_calls: Mutex::new(Vec::new()),
                listen_items: Mutex::new(VecDeque::new()),
                listen_calls: Mutex::new(Vec::new()),
                listen_sets_ready: true,
                stream_items: Mutex::new(VecDeque::new()),
                stream_calls: Mutex::new(Vec::new()),
            }
        }

        pub fn with_ask_replies(mut self, replies: Vec<Result<Value, GadgetError>>) -> Self {
            self.ask_replies = Mutex::new(replies.into());
            self
        }

        pub fn with_listen_items(mut self, items: Vec<Result<SocketItem, GadgetError>>) -> Self {
            self.listen_items = Mutex::new(items.into());
            self
        }

        pub fn with_stream_items(mut self, items: Vec<Result<SocketItem, GadgetError>>) -> Self {
            self.stream_items = Mutex::new(items.into());
            self
        }
    }

    impl Default for FakeGadget {
        fn default() -> Self {
            FakeGadget::new()
        }
    }

    impl Gadget for FakeGadget {
        async fn raw(&self, payload: Value) -> Result<Value, GadgetError> {
            self.raw_sent.lock().unwrap().push(payload);
            self.raw_replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(json!({"ok": true})))
        }

        async fn ask(
            &self,
            prompt: String,
            session_id: Option<String>,
            wait_s: Option<u64>,
        ) -> Result<Value, GadgetError> {
            self.ask_calls
                .lock()
                .unwrap()
                .push((prompt, session_id, wait_s));
            self.ask_replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(json!({"ok": true})))
        }

        fn listen(
            &self,
            session_id: Option<&str>,
            ready: Option<watch::Sender<bool>>,
            _stop: Option<watch::Receiver<bool>>,
        ) -> mpsc::UnboundedReceiver<Result<SocketItem, GadgetError>> {
            self.listen_calls
                .lock()
                .unwrap()
                .push(session_id.map(str::to_string));
            let (tx, rx) = mpsc::unbounded_channel();
            let items: Vec<_> = self.listen_items.lock().unwrap().drain(..).collect();
            let sets_ready = self.listen_sets_ready;
            tokio::spawn(async move {
                if sets_ready && let Some(ready) = ready {
                    let _ = ready.send(true);
                }
                for item in items {
                    if tx.send(item).is_err() {
                        return;
                    }
                }
            });
            rx
        }

        fn stream(
            &self,
            prompt: &str,
            session_id: Option<&str>,
        ) -> mpsc::UnboundedReceiver<Result<SocketItem, GadgetError>> {
            self.stream_calls
                .lock()
                .unwrap()
                .push((prompt.to_string(), session_id.map(str::to_string)));
            let (tx, rx) = mpsc::unbounded_channel();
            let items: Vec<_> = self.stream_items.lock().unwrap().drain(..).collect();
            tokio::spawn(async move {
                for item in items {
                    if tx.send(item).is_err() {
                        return;
                    }
                }
            });
            rx
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake::FakeGadget;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::thread;
    use std::time::Duration as StdDuration;

    /// A fake AF_UNIX server: accepts one connection, records the
    /// request bytes, writes the scripted chunks, closes. Mirrors the
    /// Python tests' `_unix_server`.
    fn unix_server(
        path: &std::path::Path,
        chunks: Vec<Vec<u8>>,
    ) -> (thread::JoinHandle<()>, Arc<StdMutex<Vec<u8>>>) {
        let listener = UnixListener::bind(path).unwrap();
        let received = Arc::new(StdMutex::new(Vec::new()));
        let received_clone = received.clone();
        let handle = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            use std::io::Read;
            use std::io::Write;
            // Read until the request line ends (partial reads are
            // possible on a stream socket).
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = conn.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.ends_with(b"\n") {
                    break;
                }
            }
            received_clone.lock().unwrap().extend_from_slice(&buf);
            for chunk in chunks {
                conn.write_all(&chunk).unwrap();
            }
        });
        (handle, received)
    }

    fn tmp_sock(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("limen-shim-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("gadget.sock")
    }

    #[tokio::test]
    async fn ask_muse_roundtrip() {
        let path = tmp_sock("ask-roundtrip");
        let (handle, received) = unix_server(
            &path,
            vec![b"{\"ok\": true, \"response\": \"hi\"}\n".to_vec()],
        );
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let reply = gadget
            .ask("prompt text".to_string(), Some("sess-1".to_string()), None)
            .await
            .unwrap();
        assert_eq!(reply, json!({"ok": true, "response": "hi"}));
        let sent: Value = serde_json::from_slice(&received.lock().unwrap()).unwrap();
        assert_eq!(
            sent,
            json!({"message": "prompt text", "session_id": "sess-1"})
        );
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn ask_muse_sends_wait_field() {
        let path = tmp_sock("ask-wait");
        let (handle, received) =
            unix_server(&path, vec![b"{\"ok\": true, \"reply\": \"hi\"}\n".to_vec()]);
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let reply = gadget
            .ask(
                "prompt text".to_string(),
                Some("sess-1".to_string()),
                Some(120),
            )
            .await
            .unwrap();
        assert_eq!(reply, json!({"ok": true, "reply": "hi"}));
        let sent: Value = serde_json::from_slice(&received.lock().unwrap()).unwrap();
        assert_eq!(
            sent,
            json!({"message": "prompt text", "session_id": "sess-1", "wait": 120})
        );
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn ask_muse_omits_session_id_for_main() {
        let path = tmp_sock("ask-main");
        let (handle, received) = unix_server(
            &path,
            vec![b"{\"ok\": true, \"response\": \"hi\"}\n".to_vec()],
        );
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        assert_eq!(
            gadget
                .ask("prompt text".to_string(), None, None)
                .await
                .unwrap(),
            json!({"ok": true, "response": "hi"})
        );
        let sent: Value = serde_json::from_slice(&received.lock().unwrap()).unwrap();
        assert_eq!(sent, json!({"message": "prompt text"}));
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn ask_muse_omits_session_id_for_main_with_wait() {
        let path = tmp_sock("ask-main-wait");
        let (handle, received) =
            unix_server(&path, vec![b"{\"ok\": true, \"reply\": \"hi\"}\n".to_vec()]);
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        gadget
            .ask("prompt text".to_string(), None, Some(120))
            .await
            .unwrap();
        let sent: Value = serde_json::from_slice(&received.lock().unwrap()).unwrap();
        assert_eq!(sent, json!({"message": "prompt text", "wait": 120}));
        handle.join().unwrap();
    }

    fn event(name: &str, mid: &str, text: &str) -> Value {
        json!({
            "event": name,
            "message_id": mid,
            "reply_to": "",
            "text": text,
            "seq": null,
        })
    }

    #[tokio::test]
    async fn iter_muse_stream_yields_events_then_result() {
        let path = tmp_sock("stream-events");
        let lines = vec![
            b"\n".to_vec(),
            format!("{}\n", json!({"event": event("message.user", "n1", "")})).into_bytes(),
            format!(
                "{}\n",
                json!({"event": event("delta.text_append", "a1", "Hi")})
            )
            .into_bytes(),
            b"{\"ok\": true, \"response\": \"done\"}\n".to_vec(),
        ];
        let (handle, received) = unix_server(&path, lines);
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let mut rx = gadget.stream("p", Some("s"));
        let mut kinds = Vec::new();
        let mut items = Vec::new();
        while let Some(item) = rx.recv().await {
            match &item {
                Ok(SocketItem::Event(_)) => kinds.push("event"),
                Ok(SocketItem::Result(_)) => kinds.push("result"),
                Err(_) => kinds.push("error"),
            }
            items.push(item);
        }
        assert_eq!(kinds, ["event", "event", "result"]);
        if let Ok(SocketItem::Event(first)) = &items[0] {
            assert_eq!(first["event"], "message.user");
        } else {
            panic!("first item is not an event");
        }
        if let Ok(SocketItem::Result(last)) = &items[2] {
            assert_eq!(last["ok"], true);
        } else {
            panic!("last item is not a result");
        }
        let sent: Value = serde_json::from_slice(&received.lock().unwrap()).unwrap();
        assert_eq!(sent["subscribe"], true);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn iter_muse_stream_ends_on_close_without_result() {
        let path = tmp_sock("stream-close");
        let lines =
            vec![format!("{}\n", json!({"event": event("message.user", "n1", "")})).into_bytes()];
        let (handle, _received) = unix_server(&path, lines);
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let mut rx = gadget.stream("p", Some("s"));
        let mut kinds = Vec::new();
        while let Some(item) = rx.recv().await {
            match item {
                Ok(SocketItem::Event(_)) => kinds.push("event"),
                Ok(SocketItem::Result(_)) => kinds.push("result"),
                Err(_) => kinds.push("error"),
            }
        }
        assert_eq!(kinds, ["event"]);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn iter_muse_stream_omits_session_id_for_main() {
        let path = tmp_sock("stream-main");
        let (handle, received) = unix_server(&path, vec![b"{\"ok\": true}\n".to_vec()]);
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let mut rx = gadget.stream("p", None);
        let mut items = Vec::new();
        while let Some(item) = rx.recv().await {
            items.push(item);
        }
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Ok(SocketItem::Result(_))));
        let sent: Value = serde_json::from_slice(&received.lock().unwrap()).unwrap();
        assert_eq!(sent, json!({"message": "p", "subscribe": true}));
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn iter_muse_listen_sends_listen_request() {
        let path = tmp_sock("listen-req");
        let lines = vec![
            format!("{}\n", json!({"event": event("message.user", "n1", "")})).into_bytes(),
            b"{\"ok\": true}\n".to_vec(),
        ];
        let (handle, received) = unix_server(&path, lines);
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let mut rx = gadget.listen(Some("s"), None, None);
        let mut kinds = Vec::new();
        while let Some(item) = rx.recv().await {
            match item {
                Ok(SocketItem::Event(_)) => kinds.push("event"),
                Ok(SocketItem::Result(_)) => kinds.push("result"),
                Err(_) => kinds.push("error"),
            }
        }
        assert_eq!(kinds, ["event", "result"]);
        let sent: Value = serde_json::from_slice(&received.lock().unwrap()).unwrap();
        assert_eq!(
            sent,
            json!({"session_id": "s", "subscribe": true, "listen": true})
        );
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn iter_muse_listen_omits_session_id_for_main() {
        let path = tmp_sock("listen-main");
        let (handle, received) = unix_server(&path, vec![b"{\"ok\": true}\n".to_vec()]);
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let mut rx = gadget.listen(None, None, None);
        let mut items = Vec::new();
        while let Some(item) = rx.recv().await {
            items.push(item);
        }
        assert_eq!(items.len(), 1);
        let sent: Value = serde_json::from_slice(&received.lock().unwrap()).unwrap();
        assert_eq!(sent, json!({"subscribe": true, "listen": true}));
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn iter_socket_sets_ready_and_stops_on_event() {
        // The hybrid hooks: ready is set once the request is on the
        // wire, and the stream ends promptly once stop is set instead
        // of waiting for the service to close the subscription.
        let path = tmp_sock("socket-stop");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            use std::io::Read;
            use std::io::Write;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = conn.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.ends_with(b"\n") {
                    break;
                }
            }
            let line = format!("{}\n", json!({"event": event("agent.status", "", "")}));
            conn.write_all(line.as_bytes()).unwrap();
            thread::sleep(StdDuration::from_secs(5));
        });
        let gadget = UnixGadget {
            socket_path: path.to_string_lossy().to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let (ready_tx, ready_rx) = watch::channel(false);
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut rx = gadget.listen(Some("s"), Some(ready_tx), Some(stop_rx));
        let first = rx.recv().await.unwrap().unwrap();
        assert!(matches!(first, SocketItem::Event(_)));
        assert!(*ready_rx.borrow());
        stop_tx.send(true).unwrap();
        let started = Instant::now();
        let mut rest = Vec::new();
        while let Some(item) = rx.recv().await {
            rest.push(item);
        }
        assert!(rest.is_empty());
        assert!(started.elapsed() < StdDuration::from_secs(2));
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn query_identity_once_returns_name() {
        let gadget = FakeGadget::new().with_ask_replies(vec![]);
        let mut gadget = gadget;
        gadget.raw_replies = std::sync::Mutex::new(
            vec![Ok(json!({"ok": true, "identity": {"name": "Muse"}}))].into(),
        );
        assert_eq!(
            query_identity_once(&gadget).await,
            (Some("Muse".to_string()), None)
        );
        let sent = gadget.raw_sent.lock().unwrap();
        assert_eq!(*sent, vec![json!({"op": "identity"})]);
    }

    #[tokio::test]
    async fn query_identity_once_returns_none_when_not_connected() {
        let mut gadget = FakeGadget::new();
        gadget.raw_replies = std::sync::Mutex::new(
            vec![Ok(
                json!({"ok": false, "error": "not connected to the Muse"}),
            )]
            .into(),
        );
        assert_eq!(
            query_identity_once(&gadget).await,
            (None, Some("not connected to the Muse".to_string()))
        );
    }

    #[tokio::test]
    async fn query_identity_once_returns_none_on_unparseable_reply() {
        // A socket error surfaces as (None, reason): the fake answers
        // with a JSON error, which the raw layer reports.
        let mut gadget = FakeGadget::new();
        gadget.raw_replies = std::sync::Mutex::new(
            vec![Err(fake::io_error("expected value at line 1 column 1"))].into(),
        );
        let (name, reason) = query_identity_once(&gadget).await;
        assert_eq!(name, None);
        assert!(reason.unwrap().contains("expected value"));
    }

    #[tokio::test]
    async fn query_identity_once_returns_none_on_socket_error() {
        let gadget = UnixGadget {
            socket_path: "/nonexistent/gadget.sock".to_string(),
            timeout: StdDuration::from_secs(5),
        };
        let (name, reason) = query_identity_once(&gadget).await;
        assert_eq!(name, None);
        assert!(reason.unwrap().contains("No such file or directory"));
    }

    #[tokio::test]
    async fn discover_muse_name_retries_then_succeeds() {
        let mut gadget = FakeGadget::new();
        gadget.raw_replies = std::sync::Mutex::new(
            vec![
                Ok(json!({"ok": false, "error": "not connected"})),
                Ok(json!({"ok": false, "error": "not connected"})),
                Ok(json!({"ok": true, "identity": {"name": "Muse"}})),
            ]
            .into(),
        );
        let name =
            discover_muse_name(&gadget, Duration::from_secs(5), Duration::from_millis(0)).await;
        assert_eq!(name, Some("Muse".to_string()));
        assert_eq!(gadget.raw_sent.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn discover_muse_name_returns_none_after_deadline() {
        let mut gadget = FakeGadget::new();
        gadget.raw_replies =
            std::sync::Mutex::new(vec![Ok(json!({"ok": false, "error": "down"}))].into());
        assert_eq!(
            discover_muse_name(&gadget, Duration::from_secs(0), Duration::from_millis(0)).await,
            None
        );
    }
}
