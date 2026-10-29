// The operations the emitted code calls: selection, application, the
// operators, coercions and scoping. Each mirrors one evaluation rule of
// Nix's eval.cc and is named after the Nix syntax.

use std::borrow::Cow;
use std::rc::Rc;

use crate::numbers::{add_int, div_int, format_float_fixed, mul_int, sub_int};
use crate::value::*;

pub type Key = Cow<'static, str>;

// A static attribute name in a selection path.
#[inline]
pub fn k(name: &'static str) -> Key {
    Cow::Borrowed(name)
}

// ---- constructors -----------------------------------------------------------

#[inline]
pub fn int(i: i64) -> Value {
    Value::Int(i)
}

#[inline]
pub fn float(f: f64) -> Value {
    Value::Float(f)
}

#[inline]
pub fn bool(b: bool) -> Value {
    Value::Bool(b)
}

#[inline]
pub fn null() -> Value {
    Value::Null
}

pub fn str_lit(s: &str) -> Value {
    mk_str(s.as_bytes().to_vec(), None)
}

pub fn list(items: Vec<Value>) -> Value {
    Value::List(Rc::new(items))
}

// ---- forcing ----------------------------------------------------------------

pub fn force_attrs(v: &Value) -> Result<Rc<Attrs>> {
    match force(v)? {
        Value::Attrs(a) => Ok(a),
        x => Err(expected(&x, "a set")),
    }
}

pub fn force_list(v: &Value) -> Result<Rc<Vec<Value>>> {
    match force(v)? {
        Value::List(l) => Ok(l),
        x => Err(expected(&x, "a list")),
    }
}

pub fn force_str(v: &Value) -> Result<Rc<NixStr>> {
    match force(v)? {
        Value::Str(s) => Ok(s),
        x => Err(expected(&x, "a string")),
    }
}

// The text of a string, which is expected to be UTF-8 wherever this is
// used (names, versions, hash algorithms).
pub fn force_string(v: &Value) -> Result<String> {
    Ok(force_str(v)?.as_str_lossy().into_owned())
}

// A string that must carry no context, like an attribute name.
pub fn force_string_no_ctx(v: &Value) -> Result<String> {
    let s = force_str(v)?;
    if s.has_context() {
        return err(format!(
            "the string '{}' is not allowed to refer to a store path",
            s.as_str_lossy()
        ));
    }
    Ok(s.as_str_lossy().into_owned())
}

pub fn force_int(v: &Value) -> Result<i64> {
    match force(v)? {
        Value::Int(i) => Ok(i),
        x => Err(expected(&x, "an integer")),
    }
}

pub fn force_bool(v: &Value) -> Result<bool> {
    match force(v)? {
        Value::Bool(b) => Ok(b),
        x => Err(expected(&x, "a Boolean")),
    }
}

pub fn force_function(v: &Value) -> Result<Value> {
    let f = force(v)?;
    if f.is_function() {
        Ok(f)
    } else {
        Err(expected(&f, "a function"))
    }
}

// ---- selection --------------------------------------------------------------

// `e.a.b`: every step forces a set and the attribute must be present.
pub fn sel(subject: &Value, path: &[Key]) -> Result<Value> {
    let mut v = force(subject)?;
    for name in path {
        let a = match &v {
            Value::Attrs(a) => a,
            x => return Err(expected(x, "a set")),
        };
        let next = match a.get(name) {
            Some(x) => x.clone(),
            None => return err(format!("attribute '{name}' missing")),
        };
        v = force(&next)?;
    }
    Ok(v)
}

// `e.a.b or d`: the default when any step is missing or not a set.
pub fn sel_or(
    subject: &Value,
    path: &[Key],
    fallback: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    let mut v = force(subject)?;
    for name in path {
        let next = match &v {
            Value::Attrs(a) => match a.get(name) {
                Some(x) => x.clone(),
                None => return fallback(),
            },
            _ => return fallback(),
        };
        v = force(&next)?;
    }
    Ok(v)
}

// `e ? a.b`: the sets along the way are forced, the final attribute is not.
pub fn has(subject: &Value, path: &[Key]) -> Result<bool> {
    let mut v = force(subject)?;
    for (i, name) in path.iter().enumerate() {
        let next = match &v {
            Value::Attrs(a) => match a.get(name) {
                Some(x) => x.clone(),
                None => return Ok(false),
            },
            _ => return Ok(false),
        };
        if i + 1 < path.len() {
            v = force(&next)?;
        }
    }
    Ok(true)
}

// The lazy attribute itself, for `inherit (e) x` and for builtins that
// pass an attribute on: the thunk is shared rather than forced.
pub fn attr(subject: &Value, name: &str) -> Result<Value> {
    let a = force_attrs(subject)?;
    match a.get(name) {
        Some(v) => Ok(v.clone()),
        None => err(format!("attribute '{name}' missing")),
    }
}

// A dynamic attribute name `${e}`: a string without context.
pub fn key(v: &Value) -> Result<Key> {
    Ok(Cow::Owned(force_string_no_ctx(v)?))
}

// The attribute set literal of a compiled module: static attributes with
// their positions, then dynamic ones, which may be null (skipped) and may
// not repeat a name.
pub struct AttrsBuilder {
    file: &'static str,
    attrs: Attrs,
}

impl AttrsBuilder {
    pub fn new(file: &'static str) -> Self {
        AttrsBuilder {
            file,
            attrs: Attrs::new(),
        }
    }

    pub fn set(&mut self, name: &'static str, line: u32, column: u32, value: Value) {
        let pos = Pos {
            file: self.file,
            line,
            column,
        };
        self.attrs.insert_pos(name, value, Some(pos));
    }

    pub fn dynamic(&mut self, name: Value, value: Value) -> Result<()> {
        let n = force(&name)?;
        if let Value::Null = n {
            return Ok(());
        }
        let name = force_string_no_ctx(&n)?;
        if self.attrs.has(&name) {
            return err(format!("dynamic attribute '{name}' already defined"));
        }
        self.attrs.insert(name, value);
        Ok(())
    }

    pub fn build(self) -> Value {
        attrs(self.attrs)
    }
}

// ---- functions --------------------------------------------------------------

// Entry of a `{ a, b ? d, ... }` function: force the argument to a set and
// reject attributes the pattern does not name, unless it ends in `...`.
pub fn formals(arg: &Value, spec: &Formals) -> Result<Rc<Attrs>> {
    let a = force_attrs(arg)?;
    if !spec.ellipsis {
        for name in a.names() {
            if !spec.has(name) {
                return err(format!("function called with unexpected argument '{name}'"));
            }
        }
    }
    Ok(a)
}

pub fn arg(attrs: &Attrs, name: &str) -> Result<Value> {
    match attrs.get(name) {
        Some(v) => Ok(v.clone()),
        None => err(format!(
            "function called without required argument '{name}'"
        )),
    }
}

// Apply a function to arguments, one at a time. Lambdas take exactly one;
// primops collect their arity; a set with `__functor` applies that to
// itself first.
pub fn app(f: &Value, args: &[Value]) -> Result<Value> {
    let mut f = force(f)?;
    let mut i = 0;
    while i < args.len() {
        match &f {
            Value::Lambda(l) => {
                let r = (l.f)(args[i].clone())?;
                f = force(&r)?;
                i += 1;
            }
            Value::PrimOp(p) => {
                let (r, used) = call_primop(p, &[], &args[i..])?;
                f = r;
                i += used;
            }
            Value::PrimOpApp(pa) => {
                let (r, used) = call_primop(&pa.op, &pa.args, &args[i..])?;
                f = r;
                i += used;
            }
            Value::Attrs(a) if a.has("__functor") => {
                let functor = force(a.get("__functor").unwrap())?;
                let g = app(&functor, &[f.clone()])?;
                f = g;
            }
            x => {
                return err(format!(
                    "attempt to call something which is not a function but {}: {}",
                    describe_type(x),
                    show(x)
                ))
            }
        }
    }
    Ok(f)
}

// Supply `more` to a primop that already has `have`: call it when its
// arity is met, or make a partial application. Returns the result and how
// many of `more` were used.
fn call_primop(p: &Rc<PrimOp>, have: &[Value], more: &[Value]) -> Result<(Value, usize)> {
    let need = p.arity - have.len();
    if more.len() < need {
        let mut args = have.to_vec();
        args.extend_from_slice(more);
        let pa = Value::PrimOpApp(Rc::new(PrimOpApp {
            op: p.clone(),
            args,
        }));
        return Ok((pa, more.len()));
    }
    let r = if have.is_empty() {
        (p.f)(&more[..need])?
    } else {
        let mut args = have.to_vec();
        args.extend_from_slice(&more[..need]);
        (p.f)(&args)?
    };
    Ok((force(&r)?, need))
}

// ---- control ----------------------------------------------------------------

pub fn as_bool(v: &Value) -> Result<bool> {
    force_bool(v)
}

pub fn assert(cond: &Value, at: &str) -> Result<()> {
    if as_bool(cond)? {
        Ok(())
    } else {
        Err(NixError::thrown(format!("assertion failed at {at}")))
    }
}

// `with e; body`: a chain of scopes searched innermost first. The set is
// forced only when a lookup reaches it.
pub struct WithScope {
    pub attrs: Value,
    pub outer: Option<Rc<WithScope>>,
}

pub fn with_scope(attrs: Value, outer: Option<&Rc<WithScope>>) -> Rc<WithScope> {
    Rc::new(WithScope {
        attrs,
        outer: outer.cloned(),
    })
}

pub fn lookup(scope: &Rc<WithScope>, name: &str) -> Result<Value> {
    let mut s = Some(scope);
    while let Some(w) = s {
        let a = force_attrs(&w.attrs)?;
        if let Some(v) = a.get(name) {
            return Ok(v.clone());
        }
        s = w.outer.as_ref();
    }
    err(format!("undefined variable '{name}'"))
}

// ---- strings and coercion ---------------------------------------------------

#[derive(Clone, Copy)]
pub struct Coerce {
    // Whether non-strings other than paths and sets are allowed: true for
    // `toString` and derivation attributes, false for interpolation and `+`.
    pub more: bool,
    // Whether a path is copied to the store (and the result gets context)
    // or rendered as its filesystem path.
    pub copy_to_store: bool,
}

pub const COERCE_INTERPOLATE: Coerce = Coerce {
    more: false,
    copy_to_store: true,
};
pub const COERCE_TO_STRING: Coerce = Coerce {
    more: true,
    copy_to_store: false,
};
pub const COERCE_PLAIN: Coerce = Coerce {
    more: false,
    copy_to_store: false,
};

// A coerced string: its bytes and context.
pub struct Coerced {
    pub bytes: Vec<u8>,
    pub context: Option<Context>,
}

impl Coerced {
    fn from_str(s: &NixStr) -> Self {
        Coerced {
            bytes: s.bytes.clone(),
            context: s.context.as_deref().cloned(),
        }
    }

    fn plain(s: impl Into<Vec<u8>>) -> Self {
        Coerced {
            bytes: s.into(),
            context: None,
        }
    }

    pub fn into_value(self) -> Value {
        mk_str(self.bytes, self.context)
    }
}

pub fn merge_context(into: &mut Option<Context>, from: Option<Context>) {
    if let Some(c) = from {
        match into {
            Some(i) => i.extend(c),
            None => *into = Some(c),
        }
    }
}

pub fn coerce_to_string(v: &Value, opts: Coerce) -> Result<Coerced> {
    let x = force(v)?;
    match &x {
        Value::Str(s) => return Ok(Coerced::from_str(s)),
        Value::Path(p) => {
            if opts.copy_to_store {
                return crate::store::copy_path_to_store(p);
            }
            return Ok(Coerced::plain(p.as_bytes()));
        }
        Value::Attrs(a) => {
            if let Some(f) = a.get("__toString") {
                let s = app(f, std::slice::from_ref(&x))?;
                return coerce_to_string(&s, opts);
            }
            if let Some(o) = a.get("outPath") {
                return coerce_to_string(o, opts);
            }
            return err("cannot coerce a set to a string: { ... }");
        }
        _ => {}
    }
    if opts.more {
        match &x {
            Value::Bool(true) => return Ok(Coerced::plain("1")),
            Value::Bool(false) | Value::Null => return Ok(Coerced::plain("")),
            Value::Int(i) => return Ok(Coerced::plain(i.to_string())),
            Value::Float(f) => return Ok(Coerced::plain(format_float_fixed(*f))),
            Value::List(l) => {
                // Elements joined by one space, except that nothing follows
                // an element that is an empty list: eval.cc keeps that quirk.
                let mut out = Coerced::plain("");
                for (i, e) in l.iter().enumerate() {
                    let s = coerce_to_string(e, opts)?;
                    out.bytes.extend_from_slice(&s.bytes);
                    merge_context(&mut out.context, s.context);
                    let empty = matches!(force(e)?, Value::List(ref l) if l.is_empty());
                    if i + 1 < l.len() && !empty {
                        out.bytes.push(b' ');
                    }
                }
                return Ok(out);
            }
            _ => {}
        }
    }
    err(format!(
        "cannot coerce {} to a string: {}",
        describe_type(&x),
        show(&x)
    ))
}

// String interpolation: `"a${b}c"` appends the pieces in order, coercing
// every value as `+` would and merging the contexts.
#[derive(Default)]
pub struct StrBuilder {
    out: Coerced,
}

impl Default for Coerced {
    fn default() -> Self {
        Coerced::plain("")
    }
}

impl StrBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lit(&mut self, s: &str) {
        self.out.bytes.extend_from_slice(s.as_bytes());
    }

    pub fn val(&mut self, v: &Value) -> Result<()> {
        let s = coerce_to_string(v, COERCE_INTERPOLATE)?;
        self.out.bytes.extend_from_slice(&s.bytes);
        merge_context(&mut self.out.context, s.context);
        Ok(())
    }

    pub fn finish(self) -> Value {
        self.out.into_value()
    }
}

// ---- paths ------------------------------------------------------------------

// Nix's canonPath: collapse `.` and `..` and repeated slashes.
pub fn canon_path(p: &str) -> Result<String> {
    if !p.starts_with('/') {
        return err(format!("not an absolute path: '{p}'"));
    }
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    Ok(format!("/{}", out.join("/")))
}

pub fn mk_path(p: &str) -> Result<Value> {
    Ok(Value::Path(canon_path(p)?.into()))
}

// A path literal: `./foo` relative to the directory of the Nix file, or an
// absolute one. Interpolated segments are appended as text.
pub struct PathBuilder {
    dir: &'static str,
    text: String,
}

impl PathBuilder {
    pub fn new(dir: &'static str) -> Self {
        PathBuilder {
            dir,
            text: String::new(),
        }
    }

    pub fn lit(&mut self, s: &str) {
        self.text.push_str(s);
    }

    pub fn val(&mut self, v: &Value) -> Result<()> {
        let s = coerce_to_string(v, COERCE_PLAIN)?;
        if s.context.is_some() {
            return err("a string that refers to a store path cannot be appended to a path");
        }
        self.text.push_str(&String::from_utf8_lossy(&s.bytes));
        Ok(())
    }

    pub fn finish(self) -> Result<Value> {
        let s = self.text;
        if s.starts_with('/') {
            return mk_path(&s);
        }
        if s == "~" || s.starts_with("~/") {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            return mk_path(&format!("{home}{}", &s[1..]));
        }
        mk_path(&format!("{}/{s}", self.dir))
    }
}

// `p + s` where p is a path: a path; the string must carry no context.
fn concat_path(p: &str, b: &Value) -> Result<Value> {
    let s = coerce_to_string(b, COERCE_PLAIN)?;
    if s.context.is_some() {
        return err("a string that refers to a store path cannot be appended to a path");
    }
    mk_path(&format!("{p}{}", String::from_utf8_lossy(&s.bytes)))
}

// ---- arithmetic -------------------------------------------------------------

fn numeric(v: &Value) -> Result<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        x => Err(expected(x, "an integer or float")),
    }
}

pub fn add(a: &Value, b: &Value) -> Result<Value> {
    let x = force(a)?;
    let y = force(b)?;
    match (&x, &y) {
        (Value::Int(i), Value::Int(j)) => Ok(Value::Int(add_int(*i, *j)?)),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            Ok(Value::Float(numeric(&x)? + numeric(&y)?))
        }
        (Value::Int(_) | Value::Float(_), _) => Err(expected(&y, "an integer or float")),
        (Value::Path(p), _) => concat_path(p, &y),
        (Value::Str(_) | Value::Attrs(_), _) => {
            let mut sa = coerce_to_string(&x, COERCE_INTERPOLATE)?;
            let sb = coerce_to_string(&y, COERCE_INTERPOLATE)?;
            sa.bytes.extend_from_slice(&sb.bytes);
            merge_context(&mut sa.context, sb.context);
            Ok(sa.into_value())
        }
        _ => err(format!(
            "cannot add {} and {}",
            describe_type(&x),
            describe_type(&y)
        )),
    }
}

pub fn sub(a: &Value, b: &Value) -> Result<Value> {
    let x = force(a)?;
    let y = force(b)?;
    if let (Value::Int(i), Value::Int(j)) = (&x, &y) {
        return Ok(Value::Int(sub_int(*i, *j)?));
    }
    Ok(Value::Float(numeric(&x)? - numeric(&y)?))
}

pub fn mul(a: &Value, b: &Value) -> Result<Value> {
    let x = force(a)?;
    let y = force(b)?;
    if let (Value::Int(i), Value::Int(j)) = (&x, &y) {
        return Ok(Value::Int(mul_int(*i, *j)?));
    }
    Ok(Value::Float(numeric(&x)? * numeric(&y)?))
}

pub fn div(a: &Value, b: &Value) -> Result<Value> {
    let x = force(a)?;
    let y = force(b)?;
    let fy = numeric(&y)?;
    if fy == 0.0 {
        return err("division by zero");
    }
    if let (Value::Int(i), Value::Int(j)) = (&x, &y) {
        return Ok(Value::Int(div_int(*i, *j)?));
    }
    Ok(Value::Float(numeric(&x)? / fy))
}

pub fn neg(a: &Value) -> Result<Value> {
    sub(&Value::Int(0), a)
}

pub fn not(a: &Value) -> Result<Value> {
    Ok(Value::Bool(!as_bool(a)?))
}

// ---- comparison -------------------------------------------------------------

// Whether two values are the same slot: the same thunk or the same heap
// value. Nix compares the elements of lists and sets by pointer first, so
// `[ f ] == [ f ]` is true even though `f == f` is not.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Thunk(x), Value::Thunk(y)) => Rc::ptr_eq(x, y),
        (Value::Lambda(x), Value::Lambda(y)) => Rc::ptr_eq(x, y),
        (Value::PrimOp(x), Value::PrimOp(y)) => Rc::ptr_eq(x, y),
        (Value::PrimOpApp(x), Value::PrimOpApp(y)) => Rc::ptr_eq(x, y),
        (Value::Attrs(x), Value::Attrs(y)) => Rc::ptr_eq(x, y),
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

pub fn eq_elem(a: &Value, b: &Value) -> Result<bool> {
    Ok(same(a, b) || eq(a, b)?)
}

// `==`, after eval.cc's eqValues. Derivations compare by outPath; functions
// are never equal.
pub fn eq(a: &Value, b: &Value) -> Result<bool> {
    let x = force(a)?;
    let y = force(b)?;
    Ok(match (&x, &y) {
        (Value::Int(i), Value::Int(j)) => i == j,
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            numeric(&x)? == numeric(&y)?
        }
        (Value::Bool(p), Value::Bool(q)) => p == q,
        (Value::Null, Value::Null) => true,
        (Value::Str(s), Value::Str(t)) => s.bytes == t.bytes,
        (Value::Path(p), Value::Path(q)) => p == q,
        (Value::List(l), Value::List(m)) => {
            if Rc::ptr_eq(l, m) {
                return Ok(true);
            }
            if l.len() != m.len() {
                return Ok(false);
            }
            for (e, f) in l.iter().zip(m.iter()) {
                if !eq_elem(e, f)? {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Attrs(s), Value::Attrs(t)) => {
            if Rc::ptr_eq(s, t) {
                return Ok(true);
            }
            if is_derivation(s)? && is_derivation(t)? {
                return eq(s.get("outPath").unwrap(), t.get("outPath").unwrap());
            }
            if s.len() != t.len() {
                return Ok(false);
            }
            for ((n, e), (m, f)) in s.iter().zip(t.iter()) {
                if n != m || !eq_elem(e, f)? {
                    return Ok(false);
                }
            }
            true
        }
        _ => false,
    })
}

fn is_derivation(a: &Attrs) -> Result<bool> {
    let (Some(t), true) = (a.get("type"), a.has("outPath")) else {
        return Ok(false);
    };
    Ok(matches!(force(t)?, Value::Str(ref s) if s.bytes == b"derivation"))
}

// `<`, after primops.cc's CompareValues: numbers, strings, paths and
// lists (lexicographically); anything else cannot be compared.
pub fn lt(a: &Value, b: &Value) -> Result<bool> {
    let x = force(a)?;
    let y = force(b)?;
    match (&x, &y) {
        (Value::Int(i), Value::Int(j)) => Ok(i < j),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            Ok(numeric(&x)? < numeric(&y)?)
        }
        (Value::Str(s), Value::Str(t)) => Ok(s.bytes < t.bytes),
        (Value::Path(p), Value::Path(q)) => Ok(p.as_bytes() < q.as_bytes()),
        (Value::List(l), Value::List(m)) => {
            for (e, f) in l.iter().zip(m.iter()) {
                if !eq_elem(e, f)? {
                    return lt(e, f);
                }
            }
            Ok(l.len() < m.len())
        }
        _ => err(format!(
            "cannot compare {} with {}",
            describe_type(&x),
            describe_type(&y)
        )),
    }
}

pub fn op_eq(a: &Value, b: &Value) -> Result<Value> {
    Ok(Value::Bool(eq(a, b)?))
}

pub fn op_ne(a: &Value, b: &Value) -> Result<Value> {
    Ok(Value::Bool(!eq(a, b)?))
}

pub fn op_lt(a: &Value, b: &Value) -> Result<Value> {
    Ok(Value::Bool(lt(a, b)?))
}

pub fn op_gt(a: &Value, b: &Value) -> Result<Value> {
    Ok(Value::Bool(lt(b, a)?))
}

pub fn op_le(a: &Value, b: &Value) -> Result<Value> {
    Ok(Value::Bool(!lt(b, a)?))
}

pub fn op_ge(a: &Value, b: &Value) -> Result<Value> {
    Ok(Value::Bool(!lt(a, b)?))
}

// ---- lists and sets ---------------------------------------------------------

pub fn concat(a: &Value, b: &Value) -> Result<Value> {
    let x = force_list(a)?;
    let y = force_list(b)?;
    if x.is_empty() {
        return Ok(Value::List(y));
    }
    if y.is_empty() {
        return Ok(Value::List(x));
    }
    let mut out = Vec::with_capacity(x.len() + y.len());
    out.extend(x.iter().cloned());
    out.extend(y.iter().cloned());
    Ok(list(out))
}

// `a // b`: right-biased, shallow. Thunks are shared, never forced.
pub fn update(a: &Value, b: &Value) -> Result<Value> {
    let x = force_attrs(a)?;
    let y = force_attrs(b)?;
    Ok(Value::Attrs(update_attrs(&x, &y)))
}

pub fn update_attrs(x: &Rc<Attrs>, y: &Rc<Attrs>) -> Rc<Attrs> {
    if y.is_empty() {
        return x.clone();
    }
    if x.is_empty() {
        return y.clone();
    }
    let mut out = (**x).clone();
    for (k, v) in y.map.iter() {
        out.map.insert(k.clone(), v.clone());
    }
    Rc::new(out)
}

// `rec { __overrides = { ... }; ... }`, a relic Nix keeps: the set's
// attributes named in __overrides take the overriding values, also for
// the references between them, and the overrides' other attributes are
// added. `cells` are the rec's own bindings, not yet forced.
pub fn rec_overrides(set: Value, cells: &[(&str, &Value)], overrides: &Value) -> Result<Value> {
    let o = force_attrs(overrides)?;
    for (name, cell) in cells {
        if let Some(v) = o.get(name) {
            init_value(cell, v.clone());
        }
    }
    update(&set, &Value::Attrs(o))
}

// `__curPos`: where it is written.
pub fn cur_pos(file: &'static str, line: u32, column: u32) -> Value {
    let mut a = Attrs::new();
    a.insert("file", string(file));
    a.insert("line", Value::Int(line as i64));
    a.insert("column", Value::Int(column as i64));
    attrs(a)
}
