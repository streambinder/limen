//! Prompt building and best-effort assistant-text extraction.
//! Direct port of the Python helpers.

use std::collections::HashMap;

use serde_json::Value;

use crate::json::{py_str, python_dumps};

/// The text of one message's content: plain strings as-is; lists of
/// content parts keep only the non-empty `text` parts, joined with
/// `\n`; anything else becomes its Python `str()`.
pub fn message_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut texts = Vec::new();
            for part in parts {
                match part {
                    Value::String(s) => {
                        if !s.is_empty() {
                            texts.push(s.clone());
                        }
                    }
                    Value::Object(map) => {
                        if map.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(text_value) = map.get("text")
                        {
                            let text = py_str(text_value);
                            if !text.is_empty() {
                                texts.push(text);
                            }
                        }
                    }
                    _ => {}
                }
            }
            texts.join("\n")
        }
        other => py_str(other),
    }
}

/// The tool name of each `tool_call_id`, from earlier assistant
/// messages in the same request, so a `tool` result can be rendered
/// with the tool it answers.
fn tool_call_names(messages: &Value) -> HashMap<String, String> {
    let mut names = HashMap::new();
    if let Value::Array(items) = messages {
        for item in items {
            let Some(map) = item.as_object() else {
                continue;
            };
            if let Some(Value::Array(calls)) = map.get("tool_calls") {
                for call in calls {
                    let Some(call_map) = call.as_object() else {
                        continue;
                    };
                    let id = call_map.get("id").and_then(Value::as_str);
                    let name = call_map
                        .get("function")
                        .and_then(Value::as_object)
                        .and_then(|func| func.get("name"))
                        .and_then(Value::as_str);
                    if let (Some(id), Some(name)) = (id, name) {
                        names.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }
    names
}

/// One assistant message's tool calls, rendered as prompt lines:
/// `assistant: [called tool `name` with arguments {...}]`.
fn tool_call_lines(map: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(Value::Array(calls)) = map.get("tool_calls") {
        for call in calls {
            let Some(call_map) = call.as_object() else {
                continue;
            };
            let Some(func) = call_map.get("function").and_then(Value::as_object) else {
                continue;
            };
            let Some(name) = func.get("name").and_then(Value::as_str) else {
                continue;
            };
            let arguments = match func.get("arguments") {
                Some(Value::String(raw)) => raw.clone(),
                Some(args) => serde_json::to_string(args).unwrap_or_default(),
                None => "{}".to_string(),
            };
            lines.push(format!(
                "assistant: [called tool `{name}` with arguments {arguments}]"
            ));
        }
    }
    lines
}

/// Flatten the request messages to `role: text` lines, joined with
/// blank lines. Empty messages and non-dicts are skipped. Assistant
/// tool calls and `tool` results render as text lines too, so the
/// follow-up turn carries the tool history.
pub fn build_prompt(messages: &Value) -> String {
    let names = tool_call_names(messages);
    let mut lines = Vec::new();
    if let Value::Array(items) = messages {
        for item in items {
            let Some(map) = item.as_object() else {
                continue;
            };
            let role = map.get("role").map_or("user".to_string(), py_str);
            if role == "tool" {
                let text = map.get("content").map_or(String::new(), message_text);
                if text.is_empty() {
                    continue;
                }
                let label = map
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| names.get(id))
                    .map_or("tool".to_string(), |name| format!("tool {name}"));
                lines.push(format!("{label}: {text}"));
                continue;
            }
            if role == "assistant" {
                lines.extend(tool_call_lines(map));
            }
            let text = map.get("content").map_or(String::new(), message_text);
            if !text.is_empty() {
                lines.push(format!("{role}: {text}"));
            }
        }
    }
    lines.join("\n\n")
}

/// Best-effort extraction of assistant text from a reply payload:
/// recurses to depth 8, parses strings that look like JSON, prefers
/// `choices[0].message.content` / `choices[0].text`, then the keys
/// `text`, `content`, `message`, `response`, `result` in order.
pub fn extract_text(node: &Value) -> String {
    extract_text_depth(node, 0)
}

fn extract_text_depth(node: &Value, depth: usize) -> String {
    if depth > 8 {
        return String::new();
    }
    match node {
        Value::String(s) => {
            let stripped = s.trim();
            if (stripped.starts_with('{') || stripped.starts_with('['))
                && let Ok(parsed) = serde_json::from_str::<Value>(stripped)
            {
                return extract_text_depth(&parsed, depth + 1);
            }
            s.clone()
        }
        Value::Object(map) => {
            if let Some(Value::Array(choices)) = map.get("choices")
                && let Some(Value::Object(first)) = choices.first()
            {
                if let Some(Value::Object(msg)) = first.get("message")
                    && let Some(Value::String(content)) = msg.get("content")
                {
                    return content.clone();
                }
                if let Some(Value::String(text)) = first.get("text") {
                    return text.clone();
                }
            }
            for key in ["text", "content", "message", "response", "result"] {
                if let Some(value) = map.get(key) {
                    if let Value::String(s) = value {
                        if !s.trim().is_empty() {
                            return s.clone();
                        }
                    } else if matches!(value, Value::Object(_) | Value::Array(_)) {
                        let found = extract_text_depth(value, depth + 1);
                        if !found.is_empty() {
                            return found;
                        }
                    }
                }
            }
            String::new()
        }
        Value::Array(items) => items
            .iter()
            .map(|item| extract_text_depth(item, depth + 1))
            .filter(|text| !text.is_empty())
            .collect(),
        _ => String::new(),
    }
}

/// The reply text of an unextractable response, as the Python shim
/// returns it: `json.dumps(response)[:8000]`.
pub fn json_fallback(response: &Value) -> String {
    crate::json::take_chars(&python_dumps(response), 8000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_text_variants() {
        assert_eq!(message_text(&json!("plain")), "plain");
        let content = json!([
            {"type": "text", "text": "one"},
            {"type": "image", "text": "ignored"},
            "two",
            {"type": "text", "text": ""},
            42,
        ]);
        assert_eq!(message_text(&content), "one\ntwo");
        assert_eq!(message_text(&Value::Null), "");
        assert_eq!(message_text(&json!(7)), "7");
    }

    #[test]
    fn build_prompt_skips_empty_and_non_dict() {
        let messages = json!([
            {"role": "system", "content": "rules"},
            {"content": "hello"},
            {"role": "user", "content": ""},
            "not-a-dict",
            {"role": "user", "content": [{"type": "text", "text": "parts"}]},
        ]);
        assert_eq!(
            build_prompt(&messages),
            "system: rules\n\nuser: hello\n\nuser: parts"
        );
        assert_eq!(build_prompt(&json!([])), "");
    }

    #[test]
    fn build_prompt_renders_tool_history() {
        let messages = json!([
            {"role": "user", "content": "what is 17 times 23?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "multiply", "arguments": "{\"a\": 17, \"b\": 23}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "391"},
            {"role": "user", "content": "thanks"},
        ]);
        assert_eq!(
            build_prompt(&messages),
            "user: what is 17 times 23?\n\n\
             assistant: [called tool `multiply` with arguments {\"a\": 17, \"b\": 23}]\n\n\
             tool multiply: 391\n\n\
             user: thanks"
        );
        let unknown = json!([
            {"role": "tool", "tool_call_id": "call_9", "content": "x"},
        ]);
        assert_eq!(build_prompt(&unknown), "tool: x");
    }

    #[test]
    fn extract_text_variants() {
        assert_eq!(extract_text(&json!("plain")), "plain");
        // Depth over 8 yields nothing: a string nested 9 deep.
        let mut deep = json!("deep");
        for _ in 0..9 {
            deep = json!([deep]);
        }
        assert_eq!(extract_text(&deep), "");
        assert_eq!(extract_text(&json!("{\"text\": \"json hi\"}")), "json hi");
        assert_eq!(extract_text(&json!("{broken json")), "{broken json");
        assert_eq!(
            extract_text(&json!({"choices": [{"message": {"content": "c"}}]})),
            "c"
        );
        assert_eq!(extract_text(&json!({"choices": [{"text": "t"}]})), "t");
        assert_eq!(
            extract_text(&json!({"choices": [{"message": {"content": 1}}]})),
            ""
        );
        assert_eq!(
            extract_text(&json!({"choices": "nope", "text": "fallback"})),
            "fallback"
        );
        assert_eq!(extract_text(&json!({"text": "  ", "response": "r"})), "r");
        assert_eq!(
            extract_text(&json!({"content": {"text": "nested"}})),
            "nested"
        );
        assert_eq!(extract_text(&json!({"message": "m"})), "m");
        assert_eq!(extract_text(&json!({"result": ["a", {"text": "b"}]})), "ab");
        assert_eq!(extract_text(&json!(["a", "b"])), "ab");
        assert_eq!(extract_text(&Value::Null), "");
        assert_eq!(extract_text(&json!(42)), "");
        assert_eq!(extract_text(&json!({})), "");
    }

    #[test]
    fn json_fallback_dumps_null() {
        assert_eq!(json_fallback(&Value::Null), "null");
    }
}
