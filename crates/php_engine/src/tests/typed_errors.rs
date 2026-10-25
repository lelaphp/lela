//! php's typed throws held across the workspace. php-8.5.12 has two print
//! paths: `zend_throw_error`, which prints twice, and `zend_type_error`,
//! `zend_value_error` and `zend_argument_count_error`, which hold one
//! `zend_vspprintf` buffer (`Z/Zend/zend.c:1825-1913`). php-src throws
//! these three classes with a free sentence through `zend_throw_error` at
//! three sites only: `zend_missing_arg_error`'s two "Too few arguments"
//! sentences (`Z/Zend/zend_execute.c:1401`, `:1411`, one call here) and
//! `Dom\TokenList`'s "does not define any supported tokens"
//! (`Z/ext/dom/token_list.c:660`). Everywhere else they are
//! [`raising::type_error`], [`raising::value_error`] and
//! [`raising::argument_count_error`], so a `throw_error` naming one of
//! them by a quoted name outside those sites is the wrong path.
//!
//! [`raising::type_error`]: crate::api::raising::type_error
//! [`raising::value_error`]: crate::api::raising::value_error
//! [`raising::argument_count_error`]: crate::api::raising::argument_count_error

use std::path::{Path, PathBuf};

/// The classes php's typed functions throw.
const TYPED: [&str; 3] = ["\"ValueError\"", "\"TypeError\"", "\"ArgumentCountError\""];

/// The files that hold php's `zend_throw_error` sites of a typed class,
/// each with its count of calls.
const PHP_SITES: [(&str, usize); 2] = [
    ("crates/php_engine/src/handlers.rs", 1),
    ("ext/dom/src/token_list.rs", 1),
];

/// The classes php throws through `zend_throw_error` and its typed
/// functions, which `zend_throw_exception` names only at the sites
/// [`EXCEPTION_SITES`] counts.
const ERRORS: [&str; 4] = [
    "\"Error\"",
    "\"ValueError\"",
    "\"TypeError\"",
    "\"ArgumentCountError\"",
];

/// The files that hold php's `zend_throw_exception` sites of one of
/// [`ERRORS`], each with its count of calls: `FTP\Connection`'s closed
/// check (`Z/ext/ftp/php_ftp.c:224`), `HashContext`'s constructor and a
/// finalized one's clone (`Z/ext/hash/hash.c:1139`, `:1412`) and the
/// delayed attribute validation's three
/// (`Z/ext/reflection/php_reflection.c:7533`). Everywhere else a
/// `throw` naming one is the wrong path: php throws it through
/// `zend_throw_error`, [`raising::type_error`] or their kin.
const EXCEPTION_SITES: [(&str, usize); 3] = [
    ("ext/ftp/src/lib.rs", 1),
    ("ext/hash/src/natives.rs", 2),
    ("ext/reflection/src/attr.rs", 3),
];

/// Every `.rs` file under `dir`, `target` directories left out.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "target") {
                sources(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The class argument of each call in `source` to one of `names`, each
/// with the place of its class among its arguments.
fn classes_of(source: &str, names: &[(&str, usize)]) -> Vec<String> {
    let mut found = Vec::new();
    for &(name, at) in names {
        for (i, _) in source.match_indices(name) {
            let before = &source[..i];
            if before
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                continue;
            }
            let line = &before[before.rfind('\n').map_or(0, |n| n + 1)..];
            if line.trim_start().starts_with("//") || line.trim_end().ends_with("fn") {
                continue;
            }
            if let Some(argument) = arguments(&source[i + name.len()..]).get(at) {
                found.push(argument.clone());
            }
        }
    }
    found
}

/// The class argument of each `throw_error(` or `throw_error_chained(`
/// call in `source`: the second argument, or the third of a chained one.
fn classes(source: &str) -> Vec<String> {
    classes_of(source, &[("throw_error(", 1), ("throw_error_chained(", 2)])
}

/// The top-level arguments of a call whose text starts past its `(`.
fn arguments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0_u32;
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in text.chars() {
        if quoted {
            current.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                quoted = true;
                current.push(c);
            }
            '(' | '[' | '{' => {
                depth += 1;
                current.push(c);
            }
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut current).trim().to_owned()),
            _ => current.push(c),
        }
    }
    let last = current.trim();
    if !last.is_empty() {
        out.push(last.to_owned());
    }
    out
}

#[test]
fn a_typed_class_is_thrown_through_its_typed_function() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["crates", "ext", "sapi"] {
        sources(&root.join(dir), &mut files);
    }
    assert!(
        files.len() > 100,
        "the workspace's sources under {}",
        root.display()
    );
    let mut wrong = Vec::new();
    for file in files {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        let typed = classes(&source)
            .iter()
            .filter(|class| TYPED.contains(&class.as_str()))
            .count();
        let shown = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .into_owned();
        // This file's examples are the reader's own test, below.
        if shown == file!() {
            continue;
        }
        let allowed = PHP_SITES
            .iter()
            .find(|(site, _)| *site == shown)
            .map_or(0, |(_, count)| *count);
        if typed != allowed {
            wrong.push(format!(
                "{shown}: {typed} typed throw_error calls, php's sites {allowed}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_class_argument_is_read_past_nested_calls_and_strings() {
    let source = r#"
        throw_error(req, "TypeError", &format!("a, {b}", c(d, e)));
        // throw_error(req, "ValueError", "a comment");
        php_ext::raising::throw_error_chained(req, prev, "ArgumentCountError", "x, y");
        my_throw_error(req, "ValueError", "not this one");
    "#;
    assert_eq!(classes(source), ["\"TypeError\"", "\"ArgumentCountError\""]);
}

#[test]
fn an_error_class_is_thrown_through_zend_throw_exception_at_phps_sites_alone() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["crates", "ext", "sapi"] {
        sources(&root.join(dir), &mut files);
    }
    let mut wrong = Vec::new();
    for file in files {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        let thrown = classes_of(&source, &[("throw(", 1)])
            .iter()
            .filter(|class| ERRORS.contains(&class.as_str()))
            .count();
        let shown = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .into_owned();
        let allowed = EXCEPTION_SITES
            .iter()
            .find(|(site, _)| *site == shown)
            .map_or(0, |(_, count)| *count);
        if thrown != allowed {
            wrong.push(format!(
                "{shown}: {thrown} throw calls naming an error class, php's sites {allowed}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
