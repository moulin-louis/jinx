// builtins.fromTOML. The toml crate (TOML 1.0, the version toml11 reads
// in Nix) parses the document; the conversion is Nix's fromTOML.cc:
// tables become sets, arrays lists, and the scalars their Nix kinds. A
// date or time is an error, as it is in Nix without the experimental
// feature parse-toml-timestamps.

use crate::value::*;

pub fn from_toml(text: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(text)
        .map_err(|_| NixError::boxed("while parsing TOML: invalid UTF-8"))?;
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        NixError::boxed(format!("while parsing TOML: {}", e.message()))
    })?;
    convert(toml::Value::Table(table))
}

fn convert(v: toml::Value) -> Result<Value> {
    Ok(match v {
        toml::Value::String(s) => {
            if s.contains('\0') {
                return err("while parsing TOML: a string contains a null byte");
            }
            string(s)
        }
        toml::Value::Integer(i) => Value::Int(i),
        toml::Value::Float(f) => Value::Float(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(_) => {
            return err("while parsing TOML: Dates and times are not supported")
        }
        toml::Value::Array(items) => Value::List(std::rc::Rc::new(
            items.into_iter().map(convert).collect::<Result<_>>()?,
        )),
        toml::Value::Table(t) => {
            let mut a = Attrs::new();
            for (k, v) in t {
                if k.contains('\0') {
                    return err("while parsing TOML: a key contains a null byte");
                }
                a.insert(k, convert(v)?);
            }
            attrs(a)
        }
    })
}
