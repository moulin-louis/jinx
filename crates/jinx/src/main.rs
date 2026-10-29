// jinx: Nix, transpiled to Rust.
//
//   jinx compile FILE.nix... [-o DIR]    write the Cargo package for FILEs
//   jinx eval FILE.nix [-A attr] [--system S] [--impure]
//                                        compile, build and run, printing
//                                        the value as nix-instantiate
//                                        --eval --strict does
//   jinx test-corpus DIR [--record] [--skip NAME.nix]
//                                        compare every DIR/*.nix with the
//                                        recorded output of Nix
//   jinx test-lang DIR [FILTER]          run Nix's eval-okay/eval-fail
//                                        language tests in DIR
//   jinx parse FILE.nix                  print the syntax tree

mod ast;
mod builtins_table;
mod compile;
mod corpus;
mod emit;
mod lexer;
mod parser;

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use compile::{absolute, build_package, cache_root, compile_closure, package_name, write_package};

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  jinx compile FILE.nix... [-o DIR]\n  jinx eval FILE.nix [-A attr] [--system SYSTEM] [--impure]\n  jinx test-corpus DIR\n  jinx parse FILE.nix"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        return usage();
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "compile" => cmd_compile(rest),
        "eval" => cmd_eval(rest),
        "test-corpus" => corpus::run(rest),
        "test-lang" => corpus::run_lang(rest),
        "parse" => cmd_parse(rest),
        _ => usage(),
    }
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

fn cmd_parse(args: &[String]) -> ExitCode {
    let Some(file) = args.first() else {
        return usage();
    };
    let file = absolute(file);
    let src = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => return fail(format!("reading '{file}': {e}")),
    };
    match parser::parse(&src, &file) {
        Ok(ast) => {
            println!("{ast:#?}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(e),
    }
}

fn cmd_compile(args: &[String]) -> ExitCode {
    let mut files = Vec::new();
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-o" {
            out = it.next().cloned();
        } else {
            files.push(absolute(a));
        }
    }
    if files.is_empty() {
        return usage();
    }
    let compiled = compile_closure(&files);
    if let Some((_, e)) = compiled.failed.iter().next() {
        return fail(e);
    }
    let package = package_name(&files);
    let dir = out
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("out").join(&package));
    if let Err(e) = write_package(&dir, &package, &compiled.modules, true) {
        return fail(e);
    }
    println!("{}", dir.display());
    ExitCode::SUCCESS
}

fn cmd_eval(args: &[String]) -> ExitCode {
    let mut file = None;
    let mut pass = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-A" | "--system" => {
                pass.push(a.clone());
                pass.extend(it.next().cloned());
            }
            "--impure" => pass.push(a.clone()),
            _ => file = Some(absolute(a)),
        }
    }
    let Some(file) = file else {
        return usage();
    };
    let entries = vec![file.clone()];
    let compiled = compile_closure(&entries);
    if let Some(e) = compiled.failed.get(&file) {
        return fail(e);
    }
    let package = package_name(&entries);
    let dir = cache_root().join("programs").join(&package);
    if let Err(e) = write_package(&dir, &package, &compiled.modules, false) {
        return fail(e);
    }
    let exe = match build_package(&dir, &package, &cache_root().join("target")) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let entry = compiled
        .modules
        .iter()
        .find(|m| m.file == file || m.file.starts_with(&format!("{file}/")))
        .map(|m| m.file.clone())
        .unwrap_or(file);
    match Command::new(exe).arg(entry).args(pass).status() {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(e) => fail(e),
    }
}
