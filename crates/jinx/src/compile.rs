// The compiler driver: Nix files to a Cargo package that evaluates them.
//
// A program is the set of files reachable from its entry points through
// literal imports. Each file becomes one module of the package
// (`src/m_<name>_<hash>.rs`, exporting `value()`), and `src/main.rs` lists
// them by absolute source path so the runtime's `import` can find them.
// The package depends on the jinx-runtime crate of this checkout and is
// built with Cargo; a shared target directory keeps the runtime compiled
// once.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::emit::emit_module;
use crate::parser::parse;

pub struct Module {
    // The Nix file, absolute and canonical.
    pub file: String,
    // The Rust module it compiles to.
    pub name: String,
    pub rust: String,
}

// Lexical canonicalisation, the same as the runtime's canon_path, so that
// the paths compiled in match the paths `import` asks for.
pub fn canon(p: &str) -> String {
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
    format!("/{}", out.join("/"))
}

pub fn absolute(p: &str) -> String {
    if p.starts_with('/') {
        return canon(p);
    }
    let cwd = std::env::current_dir().expect("current directory");
    canon(&format!("{}/{p}", cwd.display()))
}

// The .nix file a path names: a directory means its default.nix.
fn nix_file(p: &str) -> String {
    if Path::new(p).is_dir() {
        format!("{}/default.nix", p.trim_end_matches('/'))
    } else {
        p.to_string()
    }
}

fn short_hash(s: &str) -> String {
    let h = Sha256::digest(s.as_bytes());
    h.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

fn module_name(file: &str) -> String {
    let base = file.rsplit('/').next().unwrap_or(file);
    let stem: String = base
        .trim_end_matches(".nix")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("m_{stem}_{}", short_hash(file))
}

pub fn compile_file(file: &str) -> Result<(Module, Vec<String>), String> {
    let src = fs::read_to_string(file).map_err(|e| format!("reading '{file}': {e}"))?;
    let ast = parse(&src, file).map_err(|e| e.to_string())?;
    let emitted = emit_module(&ast, file).map_err(|e| e.to_string())?;
    let imports = emitted
        .imports
        .iter()
        .map(|p| nix_file(&canon(p)))
        .collect();
    Ok((
        Module {
            file: file.to_string(),
            name: module_name(file),
            rust: emitted.rust,
        },
        imports,
    ))
}

pub struct Compiled {
    pub modules: Vec<Module>,
    // Files that did not compile, with the error. An import of one fails
    // at run time, as an import of a file with a syntax error does in Nix.
    pub failed: BTreeMap<String, String>,
}

// Compile the entry files and every file they import with a literal path.
// An imported file that does not exist is skipped: the import fails when
// it is evaluated, if it ever is.
pub fn compile_closure(entries: &[String]) -> Compiled {
    let mut modules = Vec::new();
    let mut failed = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut queue: VecDeque<String> = entries.iter().map(|e| nix_file(&absolute(e))).collect();
    while let Some(file) = queue.pop_front() {
        if !seen.insert(file.clone()) {
            continue;
        }
        if !Path::new(&file).is_file() {
            continue;
        }
        match compile_file(&file) {
            Ok((m, imports)) => {
                modules.push(m);
                queue.extend(imports);
            }
            Err(e) => {
                failed.insert(file, e);
            }
        }
    }
    Compiled { modules, failed }
}

// Where the runtime crate is: JINX_RUNTIME, or next to this compiler's
// sources.
pub fn runtime_dir() -> PathBuf {
    if let Ok(p) = std::env::var("JINX_RUNTIME") {
        return PathBuf::from(p);
    }
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("../jinx-runtime");
    here.canonicalize().unwrap_or(here)
}

fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    fs::write(path, contents)
}

// Format a generated module with rustfmt when it is installed; the output
// is the same program either way. rustfmt can take minutes on deeply
// nested code, so it gets a few seconds and the file stays as it is when
// it runs out.
fn rustfmt(path: &Path) {
    let _ = Command::new("timeout")
        .args(["5", "rustfmt", "--edition", "2021"])
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status();
}

// Write the Cargo package for `modules` into `dir`.
// With `format`, each module is run through rustfmt, for reading.
pub fn write_package(
    dir: &Path,
    package: &str,
    modules: &[Module],
    format: bool,
) -> std::io::Result<()> {
    let src = dir.join("src");
    fs::create_dir_all(&src)?;
    let runtime = runtime_dir();
    let cargo = format!(
        r#"# Generated by jinx. Do not edit.
[package]
name = "{package}"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
jinx-runtime = {{ path = {runtime:?} }}

# The generated code is large and simple; the runtime is where the time goes.
[profile.dev]
opt-level = 0
debug = false

[profile.dev.package.jinx-runtime]
opt-level = 3

[workspace]
"#,
        runtime = runtime.display().to_string()
    );
    write_if_changed(&dir.join("Cargo.toml"), &cargo)?;
    let mut main = String::from(
        "// Generated by jinx. Do not edit.\n\n#![allow(unused, non_snake_case, clippy::all)]\n\n",
    );
    for m in modules {
        main.push_str(&format!("mod {};\n", m.name));
    }
    main.push_str("\nstatic MODULES: &[(&str, jinx_runtime::modules::ModuleFn)] = &[\n");
    for m in modules {
        main.push_str(&format!("    ({:?}, {}::value),\n", m.file, m.name));
    }
    main.push_str("];\n\nfn main() {\n    jinx_runtime::main(MODULES)\n}\n");
    write_if_changed(&src.join("main.rs"), &main)?;
    for m in modules {
        let path = src.join(format!("{}.rs", m.name));
        // rustfmt rewrites the file, so compare against a formatted copy
        // to keep an unchanged module's mtime, and Cargo's cache, intact.
        let stamp = src.join(format!("{}.rs.src", m.name));
        if fs::read_to_string(&stamp).ok().as_deref() == Some(m.rust.as_str()) && path.exists() {
            continue;
        }
        fs::write(&path, &m.rust)?;
        if format {
            rustfmt(&path);
        }
        fs::write(&stamp, &m.rust)?;
    }
    Ok(())
}

// Build the package and return the executable.
pub fn build_package(dir: &Path, package: &str, target: &Path) -> Result<PathBuf, String> {
    let status = Command::new("cargo")
        .args(["build", "--quiet", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", target)
        .status()
        .map_err(|e| format!("running cargo: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build of {} failed", dir.display()));
    }
    Ok(target.join("debug").join(package))
}

pub fn cache_root() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".cache")
        });
    base.join("jinx")
}

// A package name for a set of entry files, stable across runs so the
// package is rebuilt incrementally.
pub fn package_name(entries: &[String]) -> String {
    format!("jinx-{}", short_hash(&entries.join("\0")))
}
