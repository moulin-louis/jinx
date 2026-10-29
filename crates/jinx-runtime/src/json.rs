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

// Parsed by serde_json, which reads JSON as nlohmann does where it shows:
// `1` is an integer, `1.0` and `1e2` are floats, the last of duplicate
// keys wins, and an integer beyond 2^64 - 1 is a float. Nesting is
// limited by the evaluator's stack alone.
pub fn from_json(text: &[u8]) -> Result<Value> {
    use serde::Deserialize;
    let mut de = serde_json::Deserializer::from_slice(text);
    de.disable_recursion_limit();
    let parsed = serde_json::Value::deserialize(&mut de)
        .and_then(|v| de.end().map(|()| v))
        .map_err(|e| NixError::boxed(format!("while parsing JSON: {e}")))?;
    convert(parsed)
}

fn json_string(s: String) -> Result<Value> {
    if s.contains('\0') {
        return err("JSON string contains a null byte, which a Nix string cannot hold");
    }
    Ok(mk_str(s.into_bytes(), None))
}

fn convert(v: serde_json::Value) -> Result<Value> {
    Ok(match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if n.is_u64() {
                return err(format!(
                    "unsigned json number {n} outside of Nix integer range"
                ));
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => json_string(s)?,
        serde_json::Value::Array(items) => Value::List(std::rc::Rc::new(
            items.into_iter().map(convert).collect::<Result<_>>()?,
        )),
        serde_json::Value::Object(map) => {
            let mut a = Attrs::new();
            for (k, v) in map {
                if k.contains('\0') {
                    return err("JSON key contains a null byte, which a Nix string cannot hold");
                }
                a.insert(k, convert(v)?);
            }
            attrs(a)
        }
    })
}
