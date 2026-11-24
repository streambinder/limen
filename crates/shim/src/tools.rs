//! Prompt-injected tool calling.
//!
//! The gadget channel carries text turns only: the Muse cannot emit
//! native OpenAI `tool_calls`. The shim instead appends the tool
//! schemas to the prompt and asks the Muse to answer with a
//! `{"tool_calls": [...]}` JSON object when it wants a tool; the
//! reply is then parsed back into real OpenAI `tool_calls`.
//! Pi executes the tools locally; the shim only transports the calls
//! and renders the tool results back into the next prompt.
//!
//! The pattern is proven live: a probe turn with one tool definition
//! came back as exact `{"tool_calls": [...]}` JSON with the right
//! name and arguments, no refusal, no prose.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use crate::json::{py_str, python_dumps};

/// One function tool from the request's `tools` array.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Parse the request body's `tools`: only `type: "function"`
/// entries with a string `function.name` are kept; anything else is
/// ignored.
pub fn parse_tools(body: &Map<String, Value>) -> Vec<ToolDef> {
    let mut tools = Vec::new();
    if let Some(Value::Array(items)) = body.get("tools") {
        for item in items {
            let Some(map) = item.as_object() else {
                continue;
            };
            if map.get("type").and_then(Value::as_str) != Some("function") {
                continue;
            }
            let Some(func) = map.get("function").and_then(Value::as_object) else {
                continue;
            };
            let Some(name) = func.get("name").and_then(Value::as_str) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            tools.push(ToolDef {
                name: name.to_string(),
                description: func.get("description").map_or(String::new(), py_str),
                parameters: func
                    .get("parameters")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default())),
            });
        }
    }
    tools
}

/// How the request wants the tools used.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    /// Use a tool when it helps; the default.
    Auto,
    /// Never inject the tools.
    None,
    /// The named tool must be used.
    Function(String),
}

/// Parse the request body's `tool_choice`: the strings "auto",
/// "none", "required", or `{"type": "function", "function": {"name":
/// ...}}`. Anything unrecognised means auto.
pub fn parse_tool_choice(body: &Map<String, Value>) -> ToolChoice {
    match body.get("tool_choice") {
        Some(Value::String(choice)) => match choice.as_str() {
            "none" => ToolChoice::None,
            _ => ToolChoice::Auto,
        },
        Some(Value::Object(map)) => {
            if let Some(func) = map.get("function").and_then(Value::as_object)
                && let Some(name) = func.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                return ToolChoice::Function(name.to_string());
            }
            ToolChoice::Auto
        }
        _ => ToolChoice::Auto,
    }
}

/// Whether the tools are injected into the prompt at all.
pub fn tools_active(tools: &[ToolDef], choice: &ToolChoice) -> bool {
    !tools.is_empty() && *choice != ToolChoice::None
}

/// The tool appendix, in the wording the live probe obeyed: the
/// tool list, then the instruction to answer with tool_calls JSON.
pub fn tool_appendix(tools: &[ToolDef], choice: &ToolChoice) -> String {
    let mut lines = vec![
        "You have these tools available. Use one when it helps answer, instead of guessing:"
            .to_string(),
    ];
    for tool in tools {
        let params = tool.parameters.as_object().is_some_and(|p| !p.is_empty());
        if params {
            lines.push(format!(
                "- {}({}): {}",
                tool.name,
                python_dumps(&tool.parameters),
                tool.description
            ));
        } else {
            lines.push(format!("- {}: {}", tool.name, tool.description));
        }
    }
    lines.push(String::new());
    match choice {
        ToolChoice::Function(name) => lines.push(format!(
            "You must use the `{name}` tool: reply with ONLY a JSON object of the form \
             {{\"tool_calls\": [{{\"name\": \"{name}\", \"arguments\": {{...}}}}]}} \
             and nothing else."
        )),
        _ => lines.push(
            "If you use a tool, reply with ONLY a JSON object of the form \
             {\"tool_calls\": [{\"name\": \"<tool>\", \"arguments\": {...}}]} and nothing else. \
             If you answer directly, just reply with the answer."
                .to_string(),
        ),
    }
    lines.join("\n")
}

/// One parsed tool call, ready for the OpenAI response shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    /// The OpenAI `tool_calls` entry: arguments serialised as a string.
    pub fn to_openai(&self) -> Value {
        json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": python_dumps(&self.arguments),
            },
        })
    }
}

/// Call ids, unique within the shim process so a later turn's tool
/// results map back to the right call.
static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);

/// Parse a `{"tool_calls": [...]}` JSON object out of the reply
/// text: the whole text, else the outermost braces, with an optional
/// markdown fence stripped. Names must match a defined tool and
/// arguments must be an object (or a JSON string holding one);
/// anything else returns None and the reply stays plain text.
pub fn parse_tool_calls(text: &str, tools: &[ToolDef]) -> Option<Vec<ToolCall>> {
    let mut trimmed = text.trim();
    if let Some(rest) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    {
        trimmed = rest.trim().strip_suffix("```").unwrap_or(rest).trim();
    }
    let parsed = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        let start = trimmed.find('{')?;
        let end = trimmed.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str::<Value>(&trimmed[start..=end]).ok()
    })?;
    let calls = parsed.get("tool_calls")?.as_array()?;
    if calls.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for entry in calls {
        let map = entry.as_object()?;
        let name = map.get("name").and_then(Value::as_str)?;
        if !tools.iter().any(|tool| tool.name == name) {
            return None;
        }
        let arguments = match map.get("arguments") {
            None => Value::Object(Default::default()),
            Some(Value::Object(_)) => map.get("arguments").unwrap().clone(),
            Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
                Ok(Value::Object(_)) => serde_json::from_str::<Value>(raw).unwrap(),
                _ => return None,
            },
            Some(_) => return None,
        };
        let id = NEXT_CALL_ID.fetch_add(1, Ordering::SeqCst);
        out.push(ToolCall {
            id: format!("call_{id}"),
            name: name.to_string(),
            arguments,
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defs() -> Vec<ToolDef> {
        let body = json!({
            "tools": [
                {"type": "function", "function": {
                    "name": "multiply",
                    "description": "Multiply two numbers",
                    "parameters": {"type": "object", "properties": {
                        "a": {"type": "number"}, "b": {"type": "number"}}}
                }},
                {"type": "function", "function": {
                    "name": "read",
                    "description": "Read a file"
                }},
            ]
        });
        parse_tools(body.as_object().unwrap())
    }

    fn choice(value: Value) -> ToolChoice {
        parse_tool_choice(value.as_object().unwrap())
    }

    #[test]
    fn parse_tools_keeps_functions_only() {
        let body = json!({
            "tools": [
                {"type": "function", "function": {"name": "ok"}},
                {"type": "code_interpreter", "function": {"name": "no"}},
                {"type": "function", "function": {}},
                {"type": "function", "name": "not-nested"},
                "not-an-object",
            ]
        });
        let tools = parse_tools(body.as_object().unwrap());
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ok");
        assert_eq!(tools[0].description, "");
        assert!(tools[0].parameters.is_object());
    }

    #[test]
    fn parse_tool_choice_variants() {
        assert_eq!(choice(json!({"tool_choice": "none"})), ToolChoice::None);
        assert_eq!(choice(json!({"tool_choice": "auto"})), ToolChoice::Auto);
        assert_eq!(choice(json!({"tool_choice": "required"})), ToolChoice::Auto);
        assert_eq!(
            choice(json!({"tool_choice": {"type": "function", "function": {"name": "read"}}})),
            ToolChoice::Function("read".to_string())
        );
        assert_eq!(choice(json!({})), ToolChoice::Auto);
    }

    #[test]
    fn tools_active_needs_tools_and_choice() {
        assert!(tools_active(&defs(), &ToolChoice::Auto));
        assert!(!tools_active(&defs(), &ToolChoice::None));
        assert!(!tools_active(&[], &ToolChoice::Auto));
    }

    #[test]
    fn appendix_lists_tools_and_shape() {
        let appendix = tool_appendix(&defs(), &ToolChoice::Auto);
        assert!(appendix.contains("- multiply("));
        assert!(appendix.contains("Multiply two numbers"));
        assert!(appendix.contains("- read: Read a file"));
        assert!(appendix.contains(r#""tool_calls""#));
        let forced = tool_appendix(&defs(), &ToolChoice::Function("read".to_string()));
        assert!(forced.contains("You must use the `read` tool"));
    }

    #[test]
    fn parse_exact_probe_reply() {
        let calls = parse_tool_calls(
            r#"{"tool_calls": [{"name": "multiply", "arguments": {"a": 17, "b": 23}}]}"#,
            &defs(),
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "multiply");
        assert_eq!(calls[0].arguments, json!({"a": 17, "b": 23}));
        assert!(calls[0].id.starts_with("call_"));
        let openai = calls[0].to_openai();
        assert_eq!(openai["type"], "function");
        assert_eq!(openai["function"]["name"], "multiply");
        assert!(openai["function"]["arguments"].is_string());
    }

    #[test]
    fn parse_tolerates_fence_and_surrounding_text() {
        let calls = parse_tool_calls(
            "Here it is:\n```json\n{\"tool_calls\": [{\"name\": \"read\", \"arguments\": {}}]}\n```",
            &defs(),
        )
        .unwrap();
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn parse_rejects_unknown_names_and_bad_arguments() {
        assert!(
            parse_tool_calls(
                r#"{"tool_calls": [{"name": "nope", "arguments": {}}]}"#,
                &defs()
            )
            .is_none()
        );
        assert!(
            parse_tool_calls(
                r#"{"tool_calls": [{"name": "multiply", "arguments": [1]}]}"#,
                &defs()
            )
            .is_none()
        );
        assert!(parse_tool_calls("just text", &defs()).is_none());
        assert!(parse_tool_calls(r#"{"tool_calls": []}"#, &defs()).is_none());
    }

    #[test]
    fn parse_accepts_string_arguments_and_unique_ids() {
        let first = parse_tool_calls(
            r#"{"tool_calls": [{"name": "multiply", "arguments": "{\"a\": 2, \"b\": 3}"}]}"#,
            &defs(),
        )
        .unwrap();
        assert_eq!(first[0].arguments, json!({"a": 2, "b": 3}));
        let again = parse_tool_calls(
            r#"{"tool_calls": [{"name": "read", "arguments": {}}]}"#,
            &defs(),
        )
        .unwrap();
        assert_ne!(first[0].id, again[0].id);
    }
}
