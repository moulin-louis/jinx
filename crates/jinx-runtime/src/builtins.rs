// `builtins`, after libexpr/primops.cc. Each primop is a function of its
// arity taking lazy arguments; `app` supplies partial application. The
// names and the forcing behaviour follow Nix one for one; where jinx
// deviates knowingly, the comment says so.
//
// Compiled code calls a primop directly when it sees `builtins.map f xs`
// with all its arguments: `nix::b::map(&[f, xs])`. The functions are named
// after the builtins, with `'` spelled `_` and Rust keywords raw.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::json::{from_json, to_json};
use crate::numbers::{float_to_int_ceil, float_to_int_floor};
use crate::ops::*;
use crate::regex::{ere_match, ere_split, SplitPart};
use crate::sort::peeksort;
use crate::store::{
    self, copy_path_to_store_with, hash_placeholder, hex, is_store_path, make_text_path, PathFilter,
};
use crate::value::*;

// ---- settings ---------------------------------------------------------------

pub struct Settings {
    pub current_system: String,
    // Whether getEnv reads the environment (nix-instantiate's default) or
    // answers "" (--pure-eval).
    pub impure: bool,
}

thread_local! {
    pub static SETTINGS: RefCell<Settings> = RefCell::new(Settings {
        current_system: "x86_64-linux".into(),
        impure: false,
    });
}

// The Nix this runtime mirrors.
pub const NIX_VERSION: &str = "2.35.2";
pub const LANG_VERSION: i64 = 6;

fn log(line: &str) {
    eprintln!("{line}");
}

// ---- helpers ----------------------------------------------------------------

fn strv(bytes: Vec<u8>) -> Value {
    mk_str(bytes, None)
}

fn index(v: &Value, what: &str) -> Result<usize> {
    let i = force_int(v)?;
    if i < 0 {
        return err(format!("{what} {i} is out of bounds"));
    }
    Ok(i as usize)
}

// An absolute path from a path, or from anything that coerces to a string
// (a set with `outPath` or `__toString`).
fn path_of(v: &Value) -> Result<String> {
    let x = force(v)?;
    if let Value::Path(p) = &x {
        return Ok(p.to_string());
    }
    let s = coerce_to_string(&x, COERCE_PLAIN)?;
    let s = String::from_utf8_lossy(&s.bytes).into_owned();
    if !s.starts_with('/') {
        return err(format!("string '{s}' doesn't represent an absolute path"));
    }
    Ok(s)
}

fn deep_force(v: &Value) -> Result<()> {
    fn go(v: &Value, seen: &mut BTreeSet<usize>) -> Result<()> {
        match force(v)? {
            Value::List(l) => {
                if seen.insert(Rc::as_ptr(&l) as usize) {
                    for e in l.iter() {
                        go(e, seen)?;
                    }
                }
            }
            Value::Attrs(a) if seen.insert(Rc::as_ptr(&a) as usize) => {
                for (_, e) in a.iter() {
                    go(e, seen)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    go(v, &mut BTreeSet::new())
}

fn attrs_of(entries: Vec<(&str, Value)>) -> Value {
    let mut a = Attrs::new();
    for (k, v) in entries {
        a.insert(k, v);
    }
    attrs(a)
}

fn coerce_value(v: &Value, opts: Coerce) -> Result<Value> {
    Ok(coerce_to_string(v, opts)?.into_value())
}

// ---- version comparison (libstore/names.cc) ---------------------------------

fn version_components(s: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'.' || c == b'-' {
            i += 1;
            continue;
        }
        let digit = c.is_ascii_digit();
        let mut j = i;
        while j < s.len() && s[j] != b'.' && s[j] != b'-' && s[j].is_ascii_digit() == digit {
            j += 1;
        }
        out.push(&s[i..j]);
        i = j;
    }
    out
}

fn component_int(c: &[u8]) -> Option<i64> {
    if c.is_empty() || !c.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(c)
        .ok()?
        .parse::<i64>()
        .ok()
        .filter(|n| *n <= i32::MAX as i64)
}

fn components_less(a: &[u8], b: &[u8]) -> bool {
    let na = component_int(a);
    let nb = component_int(b);
    if let (Some(x), Some(y)) = (na, nb) {
        return x < y;
    }
    if a.is_empty() && nb.is_some() {
        return true;
    }
    if a == b"pre" && b != b"pre" {
        return true;
    }
    if b == b"pre" {
        return false;
    }
    if nb.is_some() {
        return true;
    }
    if na.is_some() {
        return false;
    }
    a < b
}

pub fn compare_versions(a: &[u8], b: &[u8]) -> i64 {
    let ca = version_components(a);
    let cb = version_components(b);
    for i in 0..ca.len().max(cb.len()) {
        let x = ca.get(i).copied().unwrap_or(b"");
        let y = cb.get(i).copied().unwrap_or(b"");
        if components_less(x, y) {
            return -1;
        }
        if components_less(y, x) {
            return 1;
        }
    }
    0
}

// ---- errors and control -----------------------------------------------------

pub fn abort(a: &[Value]) -> Result<Value> {
    err(format!(
        "evaluation aborted with the following error message: '{}'",
        force_string(&a[0])?
    ))
}

pub fn throw(a: &[Value]) -> Result<Value> {
    Err(NixError::thrown(force_string(&a[0])?))
}

pub fn addErrorContext(a: &[Value]) -> Result<Value> {
    match force(&a[1]) {
        Ok(v) => Ok(v),
        Err(mut e) => {
            let ctx = coerce_to_string(&a[0], COERCE_TO_STRING)?;
            e.message = format!(
                "{}\n       … {}",
                e.message,
                String::from_utf8_lossy(&ctx.bytes)
            );
            Err(e)
        }
    }
}

fn print_for_trace(v: &Value) -> Result<String> {
    Ok(match force(v)? {
        Value::Str(s) => s.as_str_lossy().into_owned(),
        x => crate::print::print_value(&x)?,
    })
}

pub fn trace(a: &[Value]) -> Result<Value> {
    log(&format!("trace: {}", print_for_trace(&a[0])?));
    force(&a[1])
}

pub fn traceVerbose(a: &[Value]) -> Result<Value> {
    force(&a[1])
}

pub fn warn(a: &[Value]) -> Result<Value> {
    log(&format!("evaluation warning: {}", force_string(&a[0])?));
    force(&a[1])
}

pub fn r#break(a: &[Value]) -> Result<Value> {
    force(&a[0])
}

pub fn tryEval(a: &[Value]) -> Result<Value> {
    match force(&a[0]) {
        Ok(v) => Ok(attrs_of(vec![("success", Value::Bool(true)), ("value", v)])),
        Err(e) if e.catchable => Ok(attrs_of(vec![
            ("success", Value::Bool(false)),
            ("value", Value::Bool(false)),
        ])),
        Err(e) => Err(e),
    }
}

pub fn seq(a: &[Value]) -> Result<Value> {
    force(&a[0])?;
    force(&a[1])
}

pub fn deepSeq(a: &[Value]) -> Result<Value> {
    deep_force(&a[0])?;
    force(&a[1])
}

// ---- types ------------------------------------------------------------------

pub fn typeOf(a: &[Value]) -> Result<Value> {
    Ok(string(force(&a[0])?.type_name()))
}

macro_rules! type_test {
    ($name:ident, $pat:pat) => {
        pub fn $name(a: &[Value]) -> Result<Value> {
            Ok(Value::Bool(matches!(force(&a[0])?, $pat)))
        }
    };
}

type_test!(isNull, Value::Null);
type_test!(isInt, Value::Int(_));
type_test!(isFloat, Value::Float(_));
type_test!(isString, Value::Str(_));
type_test!(isBool, Value::Bool(_));
type_test!(isPath, Value::Path(_));
type_test!(isList, Value::List(_));
type_test!(isAttrs, Value::Attrs(_));
type_test!(
    isFunction,
    Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_)
);

// ---- arithmetic -------------------------------------------------------------

pub fn add(a: &[Value]) -> Result<Value> {
    crate::ops::add(&a[0], &a[1])
}

pub fn sub(a: &[Value]) -> Result<Value> {
    crate::ops::sub(&a[0], &a[1])
}

pub fn mul(a: &[Value]) -> Result<Value> {
    crate::ops::mul(&a[0], &a[1])
}

pub fn div(a: &[Value]) -> Result<Value> {
    crate::ops::div(&a[0], &a[1])
}

pub fn lessThan(a: &[Value]) -> Result<Value> {
    op_lt(&a[0], &a[1])
}

pub fn bitAnd(a: &[Value]) -> Result<Value> {
    Ok(Value::Int(force_int(&a[0])? & force_int(&a[1])?))
}

pub fn bitOr(a: &[Value]) -> Result<Value> {
    Ok(Value::Int(force_int(&a[0])? | force_int(&a[1])?))
}

pub fn bitXor(a: &[Value]) -> Result<Value> {
    Ok(Value::Int(force_int(&a[0])? ^ force_int(&a[1])?))
}

pub fn ceil(a: &[Value]) -> Result<Value> {
    match force(&a[0])? {
        Value::Int(i) => Ok(Value::Int(i)),
        Value::Float(f) => Ok(Value::Int(float_to_int_ceil(f)?)),
        x => Err(expected(&x, "a float")),
    }
}

pub fn floor(a: &[Value]) -> Result<Value> {
    match force(&a[0])? {
        Value::Int(i) => Ok(Value::Int(i)),
        Value::Float(f) => Ok(Value::Int(float_to_int_floor(f)?)),
        x => Err(expected(&x, "a float")),
    }
}

// ---- strings ----------------------------------------------------------------

pub fn toString(a: &[Value]) -> Result<Value> {
    coerce_value(&a[0], COERCE_TO_STRING)
}

// stringLength and substring coerce like interpolation does: a path is its
// store path, a derivation its outPath.
pub fn stringLength(a: &[Value]) -> Result<Value> {
    Ok(Value::Int(
        coerce_to_string(&a[0], COERCE_INTERPOLATE)?.bytes.len() as i64,
    ))
}

pub fn substring(a: &[Value]) -> Result<Value> {
    let start = force_int(&a[0])?;
    if start < 0 {
        return err("negative start position in 'substring'");
    }
    let len = force_int(&a[1])?;
    let s = coerce_to_string(&a[2], COERCE_INTERPOLATE)?;
    let n = s.bytes.len();
    let from = (start as usize).min(n);
    let to = if len < 0 {
        n
    } else {
        from.saturating_add(len as usize).min(n)
    };
    Ok(mk_str(s.bytes[from..to].to_vec(), s.context))
}

pub fn concatStringsSep(a: &[Value]) -> Result<Value> {
    let sep = force_str(&a[0])?;
    let mut out = Coerced {
        bytes: Vec::new(),
        context: sep.context.as_deref().cloned(),
    };
    for (i, e) in force_list(&a[1])?.iter().enumerate() {
        let s = coerce_to_string(e, COERCE_INTERPOLATE)?;
        if i > 0 {
            out.bytes.extend_from_slice(&sep.bytes);
        }
        out.bytes.extend_from_slice(&s.bytes);
        merge_context(&mut out.context, s.context);
    }
    Ok(out.into_value())
}

pub fn replaceStrings(a: &[Value]) -> Result<Value> {
    let from: Vec<Rc<NixStr>> = force_list(&a[0])?
        .iter()
        .map(force_str)
        .collect::<Result<_>>()?;
    let to = force_list(&a[1])?;
    if from.len() != to.len() {
        return err(
            "'from' and 'to' arguments passed to builtins.replaceStrings have different lengths",
        );
    }
    let s = force_str(&a[2])?;
    let text = &s.bytes;
    let mut ctx = s.context.as_deref().cloned();
    let mut cache: Vec<Option<Rc<NixStr>>> = vec![None; from.len()];
    let mut out = Vec::new();
    let mut p = 0;
    while p <= text.len() {
        let mut found = false;
        for (i, f) in from.iter().enumerate() {
            if text[p..].starts_with(&f.bytes) {
                found = true;
                if cache[i].is_none() {
                    let t = force_str(&to[i])?;
                    merge_context(&mut ctx, t.context.as_deref().cloned());
                    cache[i] = Some(t);
                }
                out.extend_from_slice(&cache[i].as_ref().unwrap().bytes);
                if f.bytes.is_empty() {
                    if p < text.len() {
                        out.push(text[p]);
                    }
                    p += 1;
                } else {
                    p += f.bytes.len();
                }
                break;
            }
        }
        if !found {
            if p < text.len() {
                out.push(text[p]);
            }
            p += 1;
        }
    }
    Ok(mk_str(out, ctx))
}

fn groups_value(groups: crate::regex::Groups) -> Value {
    list(
        groups
            .into_iter()
            .map(|g| g.map_or(Value::Null, strv))
            .collect(),
    )
}

pub fn r#match(a: &[Value]) -> Result<Value> {
    let re = force_string(&a[0])?;
    let s = force_str(&a[1])?;
    Ok(match ere_match(&re, &s.bytes)? {
        Some(g) => groups_value(g),
        None => Value::Null,
    })
}

pub fn split(a: &[Value]) -> Result<Value> {
    let re = force_string(&a[0])?;
    let s = force_str(&a[1])?;
    let parts = ere_split(&re, &s.bytes)?;
    if parts.len() == 1 {
        return Ok(list(vec![Value::Str(s)]));
    }
    Ok(list(
        parts
            .into_iter()
            .map(|p| match p {
                SplitPart::Text(t) => strv(t),
                SplitPart::Groups(g) => groups_value(g),
            })
            .collect(),
    ))
}

pub fn splitVersion(a: &[Value]) -> Result<Value> {
    let s = force_str(&a[0])?;
    Ok(list(
        version_components(&s.bytes)
            .into_iter()
            .map(|c| strv(c.to_vec()))
            .collect(),
    ))
}

pub fn compareVersions(a: &[Value]) -> Result<Value> {
    let x = force_str(&a[0])?;
    let y = force_str(&a[1])?;
    Ok(Value::Int(compare_versions(&x.bytes, &y.bytes)))
}

pub fn parseDrvName(a: &[Value]) -> Result<Value> {
    let s = force_str(&a[0])?;
    let b = &s.bytes;
    let split =
        (0..b.len()).find(|&i| b[i] == b'-' && i + 1 < b.len() && !b[i + 1].is_ascii_alphabetic());
    let (name, version) = match split {
        Some(i) => (b[..i].to_vec(), b[i + 1..].to_vec()),
        None => (b.clone(), Vec::new()),
    };
    Ok(attrs_of(vec![
        ("name", strv(name)),
        ("version", strv(version)),
    ]))
}

pub fn hash_bytes(algo: &str, data: &[u8]) -> Result<Vec<u8>> {
    Ok(match algo {
        "md5" => Md5::digest(data).to_vec(),
        "sha1" => Sha1::digest(data).to_vec(),
        "sha256" => Sha256::digest(data).to_vec(),
        "sha512" => Sha512::digest(data).to_vec(),
        _ => return err(format!("unknown hash algorithm '{algo}'")),
    })
}

pub fn hashString(a: &[Value]) -> Result<Value> {
    let algo = force_string(&a[0])?;
    let s = force_str(&a[1])?;
    Ok(string(hex(&hash_bytes(&algo, &s.bytes)?)))
}

// Nix's baseNameOf: one trailing slash is dropped, then what follows the
// last slash; so `baseNameOf "a//"` is "".
pub fn baseNameOf(a: &[Value]) -> Result<Value> {
    let s = coerce_to_string(&a[0], COERCE_PLAIN)?;
    let mut b: &[u8] = &s.bytes;
    if b.last() == Some(&b'/') {
        b = &b[..b.len() - 1];
    }
    let start = b.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
    Ok(mk_str(b[start..].to_vec(), s.context))
}

// Nix's dirOf: everything before the last slash, "/" when that is the
// first character, "." when there is none. Paths are canonical already.
pub fn dirOf(a: &[Value]) -> Result<Value> {
    let v = force(&a[0])?;
    let s = coerce_to_string(&v, COERCE_PLAIN)?;
    let dir: &[u8] = match s.bytes.iter().rposition(|&c| c == b'/') {
        None => b".",
        Some(0) => b"/",
        Some(i) => &s.bytes[..i],
    };
    if let Value::Path(_) = v {
        return Ok(Value::Path(String::from_utf8_lossy(dir).into()));
    }
    Ok(mk_str(dir.to_vec(), s.context))
}

// ---- lists ------------------------------------------------------------------

pub fn length(a: &[Value]) -> Result<Value> {
    Ok(Value::Int(force_list(&a[0])?.len() as i64))
}

pub fn head(a: &[Value]) -> Result<Value> {
    let l = force_list(&a[0])?;
    match l.first() {
        Some(v) => force(v),
        None => err("list index 0 is out of bounds"),
    }
}

pub fn tail(a: &[Value]) -> Result<Value> {
    let l = force_list(&a[0])?;
    if l.is_empty() {
        return err("'tail' called on an empty list");
    }
    Ok(list(l[1..].to_vec()))
}

pub fn elemAt(a: &[Value]) -> Result<Value> {
    let l = force_list(&a[0])?;
    let n = index(&a[1], "list index")?;
    match l.get(n) {
        Some(v) => force(v),
        None => err(format!("list index {n} is out of bounds")),
    }
}

pub fn elem(a: &[Value]) -> Result<Value> {
    for e in force_list(&a[1])?.iter() {
        if eq_elem(&a[0], e)? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

pub fn map(a: &[Value]) -> Result<Value> {
    let l = force_list(&a[1])?;
    Ok(list(
        l.iter()
            .map(|e| {
                let f = a[0].clone();
                let e = e.clone();
                thunk(move || app(&f, std::slice::from_ref(&e)))
            })
            .collect(),
    ))
}

pub fn filter(a: &[Value]) -> Result<Value> {
    let l = force_list(&a[1])?;
    let mut out = Vec::new();
    for e in l.iter() {
        if force_bool(&app(&a[0], std::slice::from_ref(e))?)? {
            out.push(e.clone());
        }
    }
    if out.len() == l.len() {
        return Ok(Value::List(l));
    }
    Ok(list(out))
}

pub fn concatLists(a: &[Value]) -> Result<Value> {
    let mut out = Vec::new();
    for e in force_list(&a[0])?.iter() {
        out.extend(force_list(e)?.iter().cloned());
    }
    Ok(list(out))
}

pub fn concatMap(a: &[Value]) -> Result<Value> {
    let mut out = Vec::new();
    for e in force_list(&a[1])?.iter() {
        out.extend(
            force_list(&app(&a[0], std::slice::from_ref(e))?)?
                .iter()
                .cloned(),
        );
    }
    Ok(list(out))
}

pub fn genList(a: &[Value]) -> Result<Value> {
    let n = index(&a[1], "list length")?;
    Ok(list(
        (0..n)
            .map(|i| {
                let f = a[0].clone();
                thunk(move || app(&f, &[Value::Int(i as i64)]))
            })
            .collect(),
    ))
}

pub fn foldl_(a: &[Value]) -> Result<Value> {
    let mut acc = a[1].clone();
    for e in force_list(&a[2])?.iter() {
        acc = force(&app(&a[0], &[acc, e.clone()])?)?;
    }
    force(&acc)
}

pub fn any(a: &[Value]) -> Result<Value> {
    for e in force_list(&a[1])?.iter() {
        if force_bool(&app(&a[0], std::slice::from_ref(e))?)? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

pub fn all(a: &[Value]) -> Result<Value> {
    for e in force_list(&a[1])?.iter() {
        if !force_bool(&app(&a[0], std::slice::from_ref(e))?)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

pub fn partition(a: &[Value]) -> Result<Value> {
    let (mut right, mut wrong) = (Vec::new(), Vec::new());
    for e in force_list(&a[1])?.iter() {
        if force_bool(&app(&a[0], std::slice::from_ref(e))?)? {
            right.push(e.clone());
        } else {
            wrong.push(e.clone());
        }
    }
    Ok(attrs_of(vec![
        ("right", list(right)),
        ("wrong", list(wrong)),
    ]))
}

pub fn groupBy(a: &[Value]) -> Result<Value> {
    let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for e in force_list(&a[1])?.iter() {
        let k = force_string_no_ctx(&app(&a[0], std::slice::from_ref(e))?)?;
        groups.entry(k).or_default().push(e.clone());
    }
    let mut out = Attrs::new();
    for (k, v) in groups {
        out.insert(k, list(v));
    }
    Ok(attrs(out))
}

pub fn sort(a: &[Value]) -> Result<Value> {
    let mut xs: Vec<Value> = force_list(&a[1])?.iter().cloned().collect();
    for e in xs.iter_mut() {
        *e = force(e)?;
    }
    let comparator = force(&a[0])?;
    let is_less_than = matches!(&comparator, Value::PrimOp(p) if p.name == "lessThan");
    peeksort(&mut xs, |x, y| {
        if is_less_than {
            lt(x, y)
        } else {
            force_bool(&app(&comparator, &[x.clone(), y.clone()])?)
        }
    })?;
    Ok(list(xs))
}

// A key for genericClosure's "seen" set, after CompareValues: ints and
// floats compare numerically so they share a key.
fn closure_key(v: &Value) -> Result<String> {
    Ok(match force(v)? {
        Value::Int(i) => format!("n:{}", i as f64),
        Value::Float(f) => format!("n:{f}"),
        Value::Str(s) => format!("s:{}", s.as_str_lossy()),
        Value::Path(p) => format!("p:{p}"),
        Value::List(l) => {
            let ks: Vec<String> = l.iter().map(closure_key).collect::<Result<_>>()?;
            format!("l:{}", ks.join("\0"))
        }
        x => {
            return err(format!(
                "cannot compare {} with {}",
                x.type_name(),
                x.type_name()
            ))
        }
    })
}

pub fn genericClosure(a: &[Value]) -> Result<Value> {
    let args = force_attrs(&a[0])?;
    let start = force_list(&attr(&a[0], "startSet")?)?;
    let operator = args
        .get("operator")
        .cloned()
        .ok_or_else(|| NixError::boxed("attribute 'operator' missing"))?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut work: std::collections::VecDeque<Value> = start.iter().cloned().collect();
    // Keys must be comparable with each other: all numbers, all strings...
    let mut kind: Option<char> = None;
    while let Some(e) = work.pop_front() {
        let item = force(&e)?;
        let k = closure_key(&attr(&item, "key")?)?;
        let c = k.chars().next().unwrap_or(' ');
        match kind {
            None => kind = Some(c),
            Some(first) if first != c => {
                return err("cannot compare keys of different types in genericClosure")
            }
            _ => {}
        }
        if !seen.insert(k) {
            continue;
        }
        out.push(item.clone());
        for next in force_list(&app(&operator, &[item])?)?.iter() {
            work.push_back(next.clone());
        }
    }
    Ok(list(out))
}

// ---- attribute sets ---------------------------------------------------------

pub fn attrNames(a: &[Value]) -> Result<Value> {
    let s = force_attrs(&a[0])?;
    Ok(list(s.names().map(|n| string(n.to_string())).collect()))
}

pub fn attrValues(a: &[Value]) -> Result<Value> {
    let s = force_attrs(&a[0])?;
    Ok(list(s.iter().map(|(_, v)| v.clone()).collect()))
}

pub fn getAttr(a: &[Value]) -> Result<Value> {
    let n = force_string_no_ctx(&a[0])?;
    force(&attr(&a[1], &n)?)
}

pub fn hasAttr(a: &[Value]) -> Result<Value> {
    let n = force_string_no_ctx(&a[0])?;
    Ok(Value::Bool(force_attrs(&a[1])?.has(&n)))
}

pub fn removeAttrs(a: &[Value]) -> Result<Value> {
    let s = force_attrs(&a[0])?;
    let mut out = (*s).clone();
    for n in force_list(&a[1])?.iter() {
        out.map.remove(force_string_no_ctx(n)?.as_str());
    }
    Ok(attrs(out))
}

// Each attribute takes the position of its element's `value`, as in Nix.
pub fn listToAttrs(a: &[Value]) -> Result<Value> {
    let mut out = Attrs::new();
    for e in force_list(&a[0])?.iter() {
        let item = force_attrs(e)?;
        let name = force_string_no_ctx(&attr(e, "name")?)?;
        // The first occurrence wins.
        if !out.has(&name) {
            let value = attr(e, "value")?;
            out.insert_pos(name, value, item.pos("value"));
        }
    }
    Ok(attrs(out))
}

pub fn intersectAttrs(a: &[Value]) -> Result<Value> {
    let x = force_attrs(&a[0])?;
    let y = force_attrs(&a[1])?;
    let mut out = Attrs::new();
    for (k, attr) in y.map.iter() {
        if x.has(k) {
            out.map.insert(k.clone(), attr.clone());
        }
    }
    Ok(attrs(out))
}

pub fn catAttrs(a: &[Value]) -> Result<Value> {
    let n = force_string_no_ctx(&a[0])?;
    let mut out = Vec::new();
    for e in force_list(&a[1])?.iter() {
        if let Some(v) = force_attrs(e)?.get(&n) {
            out.push(v.clone());
        }
    }
    Ok(list(out))
}

// Nix gives the result of mapAttrs no positions.
pub fn mapAttrs(a: &[Value]) -> Result<Value> {
    let s = force_attrs(&a[1])?;
    let mut out = Attrs::new();
    for (k, v) in s.iter() {
        let f = a[0].clone();
        let name = string(k.to_string());
        let v = v.clone();
        out.insert(
            k.clone(),
            thunk(move || app(&f, &[name.clone(), v.clone()])),
        );
    }
    Ok(attrs(out))
}

pub fn zipAttrsWith(a: &[Value]) -> Result<Value> {
    let mut lists: BTreeMap<Rc<str>, Vec<Value>> = BTreeMap::new();
    for e in force_list(&a[1])?.iter() {
        for (k, v) in force_attrs(e)?.iter() {
            lists.entry(k.clone()).or_default().push(v.clone());
        }
    }
    let mut out = Attrs::new();
    for (k, vs) in lists {
        let f = a[0].clone();
        let name = string(k.to_string());
        let vs = list(vs);
        out.insert(k, thunk(move || app(&f, &[name.clone(), vs.clone()])));
    }
    Ok(attrs(out))
}

pub fn functionArgs(a: &[Value]) -> Result<Value> {
    let f = force_function(&a[0])?;
    let mut out = Attrs::new();
    if let Value::Lambda(l) = &f {
        if let Some(formals) = l.formals {
            for (n, has_default, line, column) in formals.args {
                let pos = Pos {
                    file: formals.file,
                    line: *line,
                    column: *column,
                };
                out.insert_pos(*n, Value::Bool(*has_default), Some(pos));
            }
        }
    }
    Ok(attrs(out))
}

pub fn unsafeGetAttrPos(a: &[Value]) -> Result<Value> {
    let n = force_string_no_ctx(&a[0])?;
    let s = force_attrs(&a[1])?;
    Ok(match s.pos(&n) {
        None => Value::Null,
        Some(p) => attrs_of(vec![
            ("file", string(p.file)),
            ("line", Value::Int(p.line as i64)),
            ("column", Value::Int(p.column as i64)),
        ]),
    })
}

// ---- JSON -------------------------------------------------------------------

pub fn toJSON(a: &[Value]) -> Result<Value> {
    let (json, ctx) = to_json(&a[0])?;
    Ok(mk_str(json, ctx))
}

pub fn fromJSON(a: &[Value]) -> Result<Value> {
    from_json(&force_str(&a[0])?.bytes)
}

// ---- contexts ---------------------------------------------------------------

pub fn hasContext(a: &[Value]) -> Result<Value> {
    Ok(Value::Bool(force_str(&a[0])?.has_context()))
}

pub fn unsafeDiscardStringContext(a: &[Value]) -> Result<Value> {
    Ok(strv(coerce_to_string(&a[0], COERCE_PLAIN)?.bytes))
}

pub fn getContext(a: &[Value]) -> Result<Value> {
    let s = force_str(&a[0])?;
    #[derive(Default)]
    struct Info {
        path: bool,
        all_outputs: bool,
        outputs: Vec<Value>,
    }
    let mut by_path: BTreeMap<String, Info> = BTreeMap::new();
    for e in s.context.iter().flat_map(|c| c.iter()) {
        if let Some(drv) = e.strip_prefix('=') {
            by_path.entry(drv.into()).or_default().all_outputs = true;
        } else if let Some(rest) = e.strip_prefix('!') {
            let (out, drv) = rest.split_once('!').unwrap_or((rest, ""));
            by_path
                .entry(drv.into())
                .or_default()
                .outputs
                .push(string(out));
        } else {
            by_path.entry(e.clone()).or_default().path = true;
        }
    }
    let mut out = Attrs::new();
    for (p, info) in by_path {
        let mut a = Attrs::new();
        if info.path {
            a.insert("path", Value::Bool(true));
        }
        if info.all_outputs {
            a.insert("allOutputs", Value::Bool(true));
        }
        if !info.outputs.is_empty() {
            a.insert("outputs", list(info.outputs));
        }
        out.insert(p, attrs(a));
    }
    Ok(attrs(out))
}

// ---- paths and files --------------------------------------------------------

pub fn import(a: &[Value]) -> Result<Value> {
    crate::modules::import(&a[0])
}

pub fn scopedImport(_: &[Value]) -> Result<Value> {
    err("builtins.scopedImport is not supported by jinx: compiled modules have no dynamic scope")
}

pub fn toPath(a: &[Value]) -> Result<Value> {
    Ok(string(path_of(&a[0])?))
}

pub fn pathExists(a: &[Value]) -> Result<Value> {
    Ok(Value::Bool(
        std::fs::symlink_metadata(path_of(&a[0])?).is_ok(),
    ))
}

pub fn readFile(a: &[Value]) -> Result<Value> {
    let p = path_of(&a[0])?;
    let data = std::fs::read(&p)
        .or_else(|_| err(format!("opening file '{p}': No such file or directory")))?;
    let ctx = is_store_path(&p).then(|| BTreeSet::from([p]));
    Ok(mk_str(data, ctx))
}

pub fn readFileType(a: &[Value]) -> Result<Value> {
    let p = path_of(&a[0])?;
    let meta = std::fs::symlink_metadata(&p).or_else(|_| {
        err(format!(
            "getting status of '{p}': No such file or directory"
        ))
    })?;
    Ok(string(store::file_type(&meta)))
}

pub fn readDir(a: &[Value]) -> Result<Value> {
    let p = path_of(&a[0])?;
    let entries = std::fs::read_dir(&p).or_else(|_| {
        err(format!(
            "opening directory '{p}': No such file or directory"
        ))
    })?;
    let mut out = Attrs::new();
    for e in entries {
        let e = e.or_else(|e| err(format!("reading directory '{p}': {e}")))?;
        let name = e.file_name().to_string_lossy().into_owned();
        let meta = std::fs::symlink_metadata(e.path())
            .or_else(|e| err(format!("reading directory '{p}': {e}")))?;
        out.insert(name, string(store::file_type(&meta)));
    }
    Ok(attrs(out))
}

pub fn hashFile(a: &[Value]) -> Result<Value> {
    let algo = force_string(&a[0])?;
    let p = path_of(&a[1])?;
    let data = std::fs::read(&p)
        .or_else(|_| err(format!("opening file '{p}': No such file or directory")))?;
    Ok(string(hex(&hash_bytes(&algo, &data)?)))
}

fn nix_filter(f: Value) -> impl FnMut(&str, &str) -> Result<bool> {
    move |path, kind| force_bool(&app(&f, &[Value::Path(path.into()), string(kind)])?)
}

pub fn filterSource(a: &[Value]) -> Result<Value> {
    let f = force_function(&a[0])?;
    let p = match force(&a[1])? {
        Value::Path(p) => p,
        x => return Err(expected(&x, "a path")),
    };
    let mut filter = nix_filter(f);
    Ok(copy_path_to_store_with(&p, None, Some(&mut filter as PathFilter))?.into_value())
}

pub fn path(a: &[Value]) -> Result<Value> {
    let args = force_attrs(&a[0])?;
    let p = path_of(&attr(&a[0], "path")?)?;
    let name = args.get("name").map(force_string_no_ctx).transpose()?;
    if args.has("sha256") || args.get("recursive").is_some() {
        return err("builtins.path with sha256 or recursive is not supported yet");
    }
    let mut filter = args.get("filter").cloned().map(nix_filter);
    let filter = filter.as_mut().map(|f| f as PathFilter);
    Ok(copy_path_to_store_with(&p, name.as_deref(), filter)?.into_value())
}

pub fn storePath(_: &[Value]) -> Result<Value> {
    err("'builtins.storePath' is not allowed in pure evaluation mode")
}

pub fn findFile(a: &[Value]) -> Result<Value> {
    let n = force_string(&a[1])?;
    Err(NixError::thrown(format!(
        "file '{n}' was not found in the Nix search path (jinx evaluates with an empty NIX_PATH)"
    )))
}

pub fn getEnv(a: &[Value]) -> Result<Value> {
    let name = force_string_no_ctx(&a[0])?;
    let impure = SETTINGS.with(|s| s.borrow().impure);
    Ok(string(if impure {
        std::env::var(name).unwrap_or_default()
    } else {
        String::new()
    }))
}

pub fn placeholder(a: &[Value]) -> Result<Value> {
    Ok(string(hash_placeholder(&force_string_no_ctx(&a[0])?)))
}

pub fn toFile(a: &[Value]) -> Result<Value> {
    let name = force_string_no_ctx(&a[0])?;
    let s = force_str(&a[1])?;
    let refs: BTreeSet<String> = s
        .context
        .iter()
        .flat_map(|c| c.iter())
        .map(|e| {
            if e.starts_with('=') || e.starts_with('!') {
                err(format!(
                    "files created by builtins.toFile may not reference derivations, but {name} references {e}"
                ))
            } else {
                Ok(e.clone())
            }
        })
        .collect::<Result<_>>()?;
    let p = make_text_path(&name, &s.bytes, &refs)?;
    Ok(mk_str(p.clone().into_bytes(), Some(BTreeSet::from([p]))))
}

fn unsupported(name: &str) -> Result<Value> {
    err(format!(
        "builtins.{name} is not available: jinx evaluates purely and fetches nothing"
    ))
}

pub fn fetchurl(_: &[Value]) -> Result<Value> {
    unsupported("fetchurl")
}
pub fn fetchTarball(_: &[Value]) -> Result<Value> {
    unsupported("fetchTarball")
}
pub fn fetchGit(_: &[Value]) -> Result<Value> {
    unsupported("fetchGit")
}
pub fn fetchTree(_: &[Value]) -> Result<Value> {
    unsupported("fetchTree")
}

pub fn derivationStrict(_: &[Value]) -> Result<Value> {
    err("derivations are not supported by the Rust runtime yet")
}

pub fn derivation(_: &[Value]) -> Result<Value> {
    err("derivations are not supported by the Rust runtime yet")
}

// ---- the table --------------------------------------------------------------

// Every primop: its Nix name, arity and function. The compiler has the
// same list (crates/jinx/src/builtins_table.rs) to call them directly.
pub const PRIMOPS: &[(&str, usize, PrimOpFn)] = &[
    ("abort", 1, abort),
    ("add", 2, add),
    ("addErrorContext", 2, addErrorContext),
    ("all", 2, all),
    ("any", 2, any),
    ("attrNames", 1, attrNames),
    ("attrValues", 1, attrValues),
    ("baseNameOf", 1, baseNameOf),
    ("bitAnd", 2, bitAnd),
    ("bitOr", 2, bitOr),
    ("bitXor", 2, bitXor),
    ("break", 1, r#break),
    ("catAttrs", 2, catAttrs),
    ("ceil", 1, ceil),
    ("compareVersions", 2, compareVersions),
    ("concatLists", 1, concatLists),
    ("concatMap", 2, concatMap),
    ("concatStringsSep", 2, concatStringsSep),
    ("deepSeq", 2, deepSeq),
    ("derivation", 1, derivation),
    ("derivationStrict", 1, derivationStrict),
    ("dirOf", 1, dirOf),
    ("div", 2, div),
    ("elem", 2, elem),
    ("elemAt", 2, elemAt),
    ("fetchGit", 1, fetchGit),
    ("fetchTarball", 1, fetchTarball),
    ("fetchTree", 1, fetchTree),
    ("fetchurl", 1, fetchurl),
    ("filter", 2, filter),
    ("filterSource", 2, filterSource),
    ("findFile", 2, findFile),
    ("floor", 1, floor),
    ("foldl'", 3, foldl_),
    ("fromJSON", 1, fromJSON),
    ("functionArgs", 1, functionArgs),
    ("genList", 2, genList),
    ("genericClosure", 1, genericClosure),
    ("getAttr", 2, getAttr),
    ("getContext", 1, getContext),
    ("getEnv", 1, getEnv),
    ("groupBy", 2, groupBy),
    ("hasAttr", 2, hasAttr),
    ("hasContext", 1, hasContext),
    ("hashFile", 2, hashFile),
    ("hashString", 2, hashString),
    ("head", 1, head),
    ("import", 1, import),
    ("intersectAttrs", 2, intersectAttrs),
    ("isAttrs", 1, isAttrs),
    ("isBool", 1, isBool),
    ("isFloat", 1, isFloat),
    ("isFunction", 1, isFunction),
    ("isInt", 1, isInt),
    ("isList", 1, isList),
    ("isNull", 1, isNull),
    ("isPath", 1, isPath),
    ("isString", 1, isString),
    ("length", 1, length),
    ("lessThan", 2, lessThan),
    ("listToAttrs", 1, listToAttrs),
    ("map", 2, map),
    ("mapAttrs", 2, mapAttrs),
    ("match", 2, r#match),
    ("mul", 2, mul),
    ("parseDrvName", 1, parseDrvName),
    ("partition", 2, partition),
    ("path", 1, path),
    ("pathExists", 1, pathExists),
    ("placeholder", 1, placeholder),
    ("readDir", 1, readDir),
    ("readFile", 1, readFile),
    ("readFileType", 1, readFileType),
    ("removeAttrs", 2, removeAttrs),
    ("replaceStrings", 3, replaceStrings),
    ("scopedImport", 2, scopedImport),
    ("seq", 2, seq),
    ("sort", 2, sort),
    ("split", 2, split),
    ("splitVersion", 1, splitVersion),
    ("storePath", 1, storePath),
    ("stringLength", 1, stringLength),
    ("sub", 2, sub),
    ("substring", 3, substring),
    ("tail", 1, tail),
    ("throw", 1, throw),
    ("toFile", 2, toFile),
    ("toJSON", 1, toJSON),
    ("toPath", 1, toPath),
    ("toString", 1, toString),
    ("trace", 2, trace),
    ("traceVerbose", 2, traceVerbose),
    ("tryEval", 1, tryEval),
    ("typeOf", 1, typeOf),
    ("unsafeDiscardStringContext", 1, unsafeDiscardStringContext),
    ("unsafeGetAttrPos", 2, unsafeGetAttrPos),
    ("warn", 2, warn),
    ("zipAttrsWith", 2, zipAttrsWith),
];

thread_local! {
    static BUILTINS: Value = make_builtins();
}

fn make_builtins() -> Value {
    let mut b = Attrs::new();
    for (name, arity, f) in PRIMOPS {
        b.insert(
            *name,
            Value::PrimOp(Rc::new(PrimOp {
                name,
                arity: *arity,
                f: *f,
            })),
        );
    }
    b.insert("true", Value::Bool(true));
    b.insert("false", Value::Bool(false));
    b.insert("null", Value::Null);
    b.insert("nixVersion", string(NIX_VERSION));
    b.insert("langVersion", Value::Int(LANG_VERSION));
    b.insert("storeDir", string(store::STORE_DIR));
    b.insert("nixPath", list(Vec::new()));
    b.insert(
        "currentSystem",
        string(SETTINGS.with(|s| s.borrow().current_system.clone())),
    );
    b.insert(
        "currentTime",
        thunk(|| err("'builtins.currentTime' is not available in pure evaluation")),
    );
    // `builtins` contains itself.
    let cell = blank();
    b.insert("builtins", cell.clone());
    let v = attrs(b);
    init_value(&cell, v.clone());
    v
}

pub fn builtins() -> Value {
    BUILTINS.with(|b| b.clone())
}

// One builtin by name, for globals like `map` used as values.
pub fn get(name: &str) -> Value {
    BUILTINS.with(|b| match b {
        Value::Attrs(a) => a.get(name).cloned().unwrap_or(Value::Null),
        _ => unreachable!(),
    })
}
