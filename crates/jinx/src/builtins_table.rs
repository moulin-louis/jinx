// What the compiler knows about builtins: every name and its arity, taken
// from the runtime's own table, and the names Nix puts in the global
// scope without the `builtins.` prefix.

use jinx_runtime::builtins::PRIMOPS;

// The builtins that are values rather than functions.
const CONSTANTS: &[&str] = &[
    "builtins",
    "currentSystem",
    "currentTime",
    "false",
    "langVersion",
    "nixPath",
    "nixVersion",
    "null",
    "storeDir",
    "true",
];

const GLOBALS: &[&str] = &[
    "abort",
    "baseNameOf",
    "break",
    "builtins",
    "derivation",
    "derivationStrict",
    "dirOf",
    "false",
    "fetchGit",
    "fetchMercurial",
    "fetchTarball",
    "fetchTree",
    "fromTOML",
    "import",
    "isNull",
    "map",
    "null",
    "placeholder",
    "removeAttrs",
    "scopedImport",
    "throw",
    "toString",
    "true",
];

// The arity of a builtin; 0 for a constant, None for no such builtin.
pub fn arity_of(name: &str) -> Option<usize> {
    if CONSTANTS.contains(&name) {
        return Some(0);
    }
    PRIMOPS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, a, _)| *a)
}

pub fn is_global(name: &str) -> bool {
    matches!(name, "true" | "false" | "null" | "builtins" | "__curPos")
        || (GLOBALS.contains(&name) && arity_of(name).is_some())
        || name
            .strip_prefix("__")
            .is_some_and(|n| arity_of(n).is_some())
}

// The function in the runtime's `b` module that implements a primop.
pub fn rust_fn_name(name: &str) -> String {
    match name {
        "match" | "break" => format!("r#{name}"),
        _ => name.replace('\'', "_"),
    }
}
