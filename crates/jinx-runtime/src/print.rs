// The value printer of `nix-instantiate --eval --strict` (libexpr/print.cc
// with every value forced), used by `jinx eval` and the corpus runner,
// whose expected outputs are Nix's.

use std::collections::HashSet;

use crate::numbers::format_float_g;
use crate::value::{force, Result, Value};

const KEYWORDS: &[&str] = &[
    "if", "then", "else", "assert", "with", "let", "in", "rec", "inherit", "or",
];

fn is_identifier(name: &str) -> bool {
    let mut cs = name.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\'' || c == '-')
}

pub fn print_attr_name(name: &str) -> String {
    if is_identifier(name) && !KEYWORDS.contains(&name) {
        name.to_string()
    } else {
        print_string(name.as_bytes())
    }
}

// Nix's string literal escaping: `"`, `\`, `$` before `{`, and the three
// control characters it spells out.
pub fn print_string(s: &[u8]) -> String {
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'"');
    for (i, &c) in s.iter().enumerate() {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'$' if s.get(i + 1) == Some(&b'{') => out.extend_from_slice(b"\\$"),
            c => out.push(c),
        }
    }
    out.push(b'"');
    String::from_utf8_lossy(&out).into_owned()
}

pub fn print_value(v: &Value) -> Result<String> {
    let mut out = String::new();
    let mut seen = HashSet::new();
    print(v, &mut out, &mut seen)?;
    Ok(out)
}

// A list or set already printed is «repeated». Nix keys that on the value
// slot, so a thunk counts as itself and a value as its heap cell.
fn print(v: &Value, out: &mut String, seen: &mut HashSet<usize>) -> Result<()> {
    let slot = match v {
        Value::Thunk(t) => Some(std::rc::Rc::as_ptr(t) as usize),
        _ => None,
    };
    let x = force(v)?;
    match &x {
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => out.push_str(&format_float_g(*f)),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
        Value::Str(s) => out.push_str(&print_string(&s.bytes)),
        Value::Path(p) => out.push_str(p),
        Value::Lambda(_) => out.push_str("<LAMBDA>"),
        Value::PrimOp(_) => out.push_str("<PRIMOP>"),
        Value::PrimOpApp(_) => out.push_str("<PRIMOP-APP>"),
        Value::List(l) => {
            if l.is_empty() {
                out.push_str("[ ]");
                return Ok(());
            }
            if !seen.insert(slot.unwrap_or(std::rc::Rc::as_ptr(l) as usize)) {
                out.push_str("«repeated»");
                return Ok(());
            }
            out.push('[');
            for e in l.iter() {
                out.push(' ');
                print(e, out, seen)?;
            }
            out.push_str(" ]");
        }
        Value::Attrs(a) => {
            if a.is_empty() {
                out.push_str("{ }");
                return Ok(());
            }
            if !seen.insert(slot.unwrap_or(std::rc::Rc::as_ptr(a) as usize)) {
                out.push_str("«repeated»");
                return Ok(());
            }
            out.push('{');
            for (n, v) in a.iter() {
                out.push(' ');
                out.push_str(&print_attr_name(n));
                out.push_str(" = ");
                print(v, out, seen)?;
                out.push(';');
            }
            out.push_str(" }");
        }
        Value::Thunk(_) => unreachable!("forced"),
    }
    Ok(())
}
