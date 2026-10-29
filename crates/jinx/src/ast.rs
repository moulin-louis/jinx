// The Nix abstract syntax, as the parser produces it and the compiler
// consumes it. Every node carries the position it started at, so that
// errors, `__curPos` and `unsafeGetAttrPos` can name the Nix file and line.

use std::fmt;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq)]
pub struct Pos {
    pub file: Rc<str>,
    pub line: u32,
    pub column: u32,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

// A string literal is a sequence of literal text and interpolated
// expressions. Indented strings arrive here with their indentation already
// stripped, so the two spellings are the same here.
#[derive(Clone, Debug)]
pub enum StrPart {
    Lit(String),
    Expr(Expr),
}

pub fn all_literal(parts: &[StrPart]) -> Option<String> {
    let mut s = String::new();
    for p in parts {
        match p {
            StrPart::Lit(t) => s.push_str(t),
            StrPart::Expr(_) => return None,
        }
    }
    Some(s)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathAnchor {
    Relative,
    Absolute,
    Home,
    Search,
}

// An attribute name in a binding or a selection path: static when it is an
// identifier or a string with no interpolation, dynamic when it is `${e}`.
#[derive(Clone, Debug)]
pub enum AttrName {
    Static { name: String, pos: Pos },
    Dynamic { expr: Box<Expr>, pos: Pos },
}

impl AttrName {
    pub fn pos(&self) -> &Pos {
        match self {
            AttrName::Static { pos, .. } | AttrName::Dynamic { pos, .. } => pos,
        }
    }

    pub fn static_name(&self) -> Option<&str> {
        match self {
            AttrName::Static { name, .. } => Some(name),
            AttrName::Dynamic { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inherited {
    No,
    // `inherit a;`
    Scope,
    // `inherit (e) a;`
    From,
}

// `name = value;` after the parser has merged attribute paths: nested
// paths become nested attribute sets, so every binding here is one level
// deep. For `inherit (e) a`, the value is the selection `e.a`.
#[derive(Clone, Debug)]
pub struct Binding {
    pub name: AttrName,
    pub value: Expr,
    pub inherited: Inherited,
}

#[derive(Clone, Debug)]
pub struct Formal {
    pub name: String,
    pub pos: Pos,
    pub fallback: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct Formals {
    pub entries: Vec<Formal>,
    pub ellipsis: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Impl,
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Update,
    Concat,
    Add,
    Sub,
    Mul,
    Div,
    PipeRight,
    PipeLeft,
}

impl BinOp {
    pub fn from_text(s: &str) -> Option<BinOp> {
        Some(match s {
            "->" => BinOp::Impl,
            "||" => BinOp::Or,
            "&&" => BinOp::And,
            "==" => BinOp::Eq,
            "!=" => BinOp::Ne,
            "<" => BinOp::Lt,
            ">" => BinOp::Gt,
            "<=" => BinOp::Le,
            ">=" => BinOp::Ge,
            "//" => BinOp::Update,
            "++" => BinOp::Concat,
            "+" => BinOp::Add,
            "-" => BinOp::Sub,
            "*" => BinOp::Mul,
            "/" => BinOp::Div,
            "|>" => BinOp::PipeRight,
            "<|" => BinOp::PipeLeft,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Var(String),
    Int(i64),
    Float(f64),
    Str(Vec<StrPart>),
    Path(PathAnchor, Vec<StrPart>),
    List(Vec<Expr>),
    Attrs {
        recursive: bool,
        bindings: Vec<Binding>,
    },
    Select {
        subject: Box<Expr>,
        path: Vec<AttrName>,
        fallback: Option<Box<Expr>>,
    },
    HasAttr {
        subject: Box<Expr>,
        path: Vec<AttrName>,
    },
    Lambda {
        param: Option<String>,
        formals: Option<Formals>,
        body: Box<Expr>,
    },
    Call {
        func: Box<Expr>,
        arg: Box<Expr>,
    },
    Let {
        bindings: Vec<Binding>,
        body: Box<Expr>,
    },
    With {
        scope: Box<Expr>,
        body: Box<Expr>,
    },
    Assert {
        condition: Box<Expr>,
        body: Box<Expr>,
    },
    If {
        condition: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    Unary(UnaryOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub pos: Pos,
    pub kind: ExprKind,
}

impl Expr {
    pub fn new(pos: Pos, kind: ExprKind) -> Self {
        Expr { pos, kind }
    }
}
