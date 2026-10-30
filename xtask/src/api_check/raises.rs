//! php's raises held in each crate's own sources (#3403, #3656), a hold of
//! the crate's `api-check` reading, so the reading of the crate that raises
//! is the one a raise fails and no crate's test reads another's sources. A
//! raise names its class by the identity the engine resolved once, never
//! by a name looked up as it throws: php passes a `zend_class_entry *`,
//! `zend_ce_error` or an extension's `*_ce_*`, kept from `MINIT`, and so
//! does every raising call here, a `ClassId` the engine fixes for Zend's
//! classes or one an extension resolved at its engine start
//! (`Start::class`). A string or a lookup of the linked table in the class
//! argument is refused.
//!
//! The pinned php has two print paths: `zend_throw_error`, which prints
//! twice, and `zend_type_error`, `zend_value_error` and
//! `zend_argument_count_error`, which hold one `zend_vspprintf` buffer
//! (`Z/Zend/zend.c:1825-1913`). php-src throws these three classes with
//! a free sentence through `zend_throw_error` at three sites only:
//! `zend_missing_arg_error`'s two "Too few arguments" sentences
//! (`Z/Zend/zend_execute.c:1401`, `:1411`, one call here) and
//! `Dom\TokenList`'s "does not define any supported tokens"
//! (`Z/ext/dom/token_list.c:660`). Everywhere else they are
//! `raising::type_error`, `raising::value_error` and
//! `raising::argument_count_error`, so a `throw_error` naming one of them
//! outside those sites is the wrong path. An identity is read by its last
//! name, `ClassId::TYPE_ERROR` or an extension's `ce.type_error`.
//!
//! The raising calls are `php_engine::api::raising`'s, which every crate
//! that raises is built from, so a change to them moves every such crate's
//! key and this hold reads it again; `php_engine`'s own reading fails
//! where one of [`RAISERS`] is no longer a function of it.

use std::path::Path;

use super::{surfaces::Held, workspace::Present};

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
/// (`Z/ext/reflection/php_reflection.c:7533`). Everywhere else a `throw`
/// naming one is the wrong path: php throws it through `zend_throw_error`,
/// `raising::type_error` or their kin.
const EXCEPTION_SITES: [(&str, usize); 3] = [
    ("ext/ftp/src/lib.rs", 1),
    ("ext/hash/src/natives.rs", 2),
    ("ext/reflection/src/attr.rs", 3),
];

/// The calls counted against php's sites, by their place in [`RAISERS`],
/// with the classes counted and the sites that hold them: `throw_error`
/// and `throw_error_chained` of a typed class, and `throw` of an error
/// class.
const COUNTED: [(&[usize], Classes, &[Site], &str); 2] = [
    (&[1, 3], &TYPED, &PHP_SITES, "typed throw_error calls"),
    (&[0], &ERRORS, &EXCEPTION_SITES, "error-class throws"),
];

/// A file of php's sites, from the root, and its count of calls.
type Site = (&'static str, usize);

/// Classes by an identity's last name.
type Classes = &'static [&'static str];

/// The crate whose `api::raising` defines [`RAISERS`].
const ENGINE: &str = "php_engine";

/// What the raises of `krate`, under the workspace at `root`, make of its
/// reading; the harness's sources, which spell raises as examples, are
/// not read.
pub(crate) fn hold(root: &Path, krate: &Present) -> anyhow::Result<Held> {
    let mut held = Held::default();
    if krate.name == crate::layout::HARNESS {
        return Ok(held);
    }
    let mut all = String::new();
    for file in krate.sources()? {
        let source = std::fs::read_to_string(&file)?;
        let shown = file.strip_prefix(root).unwrap_or(&file).to_string_lossy();
        held.problems.extend(of_file(&shown, &source));
        all.push_str(&source);
    }
    // A stand-in that defines none of them, as a fixture's, holds nothing.
    let defines = |n: &&str| all.contains(&format!("fn {n}(")) || all.contains(&format!("fn {n}<"));
    let names = RAISERS.map(|(call, _, _)| call.trim_end_matches('('));
    let problems = &mut held.problems;
    if krate.name == ENGINE && names.iter().any(defines) {
        for name in names.iter().filter(|n| !defines(n)) {
            problems.push(format!("{ENGINE} defines no raise `{name}`"));
        }
    }
    Ok(held)
}

/// What is wrong with the raises of one file, `shown` from the root: a
/// class named by a name, and a count of typed `throw_error` calls or of
/// `throw` calls naming an error class other than php's sites hold.
fn of_file(shown: &str, source: &str) -> Vec<String> {
    let mut wrong = Vec::new();
    for class in classes_of(source, &RAISERS) {
        if by_name(&class) {
            wrong.push(format!("{shown}: a raise names its class as {class}"));
        }
    }
    for (calls, wanted, sites, what) in COUNTED {
        let names: Vec<_> = calls.iter().map(|&n| RAISERS[n]).collect();
        let classes = classes_of(source, &names).into_iter().map(|c| handle(&c));
        let counted = classes.filter(|c| wanted.contains(&c.as_str())).count();
        let allowed = sites.iter().find(|s| s.0 == shown).map_or(0, |s| s.1);
        if counted != allowed {
            wrong.push(format!("{shown}: {counted} {what}, php's sites {allowed}"));
        }
    }
    wrong
}

/// The class argument of each call in `source` to one of `names`, a
/// call of another count of arguments left out.
fn classes_of(source: &str, names: &[(&str, usize, usize)]) -> Vec<String> {
    let mut found = Vec::new();
    for &(name, at, count) in names {
        for (i, _) in source.match_indices(name) {
            let before = &source[..i];
            let line = &before[before.rfind('\n').map_or(0, |n| n + 1)..];
            let last = before.chars().next_back();
            let word = last.is_some_and(|c| c.is_alphanumeric() || c == '_');
            if word || line.trim_start().starts_with("//") || line.trim_end().ends_with("fn") {
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
    let lookups = ["\"", "lookup_class(", "class_named("];
    lookups.iter().any(|l| argument.contains(l))
}

/// The top-level arguments of a call whose text starts past its `(`.
fn arguments(text: &str) -> Vec<String> {
    let (mut out, mut current) = (Vec::new(), String::new());
    let (mut depth, mut quoted, mut escaped) = (0_u32, false, false);
    for c in text.chars() {
        match (quoted, c) {
            (true, _) if escaped => escaped = false,
            (true, '\\') => escaped = true,
            (_, '"') => quoted = !quoted,
            (false, '(' | '[' | '{') => depth += 1,
            (false, ')' | ']' | '}') if depth == 0 => break,
            (false, ')' | ']' | '}') => depth -= 1,
            (false, ',') if depth == 0 => {
                out.push(std::mem::take(&mut current).trim().to_owned());
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out
}

#[cfg(test)]
mod tests;
