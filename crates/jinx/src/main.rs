// jinx: Nix, transpiled to Rust. `jinx --help` lists the commands.

mod ast;
mod builtins_table;
mod compile;
mod corpus;
mod emit;
mod lexer;
mod parser;

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

use compile::{absolute, build_package, cache_root, compile_closure, package_name, write_package};

#[derive(Parser)]
#[command(name = "jinx", version, about = "Nix, transpiled to Rust")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write the Cargo package for FILEs and the files they import.
    Compile {
        #[arg(required = true)]
        files: Vec<String>,
        /// The package directory; out/jinx-<hash> by default.
        #[arg(short)]
        out: Option<PathBuf>,
    },
    /// Compile, build and run FILE, printing its value as
    /// `nix-instantiate --eval --strict` does.
    Eval {
        file: String,
        /// Select an attribute of the value.
        #[arg(short = 'A', value_name = "ATTR.PATH")]
        attr: Option<String>,
        /// The value of builtins.currentSystem.
        #[arg(long)]
        system: Option<String>,
        /// Let getEnv read the environment.
        #[arg(long)]
        impure: bool,
    },
    /// Compare every DIR/*.nix with the recorded output of Nix in DIR/*.exp.
    TestCorpus {
        dir: String,
        /// Run nix-instantiate and rewrite the recordings.
        #[arg(long)]
        record: bool,
        /// Leave out a file, by name.
        #[arg(long, value_name = "NAME.nix")]
        skip: Vec<String>,
    },
    /// Run Nix's eval-okay/eval-fail language tests in DIR.
    TestLang {
        dir: String,
        /// Only the tests whose name contains this.
        #[arg(default_value = "")]
        filter: String,
    },
    /// Print the syntax tree of FILE.
    Parse { file: String },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Cmd::Compile { files, out } => cmd_compile(files, out),
        Cmd::Eval {
            file,
            attr,
            system,
            impure,
        } => {
            let mut pass = Vec::new();
            if let Some(a) = attr {
                pass.extend(["-A".to_string(), a]);
            }
            if let Some(s) = system {
                pass.extend(["--system".to_string(), s]);
            }
            if impure {
                pass.push("--impure".into());
            }
            cmd_eval(&file, pass)
        }
        Cmd::TestCorpus { dir, record, skip } => corpus::run(&dir, record, &skip),
        Cmd::TestLang { dir, filter } => corpus::run_lang(&dir, &filter),
        Cmd::Parse { file } => cmd_parse(&file),
    }
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::from(1)
}

fn cmd_parse(file: &str) -> ExitCode {
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

fn cmd_compile(files: Vec<String>, out: Option<PathBuf>) -> ExitCode {
    let files: Vec<String> = files.iter().map(|f| absolute(f)).collect();
    let compiled = compile_closure(&files);
    if let Some((_, e)) = compiled.failed.iter().next() {
        return fail(e);
    }
    let package = package_name(&files);
    let dir = out.unwrap_or_else(|| PathBuf::from("out").join(&package));
    if let Err(e) = write_package(&dir, &package, &compiled.modules, true) {
        return fail(e);
    }
    println!("{}", dir.display());
    ExitCode::SUCCESS
}

fn cmd_eval(file: &str, pass: Vec<String>) -> ExitCode {
    let file = absolute(file);
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
