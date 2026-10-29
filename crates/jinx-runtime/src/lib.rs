// The runtime that Rust compiled from Nix by jinx links against. Compiled
// modules `use jinx_runtime as nix;` and call what is exported here.

#![allow(non_snake_case)]

pub mod builtins;
pub mod json;
pub mod modules;
pub mod numbers;
pub mod ops;
pub mod print;
pub mod regex;
pub mod sort;
pub mod store;
pub mod value;

pub use builtins::builtins;
pub use ops::*;
pub use value::*;

// The primops by name, for direct calls: `nix::b::map(&[f, xs])`.
pub mod b {
    pub use crate::builtins::*;
}

use modules::ModuleFn;

// The `main` of a compiled program:
//   <program> FILE.nix [-A attr.path] [--system SYSTEM] [--impure]
// evaluates FILE strictly and prints it as nix-instantiate --eval --strict
// does. A function whose formals all have defaults is called with none,
// as nix-instantiate's autocall does.
pub fn main(modules: &'static [(&'static str, ModuleFn)]) -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Nix code recurses deeply; the main thread's stack is not enough.
    let child = std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || run(modules, args))
        .expect("spawn evaluator thread");
    let code = child.join().unwrap_or(101);
    std::process::exit(code)
}

fn run(modules: &'static [(&'static str, ModuleFn)], args: Vec<String>) -> i32 {
    modules::set_modules(modules);
    let mut file = None;
    let mut attr_path: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-A" => attr_path.extend(
                it.next()
                    .unwrap_or_default()
                    .split('.')
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            ),
            "--system" => {
                let s = it.next().unwrap_or_default();
                builtins::SETTINGS.with(|st| st.borrow_mut().current_system = s);
            }
            "--impure" => builtins::SETTINGS.with(|st| st.borrow_mut().impure = true),
            _ => file = Some(a),
        }
    }
    let Some(file) = file else {
        eprintln!("usage: <program> FILE.nix [-A attr.path] [--system SYSTEM] [--impure]");
        return 2;
    };
    match evaluate(&file, &attr_path).and_then(|v| print::print_value(&v)) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            eprintln!("error: {}", e.message);
            1
        }
    }
}

pub fn evaluate(file: &str, attr_path: &[String]) -> Result<Value> {
    let mut v = modules::import_file(file)?;
    if let Value::Lambda(l) = &v {
        if let Some(f) = l.formals {
            if f.args.iter().all(|(_, has_default, ..)| *has_default) {
                v = app(&v, &[attrs(Attrs::new())])?;
            }
        }
    }
    for name in attr_path {
        v = sel(&v, &[std::borrow::Cow::Owned(name.clone())])?;
    }
    Ok(v)
}
