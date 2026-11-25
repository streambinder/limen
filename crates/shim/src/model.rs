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
    /// The Muse key: the lowercase discovered name of the Muse the
    /// route addresses.
    pub muse: String,
    pub session_id: Option<String>,
    /// The named session alias the model resolved through, if any.
    pub session_alias: Option<String>,
}

/// One Muse the shim serves: its key, its alias list, its socket.
/// The key is the lowercase discovered name. The alias list holds
/// the discovered name first (canonical spelling), then the
/// configured alternates for the primary Muse only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuseInfo {
    pub key: String,
    pub aliases: Vec<String>,
    pub socket_path: String,
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

/// Resolve a model string to its Muse, alias and conversation.
///
/// Forms: `<alias>`, `<alias>/main`, `<alias>/<uuid>`,
/// `<alias>/<name>`. The Muse alias resolves FIRST, against every
/// Muse's alias list (case-insensitive); an unknown Muse is 404
/// `model_not_found` before any session lookup. The conversation in
/// the model wins over the session source; a bare alias uses the
/// session source, or the main chat when there is none. Alias
/// matching is case-insensitive and the returned alias is the
/// configured spelling. A `<name>` is a session alias from the
/// configuration of THAT Muse: it maps to a session UUID. Unknown
/// aliases are 404 `model_not_found`; a malformed conversation is
/// 400 `invalid_model`.
pub fn resolve_model(
    model: &str,
    session_source: Option<&str>,
    muses: &[MuseInfo],
    session_aliases: &HashMap<String, HashMap<String, String>>,
) -> Result<ModelRoute, ModelAddressError> {
    let trimmed = model.trim();
    let (name, conversation) = match trimmed.split_once('/') {
        Some((name, conversation)) => (name, Some(conversation)),
        None => (trimmed, None),
    };
    let Some(muse) = muses.iter().find(|muse| {
        muse.aliases
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(name))
    }) else {
        return Err(ModelAddressError {
            message: format!("The model `{model}` does not exist"),
            status: 404,
            code: "model_not_found",
        });
    };
    let alias = muse
        .aliases
        .iter()
        .find(|alias| alias.eq_ignore_ascii_case(name))
        .unwrap()
        .clone();
    let key = muse.key.clone();
    match conversation {
        None => Ok(ModelRoute {
            alias,
            muse: key,
            session_id: session_source.map(sanitize_session_id),
            session_alias: None,
        }),
        Some("main") => Ok(ModelRoute {
            alias,
            muse: key,
            session_id: None,
            session_alias: None,
        }),
        Some(conversation) if is_uuid(conversation) => Ok(ModelRoute {
            alias,
            muse: key,
            session_id: Some(conversation.to_ascii_lowercase()),
            session_alias: None,
        }),
        Some(conversation) => {
            let name = conversation.to_ascii_lowercase();
            match session_aliases.get(&key).and_then(|names| names.get(&name)) {
                Some(uuid) => Ok(ModelRoute {
                    alias,
                    muse: key,
                    session_id: Some(uuid.clone()),
                    session_alias: Some(name),
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

/// The `/v1/models` entries, per Muse in socket order: that Muse's
/// aliases, then `<first-alias-of-that-Muse>/<name>` per session
/// alias under that Muse (names sorted).
pub fn model_list(
    muses: &[MuseInfo],
    context_window: u64,
    session_aliases: &HashMap<String, HashMap<String, String>>,
) -> Value {
    let mut entries: Vec<Value> = Vec::new();
    for muse in muses {
        for alias in &muse.aliases {
            entries.push(json!({
                "id": alias,
                "object": "model",
                "created": 0,
                "owned_by": "musegadget-shim",
                "context_window": context_window,
            }));
        }
        if let Some(first) = muse.aliases.first()
            && let Some(names) = session_aliases.get(&muse.key)
        {
            let mut names: Vec<&String> = names.keys().collect();
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
    }
    Value::Array(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn muse() -> MuseInfo {
        MuseInfo {
            key: "muse".to_string(),
            aliases: vec!["muse".to_string()],
            socket_path: "/run/musegadget/musegadget.sock".to_string(),
        }
    }

    fn other() -> MuseInfo {
        MuseInfo {
            key: "other".to_string(),
            aliases: vec!["other".to_string()],
            socket_path: "/run/other/musegadget.sock".to_string(),
        }
    }

    fn muses() -> Vec<MuseInfo> {
        vec![muse()]
    }

    fn two_muses() -> Vec<MuseInfo> {
        vec![muse(), other()]
    }

    fn no_sessions() -> HashMap<String, HashMap<String, String>> {
        HashMap::new()
    }

    fn sessions_for(
        muse: &str,
        name: &str,
        uuid: &str,
    ) -> HashMap<String, HashMap<String, String>> {
        HashMap::from([(
            muse.to_string(),
            HashMap::from([(name.to_string(), uuid.to_string())]),
        )])
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
            resolve_model("muse", Some("sess-9"), &muses(), &no_sessions()).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                muse: "muse".to_string(),
                session_id: Some("sess-9".to_string()),
                session_alias: None,
            }
        );
        assert_eq!(
            resolve_model("muse", None, &muses(), &no_sessions()).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                muse: "muse".to_string(),
                session_id: None,
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_alias_match_is_case_insensitive() {
        let muses = vec![MuseInfo {
            key: "muse".to_string(),
            aliases: vec!["Muse".to_string()],
            socket_path: "/run/musegadget/musegadget.sock".to_string(),
        }];
        for form in ["muse", "Muse", "MUSE", "mUsE"] {
            assert_eq!(
                resolve_model(form, None, &muses, &no_sessions()).unwrap(),
                ModelRoute {
                    alias: "Muse".to_string(),
                    muse: "muse".to_string(),
                    session_id: None,
                    session_alias: None,
                },
                "{form}"
            );
        }
        let route = resolve_model(
            "muse/12345678-1234-1234-1234-1234567890AB",
            Some("sess-9"),
            &muses,
            &no_sessions(),
        )
        .unwrap();
        assert_eq!(route.alias, "Muse");
        assert_eq!(route.muse, "muse");
        assert_eq!(
            route.session_id,
            Some("12345678-1234-1234-1234-1234567890ab".to_string())
        );
        assert_eq!(
            resolve_model("MUSE/main", Some("sess-9"), &muses, &no_sessions()).unwrap(),
            ModelRoute {
                alias: "Muse".to_string(),
                muse: "muse".to_string(),
                session_id: None,
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_slash_uuid_wins_over_header() {
        let route = resolve_model(
            "muse/12345678-1234-1234-1234-1234567890AB",
            Some("sess-9"),
            &muses(),
            &no_sessions(),
        )
        .unwrap();
        assert_eq!(
            route,
            ModelRoute {
                alias: "muse".to_string(),
                muse: "muse".to_string(),
                session_id: Some("12345678-1234-1234-1234-1234567890ab".to_string()),
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_slash_main_is_main_even_with_header() {
        assert_eq!(
            resolve_model("muse/main", Some("sess-9"), &muses(), &no_sessions()).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                muse: "muse".to_string(),
                session_id: None,
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_unknown_alias_is_404() {
        let err = resolve_model("nope", None, &muses(), &no_sessions()).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "model_not_found");
        assert_eq!(err.message, "The model `nope` does not exist");
    }

    #[test]
    fn resolve_bad_conversation_is_400() {
        for bad in [
            "muse/not-a-name",
            "muse/",
            "muse/12345678-1234-1234-1234-1234567890ab/extra",
            "muse/12345678",
        ] {
            let err = resolve_model(bad, None, &muses(), &no_sessions()).unwrap_err();
            assert_eq!(err.status, 400, "{bad}");
            assert_eq!(err.code, "invalid_model", "{bad}");
        }
    }

    #[test]
    fn resolve_header_is_sanitized() {
        assert_eq!(
            resolve_model("muse", Some("My Session!"), &muses(), &no_sessions()).unwrap(),
            ModelRoute {
                alias: "muse".to_string(),
                muse: "muse".to_string(),
                session_id: Some("My-Session".to_string()),
                session_alias: None,
            }
        );
    }

    #[test]
    fn resolve_default_id_is_unknown() {
        // The Python suite checks that a placeholder name is not an alias.
        let err = resolve_model("limen", None, &muses(), &no_sessions()).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "model_not_found");
    }

    #[test]
    fn model_list_lists_configured_aliases() {
        let list = model_list(&two_muses(), 1_000_000, &no_sessions());
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
        let one = model_list(&muses(), 1_000_000, &no_sessions());
        assert_eq!(one.as_array().unwrap().len(), 1);
    }

    #[test]
    fn resolve_session_alias_maps_to_uuid() {
        let sessions = sessions_for("muse", "alice", "12345678-1234-1234-1234-1234567890ab");
        let route = resolve_model("muse/alice", Some("sess-9"), &muses(), &sessions).unwrap();
        assert_eq!(
            route,
            ModelRoute {
                alias: "muse".to_string(),
                muse: "muse".to_string(),
                session_id: Some("12345678-1234-1234-1234-1234567890ab".to_string()),
                session_alias: Some("alice".to_string()),
            }
        );
        let route = resolve_model("MUSE/ALICE", None, &muses(), &sessions).unwrap();
        assert_eq!(route.session_alias, Some("alice".to_string()));
        assert_eq!(route.muse, "muse");
    }

    #[test]
    fn model_list_lists_session_aliases() {
        let sessions = HashMap::from([(
            "muse".to_string(),
            HashMap::from([
                (
                    "bob".to_string(),
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
                ),
                (
                    "alice".to_string(),
                    "12345678-1234-1234-1234-1234567890ab".to_string(),
                ),
            ]),
        )]);
        let list = model_list(&muses(), 1_000_000, &sessions);
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["muse", "muse/alice", "muse/bob"]);
    }

    #[test]
    fn resolve_same_name_under_two_muses_maps_to_different_uuids() {
        let sessions = HashMap::from([
            (
                "muse".to_string(),
                HashMap::from([(
                    "alice".to_string(),
                    "12345678-1234-1234-1234-1234567890ab".to_string(),
                )]),
            ),
            (
                "other".to_string(),
                HashMap::from([(
                    "alice".to_string(),
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
                )]),
            ),
        ]);
        let route = resolve_model("muse/alice", None, &two_muses(), &sessions).unwrap();
        assert_eq!(route.muse, "muse");
        assert_eq!(
            route.session_id,
            Some("12345678-1234-1234-1234-1234567890ab".to_string())
        );
        let route = resolve_model("other/alice", None, &two_muses(), &sessions).unwrap();
        assert_eq!(route.muse, "other");
        assert_eq!(
            route.session_id,
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string())
        );
    }

    #[test]
    fn resolve_unknown_muse_is_404_before_session_lookup() {
        // "alice" exists under muse, but the Muse "nope" does not:
        // the Muse resolves first, so this is a 404, not a 400.
        let sessions = sessions_for("muse", "alice", "12345678-1234-1234-1234-1234567890ab");
        let err = resolve_model("nope/alice", None, &two_muses(), &sessions).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "model_not_found");
    }

    #[test]
    fn resolve_unknown_name_under_known_muse_is_400() {
        // "bob" exists under other, but not under muse: the name
        // lookup is per-Muse, so muse/bob is a 400.
        let sessions = sessions_for("other", "bob", "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        let err = resolve_model("muse/bob", None, &two_muses(), &sessions).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.code, "invalid_model");
    }

    #[test]
    fn model_list_lists_per_muse_session_aliases() {
        let sessions = HashMap::from([
            (
                "muse".to_string(),
                HashMap::from([(
                    "alice".to_string(),
                    "12345678-1234-1234-1234-1234567890ab".to_string(),
                )]),
            ),
            (
                "other".to_string(),
                HashMap::from([(
                    "alice".to_string(),
                    "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
                )]),
            ),
        ]);
        let list = model_list(&two_muses(), 1_000_000, &sessions);
        let ids: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["muse", "muse/alice", "other", "other/alice"]);
    }
}
