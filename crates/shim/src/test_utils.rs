//! Shared helpers for the unit tests: the subscribe-event builder
//! the Python tests' `event()` helper provides.

use serde_json::{Value, json};

/// A subscribe event with the shape the service sends.
pub fn event(name: &str, mid: &str, text: &str) -> Value {
    json!({
        "event": name,
        "message_id": mid,
        "reply_to": "",
        "text": text,
        "seq": null,
    })
}
