// builtins.toJSON and builtins.fromJSON.
//
// toJSON follows Nix's value-to-json.cc: every value is forced, attribute
// sets are written with their keys in Nix's name order, a set with
// `__toString` or `outPath` is written as the string or value behind that
// attribute, and the contexts met along the way are collected. The text
// is what nlohmann::json's dump() produces: compact, with its escaping
// rules and its float format.
//
// fromJSON keeps the int/float distinction (`1` is an int, `1.0` and `1e2`
// are floats), and the last of duplicate keys wins.

use crate::numbers::format_float_json;
use crate::ops::{app, coerce_to_string, merge_context, COERCE_INTERPOLATE};
use crate::value::*;

pub fn to_json(v: &Value) -> Result<(Vec<u8>, Option<Context>)> {
    let mut out = Vec::new();
    let mut ctx = None;
    serialise(v, &mut out, &mut ctx)?;
    Ok((out, ctx))
}

fn serialise(v: &Value, out: &mut Vec<u8>, ctx: &mut Option<Context>) -> Result<()> {
    let x = force(v)?;
    match &x {
        Value::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Value::Float(f) => out.extend_from_slice(format_float_json(*f).as_bytes()),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Null => out.extend_from_slice(b"null"),
        Value::Str(s) => {
            merge_context(ctx, s.context.as_deref().cloned());
            json_quote(&s.bytes, out);
        }
        Value::Path(_) => {
            let s = coerce_to_string(&x, COERCE_INTERPOLATE)?;
            merge_context(ctx, s.context);
            json_quote(&s.bytes, out);
        }
        Value::List(l) => {
            out.push(b'[');
            for (i, e) in l.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serialise(e, out, ctx)?;
            }
            out.push(b']');
        }
        Value::Attrs(a) => {
            if let Some(f) = a.get("__toString") {
                let r = app(f, std::slice::from_ref(&x))?;
                let s = coerce_to_string(&r, COERCE_INTERPOLATE)?;
                merge_context(ctx, s.context);
                json_quote(&s.bytes, out);
                return Ok(());
            }
            if let Some(o) = a.get("outPath") {
                return serialise(o, out, ctx);
            }
            out.push(b'{');
            for (i, (k, e)) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                json_quote(k.as_bytes(), out);
                out.push(b':');
                serialise(e, out, ctx)?;
            }
            out.push(b'}');
        }
        Value::Lambda(_) => return err("cannot convert a function to JSON"),
        Value::PrimOp(_) => return err("cannot convert a built-in function to JSON"),
        Value::PrimOpApp(_) => {
            return err("cannot convert a partially applied built-in function to JSON")
        }
        Value::Thunk(_) => unreachable!("forced"),
    }
    Ok(())
}

// Quote a string the way nlohmann::json::dump does: short escapes where
// it has them, \u00XX for other control characters, everything else as is.
pub fn json_quote(s: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
}

// ---- fromJSON ---------------------------------------------------------------

pub fn from_json(text: &[u8]) -> Result<Value> {
    let mut p = Parser { s: text, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return p.fail("expected end of input");
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

// The evaluator thread has a large stack; nesting is limited by it alone.
const MAX_DEPTH: usize = 1_000_000;

impl Parser<'_> {
    fn fail<T>(&self, what: &str) -> Result<T> {
        err(format!(
            "while parsing JSON: syntax error at byte {}: {what}",
            self.i + 1
        ))
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn literal(&mut self, word: &[u8], v: Value) -> Result<Value> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            self.fail("invalid literal")
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return self.fail("nesting too deep");
        }
        match self.peek() {
            Some(b'{') => {
                self.i += 1;
                let mut a = Attrs::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(attrs(a));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return self.fail("expected a string");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.peek() != Some(b':') {
                        return self.fail("expected ':'");
                    }
                    self.i += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    a.insert(String::from_utf8_lossy(&k).into_owned(), v);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(attrs(a));
                        }
                        _ => return self.fail("expected ',' or '}'"),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(Value::List(std::rc::Rc::new(items)));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::List(std::rc::Rc::new(items)));
                        }
                        _ => return self.fail("expected ',' or ']'"),
                    }
                }
            }
            Some(b'"') => Ok(mk_str(self.string()?, None)),
            Some(b't') => self.literal(b"true", Value::Bool(true)),
            Some(b'f') => self.literal(b"false", Value::Bool(false)),
            Some(b'n') => self.literal(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => self.fail("unexpected character"),
        }
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.i;
        let mut float = false;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.i += 1;
            }
            p.i - s
        };
        if digits(self) == 0 {
            return self.fail("invalid number");
        }
        if self.peek() == Some(b'.') {
            float = true;
            self.i += 1;
            if digits(self) == 0 {
                return self.fail("invalid number");
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            float = true;
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return self.fail("invalid number");
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.i]).unwrap();
        if !float {
            // nlohmann reads an integer up to 2^64 - 1 as one, which Nix
            // refuses beyond int64, and anything larger as a float.
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Int(i));
            }
            if text.parse::<u64>().is_ok() {
                return err(format!(
                    "unsigned json number {text} outside of Nix integer range"
                ));
            }
        }
        Ok(Value::Float(text.parse::<f64>().unwrap_or(f64::NAN)))
    }

    fn hex4(&mut self) -> Result<u32> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .and_then(|h| std::str::from_utf8(h).ok());
        let Some(n) = h.and_then(|h| u32::from_str_radix(h, 16).ok()) else {
            return self.fail("invalid \\u escape");
        };
        self.i += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<Vec<u8>> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return self.fail("unterminated string");
            };
            self.i += 1;
            match c {
                b'"' => {
                    if out.contains(&0) {
                        return err(
                            "JSON string contains a null byte, which a Nix string cannot hold",
                        );
                    }
                    return Ok(out);
                }
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return self.fail("unterminated string");
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xd800..0xdc00).contains(&cp) {
                                if !self.s[self.i..].starts_with(b"\\u") {
                                    return self.fail("invalid surrogate pair");
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return self.fail("invalid surrogate pair");
                                }
                                cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                            let mut buf = [0; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return self.fail("invalid escape"),
                    }
                }
                c if c < 0x20 => return self.fail("control character in string"),
                c => out.push(c),
            }
        }
    }
}
