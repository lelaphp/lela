//! php's raises held across the workspace. A raise names its class by
//! the identity the engine resolved once, never by a name looked up as it
//! throws (#3403): php passes a `zend_class_entry *`, `zend_ce_error` or
//! an extension's `*_ce_*`, kept from `MINIT`, and so does every raising
//! call here, a `ClassId` the engine fixes for Zend's classes or one an
//! extension resolved at its engine start (`Start::class`). A string or a
//! lookup of the linked table in the class argument is refused.
//!
//! php-8.5.12 has two print paths: `zend_throw_error`, which prints
//! twice, and `zend_type_error`, `zend_value_error` and
//! `zend_argument_count_error`, which hold one `zend_vspprintf` buffer
//! (`Z/Zend/zend.c:1825-1913`). php-src throws these three classes with
//! a free sentence through `zend_throw_error` at three sites only:
//! `zend_missing_arg_error`'s two "Too few arguments" sentences
//! (`Z/Zend/zend_execute.c:1401`, `:1411`, one call here) and
//! `Dom\TokenList`'s "does not define any supported tokens"
//! (`Z/ext/dom/token_list.c:660`). Everywhere else they are
//! [`raising::type_error`], [`raising::value_error`] and
//! [`raising::argument_count_error`], so a `throw_error` naming one of
//! them outside those sites is the wrong path. An identity is read by its
//! last name, `ClassId::TYPE_ERROR` or an extension's `ce.type_error`.
//!
//! [`raising::type_error`]: crate::api::raising::type_error
//! [`raising::value_error`]: crate::api::raising::value_error
//! [`raising::argument_count_error`]: crate::api::raising::argument_count_error

use std::path::{Path, PathBuf};

/// The raising calls that name a class, each with the place of its
/// class among its arguments and its count of arguments, so a crate's
/// own function of the same name and another shape is not read as one.
const RAISERS: [(&str, usize, usize); 10] = [
    ("throw(", 1, 3),
    ("throw_error(", 1, 3),
    ("throw_exception_ex(", 1, 3),
    ("throw_error_chained(", 2, 4),
    ("throw_chained(", 2, 4),
    ("throw_at(", 1, 5),
    ("new_throwable(", 1, 3),
    ("catch_fatal(", 2, 3),
    ("argument_error(", 1, 4),
    ("throwing_warnings(", 1, 3),
];

/// The classes php's typed functions throw, by an identity's last name.
const TYPED: [&str; 3] = ["value_error", "type_error", "argument_count_error"];

/// The files that hold php's `zend_throw_error` sites of a typed class,
/// each with its count of calls.
const PHP_SITES: [(&str, usize); 2] = [
    ("crates/php_engine/src/handlers.rs", 1),
    ("ext/dom/src/token_list.rs", 1),
];

/// The classes php throws through `zend_throw_error` and its typed
/// functions, which `zend_throw_exception` names only at the sites
/// [`EXCEPTION_SITES`] counts.
const ERRORS: [&str; 4] = ["error", "value_error", "type_error", "argument_count_error"];

/// The files that hold php's `zend_throw_exception` sites of one of
/// [`ERRORS`], each with its count of calls: `FTP\Connection`'s closed
/// check (`Z/ext/ftp/php_ftp.c:224`), `HashContext`'s constructor and a
/// finalized one's clone (`Z/ext/hash/hash.c:1139`, `:1412`) and the
/// delayed attribute validation's three
/// (`Z/ext/reflection/php_reflection.c:7533`). Everywhere else a
/// `throw` naming one is the wrong path: php throws it through
/// `zend_throw_error`, [`raising::type_error`] or their kin.
///
/// [`raising::type_error`]: crate::api::raising::type_error
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

/// The workspace's sources under `crates`, `ext` and `sapi`, each with
/// its path from the root; this file's own examples left out.
fn workspace() -> Vec<(String, String)> {
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
    files
        .into_iter()
        .filter_map(|file| {
            let source = std::fs::read_to_string(&file).ok()?;
            let shown = file.strip_prefix(&root).unwrap_or(&file);
            let shown = shown.to_string_lossy().into_owned();
            (shown != file!()).then_some((shown, source))
        })
        .collect()
}

/// The class argument of each call in `source` to one of `names`, a
/// call of another count of arguments left out.
fn classes_of(source: &str, names: &[(&str, usize, usize)]) -> Vec<String> {
    let mut found = Vec::new();
    for &(name, at, count) in names {
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
            let arguments = arguments(&source[i + name.len()..]);
            if let (Some(argument), true) = (arguments.get(at), arguments.len() == count) {
                found.push(argument.clone());
            }
        }
    }
    found
}

/// An identity's last name, lowered: `ClassId::TYPE_ERROR` and
/// `ce.type_error` are both `type_error`.
fn handle(argument: &str) -> String {
    let last = argument.rsplit(['.', ':']).next().unwrap_or(argument);
    last.trim().to_ascii_lowercase()
}

/// Whether a class argument names its class by a name: a string
/// in it, or a lookup of the linked table.
fn by_name(argument: &str) -> bool {
    argument.contains('"')
        || argument.contains("lookup_class(")
        || argument.contains("class_named(")
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

/// Each file whose count of `classes` matching `wanted` is not its
/// allowance in `sites`, with what it counted.
fn miscounted(
    names: &[(&str, usize, usize)],
    wanted: &[&str],
    sites: &[(&str, usize)],
    what: &str,
) -> Vec<String> {
    let mut wrong = Vec::new();
    for (shown, source) in workspace() {
        let counted = classes_of(&source, names)
            .iter()
            .filter(|class| wanted.contains(&handle(class).as_str()))
            .count();
        let allowed = sites
            .iter()
            .find(|(site, _)| *site == shown)
            .map_or(0, |(_, count)| *count);
        if counted != allowed {
            wrong.push(format!("{shown}: {counted} {what}, php's sites {allowed}"));
        }
    }
    wrong
}

#[test]
fn a_raise_names_its_class_by_a_handle_never_by_a_name() {
    let mut wrong = Vec::new();
    for (shown, source) in workspace() {
        for class in classes_of(&source, &RAISERS) {
            if by_name(&class) {
                wrong.push(format!("{shown}: a raise names its class as {class}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn a_typed_class_is_thrown_through_its_typed_function() {
    let names = [("throw_error(", 1, 3), ("throw_error_chained(", 2, 4)];
    let wrong = miscounted(&names, &TYPED, &PHP_SITES, "typed throw_error calls");
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn an_error_class_is_thrown_through_zend_throw_exception_at_phps_sites_alone() {
    let names = [("throw(", 1, 3)];
    let wrong = miscounted(
        &names,
        &ERRORS,
        &EXCEPTION_SITES,
        "throw calls naming an error class",
    );
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_class_argument_is_read_past_nested_calls_and_strings() {
    let source = r#"
        throw_error(req, ClassId::TYPE_ERROR, &format!("a, {b}", c(d, e)));
        // throw_error(req, ClassId::VALUE_ERROR, "a comment");
        php_ext::raising::throw_error_chained(req, prev, ce.argument_count_error, "x, y");
        my_throw_error(req, ce.value_error, "not this one");
        throw(req, "Exception", "by its name");
        throw_exception_ex(req, lookup_class(req, b"Exception")?.unwrap_or(ce.exception), "m");
        throw_error(req, "a crate's own helper of two arguments");
    "#;
    let classes = classes_of(source, &RAISERS);
    assert_eq!(
        classes,
        [
            "\"Exception\"",
            "ClassId::TYPE_ERROR",
            "lookup_class(req, b\"Exception\")?.unwrap_or(ce.exception)",
            "ce.argument_count_error",
        ]
    );
    assert_eq!(
        classes
            .iter()
            .map(|class| handle(class))
            .collect::<Vec<_>>(),
        [
            "\"exception\"",
            "type_error",
            "exception)",
            "argument_count_error"
        ]
    );
    assert_eq!(
        classes
            .iter()
            .map(|class| by_name(class))
            .collect::<Vec<_>>(),
        [true, false, true, false]
    );
}
