//! `Value`: the JSON value tree, plus small ergonomic constructors and
//! accessors callers reach for constantly (building a response object,
//! reading a request field).

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    /// Insertion-ordered key/value pairs — see the crate doc for why this
    /// isn't a `HashMap`.
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn object() -> Self {
        Value::Object(Vec::new())
    }

    pub fn array() -> Self {
        Value::Array(Vec::new())
    }

    /// Insert or replace a key in an object in place. Panics if `self`
    /// isn't `Value::Object` — a programmer error at the call site, not a
    /// runtime condition callers need to recover from.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        let Value::Object(entries) = self else {
            panic!("Value::set called on a non-object Value");
        };
        let value = value.into();
        if let Some(entry) = entries.iter_mut().find(|(k, _)| k == key) {
            entry.1 = value;
        } else {
            entries.push((key.to_string(), value));
        }
    }

    pub fn push(&mut self, value: impl Into<Value>) {
        let Value::Array(items) = self else {
            panic!("Value::push called on a non-array Value");
        };
        items.push(value.into());
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64)
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Number(n)
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Value::Number(n as f64)
    }
}

impl From<u32> for Value {
    fn from(n: u32) -> Self {
        Value::Number(n as f64)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Number(n as f64)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => v.into(),
            None => Value::Null,
        }
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Self {
        Value::Array(items.into_iter().map(Into::into).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_set_inserts_and_replaces() {
        let mut v = Value::object();
        v.set("a", 1i64);
        v.set("b", "two");
        v.set("a", 3i64);
        assert_eq!(v.get("a"), Some(&Value::Number(3.0)));
        assert_eq!(v.get("b"), Some(&Value::String("two".to_string())));
        assert_eq!(v.get("missing"), None);
    }

    #[test]
    fn array_push_and_from_vec() {
        let mut v = Value::array();
        v.push(1i64);
        v.push("two");
        assert_eq!(
            v,
            Value::Array(vec![Value::Number(1.0), Value::String("two".to_string())])
        );

        let from_vec: Value = vec![1u64, 2, 3].into();
        assert_eq!(from_vec.as_array().unwrap().len(), 3);
    }

    #[test]
    fn option_converts_to_null_or_value() {
        let some: Value = Some("x").into();
        let none: Value = Option::<&str>::None.into();
        assert_eq!(some, Value::String("x".to_string()));
        assert!(none.is_null());
    }
}
