//! The stream translator: turns the gadget service's live subscribe
//! events into the text the SSE stream emits.
//!
//! Correlation is by order, not by message id: the Linux server does
//! not share one message id across `message_start`, appends and
//! `done`, so nothing is gated on it. Nothing is emitted before the
//! turn's `message.user` event (earlier events are history replay).

use serde_json::Value;

use crate::json::{is_truthy, py_str};

/// The text of a subscribe event: the first truthy of the event's
/// own `text`, then `payload.text`, `payload.display_text`,
/// `payload.content`.
fn event_text(event: &Value) -> String {
    let payload = event.get("payload");
    for candidate in [
        event.get("text"),
        payload.and_then(|p| p.get("text")),
        payload.and_then(|p| p.get("display_text")),
        payload.and_then(|p| p.get("content")),
    ] {
        if let Some(value) = candidate
            && is_truthy(value)
        {
            return py_str(value);
        }
    }
    String::new()
}

#[derive(Debug, Default)]
pub struct StreamTranslator {
    pub follow: bool,
    pub started: bool,
    pub accumulated: String,
    pub done_text: String,
    pub finished: bool,
}

impl StreamTranslator {
    pub fn new() -> Self {
        StreamTranslator::default()
    }

    pub fn with_follow(follow: bool) -> Self {
        StreamTranslator {
            follow,
            ..StreamTranslator::default()
        }
    }

    /// Feed one event; returns the text to emit ("" when nothing).
    pub fn feed(&mut self, event: &Value) -> String {
        let name = event.get("event").map_or(String::new(), py_str);
        let text = event_text(event);
        if name == "message.user" {
            if self.follow {
                self.accumulated.clear();
                self.done_text.clear();
                self.finished = false;
            }
            self.started = true;
            return String::new();
        }
        if !self.started {
            return String::new();
        }
        if name == "delta.message_start" {
            return String::new();
        }
        if name != "delta.text_append"
            && name != "delta.message_done"
            && name != "message.assistant"
        {
            return String::new();
        }
        if name == "delta.text_append" {
            self.accumulated.push_str(&text);
            return text;
        }
        self.finished = true;
        if name == "delta.message_done" {
            self.done_text = text.clone();
        }
        if self.accumulated.is_empty() && !text.is_empty() {
            self.accumulated = text.clone();
            return text;
        }
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::event;
    use serde_json::json;

    fn nested_event(name: &str, mid: &str, text: &str, key: &str) -> Value {
        json!({"event": name, "message_id": mid, "payload": {key: text}})
    }

    #[test]
    fn drops_history_before_our_user_event() {
        let mut translator = StreamTranslator::new();
        assert_eq!(
            translator.feed(&event("delta.text_append", "old", "stale")),
            ""
        );
        assert_eq!(
            translator.feed(&event("message.assistant", "old", "stale")),
            ""
        );
        assert_eq!(translator.feed(&event("message.user", "note-1", "")), "");
        assert_eq!(translator.feed(&event("agent.status", "", "")), "");
        assert_eq!(
            translator.feed(&event("delta.text_append", "a1", "Wor")),
            "Wor"
        );
        assert_eq!(
            translator.feed(&event("delta.text_append", "a1", "ld")),
            "ld"
        );
        assert_eq!(translator.accumulated, "World");
    }

    #[test]
    fn emits_deltas_with_unshared_message_ids() {
        let mut translator = StreamTranslator::new();
        translator.feed(&event("message.user", "note-1", ""));
        translator.feed(&event("delta.message_start", "start-id", ""));
        assert_eq!(
            translator.feed(&event("delta.text_append", "append-id", "ab")),
            "ab"
        );
        assert_eq!(
            translator.feed(&event("delta.text_append", "other-id", "cd")),
            "cd"
        );
        assert_eq!(
            translator.feed(&event("delta.message_done", "done-id", "abcd")),
            ""
        );
        assert_eq!(translator.accumulated, "abcd");
        assert!(translator.finished);
        assert_eq!(translator.done_text, "abcd");
    }

    #[test]
    fn reads_text_from_payload() {
        let mut translator = StreamTranslator::new();
        translator.feed(&event("message.user", "note-1", ""));
        assert_eq!(
            translator.feed(&nested_event("delta.text_append", "a1", "ab", "text")),
            "ab"
        );
        assert_eq!(
            translator.feed(&nested_event("delta.text_append", "a2", "cd", "text")),
            "cd"
        );
        assert_eq!(
            translator.feed(&nested_event(
                "delta.message_done",
                "a3",
                "abcd",
                "display_text"
            )),
            ""
        );
        assert_eq!(translator.accumulated, "abcd");
    }

    #[test]
    fn nested_done_text_used_when_no_deltas() {
        let mut translator = StreamTranslator::new();
        translator.feed(&event("message.user", "note-1", ""));
        assert_eq!(
            translator.feed(&nested_event(
                "delta.message_done",
                "a1",
                "full text",
                "display_text"
            )),
            "full text"
        );
        assert!(translator.finished);
        assert_eq!(translator.done_text, "full text");
    }

    #[test]
    fn done_text_used_when_no_deltas() {
        let mut translator = StreamTranslator::new();
        translator.feed(&event("message.user", "note-1", ""));
        assert_eq!(
            translator.feed(&event("delta.message_done", "a1", "full text")),
            "full text"
        );
        assert!(translator.finished);
        assert_eq!(translator.done_text, "full text");
    }

    #[test]
    fn done_mismatch_keeps_streamed_text() {
        let mut translator = StreamTranslator::new();
        translator.feed(&event("message.user", "note-1", ""));
        assert_eq!(
            translator.feed(&event("delta.text_append", "a1", "abc")),
            "abc"
        );
        assert_eq!(
            translator.feed(&event("delta.message_done", "a1", "abX")),
            ""
        );
        assert_eq!(translator.accumulated, "abc");
        assert_eq!(translator.done_text, "abX");
    }

    #[test]
    fn assistant_message_without_deltas() {
        let mut translator = StreamTranslator::new();
        translator.feed(&event("message.user", "note-1", ""));
        assert_eq!(
            translator.feed(&event("message.assistant", "a1", "whole")),
            "whole"
        );
        assert!(translator.finished);
    }

    #[test]
    fn follow_mode_restarts_on_each_user_event() {
        let mut translator = StreamTranslator::with_follow(true);
        assert_eq!(translator.feed(&event("message.user", "n0", "")), "");
        assert_eq!(
            translator.feed(&event("delta.text_append", "a0", "old")),
            "old"
        );
        assert_eq!(
            translator.feed(&event("delta.message_done", "a0", "old")),
            ""
        );
        assert_eq!(translator.feed(&event("message.user", "n1", "")), "");
        assert_eq!(
            translator.feed(&event("delta.text_append", "a1", "new")),
            "new"
        );
    }
}
