// The store as evaluation sees it: store paths are computed, never
// written. Copying a source path is hashing its NAR; toFile is hashing
// its text. The algorithms are Nix's (libstore/store-api.cc,
// libutil/archive.cc) to the byte, since the paths reach derivations.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;

use sha2::{Digest, Sha256};

use crate::ops::Coerced;
use crate::value::{err, Result};

pub const STORE_DIR: &str = "/nix/store";

const NIX32_ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

pub fn nix32_encode(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let len = (bytes.len() * 8 - 1) / 5 + 1;
    let mut out = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let low = bytes[i] as u32 >> j;
        let high = if i + 1 >= bytes.len() {
            0
        } else {
            (bytes[i + 1] as u32) << (8 - j)
        };
        out.push(NIX32_ALPHABET[((low | high) & 0x1f) as usize] as char);
    }
    out
}

pub fn nix32_decode(s: &str) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    for (n, ch) in s.bytes().rev().enumerate() {
        let Some(digit) = NIX32_ALPHABET.iter().position(|&c| c == ch) else {
            return err(format!(
                "invalid character in Nix32 (Nix's Base32 variation) string: '{}'",
                ch as char
            ));
        };
        let digit = digit as u32;
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        if out.len() < i + 1 {
            out.resize(i + 1, 0);
        }
        out[i] |= (digit << j) as u8;
        let carry = digit >> (8 - j);
        if carry != 0 {
            if out.len() < i + 2 {
                out.resize(i + 2, 0);
            }
            out[i + 1] |= carry as u8;
        }
    }
    let encoded_len = if out.is_empty() {
        0
    } else {
        (out.len() * 8 - 1) / 5 + 1
    };
    if encoded_len != s.len() {
        return err(format!("invalid nix32 string '{s}': wrong length"));
    }
    Ok(out)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

// XOR-fold a hash down to `size` bytes (Nix's compressHash).
fn compress_hash(h: &[u8], size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    for (i, b) in h.iter().enumerate() {
        out[i % size] ^= b;
    }
    out
}

// The characters a store path name may contain, and its length limit.
pub fn check_store_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return err("store path name is empty");
    }
    if name.len() > 211 {
        return err(format!(
            "store path name '{name}' is longer than 211 characters"
        ));
    }
    if name.starts_with('.') {
        return err(format!("store path name '{name}' starts with a period"));
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || "+-._?=".contains(*c)))
    {
        return err(format!(
            "store path name '{name}' contains illegal character '{c}'"
        ));
    }
    Ok(())
}

pub fn make_store_path(kind: &str, hash: &[u8], name: &str) -> Result<String> {
    check_store_name(name)?;
    let fingerprint = format!("{kind}:sha256:{}:{STORE_DIR}:{name}", hex(hash));
    let h = compress_hash(&sha256(fingerprint.as_bytes()), 20);
    Ok(format!("{STORE_DIR}/{}-{name}", nix32_encode(&h)))
}

pub fn make_text_path(
    name: &str,
    contents: &[u8],
    references: &BTreeSet<String>,
) -> Result<String> {
    // References sort by basename, which for store paths is the same as by
    // the whole path.
    let mut kind = String::from("text");
    for r in references {
        kind.push(':');
        kind.push_str(r);
    }
    make_store_path(&kind, &sha256(contents), name)
}

pub fn make_fixed_output_path(
    recursive: bool,
    algo: &str,
    hash: &[u8],
    name: &str,
) -> Result<String> {
    if recursive && algo == "sha256" {
        return make_store_path("source", hash, name);
    }
    let method = if recursive { "r:" } else { "" };
    let payload = format!("fixed:out:{method}{algo}:{}:", hex(hash));
    make_store_path("output:out", &sha256(payload.as_bytes()), name)
}

pub fn hash_placeholder(output: &str) -> String {
    format!(
        "/{}",
        nix32_encode(&sha256(format!("nix-output:{output}").as_bytes()))
    )
}

pub fn is_store_path(s: &str) -> bool {
    let Some(rest) = s.strip_prefix(STORE_DIR).and_then(|r| r.strip_prefix('/')) else {
        return false;
    };
    rest.len() > 33
        && rest.as_bytes()[32] == b'-'
        && !rest.contains('/')
        && rest[..32].bytes().all(|c| NIX32_ALPHABET.contains(&c))
}

// ---- NAR serialisation ------------------------------------------------------

// Decides, for each entry below the root, whether it is copied: the path
// and its type ("regular", "directory", "symlink", "unknown").
pub type PathFilter<'a> = &'a mut dyn FnMut(&str, &str) -> Result<bool>;

struct Nar {
    hasher: Sha256,
}

impl Nar {
    fn bytes(&mut self, b: &[u8]) {
        self.hasher.update((b.len() as u64).to_le_bytes());
        self.hasher.update(b);
        let pad = (8 - b.len() % 8) % 8;
        self.hasher.update(&[0u8; 8][..pad]);
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes())
    }
}

pub fn file_type(meta: &fs::Metadata) -> &'static str {
    let t = meta.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "directory"
    } else if t.is_file() {
        "regular"
    } else {
        "unknown"
    }
}

fn io_err<T>(path: &str, e: std::io::Error) -> Result<T> {
    err(format!("reading '{path}': {e}"))
}

fn dump_node(
    nar: &mut Nar,
    path: &str,
    meta: &fs::Metadata,
    filter: &mut Option<PathFilter>,
) -> Result<()> {
    nar.str("(");
    match file_type(meta) {
        "regular" => {
            nar.str("type");
            nar.str("regular");
            if meta.permissions().mode() & 0o111 != 0 {
                nar.str("executable");
                nar.str("");
            }
            nar.str("contents");
            let data = fs::read(path).or_else(|e| io_err(path, e))?;
            nar.bytes(&data);
        }
        "directory" => {
            nar.str("type");
            nar.str("directory");
            let mut names: Vec<Vec<u8>> = Vec::new();
            for entry in fs::read_dir(path).or_else(|e| io_err(path, e))? {
                let entry = entry.or_else(|e| io_err(path, e))?;
                names.push(entry.file_name().into_encoded_bytes());
            }
            names.sort();
            for name in names {
                let name = String::from_utf8_lossy(&name).into_owned();
                let child = format!("{path}/{name}");
                let cmeta = fs::symlink_metadata(&child).or_else(|e| io_err(&child, e))?;
                if let Some(f) = filter.as_mut() {
                    if !f(&child, file_type(&cmeta))? {
                        continue;
                    }
                }
                nar.str("entry");
                nar.str("(");
                nar.str("name");
                nar.str(&name);
                nar.str("node");
                dump_node(nar, &child, &cmeta, filter)?;
                nar.str(")");
            }
        }
        "symlink" => {
            nar.str("type");
            nar.str("symlink");
            nar.str("target");
            let target = fs::read_link(path).or_else(|e| io_err(path, e))?;
            nar.bytes(target.as_os_str().as_encoded_bytes());
        }
        _ => return err(format!("file '{path}' has an unsupported type")),
    }
    nar.str(")");
    Ok(())
}

pub fn nar_hash(path: &str, mut filter: Option<PathFilter>) -> Result<Vec<u8>> {
    let root = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    let meta =
        fs::symlink_metadata(root).or_else(|_| err(format!("path '{root}' does not exist")))?;
    let mut nar = Nar {
        hasher: Sha256::new(),
    };
    nar.str("nix-archive-1");
    dump_node(&mut nar, root, &meta, &mut filter)?;
    Ok(nar.hasher.finalize().to_vec())
}

// ---- adding things ----------------------------------------------------------

thread_local! {
    // A source path keeps its store path for the whole evaluation, as Nix's
    // srcToStore cache does.
    static SOURCES: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

pub fn base_name(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    match t.rfind('/') {
        Some(i) => &t[i + 1..],
        None => t,
    }
}

pub fn copy_path_to_store(path: &str) -> Result<Coerced> {
    copy_path_to_store_with(path, None, None)
}

pub fn copy_path_to_store_with(
    path: &str,
    name: Option<&str>,
    filter: Option<PathFilter>,
) -> Result<Coerced> {
    if path.ends_with(".drv") {
        return err(format!(
            "file names are not allowed to end in '.drv': '{path}'"
        ));
    }
    let cacheable = name.is_none() && filter.is_none();
    let cached = if cacheable {
        SOURCES.with(|s| s.borrow().get(path).cloned())
    } else {
        None
    };
    let store_path = match cached {
        Some(p) => p,
        None => {
            let n = name.unwrap_or_else(|| base_name(path));
            let p = make_fixed_output_path(true, "sha256", &nar_hash(path, filter)?, n)?;
            if cacheable {
                SOURCES.with(|s| s.borrow_mut().insert(path.to_string(), p.clone()));
            }
            p
        }
    };
    Ok(Coerced {
        bytes: store_path.clone().into_bytes(),
        context: Some(BTreeSet::from([store_path])),
    })
}
