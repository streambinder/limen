//! Image input: one image per turn, validated on the shim.
//!
//! The limits mirror the gadget service (`LinkSession.send_image`):
//! JPEG, PNG, GIF or WebP, at most 2 MiB of decoded bytes. The shim
//! enforces them before the turn reaches the service, so a bad image
//! fails fast with a 400 instead of a late service rejection. Only
//! base64 data URLs are accepted: the service takes bytes, and
//! fetching remote URLs server-side is out of scope.
//!
//! The image comes from the latest user message only. An image in an
//! earlier message was delivered with the turn that introduced it,
//! and the session already holds it, so it is not sent again.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// One validated image, ready for the socket `image` field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInput {
    pub mime_type: String,
    pub data_base64: String,
    pub filename: String,
}

/// Decoded bytes at most: the service rejects anything larger.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

const IMAGE_MIME_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

fn filename_for(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" => "image.jpg",
        "image/png" => "image.png",
        "image/gif" => "image.gif",
        "image/webp" => "image.webp",
        _ => "image",
    }
}

/// Parse one `image_url` content part into a validated image.
fn parse_image_part(part: &serde_json::Map<String, Value>) -> Result<ImageInput, String> {
    let url = match part.get("image_url") {
        Some(Value::String(url)) => url.clone(),
        Some(Value::Object(map)) => match map.get("url") {
            Some(Value::String(url)) => url.clone(),
            _ => return Err("image_url must carry a url".to_string()),
        },
        _ => return Err("image_url must carry a url".to_string()),
    };
    let Some(rest) = url.strip_prefix("data:") else {
        return Err("image URLs must be base64 data URLs".to_string());
    };
    let Some((mime_type, data_base64)) = rest.split_once(";base64,") else {
        return Err("image URLs must be base64 data URLs".to_string());
    };
    if !IMAGE_MIME_TYPES.contains(&mime_type) {
        return Err("images must be JPEG, PNG, GIF or WebP".to_string());
    }
    if data_base64.is_empty() {
        return Err("the image is empty".to_string());
    }
    let decoded = STANDARD
        .decode(data_base64)
        .map_err(|_| "image data is not valid base64".to_string())?;
    if decoded.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "the image is {} bytes; images must be 2 MiB or less",
            decoded.len()
        ));
    }
    Ok(ImageInput {
        mime_type: mime_type.to_string(),
        data_base64: data_base64.to_string(),
        filename: filename_for(mime_type).to_string(),
    })
}

/// The image of the latest user message, if any. Errors when that
/// message carries more than one image, or when an image breaks the
/// format or size rules.
pub fn extract_image(messages: &Value) -> Result<Option<ImageInput>, String> {
    let Value::Array(items) = messages else {
        return Ok(None);
    };
    for item in items.iter().rev() {
        let Some(map) = item.as_object() else {
            continue;
        };
        let role = map.get("role").and_then(Value::as_str).unwrap_or("user");
        if role != "user" {
            continue;
        }
        let Some(Value::Array(parts)) = map.get("content") else {
            return Ok(None);
        };
        let mut found = Vec::new();
        for part in parts {
            let Some(part_map) = part.as_object() else {
                continue;
            };
            if part_map.get("type").and_then(Value::as_str) == Some("image_url") {
                found.push(part_map);
            }
        }
        if found.len() > 1 {
            return Err("one image per request".to_string());
        }
        return match found.first() {
            Some(part) => parse_image_part(part).map(Some),
            None => Ok(None),
        };
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data_url(mime_type: &str, bytes: &[u8]) -> String {
        format!("data:{mime_type};base64,{}", STANDARD.encode(bytes))
    }

    fn part(url: &str) -> Value {
        json!({"type": "image_url", "image_url": {"url": url}})
    }

    #[test]
    fn extract_image_from_latest_user_message() {
        let messages = json!([
            {"role": "user", "content": [part(&data_url("image/png", b"old"))]},
            {"role": "assistant", "content": "seen"},
            {"role": "user", "content": "what is this?"},
        ]);
        assert_eq!(extract_image(&messages), Ok(None));
        let messages = json!([
            {"role": "user", "content": "hello"},
            {"role": "user", "content": [
                {"type": "text", "text": "look"},
                part(&data_url("image/jpeg", b"bytes")),
            ]},
        ]);
        assert_eq!(
            extract_image(&messages),
            Ok(Some(ImageInput {
                mime_type: "image/jpeg".to_string(),
                data_base64: STANDARD.encode(b"bytes"),
                filename: "image.jpg".to_string(),
            }))
        );
    }

    #[test]
    fn extract_image_rejects_more_than_one() {
        let messages = json!([
            {"role": "user", "content": [
                part(&data_url("image/png", b"one")),
                part(&data_url("image/png", b"two")),
            ]},
        ]);
        assert_eq!(
            extract_image(&messages),
            Err("one image per request".to_string())
        );
    }

    #[test]
    fn extract_image_rejects_remote_url_and_bad_mime() {
        let messages = json!([
            {"role": "user", "content": [part("https://example.com/a.png")]},
        ]);
        assert_eq!(
            extract_image(&messages),
            Err("image URLs must be base64 data URLs".to_string())
        );
        let messages = json!([
            {"role": "user", "content": [part(&data_url("image/bmp", b"x"))]},
        ]);
        assert_eq!(
            extract_image(&messages),
            Err("images must be JPEG, PNG, GIF or WebP".to_string())
        );
    }

    #[test]
    fn extract_image_rejects_bad_base64_and_oversize() {
        let messages = json!([
            {"role": "user", "content": [part("data:image/png;base64,!!!")]},
        ]);
        assert_eq!(
            extract_image(&messages),
            Err("image data is not valid base64".to_string())
        );
        let big = vec![0u8; MAX_IMAGE_BYTES + 1];
        let messages = json!([
            {"role": "user", "content": [part(&data_url("image/webp", &big))]},
        ]);
        assert!(matches!(extract_image(&messages), Err(e) if e.contains("2 MiB or less")));
        let exact = vec![0u8; MAX_IMAGE_BYTES];
        let messages = json!([
            {"role": "user", "content": [part(&data_url("image/gif", &exact))]},
        ]);
        assert_eq!(
            extract_image(&messages).unwrap().unwrap().mime_type,
            "image/gif"
        );
    }

    #[test]
    fn extract_image_ignores_non_user_messages() {
        let messages = json!([
            {"role": "system", "content": [part(&data_url("image/png", b"x"))]},
            {"role": "user", "content": "hello"},
        ]);
        assert_eq!(extract_image(&messages), Ok(None));
        assert_eq!(extract_image(&json!("plain")), Ok(None));
        assert_eq!(extract_image(&json!([{"role": "user"}])), Ok(None));
    }
}
