// The Nix parser: recursive descent over the lexer's tokens, after the
// grammar in parser.y. The precedence table below is Nix's, lowest first.
// Two jobs beyond the grammar: merging attribute paths (`a.b = 1; a.c =
// 2;` is one nested set) and stripping the common indentation of `''`
// strings.

use crate::ast::*;
use crate::lexer::{IndentedSegment, LexResult, Lexer, Stop, SyntaxError, Token, TokenKind};

pub fn parse(src: &str, file: &str) -> Result<Expr, SyntaxError> {
    let mut p = Parser {
        lx: Lexer::new(src, file),
    };
    let e = p.parse_expr()?;
    let t = p.lx.peek()?.clone();
    if t.kind != TokenKind::Eof {
        return fail(format!("unexpected {}", describe(&t)), t.pos);
    }
    Ok(e)
}

fn fail<T>(message: impl Into<String>, pos: Pos) -> LexResult<T> {
    Err(SyntaxError {
        message: message.into(),
        pos,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Assoc {
    Left,
    Right,
    None,
}

// From parser.y, lowest precedence first. `!` and unary `-` sit between
// `//` and `++`, and between `?` and application, respectively; they are
// handled where the climb reaches their level.
const LEVELS: &[(&[&str], Assoc)] = &[
    (&["|>"], Assoc::Left),
    (&["<|"], Assoc::Right),
    (&["->"], Assoc::Right),
    (&["||"], Assoc::Left),
    (&["&&"], Assoc::Left),
    (&["==", "!="], Assoc::None),
    (&["<", ">", "<=", ">="], Assoc::None),
    (&["//"], Assoc::Right),
    // `!` lives here.
    (&["++"], Assoc::Right),
    (&["+", "-"], Assoc::Left),
    (&["*", "/"], Assoc::Left),
    // `?` and then unary `-` live here.
];
const NOT_LEVEL: usize = 8;
const HAS_ATTR_LEVEL: usize = LEVELS.len();
const NEGATE_LEVEL: usize = LEVELS.len() + 1;

struct Parser {
    lx: Lexer,
}

fn bx(e: Expr) -> Box<Expr> {
    Box::new(e)
}

impl Parser {
    fn peek(&mut self) -> LexResult<Token> {
        Ok(self.lx.peek()?.clone())
    }

    fn is_op(&mut self, text: &str) -> LexResult<bool> {
        let t = self.lx.peek()?;
        Ok(t.kind == TokenKind::Op && t.text == text)
    }

    fn is_keyword(&mut self, text: &str) -> LexResult<bool> {
        let t = self.lx.peek()?;
        Ok(t.kind == TokenKind::Keyword && t.text == text)
    }

    fn expect_op(&mut self, text: &str) -> LexResult<Token> {
        let t = self.lx.next()?;
        if t.kind != TokenKind::Op || t.text != text {
            return fail(format!("expected '{text}', got {}", describe(&t)), t.pos);
        }
        Ok(t)
    }

    fn expect_keyword(&mut self, text: &str) -> LexResult<Token> {
        let t = self.lx.next()?;
        if t.kind != TokenKind::Keyword || t.text != text {
            return fail(format!("expected '{text}', got {}", describe(&t)), t.pos);
        }
        Ok(t)
    }

    fn expect_id(&mut self) -> LexResult<Token> {
        let t = self.lx.next()?;
        if t.kind != TokenKind::Id {
            return fail(
                format!("expected an identifier, got {}", describe(&t)),
                t.pos,
            );
        }
        Ok(t)
    }

    // expr_function in parser.y: lambdas, assert, with, let, then expr_if.
    fn parse_expr(&mut self) -> LexResult<Expr> {
        let t = self.peek()?;
        if t.kind == TokenKind::Id {
            // `x: body` or `x@{ ... }: body`; otherwise an ordinary
            // expression that happens to start with an identifier.
            let id = self.lx.next()?;
            if self.is_op(":")? {
                self.lx.next()?;
                let body = self.parse_expr()?;
                return Ok(Expr::new(
                    id.pos,
                    ExprKind::Lambda {
                        param: Some(id.text),
                        formals: None,
                        body: bx(body),
                    },
                ));
            }
            if self.is_op("@")? {
                self.lx.next()?;
                self.expect_op("{")?;
                let formals = self.parse_formals()?;
                self.expect_op(":")?;
                let body = self.parse_expr()?;
                return Ok(Expr::new(
                    id.pos,
                    ExprKind::Lambda {
                        param: Some(id.text),
                        formals: Some(formals),
                        body: bx(body),
                    },
                ));
            }
            return self.continue_from(Expr::new(id.pos, ExprKind::Var(id.text)));
        }
        if t.kind == TokenKind::Op && t.text == "{" && self.lx.braces_followed_by_colon_or_at() {
            self.lx.next()?;
            let formals = self.parse_formals()?;
            let mut param = None;
            if self.is_op("@")? {
                self.lx.next()?;
                param = Some(self.expect_id()?.text);
            }
            self.expect_op(":")?;
            let body = self.parse_expr()?;
            return Ok(Expr::new(
                t.pos,
                ExprKind::Lambda {
                    param,
                    formals: Some(formals),
                    body: bx(body),
                },
            ));
        }
        if t.kind == TokenKind::Keyword {
            match t.text.as_str() {
                "assert" => {
                    self.lx.next()?;
                    let condition = self.parse_expr()?;
                    self.expect_op(";")?;
                    let body = self.parse_expr()?;
                    return Ok(Expr::new(
                        t.pos,
                        ExprKind::Assert {
                            condition: bx(condition),
                            body: bx(body),
                        },
                    ));
                }
                "with" => {
                    self.lx.next()?;
                    let scope = self.parse_expr()?;
                    self.expect_op(";")?;
                    let body = self.parse_expr()?;
                    return Ok(Expr::new(
                        t.pos,
                        ExprKind::With {
                            scope: bx(scope),
                            body: bx(body),
                        },
                    ));
                }
                "let" => {
                    self.lx.next()?;
                    // The legacy `let { body = e; ... }` is `(rec { ... }).body`.
                    if self.is_op("{")? {
                        self.lx.next()?;
                        let attrs = self.parse_attrs(t.pos.clone(), true)?;
                        return Ok(Expr::new(
                            t.pos.clone(),
                            ExprKind::Select {
                                subject: bx(attrs),
                                path: vec![AttrName::Static {
                                    name: "body".into(),
                                    pos: t.pos,
                                }],
                                fallback: None,
                            },
                        ));
                    }
                    let bindings = self.parse_bindings(false)?;
                    self.expect_keyword("in")?;
                    let body = self.parse_expr()?;
                    for b in &bindings {
                        if let AttrName::Dynamic { pos, .. } = &b.name {
                            return fail("dynamic attributes not allowed in let", pos.clone());
                        }
                    }
                    return Ok(Expr::new(
                        t.pos,
                        ExprKind::Let {
                            bindings,
                            body: bx(body),
                        },
                    ));
                }
                "if" => {
                    self.lx.next()?;
                    let condition = self.parse_expr()?;
                    self.expect_keyword("then")?;
                    let then = self.parse_expr()?;
                    self.expect_keyword("else")?;
                    let otherwise = self.parse_expr()?;
                    return Ok(Expr::new(
                        t.pos,
                        ExprKind::If {
                            condition: bx(condition),
                            then: bx(then),
                            otherwise: bx(otherwise),
                        },
                    ));
                }
                _ => {}
            }
        }
        self.parse_binary(0)
    }

    // After the identifier of an expression that turned out not to be a
    // lambda: finish it as the start of expr_op.
    fn continue_from(&mut self, first: Expr) -> LexResult<Expr> {
        let selected = self.parse_select_rest(first)?;
        let applied = self.parse_app_rest(selected)?;
        let tested = self.parse_has_attr_rest(applied)?;
        self.parse_binary_rest(tested, 0)
    }

    fn parse_binary(&mut self, level: usize) -> LexResult<Expr> {
        if level == NOT_LEVEL && self.is_op("!")? {
            let t = self.lx.next()?;
            let operand = self.parse_binary(NOT_LEVEL)?;
            return Ok(Expr::new(t.pos, ExprKind::Unary(UnaryOp::Not, bx(operand))));
        }
        if level == NEGATE_LEVEL {
            if self.is_op("-")? {
                let t = self.lx.next()?;
                let operand = self.parse_binary(NEGATE_LEVEL)?;
                return Ok(Expr::new(t.pos, ExprKind::Unary(UnaryOp::Neg, bx(operand))));
            }
            return self.parse_app();
        }
        if level == HAS_ATTR_LEVEL {
            let left = self.parse_binary(NEGATE_LEVEL)?;
            return self.parse_has_attr_rest(left);
        }
        let left = self.parse_binary(level + 1)?;
        self.parse_level_rest(left, level)
    }

    fn parse_has_attr_rest(&mut self, left: Expr) -> LexResult<Expr> {
        // The right side is an attribute path, not an operand, so
        // `a ? b ? c` is `(a ? b) ? c`.
        let mut e = left;
        while self.is_op("?")? {
            let t = self.lx.next()?;
            let path = self.parse_attr_path()?;
            e = Expr::new(
                t.pos,
                ExprKind::HasAttr {
                    subject: bx(e),
                    path,
                },
            );
        }
        Ok(e)
    }

    // Continue climbing from `left`, which has been parsed at a level at
    // least as tight as `level`.
    fn parse_binary_rest(&mut self, mut left: Expr, level: usize) -> LexResult<Expr> {
        for l in (level..LEVELS.len()).rev() {
            left = self.parse_level_rest(left, l)?;
        }
        Ok(left)
    }

    fn op_at(&mut self, level: usize) -> LexResult<Option<BinOp>> {
        let t = self.lx.peek()?;
        if t.kind == TokenKind::Op && LEVELS[level].0.contains(&t.text.as_str()) {
            return Ok(BinOp::from_text(&t.text));
        }
        Ok(None)
    }

    fn parse_level_rest(&mut self, mut left: Expr, level: usize) -> LexResult<Expr> {
        let assoc = LEVELS[level].1;
        let Some(mut op) = self.op_at(level)? else {
            return Ok(left);
        };
        if assoc == Assoc::Right {
            let t = self.lx.next()?;
            let right = self.parse_binary(level)?;
            return Ok(Expr::new(t.pos, ExprKind::Binary(op, bx(left), bx(right))));
        }
        loop {
            let t = self.lx.next()?;
            let right = self.parse_binary(level + 1)?;
            left = Expr::new(t.pos, ExprKind::Binary(op, bx(left), bx(right)));
            match self.op_at(level)? {
                None => return Ok(left),
                Some(next) => {
                    if assoc == Assoc::None {
                        let p = self.peek()?.pos;
                        return fail(
                            format!("operator '{}' is not associative", self.peek()?.text),
                            p,
                        );
                    }
                    op = next;
                }
            }
        }
    }

    fn parse_app(&mut self) -> LexResult<Expr> {
        let f = self.parse_select()?;
        self.parse_app_rest(f)
    }

    fn parse_app_rest(&mut self, mut f: Expr) -> LexResult<Expr> {
        while self.starts_simple()? {
            let arg = self.parse_select()?;
            let pos = f.pos.clone();
            f = Expr::new(
                pos,
                ExprKind::Call {
                    func: bx(f),
                    arg: bx(arg),
                },
            );
        }
        Ok(f)
    }

    // Whether the next token can begin an expr_simple, i.e. an argument.
    fn starts_simple(&mut self) -> LexResult<bool> {
        let t = self.lx.peek()?;
        Ok(match t.kind {
            TokenKind::Id
            | TokenKind::Int
            | TokenKind::Float
            | TokenKind::Uri
            | TokenKind::SPath
            | TokenKind::PathStart
            | TokenKind::StringStart
            | TokenKind::IndentStart => true,
            // `f or` calls f with the variable `or`: Nix's grammar keeps
            // that reading for compatibility.
            TokenKind::Keyword => t.text == "rec" || t.text == "or",
            TokenKind::Op => matches!(t.text.as_str(), "(" | "{" | "["),
            TokenKind::Eof => false,
        })
    }

    fn parse_select(&mut self) -> LexResult<Expr> {
        let subject = self.parse_simple()?;
        self.parse_select_rest(subject)
    }

    fn parse_select_rest(&mut self, subject: Expr) -> LexResult<Expr> {
        if !self.is_op(".")? {
            return Ok(subject);
        }
        let t = self.lx.next()?;
        let path = self.parse_attr_path()?;
        let mut fallback = None;
        if self.is_keyword("or")? {
            self.lx.next()?;
            fallback = Some(bx(self.parse_select()?));
        }
        Ok(Expr::new(
            t.pos,
            ExprKind::Select {
                subject: bx(subject),
                path,
                fallback,
            },
        ))
    }

    fn parse_attr_path(&mut self) -> LexResult<Vec<AttrName>> {
        let mut path = vec![self.parse_attr_name()?];
        while self.is_op(".")? {
            self.lx.next()?;
            path.push(self.parse_attr_name()?);
        }
        Ok(path)
    }

    // An attribute name: identifier, `or` (which is a keyword everywhere
    // else), a string, or `${e}`. A string without interpolation is static.
    fn parse_attr_name(&mut self) -> LexResult<AttrName> {
        let t = self.peek()?;
        if t.kind == TokenKind::Id || (t.kind == TokenKind::Keyword && t.text == "or") {
            self.lx.next()?;
            return Ok(AttrName::Static {
                name: t.text,
                pos: t.pos,
            });
        }
        if t.kind == TokenKind::StringStart {
            self.lx.next()?;
            let parts = self.parse_string_parts()?;
            if let Some(name) = all_literal(&parts) {
                return Ok(AttrName::Static { name, pos: t.pos });
            }
            return Ok(AttrName::Dynamic {
                expr: bx(Expr::new(t.pos.clone(), ExprKind::Str(parts))),
                pos: t.pos,
            });
        }
        if t.kind == TokenKind::Op && t.text == "${" {
            self.lx.next()?;
            let expr = self.parse_expr()?;
            self.expect_op("}")?;
            // `${"a"}` with a constant string is the static name `a`.
            if let ExprKind::Str(parts) = &expr.kind {
                if let Some(name) = all_literal(parts) {
                    return Ok(AttrName::Static { name, pos: t.pos });
                }
            }
            return Ok(AttrName::Dynamic {
                expr: bx(expr),
                pos: t.pos,
            });
        }
        fail(
            format!("expected an attribute name, got {}", describe(&t)),
            t.pos,
        )
    }

    fn parse_simple(&mut self) -> LexResult<Expr> {
        let t = self.lx.next()?;
        let pos = t.pos.clone();
        match t.kind {
            TokenKind::Id => return Ok(Expr::new(pos, ExprKind::Var(t.text))),
            TokenKind::Int => {
                let Ok(i) = t.text.parse::<i64>() else {
                    return fail(format!("invalid integer '{}'", t.text), pos);
                };
                return Ok(Expr::new(pos, ExprKind::Int(i)));
            }
            TokenKind::Float => {
                let f = t.text.parse::<f64>().unwrap_or(f64::NAN);
                return Ok(Expr::new(pos, ExprKind::Float(f)));
            }
            TokenKind::Uri => return Ok(Expr::new(pos, ExprKind::Str(vec![StrPart::Lit(t.text)]))),
            TokenKind::SPath => {
                let inner = t.text[1..t.text.len() - 1].to_string();
                return Ok(Expr::new(
                    pos,
                    ExprKind::Path(PathAnchor::Search, vec![StrPart::Lit(inner)]),
                ));
            }
            TokenKind::StringStart => {
                let parts = self.parse_string_parts()?;
                Self::check_nul(&parts, &pos)?;
                return Ok(Expr::new(pos, ExprKind::Str(parts)));
            }
            TokenKind::IndentStart => {
                let parts = self.parse_indented_string_parts()?;
                Self::check_nul(&parts, &pos)?;
                return Ok(Expr::new(pos, ExprKind::Str(parts)));
            }
            TokenKind::PathStart => return self.parse_path(pos),
            TokenKind::Keyword if t.text == "or" => {
                return Ok(Expr::new(pos, ExprKind::Var(t.text)))
            }
            TokenKind::Keyword if t.text == "rec" => {
                self.expect_op("{")?;
                return self.parse_attrs(pos, true);
            }
            TokenKind::Op => match t.text.as_str() {
                "(" => {
                    let e = self.parse_expr()?;
                    self.expect_op(")")?;
                    return Ok(e);
                }
                "{" => return self.parse_attrs(pos, false),
                "[" => {
                    let mut elements = Vec::new();
                    while !self.is_op("]")? {
                        elements.push(self.parse_select()?);
                    }
                    self.lx.next()?;
                    return Ok(Expr::new(pos, ExprKind::List(elements)));
                }
                _ => {}
            },
            _ => {}
        }
        fail(format!("unexpected {}", describe(&t)), pos)
    }

    // Nix strings cannot hold a NUL byte; a literal with one is refused.
    fn check_nul(parts: &[StrPart], pos: &Pos) -> LexResult<()> {
        for p in parts {
            if let StrPart::Lit(t) = p {
                if t.contains('\0') {
                    return fail(
                        format!("input string '{t}' cannot be represented as Nix string because it contains null bytes"),
                        pos.clone(),
                    );
                }
            }
        }
        Ok(())
    }

    fn parse_string_parts(&mut self) -> LexResult<Vec<StrPart>> {
        let mut parts = Vec::new();
        loop {
            let (text, stop) = self.lx.scan_string()?;
            if !text.is_empty() {
                parts.push(StrPart::Lit(text));
            }
            if stop == Stop::End {
                return Ok(parts);
            }
            parts.push(StrPart::Expr(self.parse_expr()?));
            self.expect_op("}")?;
        }
    }

    fn parse_indented_string_parts(&mut self) -> LexResult<Vec<StrPart>> {
        let mut parts = Vec::new();
        loop {
            let (segments, stop) = self.lx.scan_indented_string()?;
            parts.extend(segments.into_iter().map(IndentedPart::Segment));
            if stop == Stop::End {
                return Ok(strip_indentation(parts));
            }
            parts.push(IndentedPart::Expr(self.parse_expr()?));
            self.expect_op("}")?;
        }
    }

    fn parse_path(&mut self, pos: Pos) -> LexResult<Expr> {
        let mut anchor = PathAnchor::Relative;
        let mut parts = Vec::new();
        let mut first = true;
        loop {
            let (text, stop) = self.lx.scan_path(first)?;
            if first {
                if text.starts_with('/') {
                    anchor = PathAnchor::Absolute;
                } else if text.starts_with('~') {
                    anchor = PathAnchor::Home;
                }
                first = false;
            }
            if !text.is_empty() {
                parts.push(StrPart::Lit(text));
            }
            if stop == Stop::End {
                break;
            }
            parts.push(StrPart::Expr(self.parse_expr()?));
            self.expect_op("}")?;
        }
        Ok(Expr::new(pos, ExprKind::Path(anchor, parts)))
    }

    fn parse_formals(&mut self) -> LexResult<Formals> {
        let mut entries: Vec<Formal> = Vec::new();
        let mut ellipsis = false;
        while !self.is_op("}")? {
            if self.is_op("...")? {
                self.lx.next()?;
                ellipsis = true;
                if !self.is_op("}")? {
                    let p = self.peek()?.pos;
                    return fail("'...' must be the last formal", p);
                }
                break;
            }
            let id = self.expect_id()?;
            if entries.iter().any(|f| f.name == id.text) {
                return fail(
                    format!("duplicate formal function argument '{}'", id.text),
                    id.pos,
                );
            }
            let fallback = if self.is_op("?")? {
                self.lx.next()?;
                Some(self.parse_expr()?)
            } else {
                None
            };
            entries.push(Formal {
                name: id.text,
                pos: id.pos,
                fallback,
            });
            if self.is_op(",")? {
                self.lx.next()?;
                continue;
            }
            if !self.is_op("}")? {
                let t = self.peek()?;
                return fail(format!("expected ',' or '}}', got {}", describe(&t)), t.pos);
            }
        }
        self.expect_op("}")?;
        Ok(Formals { entries, ellipsis })
    }

    // After the `{`.
    fn parse_attrs(&mut self, pos: Pos, recursive: bool) -> LexResult<Expr> {
        let bindings = self.parse_bindings(true)?;
        self.expect_op("}")?;
        Ok(Expr::new(
            pos,
            ExprKind::Attrs {
                recursive,
                bindings,
            },
        ))
    }

    // Bindings up to the closing token, merged into one level of names.
    fn parse_bindings(&mut self, until_brace: bool) -> LexResult<Vec<Binding>> {
        let mut merger = AttrMerger::default();
        loop {
            let done = if until_brace {
                self.is_op("}")?
            } else {
                self.is_keyword("in")?
            };
            if done {
                break;
            }
            if self.is_keyword("inherit")? {
                self.lx.next()?;
                let from = if self.is_op("(")? {
                    self.lx.next()?;
                    let e = self.parse_expr()?;
                    self.expect_op(")")?;
                    Some(e)
                } else {
                    None
                };
                while !self.is_op(";")? {
                    let name = self.parse_attr_name()?;
                    let AttrName::Static { name: n, pos } = &name else {
                        return fail(
                            "dynamic attributes not allowed in inherit",
                            name.pos().clone(),
                        );
                    };
                    match &from {
                        Some(f) => {
                            let value = Expr::new(
                                pos.clone(),
                                ExprKind::Select {
                                    subject: bx(f.clone()),
                                    path: vec![name.clone()],
                                    fallback: None,
                                },
                            );
                            merger.add(vec![name.clone()], value, Inherited::From)?;
                        }
                        None => {
                            let value = Expr::new(pos.clone(), ExprKind::Var(n.clone()));
                            merger.add(vec![name.clone()], value, Inherited::Scope)?;
                        }
                    }
                }
                self.expect_op(";")?;
                continue;
            }
            let path = self.parse_attr_path()?;
            self.expect_op("=")?;
            let value = self.parse_expr()?;
            self.expect_op(";")?;
            merger.add(path, value, Inherited::No)?;
        }
        Ok(merger.finish())
    }
}

// Builds the bindings of one attribute set from paths. `a.b = 1; a.c = 2;`
// puts `b` and `c` inside one set `a`; `a = { b = 1; }; a.c = 2;` extends
// the literal set; anything else that names an attribute twice is an
// error, as in Nix. Dynamic names are kept in order after the static ones,
// which is also where Nix puts them.
#[derive(Default)]
struct AttrMerger {
    statics: Vec<(String, Static)>,
    dynamics: Vec<Binding>,
}

struct Static {
    binding: Binding,
    nested: Option<AttrMerger>,
    recursive: bool,
}

impl AttrMerger {
    fn find(&mut self, name: &str) -> Option<&mut Static> {
        self.statics
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s)
    }

    fn from_bindings(bindings: Vec<Binding>) -> LexResult<AttrMerger> {
        let mut m = AttrMerger::default();
        for b in bindings {
            m.add(vec![b.name], b.value, b.inherited)?;
        }
        Ok(m)
    }

    fn add(&mut self, path: Vec<AttrName>, value: Expr, inherited: Inherited) -> LexResult<()> {
        let mut it = path.into_iter();
        let head = it.next().expect("non-empty attribute path");
        let rest: Vec<AttrName> = it.collect();
        let (name, hpos) = match head {
            AttrName::Dynamic { .. } => {
                let value = if rest.is_empty() {
                    value
                } else {
                    wrap_path(rest, value)
                };
                self.dynamics.push(Binding {
                    name: head,
                    value,
                    inherited,
                });
                return Ok(());
            }
            AttrName::Static { ref name, ref pos } => (name.clone(), pos.clone()),
        };
        if rest.is_empty() {
            if let Some(existing) = self.find(&name) {
                // `a = { ... }; a = { ... };` merges two literal sets;
                // everything else is a redefinition.
                if let (Some(nested), ExprKind::Attrs { bindings, .. }, Inherited::No) =
                    (existing.nested.as_mut(), &value.kind, inherited)
                {
                    for b in bindings.clone() {
                        nested.add(vec![b.name], b.value, b.inherited)?;
                    }
                    return Ok(());
                }
                return fail(
                    format!(
                        "attribute '{name}' already defined at {}",
                        existing.binding.name.pos()
                    ),
                    hpos,
                );
            }
            let (nested, recursive) = match (&value.kind, inherited) {
                (
                    ExprKind::Attrs {
                        bindings,
                        recursive,
                    },
                    Inherited::No,
                ) => (
                    Some(AttrMerger::from_bindings(bindings.clone())?),
                    *recursive,
                ),
                _ => (None, false),
            };
            self.statics.push((
                name,
                Static {
                    binding: Binding {
                        name: head,
                        value,
                        inherited,
                    },
                    nested,
                    recursive,
                },
            ));
            return Ok(());
        }
        match self.find(&name) {
            None => {
                let mut nested = AttrMerger::default();
                nested.add(rest, value, inherited)?;
                let placeholder = Binding {
                    name: head,
                    value: Expr::new(
                        hpos,
                        ExprKind::Attrs {
                            recursive: false,
                            bindings: vec![],
                        },
                    ),
                    inherited: Inherited::No,
                };
                self.statics.push((
                    name,
                    Static {
                        binding: placeholder,
                        nested: Some(nested),
                        recursive: false,
                    },
                ));
                Ok(())
            }
            Some(existing) => match existing.nested.as_mut() {
                None => fail(
                    format!(
                        "attribute '{name}' already defined at {}",
                        existing.binding.name.pos()
                    ),
                    hpos,
                ),
                Some(nested) => nested.add(rest, value, inherited),
            },
        }
    }

    fn finish(self) -> Vec<Binding> {
        let mut out = Vec::new();
        for (_, s) in self.statics {
            match s.nested {
                None => out.push(s.binding),
                Some(nested) => {
                    let pos = match &s.binding.value.kind {
                        ExprKind::Attrs { .. } => s.binding.value.pos.clone(),
                        _ => s.binding.name.pos().clone(),
                    };
                    out.push(Binding {
                        name: s.binding.name,
                        value: Expr::new(
                            pos,
                            ExprKind::Attrs {
                                recursive: s.recursive,
                                bindings: nested.finish(),
                            },
                        ),
                        inherited: s.binding.inherited,
                    });
                }
            }
        }
        out.extend(self.dynamics);
        out
    }
}

fn wrap_path(path: Vec<AttrName>, value: Expr) -> Expr {
    let mut it = path.into_iter();
    let Some(head) = it.next() else {
        return value;
    };
    let rest: Vec<AttrName> = it.collect();
    let pos = head.pos().clone();
    Expr::new(
        pos,
        ExprKind::Attrs {
            recursive: false,
            bindings: vec![Binding {
                name: head,
                value: wrap_path(rest, value),
                inherited: Inherited::No,
            }],
        },
    )
}

// A part of an indented string before stripping: text, an escape
// sequence's value, or an interpolation.
enum IndentedPart {
    Segment(IndentedSegment),
    Expr(Expr),
}

// Nix's stripIndentation, from parser.y: the least indentation of any line
// with content is removed from every line; a line that starts with an
// interpolation or an escape counts with the spaces before it; only
// spaces count, not tabs; and a final line of nothing but spaces is
// dropped. An escaped character ends a line's leading whitespace the way
// an interpolation does, and a newline it produces does not begin a line.
fn strip_indentation(parts: Vec<IndentedPart>) -> Vec<StrPart> {
    let is_text = |p: &IndentedPart| matches!(p, IndentedPart::Segment(s) if !s.escaped);
    let mut at_start = true;
    let mut min_indent = usize::MAX;
    let mut cur = 0;
    for part in &parts {
        let IndentedPart::Segment(seg) = part else {
            if at_start {
                at_start = false;
                min_indent = min_indent.min(cur);
            }
            continue;
        };
        if seg.escaped {
            if at_start {
                at_start = false;
                min_indent = min_indent.min(cur);
            }
            continue;
        }
        for c in seg.text.chars() {
            if at_start {
                if c == ' ' {
                    cur += 1;
                } else if c == '\n' {
                    cur = 0;
                } else {
                    at_start = false;
                    min_indent = min_indent.min(cur);
                }
            } else if c == '\n' {
                at_start = true;
                cur = 0;
            }
        }
    }

    let mut out: Vec<StrPart> = Vec::new();
    fn push_text(out: &mut Vec<StrPart>, t: &str) {
        if t.is_empty() {
            return;
        }
        if let Some(StrPart::Lit(last)) = out.last_mut() {
            last.push_str(t);
        } else {
            out.push(StrPart::Lit(t.to_string()));
        }
    }
    at_start = true;
    let mut dropped = 0;
    let n = parts.len();
    for (i, part) in parts.into_iter().enumerate() {
        if !is_text(&part) {
            at_start = false;
            dropped = 0;
            match part {
                IndentedPart::Segment(s) => push_text(&mut out, &s.text),
                IndentedPart::Expr(e) => out.push(StrPart::Expr(e)),
            }
            continue;
        }
        let IndentedPart::Segment(seg) = part else {
            unreachable!()
        };
        let mut s = String::new();
        for c in seg.text.chars() {
            if at_start {
                if c == ' ' {
                    if dropped >= min_indent {
                        s.push(c);
                    }
                    dropped += 1;
                } else if c == '\n' {
                    dropped = 0;
                    s.push(c);
                } else {
                    at_start = false;
                    dropped = 0;
                    s.push(c);
                }
            } else {
                s.push(c);
                if c == '\n' {
                    at_start = true;
                }
            }
        }
        if i + 1 == n {
            if let Some(p) = s.rfind('\n') {
                if s[p + 1..].bytes().all(|b| b == b' ') {
                    s.truncate(p + 1);
                }
            }
        }
        push_text(&mut out, &s);
    }
    out
}

fn describe(t: &Token) -> String {
    match t.kind {
        TokenKind::Eof => "end of file".into(),
        TokenKind::Op => format!("'{}'", t.text),
        TokenKind::Keyword => format!("keyword '{}'", t.text),
        TokenKind::StringStart => "a string".into(),
        TokenKind::IndentStart => "an indented string".into(),
        TokenKind::PathStart => "a path".into(),
        TokenKind::Id => format!("id '{}'", t.text),
        TokenKind::Int => format!("int '{}'", t.text),
        TokenKind::Float => format!("float '{}'", t.text),
        TokenKind::Uri => format!("uri '{}'", t.text),
        TokenKind::SPath => format!("spath '{}'", t.text),
    }
}
