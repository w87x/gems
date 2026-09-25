//! Serialization: `Display` for `Value` writes compact JSON (no
//! insignificant whitespace) — sufficient for both callers (an HTTP
//! response body, a JSON-RPC message) since neither needs pretty-printing.

use crate::value::Value;
use std::fmt;

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => write_number(f, *n),
            Value::String(s) => write_json_string(f, s),
            Value::Array(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Object(entries) => {
                write!(f, "{{")?;
                for (i, (key, value)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write_json_string(f, key)?;
                    write!(f, ":{value}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

fn write_number(f: &mut fmt::Formatter<'_>, n: f64) -> fmt::Result {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        write!(f, "{}", n as i64)
    } else {
        write!(f, "{n}")
    }
}

fn write_json_string(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    write!(f, "\"")?;
    for c in s.chars() {
        match c {
            '"' => write!(f, "\\\"")?,
            '\\' => write!(f, "\\\\")?,
            '\n' => write!(f, "\\n")?,
            '\r' => write!(f, "\\r")?,
            '\t' => write!(f, "\\t")?,
            c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
            c => write!(f, "{c}")?,
        }
    }
    write!(f, "\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    #[test]
    fn serializes_primitives() {
        assert_eq!(Value::Null.to_string(), "null");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Number(42.0).to_string(), "42");
        assert_eq!(Value::Number(3.5).to_string(), "3.5");
    }

    #[test]
    fn serializes_strings_with_escapes() {
        assert_eq!(
            Value::String("a\"b\\c\nd".to_string()).to_string(),
            r#""a\"b\\c\nd""#
        );
    }

    #[test]
    fn serializes_object_preserving_key_order() {
        let mut v = Value::object();
        v.set("z", 1i64);
        v.set("a", 2i64);
        assert_eq!(v.to_string(), r#"{"z":1,"a":2}"#);
    }

    #[test]
    fn roundtrips_through_parse() {
        let mut v = Value::object();
        v.set("name", "widget");
        v.set("count", 3i64);
        v.set("tags", vec!["a", "b"]);
        v.set("active", true);
        v.set("note", Option::<&str>::None);

        let serialized = v.to_string();
        let reparsed = parse(&serialized).unwrap();
        assert_eq!(reparsed, v);
    }
}
