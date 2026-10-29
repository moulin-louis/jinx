// Regular expressions for builtins.match and builtins.split.
//
// Nix compiles a pattern with std::regex under std::regex::extended, so the
// grammar is POSIX ERE, and runs libstdc++'s backtracking executor:
// alternatives are tried left to right and quantifiers are greedy. The
// regex crate's leftmost-first semantics pick the same match, so this
// module parses the ERE, translates it into the regex crate's syntax, and
// lets that engine run it, on bytes, as Nix's matcher does.
//
// Known deviations, as in the TypeScript runtime: captures inside a
// repeated group report the last iteration that took part, and split takes
// the first alternative rather than the longest.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use regex::bytes::Regex;

use crate::value::Result;

pub struct Compiled {
    pub match_re: Regex,
    pub split_re: Regex,
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Rc<Compiled>>> = RefCell::new(HashMap::new());
}

// The characters `\` may precede in an ERE; libstdc++ rejects every other
// escape.
const ESCAPABLE: &[u8] = b"()*+?|^$.[\\{";

fn invalid(pattern: &str) -> Box<crate::value::NixError> {
    crate::value::NixError::boxed(format!("invalid regular expression '{pattern}'"))
}

fn push_byte(out: &mut String, b: u8) {
    if b.is_ascii_alphanumeric() || b == b'_' {
        out.push(b as char);
    } else {
        out.push_str(&format!("\\x{b:02x}"));
    }
}

// Translate an ERE into the regex crate's syntax, byte by byte.
fn translate(pattern: &str) -> Result<String> {
    let p = pattern.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut depth = 0usize;
    while i < p.len() {
        let c = p[i];
        match c {
            b'\\' => {
                let Some(&e) = p.get(i + 1) else {
                    return Err(invalid(pattern));
                };
                if !ESCAPABLE.contains(&e) {
                    return Err(invalid(pattern));
                }
                push_byte(&mut out, e);
                i += 2;
            }
            b'.' => {
                out.push('.');
                i += 1;
            }
            b'^' => {
                out.push('^');
                i += 1;
            }
            b'$' => {
                out.push('$');
                i += 1;
            }
            b'(' => {
                depth += 1;
                out.push('(');
                i += 1;
            }
            b')' => {
                if depth == 0 {
                    return Err(invalid(pattern));
                }
                depth -= 1;
                out.push(')');
                i += 1;
            }
            b'|' | b'*' | b'+' | b'?' => {
                out.push(c as char);
                i += 1;
            }
            b'{' => {
                let end = p[i..]
                    .iter()
                    .position(|&b| b == b'}')
                    .ok_or_else(|| invalid(pattern))?;
                let body = &pattern[i + 1..i + end];
                if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit() || b == b',') {
                    return Err(invalid(pattern));
                }
                out.push('{');
                out.push_str(body);
                out.push('}');
                i += end + 1;
            }
            b'[' => {
                i = translate_bracket(pattern, i, &mut out)?;
            }
            _ => {
                push_byte(&mut out, c);
                i += 1;
            }
        }
    }
    if depth != 0 {
        return Err(invalid(pattern));
    }
    Ok(out)
}

const CLASSES: &[&str] = &[
    "alnum", "alpha", "blank", "cntrl", "digit", "graph", "lower", "print", "punct", "space",
    "upper", "xdigit",
];

// A bracket expression starting at `start` (the `[`). Returns the index
// after its `]`. A `]` first in the set is literal, and so is a backslash.
fn translate_bracket(pattern: &str, start: usize, out: &mut String) -> Result<usize> {
    let p = pattern.as_bytes();
    let mut i = start + 1;
    out.push('[');
    if p.get(i) == Some(&b'^') {
        out.push('^');
        i += 1;
    }
    let mut first = true;
    loop {
        let Some(&c) = p.get(i) else {
            return Err(invalid(pattern));
        };
        if c == b']' && !first {
            out.push(']');
            return Ok(i + 1);
        }
        first = false;
        if c == b'[' && p.get(i + 1) == Some(&b':') {
            let rest = &pattern[i + 2..];
            let end = rest.find(":]").ok_or_else(|| invalid(pattern))?;
            let name = &rest[..end];
            if !CLASSES.contains(&name) {
                return Err(invalid(pattern));
            }
            out.push_str(&format!("[:{name}:]"));
            i += 2 + end + 2;
            continue;
        }
        // A range a-z, unless the `-` is last.
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|&b| b != b']') {
            let hi = p[i + 2];
            if hi < c {
                return Err(invalid(pattern));
            }
            push_byte(out, c);
            out.push('-');
            push_byte(out, hi);
            i += 3;
            continue;
        }
        push_byte(out, c);
        i += 1;
    }
}

pub fn compile(pattern: &str) -> Result<Rc<Compiled>> {
    if let Some(c) = CACHE.with(|c| c.borrow().get(pattern).cloned()) {
        return Ok(c);
    }
    let source = translate(pattern)?;
    // `.` matches a newline in POSIX; bytes, not code points, as in Nix.
    let build = |s: String| Regex::new(&s).map_err(|_| invalid(pattern));
    let compiled = Rc::new(Compiled {
        match_re: build(format!(r"(?s-u)\A(?:{source})\z"))?,
        split_re: build(format!(r"(?s-u){source}"))?,
    });
    CACHE.with(|c| c.borrow_mut().insert(pattern.to_string(), compiled.clone()));
    Ok(compiled)
}

pub type Groups = Vec<Option<Vec<u8>>>;

fn groups_of(caps: &regex::bytes::Captures) -> Groups {
    caps.iter()
        .skip(1)
        .map(|m| m.map(|m| m.as_bytes().to_vec()))
        .collect()
}

// builtins.match: the whole string must match.
pub fn ere_match(pattern: &str, s: &[u8]) -> Result<Option<Groups>> {
    let re = compile(pattern)?;
    Ok(re.match_re.captures(s).map(|c| groups_of(&c)))
}

pub enum SplitPart {
    Text(Vec<u8>),
    Groups(Groups),
}

// builtins.split: the text before each match, that match's groups, and the
// text after the last match.
pub fn ere_split(pattern: &str, s: &[u8]) -> Result<Vec<SplitPart>> {
    let re = compile(pattern)?;
    let mut result = Vec::new();
    let mut prefix_start = 0;
    let mut search_from = 0;
    while search_from <= s.len() {
        let Some(caps) = re.split_re.captures_at(s, search_from) else {
            break;
        };
        let m = caps.get(0).unwrap();
        result.push(SplitPart::Text(s[prefix_start..m.start()].to_vec()));
        result.push(SplitPart::Groups(groups_of(&caps)));
        prefix_start = m.end();
        // libstdc++'s regex_iterator steps past an empty match by one
        // character, and stops once it sits at the end of the input.
        if m.start() == m.end() {
            if m.start() >= s.len() {
                break;
            }
            search_from = m.start() + 1;
            continue;
        }
        search_from = m.end();
    }
    result.push(SplitPart::Text(s[prefix_start..].to_vec()));
    Ok(result)
}
