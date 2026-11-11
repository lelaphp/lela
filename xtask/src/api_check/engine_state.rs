//! The sensor of the manifest rule whose `check` names [`CHECK`] (#4348
//! ruling 2, ADR-0005): the seal on `EngineState`. The engine's four
//! entries that keep a value engine-wide are bounded by the trait, and
//! `#[derive(EngineState)]` holds every field of a type to it, so a type
//! holding a pointer into a request's heap does not compile there; what the
//! compiler cannot refuse is an impl written by hand, which holds no field
//! to anything. So a hand-written impl stands only where `unsafe` may, in
//! the homes the `unsafe.homes` rule names: `php_heap`, the trait's own
//! crate, for the primitives, the standard containers and its frozen
//! types, and an extension's `src/ffi/` for a C library's immutable
//! object, beside its `unsafe impl Sync`. Anywhere else the rule's scope
//! covers, one fails the crate's reading, naming its line.
//!
//! A hand-written impl is `impl`, its generics, and a path whose last
//! segment is `EngineState` before `for`, read over the text with its
//! comments and literals blanked, so one a `macro_rules!` body writes is
//! read too; the derive's expansion is in no source, and its own `quote!`
//! names the trait through `#root`, which is no path.

use std::path::Path;

use anyhow::{Context, Result};

use crate::manifest::{Crate, Manifest, Rule};
use crate::rust;
use crate::unsafe_audit::{self, Home};

/// The check the rule's `check` predicate names.
pub(crate) const CHECK: &str = "engine-state";

/// The trait the rule seals.
const TRAIT: &str = "EngineState";

/// Each hand-written impl of the trait in `files`, the sources of
/// `krate` relative to the root, that `rule` covers and no home of
/// `unsafe.homes` does, as `path:line`.
pub(crate) fn outside_the_homes(
    root: &Path,
    manifest: &Manifest,
    rule: &Rule,
    krate: &Crate,
    files: &[String],
) -> Result<Vec<String>> {
    let homes: Vec<Home> = (manifest.rule(unsafe_audit::RULE).iter())
        .flat_map(|rule| rule.predicate.allow.iter().flatten())
        .map(|home| Home::parse(home))
        .collect::<Result<_>>()?;
    let mut found = Vec::new();
    for file in files {
        let path = Path::new(file);
        if !rule.covers(path) || rule.exempts(path) || homes.iter().any(|h| h.covers(krate, file)) {
            continue;
        }
        let text =
            std::fs::read_to_string(root.join(path)).with_context(|| format!("reading {file}"))?;
        found.extend(
            impls(&text)
                .into_iter()
                .map(|line| format!("{file}:{line}")),
        );
    }
    Ok(found)
}

/// The lines of each `impl .. EngineState for` in `text`.
fn impls(text: &str) -> Vec<usize> {
    let code = rust::without_comments_or_literals(text);
    let bytes = code.as_bytes();
    let word = |at: usize, w: &str| {
        code[at..].starts_with(w)
            && (at == 0 || !ident_byte(bytes[at - 1]))
            && bytes.get(at + w.len()).is_none_or(|&b| !ident_byte(b))
    };
    let skip_space = |mut at: usize| {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at
    };
    let mut lines = Vec::new();
    for (at, _) in code.match_indices("impl") {
        if !word(at, "impl") {
            continue;
        }
        let mut next = skip_space(at + "impl".len());
        if bytes.get(next) == Some(&b'<') {
            let mut depth = 0usize;
            while let Some(&b) = bytes.get(next) {
                next += 1;
                match b {
                    b'<' => depth += 1,
                    b'>' if next >= 2 && bytes[next - 2] == b'-' => {}
                    b'>' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            next = skip_space(next);
        }
        let start = next;
        while bytes
            .get(next)
            .is_some_and(|&b| ident_byte(b) || b == b':' || b == b'$')
        {
            next += 1;
        }
        let path = &code[start..next];
        let last = path.rsplit("::").next().unwrap_or(path);
        if last == TRAIT && word(skip_space(next), "for") {
            lines.push(code[..at].matches('\n').count() + 1);
        }
    }
    lines
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::impls;

    #[test]
    fn an_impl_by_any_path_and_generics_is_read_and_its_line_named() {
        let text = "struct A;\nimpl EngineState for A {}\n\
                    impl<T: Send + Option<u8>, F: Fn() -> u8> php_ext::abi::EngineState\n    for B<T, F> {}\n\
                    macro_rules! m { ($t:ty) => { impl $crate::EngineState for $t {} } }\n";
        assert_eq!(impls(text), vec![2, 3, 5]);
    }

    #[test]
    fn a_derive_a_comment_a_string_and_another_trait_are_not_impls() {
        let text = "#[derive(EngineState)]\nstruct A;\n// impl EngineState for A {}\n\
                    const S: &str = \"impl EngineState for A\";\nimpl EngineStateful for A {}\n\
                    impl Send for A {}\nquote!(impl #impl_generics #root::EngineState for #name {});\n\
                    fn implEngineState() {}\n";
        assert_eq!(impls(text), Vec::<usize>::new());
    }
}
