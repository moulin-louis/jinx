// `import`: from a Nix path to the module compiled from it.
//
// A compiled program carries a table of its modules, one function per
// source file returning that file's value. import() resolves the path
// (a directory means its default.nix), finds the file in the table, and
// evaluates it once: the value is kept, as Nix's import cache keeps it.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::ops::canon_path;
use crate::value::*;

pub type ModuleFn = fn() -> Result<Value>;

thread_local! {
    static MODULES: RefCell<&'static [(&'static str, ModuleFn)]> = const { RefCell::new(&[]) };
    static LOADED: RefCell<HashMap<String, Value>> = RefCell::new(HashMap::new());
}

pub fn set_modules(table: &'static [(&'static str, ModuleFn)]) {
    MODULES.with(|m| *m.borrow_mut() = table);
}

// The .nix file a path names: a directory means its default.nix.
pub fn resolve_nix_file(p: &str) -> Result<String> {
    match std::fs::metadata(p) {
        Ok(m) if m.is_dir() => Ok(format!("{}/default.nix", p.trim_end_matches('/'))),
        Ok(_) => Ok(p.to_string()),
        Err(_) => err(format!("path '{p}' does not exist")),
    }
}

pub fn import(arg: &Value) -> Result<Value> {
    let p = match force(arg)? {
        Value::Path(p) => p.to_string(),
        x => {
            let s = crate::ops::coerce_to_string(&x, crate::ops::COERCE_PLAIN)?;
            String::from_utf8_lossy(&s.bytes).into_owned()
        }
    };
    if !p.starts_with('/') {
        return err(format!("string '{p}' doesn't represent an absolute path"));
    }
    let file = resolve_nix_file(&canon_path(&p)?)?;
    import_file(&file)
}

pub fn import_file(file: &str) -> Result<Value> {
    if let Some(v) = LOADED.with(|l| l.borrow().get(file).cloned()) {
        return force(&v);
    }
    let f = MODULES.with(|m| {
        m.borrow()
            .iter()
            .find(|(name, _)| *name == file)
            .map(|(_, f)| *f)
    });
    let Some(f) = f else {
        return err(format!(
            "cannot import '{file}': it was not compiled into this program (jinx compiles the files a program imports ahead of time)"
        ));
    };
    let v = thunk(f);
    LOADED.with(|l| l.borrow_mut().insert(file.to_string(), v.clone()));
    force(&v)
}
