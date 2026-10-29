# The Rust port

jinx is being rewritten in Rust: the compiler (`crates/jinx`) is a Rust
program, and what it writes is Rust. A Nix file becomes a Rust module, a
set of files becomes a Cargo package linked against the runtime crate
(`crates/jinx-runtime`), and evaluating Nix means building and running
that package. The TypeScript implementation under `src/` stays the
reference until the Rust one reaches parity with it.

## Status

This is milestone 1: the language core.

- Parser, scope resolution and the Rust emitter, ported from
  `src/syntax` and `src/compiler`.
- The runtime's value model, laziness, operators and about a hundred
  builtins, string contexts, store paths of sources (NAR hashing,
  `filterSource`, `builtins.path`, `toFile`), JSON, TOML,
  `convertHash`, `sort` (Nix's peeksort, comparison for comparison),
  and the `nix-instantiate --eval --strict` printer.
- `tests/corpus`: 7 of 8 files agree with Nix. `derivations.nix` waits
  for derivations.
- Nix's own language tests (`eval-okay-*`, `eval-fail-*` from
  [numtide/nix-conformance]): 595 of the 704 that run pass (cases
  whose `.flags` change the result are skipped). Of the 109 that fail,
  66 are regex corner cases, 22 need derivations, 12 need flake
  references or `toXML`, and the rest depend on the suite's
  environment (`/pwd`, `/fake-home`, environment variables) or on
  `scopedImport`.

Not yet ported: `derivation` and `derivationStrict`, `toXML`, flake
references, nixpkgs-scale compilation, and the `instantiate` and
`eval-jobs` commands.

## Building blocks

Where a crate does the job to Nix's byte, the runtime uses it:

| Crate                  | For                                                           |
| ---------------------- | ------------------------------------------------------------- |
| `imbl`                 | attribute sets: a persistent B-tree, so `//` shares structure |
| `serde_json`           | parsing `fromJSON`                                            |
| `toml` 0.8             | parsing `fromTOML`; TOML 1.0, as Nix's toml11 (0.9 reads 1.1) |
| `regex`                | `match` and `split`, after translating the ERE                |
| `sha1`, `sha2`, `md-5` | hashes                                                        |
| `data-encoding`        | base16 and base64                                             |
| `clap`                 | the command line                                              |

Hand-written, because the output must be Nix's and no crate gives it:
`toJSON` (nlohmann's float format and escapes), the float printers
(`%g`, `%f`), Nix's base32 (its own bit order), NAR serialisation,
store path computation, and `sort` (the exact comparison sequence of
Nix's peeksort matters for comparators that are not orders). snix's
`nix-compat` covers store paths, NARs and derivations, but is not on
crates.io; it may come in from git with derivations.

## Using it

```console
$ cargo build
$ target/debug/jinx eval file.nix [-A attr.path] [--system SYSTEM]
$ target/debug/jinx compile file.nix -o out/file    # the Cargo package, formatted
$ target/debug/jinx test-corpus tests/corpus --skip derivations.nix
$ target/debug/jinx test-lang /path/to/nix-conformance/lang
```

`eval` compiles the file and every file it imports with a literal path
into a package under `~/.cache/jinx/programs/`, builds it with Cargo
into the shared `~/.cache/jinx/target`, and runs it. The first build
compiles the runtime (optimised); later ones compile only the
generated modules (unoptimised, since they are large and simple).

## What the output looks like

```nix
let
  add = { x, y ? 1 }: x + y;
in
{ three = add { x = 2; }; }
```

becomes

```rust
static FORMALS_1: nix::Formals = nix::Formals {
    file: FILE,
    args: &[("x", false, 2, 11), ("y", true, 2, 14)],
    ellipsis: false,
};

pub fn value() -> nix::Result<nix::Value> {
    Ok({
        let add = nix::blank();
        nix::init_value(
            &add,
            nix::lambda_formals(&FORMALS_1, move |j_arg: nix::Value| {
                let j_args = nix::formals(&j_arg, &FORMALS_1)?;
                let x = nix::arg(&j_args, "x")?;
                let y = nix::blank();
                match j_args.get("y") {
                    Some(j_v) => nix::init_value(&y, j_v.clone()),
                    None => nix::init_thunk(&y, move || Ok(nix::int(1))),
                }
                Ok(nix::add(&x, &y)?)
            }),
        );
        {
            let mut j_m = nix::AttrsBuilder::new(FILE);
            j_m.set(
                "three",
                4,
                3,
                nix::thunk({
                    let add = add.clone();
                    move || {
                        Ok(nix::app(
                            &add,
                            &[nix::thunk(move || {
                                Ok({
                                    let mut j_m = nix::AttrsBuilder::new(FILE);
                                    j_m.set("x", 4, 17, nix::int(2));
                                    j_m.build()
                                })
                            })],
                        )?)
                    }
                }),
            );
            j_m.build()
        }
    })
}
```

## Design

The decisions of [design.md](./design.md) carry over: thunks go where
Nix's `Expr::maybeThunk` puts them, variables are resolved at compile
time except under `with`, `inherit (e) x` is a thunk, and every builtin
whose output reaches a store path produces Nix's bytes. What changes
with the target language:

**Values.** `nix::Value` is one enum, cheap to clone: integers are
`i64` with checked arithmetic, strings are bytes plus a context (so
`stringLength`, `substring` and `match` have byte semantics with no
conversion), attribute sets are persistent ordered maps keyed by name,
which is Nix's enumeration order, each attribute carrying its position.
Copying a set is O(1) and `//` copies only what it changes, which does
the job of the TypeScript runtime's layered sets. A thunk
is a variant of the same enum, so a lazy position and a strict one have
the same type; `nix::force` turns one into the other.

**Errors.** Every operation returns `nix::Result<Value>` and emitted
code propagates with `?`. `tryEval` catches the errors marked
catchable (`throw`, `assert`); a thunk whose evaluation failed is
re-armed, as in Nix.

**Closures own what they use.** A Nix lambda or thunk is a Rust `move`
closure. The emitter records which outer variables each closure refers
to while it resolves names, and clones exactly those into it, which is
the `{ let add = add.clone(); move || ... }` above. Values are
reference-counted, so a clone is a counter increment.

**Recursive bindings.** `let`, `rec` and formals' defaults may refer to
any of their siblings, in any order. Each such binding is declared as a
blank cell (`nix::blank()`) before any is defined, and defined with
`init_thunk` or `init_value` afterwards, so every closure can capture
every sibling. Literals need nothing and are plain values. Reference
cycles (a fixpoint, a recursive let) are never freed: an evaluation is
a process that exits when it is done.

**Imports.** A program is the set of files reachable from its entry
points through literal paths. `src/main.rs` of the package lists each
module by the absolute path of its source, and `import` looks the path
up there, evaluating each module once. Importing a file that was not
compiled in is an error; the TypeScript runtime compiles such a file on
demand, which a compiled Rust program cannot do.

**Regular expressions.** POSIX EREs are translated to the `regex`
crate's syntax and run on bytes. The crate's leftmost-first matching
agrees with libstdc++ for `match`, but `split` uses `regex_search`,
which in libstdc++ prefers the longest alternative, and captures inside
repeated groups differ; porting libstdc++'s executor is planned.

**Deep recursion.** The program evaluates on a thread with a 1 GiB
stack, since Nix code recurses deeply and each Nix call is several Rust
frames.

[numtide/nix-conformance]: https://github.com/numtide/nix-conformance
