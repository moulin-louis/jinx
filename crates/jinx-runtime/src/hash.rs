// Hashes as Nix spells them: base16, nix32, base64 and SRI, with or
// without an `algo:` prefix (libutil/hash.cc). The encodings are
// data-encoding's, except nix32, whose bit order is Nix's own (store.rs).

use std::sync::LazyLock;

use data_encoding::{Encoding, Specification, BASE64, HEXLOWER, HEXLOWER_PERMISSIVE};

use crate::store::{nix32_decode, nix32_encode};
use crate::value::{err, Result};

pub fn size_of(algo: &str) -> Option<usize> {
    Some(match algo {
        "md5" => 16,
        "sha1" => 20,
        "sha256" => 32,
        "sha512" => 64,
        _ => return None,
    })
}

pub fn hex(bytes: &[u8]) -> String {
    HEXLOWER.encode(bytes)
}

pub fn base64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

// Nix's base64Decode is lenient: it stops at the first `=`, skips
// newlines, and ignores leftover bits. nixpkgs has a hash with junk after
// its padding that Nix accepts.
static BASE64_LENIENT: LazyLock<Encoding> = LazyLock::new(|| {
    let mut spec = Specification::new();
    spec.symbols
        .push_str("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/");
    spec.check_trailing_bits = false;
    spec.encoding().expect("base64 specification")
});

fn decode_base64(s: &str) -> Result<Vec<u8>> {
    let body: String = s
        .split('=')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|&c| c != '\n')
        .collect();
    // A partial final group of one character carries no whole byte.
    let usable = if body.len() % 4 == 1 {
        &body[..body.len() - 1]
    } else {
        &body[..]
    };
    BASE64_LENIENT
        .decode(usable.as_bytes())
        .or_else(|_| err(format!("invalid character in Base64 string: '{s}'")))
}

fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

fn nix32_len(n: usize) -> usize {
    (n * 8 - 1) / 5 + 1
}

// Parse a hash in any of Nix's spellings. `algo` is the algorithm the
// context says it must have, if any.
pub fn parse_hash(text: &str, algo: Option<&str>) -> Result<(String, Vec<u8>)> {
    if text.is_empty() {
        // The empty string is the all-zero hash (newHashAllowEmpty).
        let Some(a) = algo else {
            return err("empty hash requires explicit hash algorithm");
        };
        let size = size_of(a).ok_or_else(|| {
            crate::value::NixError::boxed(format!("unknown hash algorithm '{a}'"))
        })?;
        return Ok((a.to_string(), vec![0; size]));
    }
    let (prefix, rest, sri) = match (text.find(':'), text.find('-')) {
        (Some(i), _) => (Some(&text[..i]), &text[i + 1..], false),
        (None, Some(i)) => (Some(&text[..i]), &text[i + 1..], true),
        _ => (None, text, false),
    };
    if let Some(p) = prefix {
        if size_of(p).is_none() {
            return err(format!("unknown hash algorithm '{p}'"));
        }
    }
    let resolved = match (prefix, algo) {
        (None, None) => {
            return err(format!(
            "hash '{text}' does not include a type, nor is the type otherwise known from context"
        ))
        }
        (Some(p), Some(a)) if p != a => {
            return err(format!("hash '{text}' should have type '{a}'"))
        }
        (Some(p), _) => p,
        (None, Some(a)) => a,
    };
    let size = size_of(resolved).ok_or_else(|| {
        crate::value::NixError::boxed(format!("unknown hash algorithm '{resolved}'"))
    })?;
    let (bytes, encoding) = if sri {
        (decode_base64(rest)?, "SRI")
    } else if rest.len() == size * 2 {
        let b = HEXLOWER_PERMISSIVE
            .decode(rest.as_bytes())
            .or_else(|_| err(format!("invalid base16 hash '{rest}'")))?;
        (b, "base16")
    } else if rest.len() == nix32_len(size) {
        (nix32_decode(rest)?, "nix32")
    } else if rest.len() == base64_len(size) {
        (decode_base64(rest)?, "base64")
    } else {
        return err(format!(
            "hash '{rest}' has wrong length for hash algorithm '{resolved}'"
        ));
    };
    if bytes.len() != size {
        return err(format!(
            "invalid {encoding} hash '{rest}', length {} != expected length {size}",
            bytes.len()
        ));
    }
    Ok((resolved.to_string(), bytes))
}

// Write a hash in a format builtins.convertHash names.
pub fn format_hash(algo: &str, bytes: &[u8], format: &str) -> Result<String> {
    Ok(match format {
        "base16" => hex(bytes),
        "nix32" | "base32" => nix32_encode(bytes),
        "base64" => base64(bytes),
        "sri" => format!("{algo}-{}", base64(bytes)),
        _ => return err(format!("unknown hash format '{format}'")),
    })
}
