//! Hand-written recursive-descent JSON parser, same style as
//! `gems-query`'s lexer/parser (char-at-a-time, no lookahead buffer beyond
//! `peekable`).

use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON parse error: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

/// Object/array nesting depth this parser will follow before giving up.
/// `parse_value` recurses through `parse_object`/`parse_array` once per
/// level of nesting, so an unbounded input like `"[[[[[...".repeat(n)`
/// would otherwise recurse until the call stack overflows — a stack
/// overflow aborts the whole process unconditionally in Rust, unlike
/// every other error case here which returns a normal `Result`. This
/// matters most for `gems-mcp`, which parses JSON-RPC arguments directly
/// from an MCP client over stdio. 64 levels is far deeper than any real
/// entity/query payload this workspace produces needs.
const MAX_NESTING_DEPTH: usize = 64;

pub fn parse(src: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        chars: src.chars().peekable(),
        depth: 0,
    };
    parser.skip_whitespace();
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.chars.peek().is_some() {
        return Err(ParseError("unexpected trailing input".to_string()));
    }
    Ok(value)
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn skip_whitespace(&mut self) {
        while matches!(self.chars.peek(), Some(c) if c.is_whitespace()) {
            self.chars.next();
        }
    }

    fn expect(&mut self, c: char) -> Result<(), ParseError> {
        match self.chars.next() {
            Some(actual) if actual == c => Ok(()),
            other => Err(ParseError(format!("expected '{c}', found {other:?}"))),
        }
    }

    fn expect_literal(&mut self, literal: &str) -> Result<(), ParseError> {
        for expected in literal.chars() {
            self.expect(expected)?;
        }
        Ok(())
    }

    fn parse_value(&mut self) -> Result<Value, ParseError> {
        self.skip_whitespace();
        match self.chars.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => Ok(Value::String(self.parse_string()?)),
            Some('t') => {
                self.expect_literal("true")?;
                Ok(Value::Bool(true))
            }
            Some('f') => {
                self.expect_literal("false")?;
                Ok(Value::Bool(false))
            }
            Some('n') => {
                self.expect_literal("null")?;
                Ok(Value::Null)
            }
            Some(c) if *c == '-' || c.is_ascii_digit() => self.parse_number(),
            other => Err(ParseError(format!("unexpected character: {other:?}"))),
        }
    }

    /// Runs `f` one level deeper in object/array nesting, decrementing the
    /// depth counter again afterward regardless of whether `f` succeeded —
    /// takes `self` as a parameter to `f` rather than having `f` capture
    /// it, so this method itself doesn't need to hold a borrow of `self`
    /// across the call.
    fn with_nesting<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        self.depth += 1;
        if self.depth > MAX_NESTING_DEPTH {
            self.depth -= 1;
            return Err(ParseError(format!(
                "exceeds the maximum nesting depth of {MAX_NESTING_DEPTH}"
            )));
        }
        let result = f(self);
        self.depth -= 1;
        result
    }

    fn parse_object(&mut self) -> Result<Value, ParseError> {
        self.with_nesting(|this| {
            this.expect('{')?;
            let mut entries = Vec::new();
            this.skip_whitespace();
            if this.chars.peek() == Some(&'}') {
                this.chars.next();
                return Ok(Value::Object(entries));
            }
            loop {
                this.skip_whitespace();
                let key = this.parse_string()?;
                this.skip_whitespace();
                this.expect(':')?;
                let value = this.parse_value()?;
                entries.push((key, value));
                this.skip_whitespace();
                match this.chars.next() {
                    Some(',') => continue,
                    Some('}') => break,
                    other => {
                        return Err(ParseError(format!("expected ',' or '}}', found {other:?}")))
                    }
                }
            }
            Ok(Value::Object(entries))
        })
    }

    fn parse_array(&mut self) -> Result<Value, ParseError> {
        self.with_nesting(|this| {
            this.expect('[')?;
            let mut items = Vec::new();
            this.skip_whitespace();
            if this.chars.peek() == Some(&']') {
                this.chars.next();
                return Ok(Value::Array(items));
            }
            loop {
                let value = this.parse_value()?;
                items.push(value);
                this.skip_whitespace();
                match this.chars.next() {
                    Some(',') => continue,
                    Some(']') => break,
                    other => {
                        return Err(ParseError(format!("expected ',' or ']', found {other:?}")))
                    }
                }
            }
            Ok(Value::Array(items))
        })
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        self.expect('"')?;
        let mut s = String::new();
        loop {
            match self.chars.next() {
                None => return Err(ParseError("unterminated string".to_string())),
                Some('"') => break,
                Some('\\') => match self.chars.next() {
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('/') => s.push('/'),
                    Some('b') => s.push('\u{8}'),
                    Some('f') => s.push('\u{c}'),
                    Some('n') => s.push('\n'),
                    Some('r') => s.push('\r'),
                    Some('t') => s.push('\t'),
                    Some('u') => s.push(self.parse_unicode_escape()?),
                    other => return Err(ParseError(format!("invalid escape: \\{other:?}"))),
                },
                Some(c) => s.push(c),
            }
        }
        Ok(s)
    }

    fn parse_unicode_escape(&mut self) -> Result<char, ParseError> {
        let mut hex = String::with_capacity(4);
        for _ in 0..4 {
            hex.push(
                self.chars
                    .next()
                    .ok_or_else(|| ParseError("unterminated \\u escape".to_string()))?,
            );
        }
        let code = u32::from_str_radix(&hex, 16)
            .map_err(|_| ParseError(format!("invalid \\u escape: {hex}")))?;
        char::from_u32(code)
            .ok_or_else(|| ParseError(format!("\\u{hex} is not a valid standalone code point")))
    }

    fn parse_number(&mut self) -> Result<Value, ParseError> {
        let mut raw = String::new();
        if self.chars.peek() == Some(&'-') {
            raw.push(self.chars.next().unwrap());
        }
        while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
            raw.push(self.chars.next().unwrap());
        }
        if self.chars.peek() == Some(&'.') {
            raw.push(self.chars.next().unwrap());
            while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
                raw.push(self.chars.next().unwrap());
            }
        }
        if matches!(self.chars.peek(), Some('e') | Some('E')) {
            raw.push(self.chars.next().unwrap());
            if matches!(self.chars.peek(), Some('+') | Some('-')) {
                raw.push(self.chars.next().unwrap());
            }
            while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
                raw.push(self.chars.next().unwrap());
            }
        }
        raw.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| ParseError(format!("invalid number: {raw}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_primitives() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("42").unwrap(), Value::Number(42.0));
        assert_eq!(parse("-3.5").unwrap(), Value::Number(-3.5));
        assert_eq!(parse("1e3").unwrap(), Value::Number(1000.0));
    }

    #[test]
    fn parses_strings_with_escapes() {
        assert_eq!(
            parse(r#""hello\nworld""#).unwrap(),
            Value::String("hello\nworld".to_string())
        );
        assert_eq!(
            parse(r#""quote:\"end""#).unwrap(),
            Value::String("quote:\"end".to_string())
        );
        assert_eq!(parse(r#""AB""#).unwrap(), Value::String("AB".to_string()));
    }

    #[test]
    fn parses_nested_object_and_array() {
        let src =
            r#"{"name": "widget", "tags": ["a", "b"], "count": 3, "active": true, "note": null}"#;
        let value = parse(src).unwrap();
        assert_eq!(value.get("name").unwrap().as_str(), Some("widget"));
        assert_eq!(value.get("count").unwrap().as_f64(), Some(3.0));
        assert_eq!(value.get("active").unwrap(), &Value::Bool(true));
        assert!(value.get("note").unwrap().is_null());
        assert_eq!(value.get("tags").unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn preserves_object_key_order() {
        let value = parse(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        let Value::Object(entries) = value else {
            panic!("expected object");
        };
        let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["z", "a", "m"]);
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(parse("42 43").is_err());
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(parse(r#""abc"#).is_err());
    }

    #[test]
    fn rejects_trailing_comma() {
        assert!(parse(r#"{"a": 1,}"#).is_err());
        assert!(parse(r#"[1, 2,]"#).is_err());
    }

    #[test]
    fn rejects_excessive_nesting_instead_of_overflowing_the_stack() {
        let nested = "[".repeat(MAX_NESTING_DEPTH + 1) + &"]".repeat(MAX_NESTING_DEPTH + 1);
        assert!(parse(&nested).is_err());
    }

    #[test]
    fn accepts_nesting_at_the_depth_limit() {
        let nested = "[".repeat(MAX_NESTING_DEPTH) + &"]".repeat(MAX_NESTING_DEPTH);
        assert!(parse(&nested).is_ok());
    }

    #[test]
    fn empty_object_and_array() {
        assert_eq!(parse("{}").unwrap(), Value::Object(vec![]));
        assert_eq!(parse("[]").unwrap(), Value::Array(vec![]));
    }
}
