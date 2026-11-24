//! Model addressing: session id sanitizing, `resolve_model` and the
//! `/v1/models` listing. Direct port of the Python helpers.

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
}

#[derive(Debug, Clone)]
pub struct ModelAddressError {
    pub message: String,
    pub status: u16,
    pub code: &'static str,
}

fn is_uuid(text: &str) -> bool {
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
/// Forms: `<alias>`, `<alias>/main`, `<alias>/<uuid>`. The
/// conversation in the model wins over the session source; a bare
/// alias uses the session source, or the main chat when there is
/// none. Alias matching is case-insensitive and the returned alias
/// is the configured spelling. Unknown aliases are 404
/// `model_not_found`; a malformed conversation is 400 `invalid_model`.
pub fn resolve_model(
    model: &str,
    session_source: Option<&str>,
    aliases: &[String],
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
        }),
        Some("main") => Ok(ModelRoute {
            alias,
            session_id: None,
        }),
        Some(conversation) if is_uuid(conversation) => Ok(ModelRoute {
            alias,
            session_id: Some(conversation.to_ascii_lowercase()),
        }),
        Some(_) => Err(ModelAddressError {
            message: format!(
                "Invalid model `{model}`: expected <alias> or <alias>/<uuid> or <alias>/main"
            ),
            status: 400,
            code: "invalid_model",
        }),
    }
}

/// The `/v1/models` entries for the configured aliases.
pub fn model_list(aliases: &[String]) -> Value {
    Value::Array(
        aliases
            .iter()
            .map(|alias| {
                json!({
                    "id": alias,
                    "object": "model",
                    "created": 0,
                    "owned_by": "musegadget-shim",
                })
            })
            .collect(),
    )
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
        assert_eq!(
            resolve_model("muse", Some("sess-9"), &aliases()).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                session_id: Some("sess-9".to_string()),
            }
        );
        assert_eq!(
            resolve_model("muse", None, &aliases()).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                session_id: None,
            }
        );
    }

    #[test]
    fn resolve_alias_match_is_case_insensitive() {
        let aliases = vec!["Muse".to_string()];
        for form in ["muse", "Muse", "MUSE", "mUsE"] {
            assert_eq!(
                resolve_model(form, None, &aliases).unwrap(),
                ModelRoute {
                    alias: "Muse".to_string(),
                    session_id: None,
                },
                "{form}"
            );
        }
        let route = resolve_model(
            "muse/12345678-1234-1234-1234-1234567890AB",
            Some("sess-9"),
            &aliases,
        )
        .unwrap();
        assert_eq!(route.alias, "Muse");
        assert_eq!(
            route.session_id,
            Some("12345678-1234-1234-1234-1234567890ab".to_string())
        );
        assert_eq!(
            resolve_model("MUSE/main", Some("sess-9"), &aliases).unwrap(),
            ModelRoute {
                alias: "Muse".to_string(),
                session_id: None,
            }
        );
    }

    #[test]
    fn resolve_slash_uuid_wins_over_header() {
        let route = resolve_model(
            "muse/12345678-1234-1234-1234-1234567890AB",
            Some("sess-9"),
            &aliases(),
        )
        .unwrap();
        assert_eq!(
            route,
            ModelRoute {
                alias: "muse".to_string(),
                session_id: Some("12345678-1234-1234-1234-1234567890ab".to_string()),
            }
        );
    }

    #[test]
    fn resolve_slash_main_is_main_even_with_header() {
        assert_eq!(
            resolve_model("muse/main", Some("sess-9"), &aliases()).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                session_id: None,
            }
        );
    }

    #[test]
    fn resolve_unknown_alias_is_404() {
        let err = resolve_model("nope", None, &aliases()).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "model_not_found");
        assert_eq!(err.message, "The model `nope` does not exist");
    }

    #[test]
    fn resolve_bad_conversation_is_400() {
        for bad in [
            "muse/not-a-uuid",
            "muse/",
            "muse/12345678-1234-1234-1234-1234567890ab/extra",
            "muse/12345678",
        ] {
            let err = resolve_model(bad, None, &aliases()).unwrap_err();
            assert_eq!(err.status, 400, "{bad}");
            assert_eq!(err.code, "invalid_model", "{bad}");
        }
    }

    #[test]
    fn resolve_header_is_sanitized() {
        assert_eq!(
            resolve_model("muse", Some("My Session!"), &aliases()).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                session_id: Some("My-Session".to_string()),
            }
        );
    }

    #[test]
    fn resolve_default_id_is_unknown() {
        // The Python suite checks that a placeholder name is not an alias.
        let err = resolve_model("limen", None, &aliases()).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "model_not_found");
    }

    #[test]
    fn model_list_lists_configured_aliases() {
        let two = vec!["muse".to_string(), "other".to_string()];
        let list = model_list(&two);
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["muse", "other"]);
        let one = model_list(&aliases()[..]);
        assert_eq!(one.as_array().unwrap().len(), 1);
    }
}
