//! Python-compatible `json.dumps` for response bodies.
//!
//! The Python shim serializes every response with the `json.dumps`
//! defaults: `", "` and `": "` separators and `ensure_ascii=True`
//! (non-ASCII escaped as `\uXXXX`, surrogate pairs above U+FFFF).
//! `serde_json::to_string` is compact and leaves non-ASCII raw, so
//! responses would differ byte for byte. This module reproduces the
//! Python encoding so the Rust shim answers byte-identically.

use serde_json::Value;

/// Serialize `value` the way Python `json.dumps(value)` does
/// (default separators, `ensure_ascii=True`).
pub fn python_dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&python_number(n)),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_string(out, key);
                out.push_str(": ");
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

/// Python `repr` of a JSON number: integers as-is, floats in the
/// shortest round-trip form with an exponent that always carries a
/// sign and at least two digits (`1e+16`, `1.5e-07`).
fn python_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let text = n.to_string();
    python_float(&text)
}

fn python_float(text: &str) -> String {
    let Some((mantissa, exp)) = text.split_once(['e', 'E']) else {
        return text.to_string();
    };
    let (sign, digits) = match exp.strip_prefix('-') {
        Some(rest) => ('-', rest),
        None => ('+', exp.strip_prefix('+').unwrap_or(exp)),
    };
    let mut digits = digits.to_string();
    while digits.len() < 2 {
        digits.insert(0, '0');
    }
    format!("{mantissa}e{sign}{digits}")
}

/// Escape a string the way Python `json.dumps` does with
/// `ensure_ascii=True`.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x80 => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xFFFF {
                    out.push_str(&format!("\\u{cp:04x}"));
                } else {
                    // Surrogate pair, like Python's encoder.
                    let v = cp - 0x1_0000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python `str()` of a JSON value, for log lines and error messages:
/// strings as-is, `None` for null, `True`/`False` for booleans,
/// numbers as-is, anything else as `python_dumps`.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => python_number(n),
        Value::String(s) => s.clone(),
        other => python_dumps(other),
    }
}

/// Python truthiness of a JSON value: null, false, zero, empty string,
/// empty array and empty object are false; anything else is true.
pub fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// The first `chars` Unicode scalar values of `s` (Python slices
/// strings by character, not by byte).
pub fn take_chars(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dumps_uses_python_separators() {
        assert_eq!(
            python_dumps(&json!({"object": "list", "data": [1, 2]})),
            r#"{"object": "list", "data": [1, 2]}"#
        );
        assert_eq!(
            python_dumps(&json!({"a": true, "b": null})),
            r#"{"a": true, "b": null}"#
        );
        assert_eq!(python_dumps(&json!([])), "[]");
        assert_eq!(python_dumps(&json!({})), "{}");
    }

    #[test]
    fn dumps_escapes_non_ascii_and_controls() {
        assert_eq!(python_dumps(&json!("αβ")), r#""\u03b1\u03b2""#);
        assert_eq!(python_dumps(&json!("a\nb\t\"q\"\\")), r#""a\nb\t\"q\"\\""#);
        assert_eq!(python_dumps(&json!("\u{1}")), r#""\u0001""#);
        assert_eq!(python_dumps(&json!("\u{8}\u{c}")), r#""\b\f""#);
        // Astral plane: one surrogate pair, lowercase hex.
        assert_eq!(python_dumps(&json!("\u{1F600}")), r#""\ud83d\ude00""#);
        // DEL (0x7f) is ASCII: Python leaves it raw.
        assert_eq!(python_dumps(&json!("\u{7f}")), "\"\u{7f}\"");
    }

    #[test]
    fn dumps_floats_use_python_exponents() {
        assert_eq!(python_float("1e16"), "1e+16");
        assert_eq!(python_float("1.5e-7"), "1.5e-07");
        assert_eq!(python_float("1.0"), "1.0");
        assert_eq!(python_float("2.5"), "2.5");
    }

    #[test]
    fn py_str_matches_python_str() {
        assert_eq!(py_str(&Value::Null), "None");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(false)), "False");
        assert_eq!(py_str(&json!(7)), "7");
        assert_eq!(py_str(&json!("hi")), "hi");
        assert_eq!(py_str(&json!({"a": 1})), r#"{"a": 1}"#);
    }

    #[test]
    fn truthiness_matches_python() {
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&json!(false)));
        assert!(!is_truthy(&json!(0)));
        assert!(!is_truthy(&json!("")));
        assert!(!is_truthy(&json!([])));
        assert!(!is_truthy(&json!({})));
        assert!(is_truthy(&json!(1)));
        assert!(is_truthy(&json!("x")));
        assert!(is_truthy(&json!([0])));
        assert!(is_truthy(&json!({"a": 0})));
    }

    #[test]
    fn take_chars_slices_by_character() {
        assert_eq!(take_chars("héllo", 3), "hél");
        assert_eq!(take_chars("ab", 10), "ab");
    }
}
