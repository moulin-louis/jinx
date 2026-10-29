// How a Nix value lives in Rust. One enum, cheap to clone (everything
// behind an Rc), where a thunk is one of the variants: a `Value` in a lazy
// position may be a `Thunk`, and force() turns it into one that is not.
//
//   int      Int(i64)          checked 64-bit arithmetic, as Nix's
//   float    Float(f64)
//   bool     Bool
//   null     Null
//   string   Str(NixStr)       bytes plus a store-path context
//   path     Path              an absolute, canonical path
//   list     List              elements are values or thunks
//   attrs    Attrs             sorted by name, each with its position
//   lambda   Lambda            a closure of one argument, with formals
//   primop   PrimOp, PrimOpApp a builtin, and one partially applied
//
// Reference cycles (a recursive let, a fixpoint) are never collected; an
// evaluation is one process that exits when it is done.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt;
use std::rc::Rc;

pub type Result<T> = std::result::Result<T, Box<NixError>>;

#[derive(Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Rc<NixStr>),
    Path(Rc<str>),
    List(Rc<Vec<Value>>),
    Attrs(Rc<Attrs>),
    Lambda(Rc<Lambda>),
    PrimOp(Rc<PrimOp>),
    PrimOpApp(Rc<PrimOpApp>),
    Thunk(Rc<Thunk>),
}

// ---- strings ----------------------------------------------------------------

// A Nix string: bytes, and the store paths it refers to. Context elements
// use Nix's own spelling:
//   /nix/store/...            a source or output path
//   !out!/nix/store/....drv   one output of a derivation
//   =/nix/store/....drv       the derivation itself and all its outputs
pub type Context = BTreeSet<String>;

pub struct NixStr {
    pub bytes: Vec<u8>,
    pub context: Option<Box<Context>>,
}

impl NixStr {
    pub fn as_str_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    pub fn has_context(&self) -> bool {
        self.context.as_ref().is_some_and(|c| !c.is_empty())
    }
}

pub fn mk_str(bytes: Vec<u8>, context: Option<Context>) -> Value {
    let context = context.filter(|c| !c.is_empty()).map(Box::new);
    Value::Str(Rc::new(NixStr { bytes, context }))
}

pub fn string(s: impl Into<String>) -> Value {
    mk_str(s.into().into_bytes(), None)
}

// ---- attribute sets ---------------------------------------------------------

// Where an attribute was defined, for builtins.unsafeGetAttrPos. Only
// literals in compiled modules have positions, so the file is static.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub file: &'static str,
    pub line: u32,
    pub column: u32,
}

#[derive(Clone)]
pub struct Attr {
    pub value: Value,
    pub pos: Option<Pos>,
}

// A persistent ordered map (imbl's B-tree with shared nodes): cloning a
// set is O(1) and `//` copies only the paths it changes, which is what
// nixpkgs needs, where every overlay is a `//` on a set of tens of
// thousands of attributes. Names are ordered by their bytes, which is the
// order Nix enumerates them in, so iteration needs no sort.
pub type AttrMap = imbl::GenericOrdMap<Rc<str>, Attr, imbl::shared_ptr::RcK>;

#[derive(Clone, Default)]
pub struct Attrs {
    pub map: AttrMap,
}

impl Attrs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.map.get(name).map(|a| &a.value)
    }

    pub fn has(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    pub fn pos(&self, name: &str) -> Option<Pos> {
        self.map.get(name).and_then(|a| a.pos)
    }

    pub fn insert(&mut self, name: impl Into<Rc<str>>, value: Value) {
        self.map.insert(name.into(), Attr { value, pos: None });
    }

    pub fn insert_pos(&mut self, name: impl Into<Rc<str>>, value: Value, pos: Option<Pos>) {
        self.map.insert(name.into(), Attr { value, pos });
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Rc<str>, &Value)> {
        self.map.iter().map(|(k, a)| (k, &a.value))
    }

    pub fn names(&self) -> impl Iterator<Item = &Rc<str>> {
        self.map.keys()
    }
}

pub fn attrs(a: Attrs) -> Value {
    Value::Attrs(Rc::new(a))
}

// ---- functions --------------------------------------------------------------

// `{ a, b ? 1, ... }`: each name, whether it has a default, and where it
// is written (functionArgs reports the positions).
pub struct Formals {
    pub file: &'static str,
    pub args: &'static [(&'static str, bool, u32, u32)],
    pub ellipsis: bool,
}

impl Formals {
    pub fn has(&self, name: &str) -> bool {
        self.args.iter().any(|(n, ..)| *n == name)
    }
}

pub type LambdaFn = dyn Fn(Value) -> Result<Value>;

pub struct Lambda {
    pub f: Box<LambdaFn>,
    pub formals: Option<&'static Formals>,
}

pub type PrimOpFn = fn(&[Value]) -> Result<Value>;

pub struct PrimOp {
    pub name: &'static str,
    pub arity: usize,
    pub f: PrimOpFn,
}

pub struct PrimOpApp {
    pub op: Rc<PrimOp>,
    pub args: Vec<Value>,
}

pub fn lambda(f: impl Fn(Value) -> Result<Value> + 'static) -> Value {
    Value::Lambda(Rc::new(Lambda {
        f: Box::new(f),
        formals: None,
    }))
}

pub fn lambda_formals(
    formals: &'static Formals,
    f: impl Fn(Value) -> Result<Value> + 'static,
) -> Value {
    Value::Lambda(Rc::new(Lambda {
        f: Box::new(f),
        formals: Some(formals),
    }))
}

// ---- thunks -----------------------------------------------------------------

type ThunkFn = dyn Fn() -> Result<Value>;

enum ThunkState {
    // Declared by a recursive let and not yet given its closure. Nothing
    // can force one: every binding of a group is initialised before any
    // code that could run.
    Blank,
    Pending(Rc<ThunkFn>),
    // Being forced; forcing it again is Nix's infinite recursion.
    Blackhole,
    Done(Value),
}

pub struct Thunk {
    state: RefCell<ThunkState>,
}

impl Thunk {
    pub fn is_forced(&self) -> bool {
        matches!(*self.state.borrow(), ThunkState::Done(_))
    }

    pub fn force(&self) -> Result<Value> {
        let f = {
            let mut st = self.state.borrow_mut();
            match &*st {
                ThunkState::Done(v) => return Ok(v.clone()),
                ThunkState::Blackhole => {
                    return Err(NixError::boxed("infinite recursion encountered"))
                }
                ThunkState::Blank => {
                    return Err(NixError::boxed("internal: forced an uninitialised binding"))
                }
                ThunkState::Pending(f) => {
                    let f = f.clone();
                    *st = ThunkState::Blackhole;
                    f
                }
            }
        };
        match f().and_then(|v| force(&v)) {
            Ok(v) => {
                *self.state.borrow_mut() = ThunkState::Done(v.clone());
                Ok(v)
            }
            Err(e) => {
                // Nix forces a thunk again after an error, so the closure
                // comes back; a tryEval that caught the error may force it
                // once more.
                *self.state.borrow_mut() = ThunkState::Pending(f);
                Err(e)
            }
        }
    }
}

pub fn thunk(f: impl Fn() -> Result<Value> + 'static) -> Value {
    Value::Thunk(Rc::new(Thunk {
        state: RefCell::new(ThunkState::Pending(Rc::new(f))),
    }))
}

// A binding of a recursive group, given its definition by init_*.
pub fn blank() -> Value {
    Value::Thunk(Rc::new(Thunk {
        state: RefCell::new(ThunkState::Blank),
    }))
}

pub fn init_thunk(cell: &Value, f: impl Fn() -> Result<Value> + 'static) {
    if let Value::Thunk(t) = cell {
        *t.state.borrow_mut() = ThunkState::Pending(Rc::new(f));
    }
}

pub fn init_value(cell: &Value, v: Value) {
    if let Value::Thunk(t) = cell {
        let st = match v {
            Value::Thunk(inner) => {
                // Share the other thunk's work rather than copying its state.
                let inner = Value::Thunk(inner);
                ThunkState::Pending(Rc::new(move || force(&inner)))
            }
            v => ThunkState::Done(v),
        };
        *t.state.borrow_mut() = st;
    }
}

#[inline]
pub fn force(v: &Value) -> Result<Value> {
    match v {
        Value::Thunk(t) => t.force(),
        v => Ok(v.clone()),
    }
}

// ---- errors -----------------------------------------------------------------

// Any evaluation error. `catchable` is what builtins.tryEval may catch:
// throw and assert, but not abort, type errors or infinite recursion.
#[derive(Debug)]
pub struct NixError {
    pub message: String,
    pub catchable: bool,
}

impl NixError {
    pub fn new(message: impl Into<String>) -> Self {
        NixError {
            message: message.into(),
            catchable: false,
        }
    }

    pub fn boxed(message: impl Into<String>) -> Box<Self> {
        Box::new(Self::new(message))
    }

    pub fn thrown(message: impl Into<String>) -> Box<Self> {
        Box::new(NixError {
            message: message.into(),
            catchable: true,
        })
    }
}

impl fmt::Display for NixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub fn err<T>(message: impl Into<String>) -> Result<T> {
    Err(NixError::boxed(message))
}

// ---- types ------------------------------------------------------------------

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Path(_) => "path",
            Value::List(_) => "list",
            Value::Attrs(_) => "set",
            Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_) => "lambda",
            Value::Thunk(_) => "thunk",
        }
    }

    pub fn is_function(&self) -> bool {
        matches!(
            self,
            Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_)
        )
    }
}

// The article Nix uses in "value is an integer while a string was expected".
pub fn describe_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a Boolean",
        Value::Int(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Str(_) => "a string",
        Value::Path(_) => "a path",
        Value::List(_) => "a list",
        Value::Attrs(_) => "a set",
        Value::Lambda(_) | Value::PrimOp(_) | Value::PrimOpApp(_) => "a function",
        Value::Thunk(_) => "a thunk",
    }
}

// A short rendering of a value for error messages, never forcing anything.
pub fn show(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => crate::numbers::format_float_g(*f),
        Value::Str(s) => crate::print::print_string(&s.bytes),
        Value::Path(p) => p.to_string(),
        Value::List(_) => "[ ... ]".into(),
        Value::Attrs(_) => "{ ... }".into(),
        Value::Lambda(_) => "«lambda»".into(),
        Value::PrimOp(p) => format!("«primop {}»", p.name),
        Value::PrimOpApp(p) => format!("«partially applied primop {}»", p.op.name),
        Value::Thunk(_) => "«thunk»".into(),
    }
}

pub fn expected(v: &Value, what: &str) -> Box<NixError> {
    NixError::boxed(format!(
        "expected {what} but found {}: {}",
        describe_type(v),
        show(v)
    ))
}
