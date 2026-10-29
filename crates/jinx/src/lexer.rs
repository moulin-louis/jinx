// The Nix lexer, hand-written after Nix's lexer.l: the same token classes
// and the same longest-match decisions (`x:y` is a URI, `a-b` is one
// identifier, `<b>` is a search path).
//
// Strings and paths are not tokenised here. They contain expressions, so
// the parser drives them: it consumes the opening quote as a token and then
// asks the lexer to scan literal text up to the next `${` or closing
// delimiter, parses the expression itself, and asks again. So the lexer
// keeps one token of lookahead and the parser never peeks past a `}`.

use crate::ast::Pos;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Id,
    Int,
    Float,
    Uri,
    SPath,
    PathStart,
    StringStart,
    IndentStart,
    Keyword,
    Op,
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokenKind,
    // The identifier, keyword, operator, or the literal's source text.
    pub text: String,
    pub pos: Pos,
}

#[derive(Debug)]
pub struct SyntaxError {
    pub message: String,
    pub pos: Pos,
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.pos, self.message)
    }
}

pub type LexResult<T> = Result<T, SyntaxError>;

// A piece of an indented string: literal text, or an escape sequence's
// value, which the indentation rules treat differently.
#[derive(Clone, Debug)]
pub struct IndentedSegment {
    pub text: String,
    pub escaped: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Interpolation,
    End,
}

const KEYWORDS: &[&str] = &[
    "if", "then", "else", "assert", "with", "let", "in", "rec", "inherit", "or",
];

// Longest first, since the scanner takes the first that matches.
const OPERATORS: &[&str] = &[
    "...", "==", "!=", "<=", ">=", "&&", "||", "->", "//", "++", "${", "|>", "<|", ".", ";", ",",
    "(", ")", "{", "}", "[", "]", "=", ":", "?", "!", "-", "+", "*", "/", "<", ">", "@",
];

fn is_path_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'+')
}

fn is_uri_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"%/?:@&=+$,-_.!~*'".contains(&c)
}

pub struct Lexer {
    src: Vec<u8>,
    pos: usize,
    line: u32,
    line_start: usize,
    lookahead: Option<Token>,
    pub file: Rc<str>,
}

impl Lexer {
    pub fn new(src: &str, file: &str) -> Self {
        Lexer {
            src: src.as_bytes().to_vec(),
            pos: 0,
            line: 1,
            line_start: 0,
            lookahead: None,
            file: file.into(),
        }
    }

    // The position of the next unread character.
    pub fn here(&self) -> Pos {
        Pos {
            file: self.file.clone(),
            line: self.line,
            column: (self.pos - self.line_start + 1) as u32,
        }
    }

    pub fn peek(&mut self) -> LexResult<&Token> {
        if self.lookahead.is_none() {
            let t = self.scan()?;
            self.lookahead = Some(t);
        }
        Ok(self.lookahead.as_ref().unwrap())
    }

    pub fn next(&mut self) -> LexResult<Token> {
        self.peek()?;
        Ok(self.lookahead.take().unwrap())
    }

    fn fail<T>(&self, message: impl Into<String>, pos: Pos) -> LexResult<T> {
        Err(SyntaxError {
            message: message.into(),
            pos,
        })
    }

    fn at(&self, i: usize) -> Option<u8> {
        self.src.get(i).copied()
    }

    fn starts_with(&self, i: usize, s: &str) -> bool {
        self.src[i.min(self.src.len())..].starts_with(s.as_bytes())
    }

    fn advance(&mut self, n: usize) {
        for _ in 0..n {
            if self.at(self.pos) == Some(b'\n') {
                self.line += 1;
                self.line_start = self.pos + 1;
            }
            self.pos += 1;
        }
    }

    fn text(&self, len: usize) -> String {
        String::from_utf8_lossy(&self.src[self.pos..self.pos + len]).into_owned()
    }

    fn skip_trivia(&mut self) -> LexResult<()> {
        loop {
            match self.at(self.pos) {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.advance(1),
                Some(b'#') => {
                    while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.at(self.pos + 1) == Some(b'*') => {
                    let start = self.here();
                    let rest = &self.src[self.pos + 2..];
                    let Some(end) = rest.windows(2).position(|w| w == b"*/") else {
                        return self.fail("unterminated comment", start);
                    };
                    self.advance(end + 4);
                }
                _ => return Ok(()),
            }
        }
    }

    // ---- the token classes, as lengths of a match at `i` ----------------------

    fn run(&self, mut i: usize, pred: impl Fn(u8) -> bool) -> usize {
        while self.at(i).is_some_and(&pred) {
            i += 1;
        }
        i
    }

    fn match_id(&self, i: usize) -> Option<usize> {
        let c = self.at(i)?;
        if !(c.is_ascii_alphabetic() || c == b'_') {
            return None;
        }
        let end = self.run(i + 1, |c| {
            c.is_ascii_alphanumeric() || matches!(c, b'_' | b'\'' | b'-')
        });
        Some(end - i)
    }

    fn match_int(&self, i: usize) -> Option<usize> {
        let end = self.run(i, |c| c.is_ascii_digit());
        (end > i).then_some(end - i)
    }

    // (([1-9][0-9]*\.[0-9]*)|(0?\.[0-9]+))([Ee][+-]?[0-9]+)?
    fn match_float(&self, i: usize) -> Option<usize> {
        let mut j;
        if self.at(i).is_some_and(|c| (b'1'..=b'9').contains(&c)) {
            j = self.run(i + 1, |c| c.is_ascii_digit());
            if self.at(j) != Some(b'.') {
                return self.match_float_alt(i);
            }
            j = self.run(j + 1, |c| c.is_ascii_digit());
        } else {
            return self.match_float_alt(i);
        }
        Some(self.exponent(j) - i)
    }

    fn match_float_alt(&self, i: usize) -> Option<usize> {
        let mut j = i;
        if self.at(j) == Some(b'0') {
            j += 1;
        }
        if self.at(j) != Some(b'.') {
            return None;
        }
        let end = self.run(j + 1, |c| c.is_ascii_digit());
        if end == j + 1 {
            return None;
        }
        Some(self.exponent(end) - i)
    }

    fn exponent(&self, j: usize) -> usize {
        if matches!(self.at(j), Some(b'e' | b'E')) {
            let mut k = j + 1;
            if matches!(self.at(k), Some(b'+' | b'-')) {
                k += 1;
            }
            let end = self.run(k, |c| c.is_ascii_digit());
            if end > k {
                return end;
            }
        }
        j
    }

    // [a-zA-Z][a-zA-Z0-9+\-.]*:[a-zA-Z0-9%/?:@&=+$,\-_.!~*']+
    fn match_uri(&self, i: usize) -> Option<usize> {
        if !self.at(i)?.is_ascii_alphabetic() {
            return None;
        }
        let j = self.run(i + 1, |c| {
            c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.')
        });
        if self.at(j) != Some(b':') {
            return None;
        }
        let end = self.run(j + 1, is_uri_char);
        (end > j + 1).then_some(end - i)
    }

    // <[pc]+(\/[pc]+)*>
    fn match_spath(&self, i: usize) -> Option<usize> {
        if self.at(i) != Some(b'<') {
            return None;
        }
        let mut j = self.run(i + 1, is_path_char);
        if j == i + 1 {
            return None;
        }
        while self.at(j) == Some(b'/') {
            let e = self.run(j + 1, is_path_char);
            if e == j + 1 {
                return None;
            }
            j = e;
        }
        (self.at(j) == Some(b'>')).then_some(j + 1 - i)
    }

    // (\/[pc]+)+ from j, at least one segment; then an optional `/`.
    fn segments(&self, mut j: usize) -> Option<usize> {
        let mut any = false;
        while self.at(j) == Some(b'/') {
            let e = self.run(j + 1, is_path_char);
            if e == j + 1 {
                break;
            }
            j = e;
            any = true;
        }
        if !any {
            return None;
        }
        if self.at(j) == Some(b'/') {
            j += 1;
        }
        Some(j)
    }

    // [pc]*(\/[pc]+)+\/?
    fn match_path(&self, i: usize) -> Option<usize> {
        let j = self.run(i, is_path_char);
        Some(self.segments(j)? - i)
    }

    // ~(\/[pc]+)+\/?
    fn match_hpath(&self, i: usize) -> Option<usize> {
        if self.at(i) != Some(b'~') {
            return None;
        }
        Some(self.segments(i + 1)? - i)
    }

    // [pc]*\/
    fn match_path_seg(&self, i: usize) -> Option<usize> {
        let j = self.run(i, is_path_char);
        (self.at(j) == Some(b'/')).then_some(j + 1 - i)
    }

    // ~\/
    fn match_hpath_start(&self, i: usize) -> Option<usize> {
        self.starts_with(i, "~/").then_some(2)
    }

    // [pc]+(\/[pc]+)*\/? | (\/[pc]+)+\/?
    fn match_path_cont(&self, i: usize) -> Option<usize> {
        let j = self.run(i, is_path_char);
        if j > i {
            let mut k = j;
            while self.at(k) == Some(b'/') {
                let e = self.run(k + 1, is_path_char);
                if e == k + 1 {
                    break;
                }
                k = e;
            }
            if self.at(k) == Some(b'/') {
                k += 1;
            }
            return Some(k - i);
        }
        if self.at(i) == Some(b'/') && self.starts_with(i + 1, "${") {
            return Some(1);
        }
        Some(self.segments(i)? - i)
    }

    fn scan(&mut self) -> LexResult<Token> {
        self.skip_trivia()?;
        let pos = self.here();
        let Some(c) = self.at(self.pos) else {
            return Ok(Token {
                kind: TokenKind::Eof,
                text: String::new(),
                pos,
            });
        };
        if c == b'"' {
            self.advance(1);
            return Ok(Token {
                kind: TokenKind::StringStart,
                text: "\"".into(),
                pos,
            });
        }
        if c == b'\'' && self.at(self.pos + 1) == Some(b'\'') {
            self.advance(2);
            // The opening quotes swallow the rest of their line when it is
            // blank, as Nix's lexer rule `''( *\n)?` does.
            let spaces = self.run(self.pos, |c| c == b' ');
            if self.at(spaces) == Some(b'\n') {
                self.advance(spaces + 1 - self.pos);
            }
            return Ok(Token {
                kind: TokenKind::IndentStart,
                text: "''".into(),
                pos,
            });
        }

        // Everything that starts with a path, number, identifier or URI
        // character competes on length, as flex would decide it.
        let i = self.pos;
        let mut best: Option<(TokenKind, usize)> = None;
        fn consider(best: &mut Option<(TokenKind, usize)>, kind: TokenKind, len: Option<usize>) {
            if let Some(l) = len {
                if l > best.map_or(0, |b| b.1) {
                    *best = Some((kind, l));
                }
            }
        }
        consider(&mut best, TokenKind::Id, self.match_id(i));
        consider(&mut best, TokenKind::Int, self.match_int(i));
        consider(&mut best, TokenKind::Float, self.match_float(i));
        consider(&mut best, TokenKind::Uri, self.match_uri(i));
        consider(&mut best, TokenKind::SPath, self.match_spath(i));
        let path = self.match_path(i).or_else(|| self.match_hpath(i));
        consider(&mut best, TokenKind::PathStart, path);
        // `./${x}` and `~/${x}`: a bare segment followed by an
        // interpolation starts a path too.
        if best.map(|b| b.0) != Some(TokenKind::PathStart) {
            if let Some(seg) = self.match_path_seg(i).or_else(|| self.match_hpath_start(i)) {
                if self.starts_with(i + seg, "${") {
                    consider(&mut best, TokenKind::PathStart, Some(seg));
                }
            }
        }
        if let Some((kind, len)) = best {
            let text = self.text(len);
            if kind == TokenKind::PathStart {
                // The parser scans the segments; the token only announces
                // the path. A trailing slash is only legal before an
                // interpolation.
                if text.ends_with('/') && !self.starts_with(i + len, "${") {
                    return self.fail("path has a trailing slash", pos);
                }
                return Ok(Token { kind, text, pos });
            }
            self.advance(len);
            if kind == TokenKind::Id && KEYWORDS.contains(&text.as_str()) {
                return Ok(Token {
                    kind: TokenKind::Keyword,
                    text,
                    pos,
                });
            }
            return Ok(Token { kind, text, pos });
        }

        for op in OPERATORS {
            if self.starts_with(self.pos, op) {
                self.advance(op.len());
                return Ok(Token {
                    kind: TokenKind::Op,
                    text: op.to_string(),
                    pos,
                });
            }
        }
        let ch = String::from_utf8_lossy(&self.src[self.pos..])
            .chars()
            .next()
            .unwrap_or('?');
        self.fail(format!("unexpected character '{ch}'"), pos)
    }

    // ---- strings, driven by the parser -------------------------------------

    // Scan literal text of a `"` string up to an interpolation or the
    // closing quote. The lookahead must be empty, which the parser
    // guarantees by consuming the opening token with next().
    pub fn scan_string(&mut self) -> LexResult<(String, Stop)> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.at(self.pos) else {
                return self.fail("unterminated string", self.here());
            };
            match c {
                b'"' => {
                    self.advance(1);
                    return Ok((String::from_utf8_lossy(&out).into_owned(), Stop::End));
                }
                b'\\' => {
                    let Some(e) = self.at(self.pos + 1) else {
                        return self.fail("unterminated string", self.here());
                    };
                    out.push(unescape(e));
                    self.advance(2);
                }
                b'$' if self.at(self.pos + 1) == Some(b'{') => {
                    self.advance(2);
                    return Ok((
                        String::from_utf8_lossy(&out).into_owned(),
                        Stop::Interpolation,
                    ));
                }
                // `$$` is two literal dollars; it is how `$${` is written.
                b'$' if self.at(self.pos + 1) == Some(b'$') => {
                    out.extend_from_slice(b"$$");
                    self.advance(2);
                }
                c => {
                    out.push(c);
                    self.advance(1);
                }
            }
        }
    }

    // The same for '' strings, as segments: an escape (three quotes, ''$,
    // ''\n) is its own segment, because Nix's indentation stripping treats
    // an escaped character like an interpolation rather than like text.
    pub fn scan_indented_string(&mut self) -> LexResult<(Vec<IndentedSegment>, Stop)> {
        let mut segments = Vec::new();
        let mut text: Vec<u8> = Vec::new();
        fn flush(text: &mut Vec<u8>, segments: &mut Vec<IndentedSegment>) {
            if !text.is_empty() {
                segments.push(IndentedSegment {
                    text: String::from_utf8_lossy(text).into_owned(),
                    escaped: false,
                });
                text.clear();
            }
        }
        loop {
            let Some(c) = self.at(self.pos) else {
                return self.fail("unterminated indented string", self.here());
            };
            if c == b'\'' && self.at(self.pos + 1) == Some(b'\'') {
                let escaped = match self.at(self.pos + 2) {
                    Some(b'$') => Some(("$".to_string(), 3)),
                    Some(b'\'') => Some(("''".to_string(), 3)),
                    Some(b'\\') => {
                        let Some(f) = self.at(self.pos + 3) else {
                            return self.fail("unterminated indented string", self.here());
                        };
                        Some(((unescape(f) as char).to_string(), 4))
                    }
                    _ => None,
                };
                flush(&mut text, &mut segments);
                match escaped {
                    Some((t, len)) => {
                        segments.push(IndentedSegment {
                            text: t,
                            escaped: true,
                        });
                        self.advance(len);
                        continue;
                    }
                    None => {
                        self.advance(2);
                        return Ok((segments, Stop::End));
                    }
                }
            }
            if c == b'$' && self.at(self.pos + 1) == Some(b'{') {
                self.advance(2);
                flush(&mut text, &mut segments);
                return Ok((segments, Stop::Interpolation));
            }
            if c == b'$' && self.at(self.pos + 1) == Some(b'$') {
                text.extend_from_slice(b"$$");
                self.advance(2);
                continue;
            }
            text.push(c);
            self.advance(1);
        }
    }

    // After a path-start token (whose text the lexer has not consumed), or
    // after an interpolation inside a path: the next literal run of path
    // characters, and whether an interpolation follows.
    pub fn scan_path(&mut self, first: bool) -> LexResult<(String, Stop)> {
        let i = self.pos;
        let len = if first {
            self.match_path(i)
                .or_else(|| self.match_path_seg(i))
                .or_else(|| self.match_hpath(i))
                .or_else(|| self.match_hpath_start(i))
        } else {
            self.match_path_cont(i)
        }
        .unwrap_or(0);
        let text = self.text(len);
        self.advance(len);
        if self.starts_with(self.pos, "${") {
            self.advance(2);
            return Ok((text, Stop::Interpolation));
        }
        if text.ends_with('/') {
            return self.fail("path has a trailing slash", self.here());
        }
        Ok((text, Stop::End))
    }

    // With a `{` in the lookahead: whether its matching `}` is followed by
    // `:` or `@`, which makes the braces a lambda's formals rather than an
    // attribute set. A scan over the source text that only tracks nesting
    // and skips strings and comments; nothing is tokenised.
    pub fn braces_followed_by_colon_or_at(&self) -> bool {
        #[derive(Clone, Copy)]
        enum Frame {
            Code(u32),
            Str,
            Indented,
        }
        let src = &self.src;
        let mut frames = vec![Frame::Code(1)];
        let mut i = self.pos;
        let skip_comment_end = |i: usize| -> usize {
            match src[i + 2..].windows(2).position(|w| w == b"*/") {
                Some(e) => i + 2 + e + 2,
                None => src.len(),
            }
        };
        while i < src.len() {
            let c = src[i];
            let d = src.get(i + 1).copied();
            let top = frames.len() - 1;
            match frames[top] {
                Frame::Str => {
                    if c == b'\\' {
                        i += 2;
                    } else if c == b'$' && d == Some(b'{') {
                        frames.push(Frame::Code(1));
                        i += 2;
                    } else if c == b'"' {
                        frames.pop();
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                Frame::Indented => {
                    if c == b'\'' && d == Some(b'\'') {
                        match src.get(i + 2) {
                            Some(b'$' | b'\'') => i += 3,
                            Some(b'\\') => i += 4,
                            _ => {
                                frames.pop();
                                i += 2;
                            }
                        }
                    } else if c == b'$' && d == Some(b'{') {
                        frames.push(Frame::Code(1));
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Frame::Code(depth) => {
                    if c == b'#' {
                        while i < src.len() && src[i] != b'\n' {
                            i += 1;
                        }
                    } else if c == b'/' && d == Some(b'*') {
                        i = skip_comment_end(i);
                    } else if c == b'"' {
                        frames.push(Frame::Str);
                        i += 1;
                    } else if c == b'\'' && d == Some(b'\'') {
                        frames.push(Frame::Indented);
                        i += 2;
                    } else if matches!(c, b'{' | b'(' | b'[') {
                        frames[top] = Frame::Code(depth + 1);
                        i += 1;
                    } else if c == b'$' && d == Some(b'{') {
                        frames[top] = Frame::Code(depth + 1);
                        i += 2;
                    } else if matches!(c, b'}' | b')' | b']') {
                        i += 1;
                        if depth == 1 {
                            frames.pop();
                            if frames.is_empty() {
                                break;
                            }
                        } else {
                            frames[top] = Frame::Code(depth - 1);
                        }
                    } else {
                        i += 1;
                    }
                }
            }
        }
        // Skip trivia after the closing brace.
        loop {
            match src.get(i) {
                Some(b' ' | b'\t' | b'\r' | b'\n') => i += 1,
                Some(b'#') => {
                    while i < src.len() && src[i] != b'\n' {
                        i += 1;
                    }
                }
                Some(b'/') if src.get(i + 1) == Some(&b'*') => i = skip_comment_end(i),
                _ => break,
            }
        }
        matches!(src.get(i), Some(b':' | b'@'))
    }
}

fn unescape(c: u8) -> u8 {
    match c {
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        c => c,
    }
}
