// The differential corpus: every DIR/*.nix is evaluated by jinx and the
// result compared with what `nix-instantiate --eval --strict` printed for
// it, recorded in DIR/*.exp as "ok" or "error" and the text. With
// --record, nix-instantiate is run and the recordings are rewritten.
//
// All the files are compiled into one program, built once, and the
// program is run per file.
//
// `jinx test-lang DIR` runs Nix's own language tests the same way, by the
// conventions of its tests/functional/lang.sh: eval-okay-NAME.nix must
// print NAME.exp, eval-fail-NAME.nix must fail. Cases with a .flags file
// are skipped.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use crate::compile::{
    absolute, build_package, cache_root, compile_closure, package_name, write_package,
};

struct Outcome {
    ok: bool,
    text: String,
}

fn nix_outcome(file: &str) -> Outcome {
    match Command::new("nix-instantiate")
        .args(["--eval", "--strict", "--read-write-mode", file])
        .output()
    {
        Ok(o) if o.status.success() => Outcome {
            ok: true,
            text: String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
        },
        Ok(o) => Outcome {
            ok: false,
            text: String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
        },
        Err(e) => Outcome {
            ok: false,
            text: format!("running nix-instantiate: {e}"),
        },
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() > 300 {
        format!("{}…", s.chars().take(300).collect::<String>())
    } else {
        s.to_string()
    }
}

// Compile `files` into one program and build it.
fn build(files: &[String]) -> Result<(crate::compile::Compiled, PathBuf), String> {
    let compiled = compile_closure(files);
    let package = package_name(files);
    let pkg_dir: PathBuf = cache_root().join("programs").join(&package);
    write_package(&pkg_dir, &package, &compiled.modules, false).map_err(|e| e.to_string())?;
    let exe = build_package(&pkg_dir, &package, &cache_root().join("target"))?;
    Ok((compiled, exe))
}

fn evaluate(compiled: &crate::compile::Compiled, exe: &PathBuf, file: &str) -> Outcome {
    if let Some(e) = compiled.failed.get(file) {
        return Outcome {
            ok: false,
            text: format!("error: {e}"),
        };
    }
    let mut cmd = Command::new("timeout");
    cmd.arg("20").arg(exe).arg(file);
    match cmd.output() {
        Ok(o) if o.status.success() => Outcome {
            ok: true,
            text: String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
        },
        Ok(o) => Outcome {
            ok: false,
            text: String::from_utf8_lossy(&o.stderr).trim_end().to_string(),
        },
        Err(e) => Outcome {
            ok: false,
            text: format!("internal: {e}"),
        },
    }
}

pub fn run_lang(dir: &str, filter: &str) -> ExitCode {
    let dir = absolute(dir);
    let mut names: Vec<String> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                (n.starts_with("eval-okay-") || n.starts_with("eval-fail-"))
                    && n.ends_with(".nix")
                    && n.contains(filter)
            })
            .collect(),
        Err(e) => {
            eprintln!("error: reading '{dir}': {e}");
            return ExitCode::from(1);
        }
    };
    names.sort();
    let stem = |n: &str| format!("{dir}/{}", n.trim_end_matches(".nix"));
    let mut skipped = 0;
    names.retain(|n| {
        let s = stem(n);
        let flags = fs::read_to_string(format!("{s}.flags")).ok();
        // Turning on parse-toml-timestamps does not change whether an
        // invalid document is refused, so those failure cases still run.
        let harmless = n.starts_with("eval-fail-")
            && flags.as_deref().map(str::trim)
                == Some("--extra-experimental-features parse-toml-timestamps");
        let skip = (flags.is_some() && !harmless)
            || std::path::Path::new(&format!("{s}.exp-disabled")).exists()
            || (n.starts_with("eval-okay-") && !std::path::Path::new(&format!("{s}.exp")).exists());
        skipped += skip as usize;
        !skip
    });
    let files: Vec<String> = names.iter().map(|n| format!("{dir}/{n}")).collect();
    let (compiled, exe) = match build(&files) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let mut pass = 0;
    let mut failures = Vec::new();
    for (name, file) in names.iter().zip(&files) {
        let got = evaluate(&compiled, &exe, file);
        if name.starts_with("eval-fail-") {
            if got.ok {
                failures.push(format!(
                    "{name}\n  expected an error, got: {}",
                    truncate(&got.text)
                ));
            } else {
                pass += 1;
            }
            continue;
        }
        let expected = fs::read_to_string(format!("{}.exp", stem(name))).unwrap_or_default();
        if got.ok && got.text == expected.trim_end() {
            pass += 1;
            continue;
        }
        failures.push(format!(
            "{name}\n  nix:  {}\n  jinx: {} {}",
            truncate(expected.trim_end()),
            if got.ok { "ok" } else { "error" },
            truncate(&got.text)
        ));
    }
    for f in &failures {
        println!("{f}");
    }
    println!(
        "{pass}/{} language tests pass ({skipped} skipped)",
        files.len()
    );
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

pub fn run(dir: &str, record: bool, skip: &[String]) -> ExitCode {
    let dir = absolute(dir);
    let mut files: Vec<String> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".nix") && !skip.contains(n))
            .map(|n| format!("{dir}/{n}"))
            .collect(),
        Err(e) => {
            eprintln!("error: reading '{dir}': {e}");
            return ExitCode::from(1);
        }
    };
    files.sort();

    let (compiled, exe) = match build(&files) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };

    let mut pass = 0;
    let mut failures = Vec::new();
    for file in &files {
        let name = file.rsplit('/').next().unwrap();
        let exp_file = format!("{}.exp", file.trim_end_matches(".nix"));
        let expected = if record {
            let o = nix_outcome(file);
            let kind = if o.ok { "ok" } else { "error" };
            let _ = fs::write(&exp_file, format!("{kind}\n{}\n", o.text));
            o
        } else {
            match fs::read_to_string(&exp_file) {
                Ok(s) => {
                    let (kind, text) = s.split_once('\n').unwrap_or((s.as_str(), ""));
                    Outcome {
                        ok: kind == "ok",
                        text: text.trim_end().to_string(),
                    }
                }
                Err(_) => {
                    failures.push(format!("{name}: no recording"));
                    continue;
                }
            }
        };
        let got = evaluate(&compiled, &exe, file);
        if got.ok == expected.ok && (!got.ok || got.text == expected.text) {
            pass += 1;
            continue;
        }
        let kind = |o: &Outcome| if o.ok { "ok" } else { "error" };
        failures.push(format!(
            "{name}\n  nix:  {} {}\n  jinx: {} {}",
            kind(&expected),
            truncate(&expected.text),
            kind(&got),
            truncate(&got.text)
        ));
    }
    for f in &failures {
        println!("{f}");
    }
    println!("{pass}/{} corpus files agree with nix", files.len());
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
