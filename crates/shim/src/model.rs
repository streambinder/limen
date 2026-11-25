//! Model addressing: session id sanitizing, `resolve_model` and the
//! `/v1/models` listing. Direct port of the Python helpers.

use std::collections::HashMap;

use serde_json::{Value, json};

/// Replace every character outside `[A-Za-z0-9-]` with `-`, strip
/// leading and trailing dashes, truncate to 64 characters, and fall
/// back to `pi-main` when nothing is left.
pub fn sanitize_session_id(raw: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = replaced.trim_matches('-');
    let truncated: String = trimmed.chars().take(64).collect();
    if truncated.is_empty() {
        "pi-main".to_string()
    } else {
        truncated
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoute {
    pub alias: String,
    pub session_id: Option<String>,
    /// The named session alias the model resolved through, if any.
    pub session_alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelAddressError {
    pub message: String,
    pub status: u16,
    pub code: &'static str,
}

pub(crate) fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// Resolve a model string to its alias and conversation.
///
/// Forms: `<alias>`, `<alias>/main`, `<alias>/<uuid>`,
/// `<alias>/<name>`. The conversation in the model wins over the
/// session source; a bare alias uses the session source, or the main
/// chat when there is none. Alias matching is case-insensitive and
/// the returned alias is the configured spelling. A `<name>` is a
/// session alias from the configuration: it maps to a session UUID.
/// Unknown aliases are 404 `model_not_found`; a malformed
/// conversation is 400 `invalid_model`.
pub fn resolve_model(
    model: &str,
    session_source: Option<&str>,
    aliases: &[String],
    session_aliases: &HashMap<String, String>,
) -> Result<ModelRoute, ModelAddressError> {
    let trimmed = model.trim();
    let (name, conversation) = match trimmed.split_once('/') {
        Some((name, conversation)) => (name, Some(conversation)),
        None => (trimmed, None),
    };
    let Some(canonical) = aliases
        .iter()
        .find(|alias| alias.eq_ignore_ascii_case(name))
    else {
        return Err(ModelAddressError {
            message: format!("The model `{model}` does not exist"),
            status: 404,
            code: "model_not_found",
        });
    };
    let alias = canonical.clone();
    match conversation {
        None => Ok(ModelRoute {
            alias,
            session_id: session_source.map(sanitize_session_id),
            session_alias: None,
        }),
        Some("main") => Ok(ModelRoute {
            alias,
            session_id: None,
            session_alias: None,
        }),
        Some(conversation) if is_uuid(conversation) => Ok(ModelRoute {
            alias,
            session_id: Some(conversation.to_ascii_lowercase()),
            session_alias: None,
        }),
        Some(conversation) => {
            let key = conversation.to_ascii_lowercase();
            match session_aliases.get(&key) {
                Some(uuid) => Ok(ModelRoute {
                    alias,
                    session_id: Some(uuid.clone()),
                    session_alias: Some(key),
                }),
                None => Err(ModelAddressError {
                    message: format!(
                        "Invalid model `{model}`: expected <alias> or <alias>/<uuid> or <alias>/<name> or <alias>/main"
                    ),
                    status: 400,
                    code: "invalid_model",
                }),
            }
        }
    }
}

/// The `/v1/models` entries for the configured aliases, plus one
/// `<alias>/<name>` entry per session alias, under the first alias.
pub fn model_list(
    aliases: &[String],
    context_window: u64,
    session_aliases: &HashMap<String, String>,
) -> Value {
    let mut entries: Vec<Value> = aliases
        .iter()
        .map(|alias| {
            json!({
                "id": alias,
                "object": "model",
                "created": 0,
                "owned_by": "musegadget-shim",
                "context_window": context_window,
            })
        })
        .collect();
    if let Some(first) = aliases.first() {
        let mut names: Vec<&String> = session_aliases.keys().collect();
        names.sort();
        for name in names {
            entries.push(json!({
                "id": format!("{first}/{name}"),
                "object": "model",
                "created": 0,
                "owned_by": "musegadget-shim",
                "context_window": context_window,
            }));
        }
    }
    Value::Array(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> Vec<String> {
        vec!["muse".to_string()]
    }

    #[test]
    fn sanitize_session_id_cases() {
        assert_eq!(sanitize_session_id("abc-123"), "abc-123");
        assert_eq!(sanitize_session_id("My Session!"), "My-Session");
        assert_eq!(sanitize_session_id("!!!"), "pi-main");
        assert_eq!(sanitize_session_id(""), "pi-main");
        assert_eq!(sanitize_session_id(&"x".repeat(100)), "x".repeat(64));
        assert_eq!(sanitize_session_id("--lead-trail--"), "lead-trail");
    }

    #[test]
    fn resolve_bare_alias_uses_session_source() {
        let none: HashMap<String, String> = HashMap::new();
        assert_eq!(
            resolve_model("muse", Some("sess-9"), &aliases(), &none).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                session_id: Some("sess-9".to_string()),
                session_alias: None,
            }
        );
        assert_eq!(
            resolve_model("muse", None, &aliases(), &none).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                session_id: None,
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_alias_match_is_case_insensitive() {
        let aliases = vec!["Muse".to_string()];
        let none: HashMap<String, String> = HashMap::new();
        for form in ["muse", "Muse", "MUSE", "mUsE"] {
            assert_eq!(
                resolve_model(form, None, &aliases, &none).unwrap(),
                ModelRoute {
                    alias: "Muse".to_string(),
                    session_id: None,
                    session_alias: None,
                },
                "{form}"
            );
        }
        let route = resolve_model(
            "muse/12345678-1234-1234-1234-1234567890AB",
            Some("sess-9"),
            &aliases,
            &none,
        )
        .unwrap();
        assert_eq!(route.alias, "Muse");
        assert_eq!(
            route.session_id,
            Some("12345678-1234-1234-1234-1234567890ab".to_string())
        );
        assert_eq!(
            resolve_model("MUSE/main", Some("sess-9"), &aliases, &none).unwrap(),
            ModelRoute {
                alias: "Muse".to_string(),
                session_id: None,
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_slash_uuid_wins_over_header() {
        let none: HashMap<String, String> = HashMap::new();
        let route = resolve_model(
            "muse/12345678-1234-1234-1234-1234567890AB",
            Some("sess-9"),
            &aliases(),
            &none,
        )
        .unwrap();
        assert_eq!(
            route,
            ModelRoute {
                alias: "muse".to_string(),
                session_id: Some("12345678-1234-1234-1234-1234567890ab".to_string()),
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_slash_main_is_main_even_with_header() {
        let none: HashMap<String, String> = HashMap::new();
        assert_eq!(
            resolve_model("muse/main", Some("sess-9"), &aliases(), &none).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                session_id: None,
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_unknown_alias_is_404() {
        let none: HashMap<String, String> = HashMap::new();
        let err = resolve_model("nope", None, &aliases(), &none).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "model_not_found");
        assert_eq!(err.message, "The model `nope` does not exist");
    }

    #[test]
    fn resolve_bad_conversation_is_400() {
        let none: HashMap<String, String> = HashMap::new();
        for bad in [
            "muse/not-a-name",
            "muse/",
            "muse/12345678-1234-1234-1234-1234567890ab/extra",
            "muse/12345678",
        ] {
            let err = resolve_model(bad, None, &aliases(), &none).unwrap_err();
            assert_eq!(err.status, 400, "{bad}");
            assert_eq!(err.code, "invalid_model", "{bad}");
        }
    }

    #[test]
    fn resolve_header_is_sanitized() {
        let none: HashMap<String, String> = HashMap::new();
        assert_eq!(
            resolve_model("muse", Some("My Session!"), &aliases(), &none).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                session_id: Some("My-Session".to_string()),
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_default_id_is_unknown() {
        let none: HashMap<String, String> = HashMap::new();
        // The Python suite checks that a placeholder name is not an alias.
        let err = resolve_model("limen", None, &aliases(), &none).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "model_not_found");
    }

    #[test]
    fn model_list_lists_configured_aliases() {
        let two = vec!["muse".to_string(), "other".to_string()];
        let none: HashMap<String, String> = HashMap::new();
        let list = model_list(&two, 1_000_000, &none);
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["muse", "other"]);
        assert!(
            list.as_array()
                .unwrap()
                .iter()
                .all(|m| m["context_window"].as_u64() == Some(1_000_000))
        );
        let one = model_list(&aliases()[..], 1_000_000, &none);
        assert_eq!(one.as_array().unwrap().len(), 1);
    }

    #[test]
    fn resolve_session_alias_maps_to_uuid() {
        let sessions: HashMap<String, String> = HashMap::from([(
            "alice".to_string(),
            "12345678-1234-1234-1234-1234567890ab".to_string(),
        )]);
        let route = resolve_model("muse/alice", Some("sess-9"), &aliases(), &sessions).unwrap();
        assert_eq!(
            route,
            ModelRoute {
                alias: "muse".to_string(),
                session_id: Some("12345678-1234-1234-1234-1234567890ab".to_string()),
                session_alias: Some("alice".to_string()),
            }
        );
        let route = resolve_model("MUSE/ALICE", None, &aliases(), &sessions).unwrap();
        assert_eq!(route.session_alias, Some("alice".to_string()));
    }

    #[test]
    fn model_list_lists_session_aliases() {
        let sessions: HashMap<String, String> = HashMap::from([
            (
                "bob".to_string(),
                "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
            ),
            (
                "alice".to_string(),
                "12345678-1234-1234-1234-1234567890ab".to_string(),
            ),
        ]);
        let list = model_list(&aliases()[..], 1_000_000, &sessions);
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["muse", "muse/alice", "muse/bob"]);
    }
}
