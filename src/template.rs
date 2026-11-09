//! MiniJinja rendering of a templated file's src, with `vars`, `facts` and,
//! for an entry that lists them, `secrets`.
//!
//! The engine has strict undefined, a fuel limit, no loader (`include`,
//! `import` and `extends` do not parse), trim/lstrip blocks, the trailing
//! newline kept, no auto-escape and a `b64encode` filter. Error text gives
//! the position and the variable's name, never a value.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use minijinja::syntax::SyntaxConfig;
use minijinja::value::ValueKind;
use minijinja::{AutoEscape, Environment, Error, ErrorKind, UndefinedBehavior, Value};
use zeroize::Zeroizing;

use crate::document::MAX_BYTES;
use crate::facts::Facts;
use crate::inventory::{Resolved, VarValue};

/// Engine steps one render may take.
const FUEL: u64 = 1_000_000;
/// Statements that would read another template.
const REFUSED: &[&str] = &["include", "import", "extends", "from"];

/// Why `b64encode` refused its argument.
const B64_NEEDS_STRING: &str = "b64encode needs a string";

/// Parses `src`; `Err` is a syntax error with its line.
pub fn check(src: &str) -> Result<(), String> {
    environment(Arc::default())
        .template_from_str(src)
        .map(drop)
        .map_err(|err| describe(&err, src, None))
}

/// A render's text, and each `b64encode` output whose input held the value
/// of one of its `secrets`, so that step output can be scrubbed of it too.
pub struct Render {
    pub text: Zeroizing<String>,
    pub encoded: Vec<Zeroizing<String>>,
}

/// Renders `src` with `vars.KEY`, `facts.KEY` and, when `secrets` is not
/// empty, `secrets.NAME`; the output is at most [`MAX_BYTES`] bytes.
pub fn render(
    src: &str,
    vars: &Resolved<VarValue>,
    facts: &Facts,
    secrets: &BTreeMap<String, Zeroizing<String>>,
) -> Result<Render, String> {
    let encoded = Arc::new(Encoded {
        secrets: secrets.values().cloned().collect(),
        outputs: Mutex::default(),
    });
    let env = environment(encoded.clone());
    let template = env
        .template_from_str(src)
        .map_err(|err| describe(&err, src, None))?;
    let ctx = context(vars, facts, secrets);
    let mut out = Capped::new();
    match template.render_captured_to(&ctx, &mut out) {
        Ok(_) => {}
        Err(_) if out.over => return Err("rendered output exceeds the 1 MiB limit".to_owned()),
        Err(err) => {
            let missing = missing_names(&template.undeclared_variables(true), &ctx);
            return Err(describe(&err, src, Some(&missing)));
        }
    }
    let text = std::str::from_utf8(&out.buf).map_err(|_| "rendered output is not UTF-8")?;
    let mut exact = Zeroizing::new(String::with_capacity(text.len()));
    exact.push_str(text);
    drop(env);
    let encoded = std::mem::take(
        &mut *(encoded.outputs)
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    Ok(Render {
        text: exact,
        encoded,
    })
}

/// The values `b64encode` looks for in its input, and the outputs it
/// recorded for them.
#[derive(Default)]
struct Encoded {
    secrets: Vec<Zeroizing<String>>,
    outputs: Mutex<Vec<Zeroizing<String>>>,
}

/// The engine. `replace` is removed (its output has no size limit); `~` and
/// `*` cannot be capped the same way (minijinja bounds a repetition at 100 MB)
/// and stay a known self-DoS limit: the template's author owns the inventory.
/// `b64encode` records into `encoded`.
fn environment(encoded: Arc<Encoded>) -> Environment<'static> {
    let mut env = Environment::new();
    env.remove_filter("replace");
    env.add_filter("b64encode", move |value: Value| -> Result<String, Error> {
        let text = (value.as_str())
            .filter(|_| value.kind() == ValueKind::String)
            .ok_or_else(|| Error::new(ErrorKind::InvalidOperation, B64_NEEDS_STRING))?;
        let out = b64encode(text.as_bytes());
        if (encoded.secrets.iter()).any(|secret| text.contains(secret.as_str())) {
            (encoded
                .outputs
                .lock()
                .unwrap_or_else(PoisonError::into_inner))
            .push(Zeroizing::new(out.clone()));
        }
        Ok(out)
    });
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_fuel(Some(FUEL));
    env.set_syntax(
        SyntaxConfig::builder()
            .trim_blocks(true)
            .lstrip_blocks(true)
            .keep_trailing_newline(true)
            .build()
            .expect("the default delimiters are valid"),
    );
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env
}

/// `bytes` in standard base64 (RFC 4648 §4): `+/`, `=` padding, no line
/// breaks.
fn b64encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63])
            } else {
                '='
            });
        }
    }
    out
}

fn context(
    vars: &Resolved<VarValue>,
    facts: &Facts,
    secrets: &BTreeMap<String, Zeroizing<String>>,
) -> Value {
    let vars = Value::from_pairs(
        vars.iter()
            .map(|(key, (value, _))| (key.as_str(), to_value(value))),
    );
    let facts = Value::from_pairs(
        facts
            .0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    let mut pairs = vec![("vars", vars), ("facts", facts)];
    if !secrets.is_empty() {
        let secrets = (secrets.iter()).map(|(name, value)| (name.as_str(), value.as_str()));
        pairs.push(("secrets", Value::from_pairs(secrets)));
    }
    Value::from_pairs(pairs)
}

fn to_value(value: &VarValue) -> Value {
    match value {
        VarValue::Bool(b) => Value::from(*b),
        VarValue::Int(n) => Value::from(*n),
        VarValue::String(s) => Value::from(s.as_str()),
        VarValue::Array(items) => items.iter().map(to_value).collect(),
        VarValue::Table(table) => Value::from_pairs(
            table
                .iter()
                .map(|(key, value)| (key.as_str(), to_value(value))),
        ),
    }
}

/// The undeclared dotted names that do not resolve in `ctx`, each cut at its
/// first missing part.
fn missing_names(names: &HashSet<String>, ctx: &Value) -> Vec<String> {
    let mut missing: Vec<String> = names
        .iter()
        .filter_map(|name| {
            let mut at = ctx.clone();
            let mut seen = Vec::new();
            for part in name.split('.') {
                seen.push(part);
                match at.get_attr(part) {
                    Ok(next) if !next.is_undefined() => at = next,
                    _ => return Some(seen.join(".")),
                }
            }
            None
        })
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// `line L: <reason>` (no column: spans need minijinja's `debug` feature,
/// which prints context values).
fn describe(err: &Error, src: &str, missing: Option<&[String]>) -> String {
    let reason = match err.kind() {
        ErrorKind::OutOfFuel => return "fuel exhausted".to_owned(),
        ErrorKind::InvalidOperation if err.detail() == Some(B64_NEEDS_STRING) => {
            B64_NEEDS_STRING.to_owned()
        }
        ErrorKind::SyntaxError => {
            let detail = err.detail().unwrap_or_default();
            match REFUSED
                .iter()
                .find(|stmt| detail == format!("unknown statement {stmt}"))
            {
                Some(stmt) => format!("{stmt} is not allowed"),
                None if detail.is_empty() => "syntax error".to_owned(),
                None => format!("syntax error: {detail}"),
            }
        }
        ErrorKind::UndefinedError => {
            let missing = missing.unwrap_or_default();
            let line = err
                .line()
                .and_then(|n| src.lines().nth(n - 1))
                .unwrap_or_default();
            let on_line: Vec<&String> = missing.iter().filter(|n| used(line, n)).collect();
            let names: Vec<&String> = if on_line.is_empty() {
                missing.iter().filter(|n| used(src, n)).collect()
            } else {
                on_line
            };
            match names.as_slice() {
                [] => "undefined variable".to_owned(),
                names => {
                    let shown: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
                    format!("undefined variable {}", shown.join(", "))
                }
            }
        }
        kind => kind.to_string(),
    };
    match err.line() {
        Some(line) => format!("line {line}: {reason}"),
        None => reason,
    }
}

/// Whether `text` uses the dotted `name` as a whole path (not inside a longer
/// identifier) other than under `is defined`, `is undefined` or `default`.
fn used(text: &str, name: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(name).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let rest = &text[at + name.len()..];
        if before.is_some_and(|c| ident(c) || c == '.') || rest.starts_with(ident) {
            return false;
        }
        let after = rest
            .trim_start_matches(|c: char| ident(c) || c == '.')
            .trim_start();
        let guarded = ["is defined", "is undefined", "is not defined"]
            .iter()
            .any(|test| after.starts_with(test))
            || after.strip_prefix('|').is_some_and(|filter| {
                let filter = filter.trim_start();
                filter.starts_with("default") || filter.starts_with("d(")
            });
        !guarded
    })
}

/// A writer that refuses to grow past [`MAX_BYTES`]. Its buffer is reserved
/// once at full size, so it never reallocates and leaves no unzeroed copy.
struct Capped {
    buf: Zeroizing<Vec<u8>>,
    over: bool,
}

impl Capped {
    fn new() -> Self {
        Capped {
            buf: Zeroizing::new(Vec::with_capacity(MAX_BYTES as usize)),
            over: false,
        }
    }
}

impl io::Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (self.buf.len() + bytes.len()) as u64 > MAX_BYTES {
            self.over = true;
            return Err(io::Error::other("rendered output exceeds the limit"));
        }
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::inventory::Level;

    fn vars(pairs: &[(&str, VarValue)]) -> Resolved<VarValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (v.clone(), Level::Global)))
            .collect()
    }

    fn facts() -> Facts {
        Facts(BTreeMap::from([
            ("hostname".to_owned(), "web1".to_owned()),
            ("os".to_owned(), "linux".to_owned()),
        ]))
    }

    fn s(v: &str) -> VarValue {
        VarValue::String(v.to_owned())
    }

    fn ok(src: &str, vars: &Resolved<VarValue>) -> String {
        render(src, vars, &facts(), &BTreeMap::new())
            .unwrap()
            .text
            .to_string()
    }

    fn err(src: &str, vars: &Resolved<VarValue>) -> String {
        render(src, vars, &facts(), &BTreeMap::new()).err().unwrap()
    }

    fn secrets(pairs: &[(&str, &str)]) -> BTreeMap<String, Zeroizing<String>> {
        (pairs.iter())
            .map(|(name, value)| ((*name).to_owned(), Zeroizing::new((*value).to_owned())))
            .collect()
    }

    #[test]
    fn renders_the_listed_secrets_and_records_their_encoded_forms() {
        let listed = secrets(&[("TOKEN", "fake-value-one"), ("OTHER", "fake-value-two")]);
        let src = "T={{ secrets.TOKEN }}\n\
                   A={{ (\"cmj:\" ~ secrets.TOKEN) | b64encode }}\n\
                   P={{ \"plain\" | b64encode }}\n";
        let out = render(src, &vars(&[]), &facts(), &listed).unwrap();
        assert_eq!(
            *out.text,
            "T=fake-value-one\nA=Y21qOmZha2UtdmFsdWUtb25l\nP=cGxhaW4=\n"
        );
        let encoded: Vec<&str> = out.encoded.iter().map(|e| e.as_str()).collect();
        assert_eq!(encoded, ["Y21qOmZha2UtdmFsdWUtb25l"]);
    }

    #[test]
    fn an_unlisted_secret_is_undefined_and_none_adds_no_secrets_key() {
        let listed = secrets(&[("TOKEN", "fake-value-one")]);
        let why = render("{{ secrets.NOPE }}", &vars(&[]), &facts(), &listed)
            .err()
            .unwrap();
        assert_eq!(why, "line 1: undefined variable `secrets.NOPE`");
        assert!(!why.contains("fake-value-one"));
        assert_eq!(
            err("{{ secrets.TOKEN }}", &vars(&[])),
            "line 1: undefined variable `secrets`"
        );
    }

    #[test]
    fn b64encode_is_standard_base64_with_padding() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("\u{ff}\u{fe}?>", "w7/Dvj8+"),
        ] {
            assert_eq!(b64encode(input.as_bytes()), want, "{input:?}");
        }
        assert_eq!(b64encode(&[0xfb, 0xff]), "+/8=");
        assert_eq!(
            ok("{{ 'user:pass' | b64encode }}", &vars(&[])),
            "dXNlcjpwYXNz"
        );
    }

    #[test]
    fn b64encode_refuses_what_is_not_a_string() {
        let v = vars(&[("port", VarValue::Int(8080))]);
        assert_eq!(
            err("ok\n{{ vars.port | b64encode }}", &v),
            "line 2: b64encode needs a string"
        );
        assert_eq!(
            err("{{ [1] | b64encode }}", &v),
            "line 1: b64encode needs a string"
        );
    }

    #[test]
    fn renders_vars_and_facts() {
        let v = vars(&[("port", VarValue::Int(8080)), ("tls", VarValue::Bool(true))]);
        assert_eq!(
            ok(
                "{{ facts.hostname }}:{{ vars.port }} tls={{ vars.tls }}",
                &v
            ),
            "web1:8080 tls=True"
        );
    }

    #[test]
    fn renders_nested_tables_arrays_and_loops() {
        let v = vars(&[(
            "site",
            VarValue::Table(BTreeMap::from([
                ("name".to_owned(), s("example")),
                (
                    "aliases".to_owned(),
                    VarValue::Array(vec![s("a.example"), s("b.example")]),
                ),
            ])),
        )]);
        let src = "{{ vars.site.name }} {{ vars.site.aliases[1] }}\n\
                   {% for a in vars.site.aliases %}{{ a }};{% endfor %}";
        assert_eq!(ok(src, &v), "example b.example\na.example;b.example;");
    }

    #[test]
    fn defined_test_and_default_work_under_strict() {
        let src = "{% if vars.x is defined %}x{% endif %}{{ vars.y | default('d') }}";
        assert_eq!(ok(src, &vars(&[])), "d");
    }

    #[test]
    fn does_not_auto_escape() {
        let v = vars(&[("html", s("<a href=\"x\">&</a>"))]);
        assert_eq!(ok("{{ vars.html }}", &v), "<a href=\"x\">&</a>");
    }

    #[test]
    fn undefined_variable_names_it_without_values() {
        let v = vars(&[("password", s("hunter2")), ("port", VarValue::Int(80))]);
        assert_eq!(
            err("{{ vars.password }}\nport {{ vars.prot }}\n", &v),
            "line 2: undefined variable `vars.prot`"
        );
        let why = err("{{ vars.password }} {{ vars.port.x.y }}", &v);
        assert_eq!(why, "line 1: undefined variable `vars.port.x`");
        assert!(!why.contains("hunter2") && !why.contains("80"));
        assert_eq!(
            err("{{ facts.nope }}", &v),
            "line 1: undefined variable `facts.nope`"
        );
    }

    #[test]
    fn undefined_names_match_whole_paths_and_skip_guards() {
        let v = vars(&[("port", VarValue::Int(80))]);
        assert_eq!(
            err("{{ vars.port }} {{ vars.p }}", &v),
            "line 1: undefined variable `vars.p`"
        );
        let src = "{% if vars.x is defined %}x{% endif %}{{ vars.y | default(1) }}{{ vars.z }}";
        assert_eq!(err(src, &v), "line 1: undefined variable `vars.z`");
    }

    #[test]
    fn replace_filter_is_removed() {
        let why = err("{{ 'a' | replace('a', 'b') }}", &vars(&[]));
        assert!(why.starts_with("line 1: "), "{why}");
        assert!(!why.contains('b'), "{why}");
    }

    #[test]
    fn undefined_loop_attribute_gives_the_line() {
        let v = vars(&[("users", VarValue::Array(vec![s("root")]))]);
        assert_eq!(
            err("{% for u in vars.users %}\n{{ u.name }}\n{% endfor %}", &v),
            "line 2: undefined variable"
        );
    }

    #[test]
    fn syntax_error_gives_the_line() {
        let why = check("ok\n{{ vars. }}\n").unwrap_err();
        assert!(why.starts_with("line 2: syntax error: "), "{why}");
        assert_eq!(err("ok\n{{ vars. }}\n", &vars(&[])), why);
        assert_eq!(check("{{ vars.a }}\n{% if x %}y{% endif %}\n"), Ok(()));
    }

    #[test]
    fn include_import_and_extends_are_refused() {
        for (src, stmt) in [
            ("a\n{% include 'other' %}", "include"),
            ("a\n{% import 'other' as o %}", "import"),
            ("a\n{% from 'other' import x %}", "from"),
            ("a\n{% extends 'base' %}", "extends"),
        ] {
            let want = format!("line 2: {stmt} is not allowed");
            assert_eq!(check(src), Err(want.clone()));
            assert_eq!(err(src, &vars(&[])), want);
        }
    }

    #[test]
    fn fuel_exhaustion_is_refused() {
        let src =
            "{% for i in range(100000) %}{% for j in range(100000) %}{% endfor %}{% endfor %}";
        assert_eq!(err(src, &vars(&[])), "fuel exhausted");
    }

    #[test]
    fn output_over_the_limit_is_refused() {
        let src = "{% for i in range(2000) %}{{ 'x' * 1000 }}{% endfor %}";
        assert_eq!(
            err(src, &vars(&[])),
            "rendered output exceeds the 1 MiB limit"
        );
        let at_limit = format!("{{{{ 'x' * {MAX_BYTES} }}}}");
        assert_eq!(ok(&at_limit, &vars(&[])).len() as u64, MAX_BYTES);
    }

    #[test]
    fn writer_stops_at_the_limit() {
        use std::io::Write;
        let mut out = Capped::new();
        let reserved = (out.buf.as_ptr(), out.buf.capacity());
        let chunk = [b'x'; 4096];
        while out.write_all(&chunk).is_ok() {}
        assert!(out.over);
        assert_eq!((out.buf.as_ptr(), out.buf.capacity()), reserved);
        assert_eq!(out.buf.len() as u64, MAX_BYTES);
        assert!(out.write_all(b"y").is_err());
    }

    #[test]
    fn output_keeps_no_spare_capacity() {
        let out = render(
            "{{ vars.a }}\n",
            &vars(&[("a", s("short"))]),
            &facts(),
            &BTreeMap::new(),
        )
        .unwrap()
        .text;
        assert_eq!(*out, "short\n");
        assert!(out.capacity() <= out.len() + 64, "{}", out.capacity());
    }

    #[test]
    fn trims_and_lstrips_blocks() {
        let v = vars(&[("on", VarValue::Bool(true))]);
        assert_eq!(
            ok("a\n    {% if vars.on %}\nb\n    {% endif %}\nc\n", &v),
            "a\nb\nc\n"
        );
    }

    #[test]
    fn keeps_the_trailing_newline_verbatim() {
        let v = vars(&[]);
        assert_eq!(ok("x\n", &v), "x\n");
        assert_eq!(ok("x\n\n", &v), "x\n\n");
        assert_eq!(ok("x", &v), "x");
    }
}
