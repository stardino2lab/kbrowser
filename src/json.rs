//! Minimal JSON value, parser and writer. CDP payloads are dynamic, so a
//! single `Value` type with forgiving accessors is all the daemon needs.

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

static NULL: Value = Value::Null;
const MAX_DEPTH: usize = 256;

impl Value {
    /// Object member lookup; `Null` when missing or not an object.
    pub fn get(&self, key: &str) -> &Value {
        match self {
            Value::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
    pub fn at(&self, path: &[&str]) -> &Value {
        path.iter().fold(self, |v, k| v.get(k))
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    /// String content, or "" when not a string.
    pub fn str(&self) -> &str {
        self.as_str().unwrap_or("")
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().map(|n| n as i64)
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// Array items, or an empty slice when not an array.
    pub fn arr(&self) -> &[Value] {
        match self {
            Value::Arr(a) => a,
            _ => &[],
        }
    }
    /// Move a member out of an object (leaves `Null`); avoids copying big strings.
    pub fn take(&mut self, key: &str) -> Value {
        match self {
            Value::Obj(m) => m
                .iter_mut()
                .find(|(k, _)| k == key)
                .map(|(_, v)| std::mem::replace(v, Value::Null))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn set(&mut self, key: &str, v: impl Into<Value>) {
        if let Value::Obj(m) = self {
            let v = v.into();
            match m.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = v,
                None => m.push((key.to_string(), v)),
            }
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<&String> for Value {
    fn from(s: &String) -> Self {
        Value::Str(s.clone())
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Num(n)
    }
}
impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Num(n as f64)
    }
}
impl From<i32> for Value {
    fn from(n: i32) -> Self {
        Value::Num(n as f64)
    }
}
impl From<u32> for Value {
    fn from(n: u32) -> Self {
        Value::Num(n as f64)
    }
}
impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Value::Num(n as f64)
    }
}
impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Value::Num(n as f64)
    }
}
impl From<Vec<Value>> for Value {
    fn from(a: Vec<Value>) -> Self {
        Value::Arr(a)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Self {
        o.map(Into::into).unwrap_or(Value::Null)
    }
}

/// `obj! { "key" => value, ... }` builds a `Value::Obj`.
#[macro_export]
macro_rules! obj {
    () => { $crate::json::Value::Obj(Vec::new()) };
    ($($k:expr => $v:expr),+ $(,)?) => {
        $crate::json::Value::Obj(vec![$(($k.to_string(), $crate::json::Value::from($v))),+])
    };
}

pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// JSON string literal for `s` (also a valid JS string literal).
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_str(&mut out, s);
    out
}

fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => {
            if !n.is_finite() {
                out.push_str("null");
            } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
                out.push_str(&format!("{}", *n as i64));
            } else {
                out.push_str(&format!("{}", n));
            }
        }
        Value::Str(s) => write_str(out, s),
        Value::Arr(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, x);
            }
            out.push(']');
        }
        Value::Obj(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, k);
                out.push(':');
                write_value(out, x);
            }
            out.push('}');
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        write_value(&mut s, self);
        f.write_str(&s)
    }
}

pub fn parse(src: &str) -> Result<Value, String> {
    let mut p = Parser { b: src.as_bytes(), i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing data at byte {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }
    fn err<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{} at byte {}", what, self.i))
    }
    fn lit(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            self.err("invalid literal")
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        match self.b.get(self.i).copied() {
            None => self.err("unexpected end"),
            Some(b'n') => self.lit("null", Value::Null),
            Some(b't') => self.lit("true", Value::Bool(true)),
            Some(b'f') => self.lit("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::Str),
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Value::Arr(a));
                }
                loop {
                    self.ws();
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::Arr(a));
                        }
                        _ => return self.err("expected , or ]"),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut m = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Value::Obj(m));
                }
                loop {
                    self.ws();
                    if self.b.get(self.i) != Some(&b'"') {
                        return self.err("expected object key");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return self.err("expected :");
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    m.push((k, v));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Obj(m));
                        }
                        _ => return self.err("expected , or }"),
                    }
                }
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                while self.i < self.b.len()
                    && matches!(self.b[self.i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                {
                    self.i += 1;
                }
                let s = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("");
                match s.parse::<f64>() {
                    Ok(n) => Ok(Value::Num(n)),
                    Err(_) => self.err("invalid number"),
                }
            }
            Some(_) => self.err("unexpected character"),
        }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        if self.i + 4 > self.b.len() {
            return self.err("short \\u escape");
        }
        let s = std::str::from_utf8(&self.b[self.i..self.i + 4]).unwrap_or("");
        match u32::from_str_radix(s, 16) {
            Ok(n) => {
                self.i += 4;
                Ok(n)
            }
            Err(_) => self.err("bad \\u escape"),
        }
    }
    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i] != b'"' && self.b[self.i] != b'\\' {
                self.i += 1;
            }
            out.extend_from_slice(&self.b[start..self.i]);
            match self.b.get(self.i) {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(String::from_utf8_lossy(&out).into_owned());
                }
                _ => {
                    self.i += 1;
                    let e = match self.b.get(self.i) {
                        Some(e) => *e,
                        None => return self.err("unterminated escape"),
                    };
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                // High surrogate: needs a following low surrogate.
                                if self.b[self.i..].starts_with(b"\\u") {
                                    let save = self.i;
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&lo) {
                                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                        char::from_u32(cp).unwrap_or('\u{FFFD}')
                                    } else {
                                        self.i = save; // lone high; re-read next escape
                                        '\u{FFFD}'
                                    }
                                } else {
                                    '\u{FFFD}'
                                }
                            } else {
                                // Lone low surrogates map to U+FFFD.
                                char::from_u32(hi).unwrap_or('\u{FFFD}')
                            }
                        }
                        _ => return self.err("bad escape"),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let src = r#"{"id":1,"result":{"a":[1,2.5,-3e2,true,false,null],"s":"x\"y\\z\n"}}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.get("id").as_i64(), Some(1));
        assert_eq!(v.at(&["result", "a"]).arr().len(), 6);
        assert_eq!(v.at(&["result", "a"]).arr()[2].as_f64(), Some(-300.0));
        assert_eq!(v.at(&["result", "s"]).str(), "x\"y\\z\n");
        assert_eq!(parse(&v.to_string()).unwrap(), v);
    }

    #[test]
    fn surrogates() {
        assert_eq!(parse(r#""😀""#).unwrap().str(), "\u{1F600}");
        assert_eq!(parse(r#""a\uD83Db""#).unwrap().str(), "a\u{FFFD}b");
        assert_eq!(parse(r#""\uDE00""#).unwrap().str(), "\u{FFFD}");
        assert_eq!(parse(r#""\uD83DA""#).unwrap().str(), "\u{FFFD}A");
        assert_eq!(parse("\"\u{d55c}\u{ae00}\"").unwrap().str(), "\u{d55c}\u{ae00}");
    }

    #[test]
    fn control_chars_escaped_on_output() {
        let v = Value::Str("a\u{1}b\tc".into());
        assert_eq!(v.to_string(), "\"a\\u0001b\\tc\"");
        assert_eq!(parse(&v.to_string()).unwrap(), v);
    }

    #[test]
    fn numbers() {
        assert_eq!(Value::Num(42.0).to_string(), "42");
        assert_eq!(Value::Num(1.5).to_string(), "1.5");
        assert_eq!(Value::Num(f64::NAN).to_string(), "null");
        assert_eq!(parse("9007199254740993").unwrap().as_f64(), Some(9007199254740992.0));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "{", "[1,]", "{\"a\"}", "tru", "\"abc", "1 2", "{\"a\":1,}"] {
            assert!(parse(bad).is_err(), "should reject {:?}", bad);
        }
    }

    #[test]
    fn depth_limit_does_not_overflow_stack() {
        let deep = "[".repeat(100_000);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn obj_macro_and_set() {
        let mut v = obj! { "a" => 1, "b" => "x" };
        v.set("a", 2);
        v.set("c", true);
        assert_eq!(v.to_string(), r#"{"a":2,"b":"x","c":true}"#);
        assert!(v.get("missing").is_null());
    }
}
