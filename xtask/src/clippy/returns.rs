//! The sensor of the manifest rule whose `check` names [`CHECK`],
//! `tests.no-silent-return` (#3079): a test that returns early passes
//! without asserting, so a `return` at a `#[test]` fn's own level fails
//! the crate's reading, naming the site, unless it is `return Err(..)`.
//! A return inside a closure, an async block or an item nested in the
//! body is that one's own and not the test's. Every `.rs` file of the
//! crate the rule's scope covers is read, its test modules and `tests/`
//! alike; a fn is a test by an attribute whose last segment is `test`,
//! so `#[test]` and `#[tokio::test]` both are.

use std::path::Path;

use anyhow::{Context, Result};
use syn::visit::{self, Visit};

use crate::manifest::{Crate, Rule};

/// The lint the rule's `check` names.
pub(super) const CHECK: &str = "silent-return";

/// Each test-level return in `krate`'s sources `rule` covers, as
/// `file:line`; `None` when the rule covers no source of the crate.
pub(super) fn sites(root: &Path, krate: &Crate, rule: &Rule) -> Result<Option<Vec<String>>> {
    let mut covered = None;
    for file in crate::git::files(root, &krate.path())? {
        let path = Path::new(&file);
        if path.extension().is_none_or(|e| e != "rs") || !rule.covers(path) || rule.exempts(path) {
            continue;
        }
        let text =
            std::fs::read_to_string(root.join(path)).with_context(|| format!("reading {file}"))?;
        let mut tests = Tests::default();
        tests.visit_file(&crate::rust::parsed(&text, path)?);
        let sites = covered.get_or_insert_with(Vec::new);
        sites.extend(tests.0.into_iter().map(|line| format!("{file}:{line}")));
    }
    Ok(covered)
}

/// The lines of the returns at each test's own level.
#[derive(Default)]
struct Tests(Vec<usize>);

impl<'ast> Visit<'ast> for Tests {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        let test = f
            .attrs
            .iter()
            .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"));
        if test {
            Body(&mut self.0).visit_block(&f.block);
        } else {
            visit::visit_item_fn(self, f);
        }
    }
}

/// One test's body, walked to its own level and no further.
struct Body<'a>(&'a mut Vec<usize>);

impl<'ast> Visit<'ast> for Body<'_> {
    fn visit_expr_return(&mut self, r: &'ast syn::ExprReturn) {
        let err = matches!(r.expr.as_deref(), Some(syn::Expr::Call(call))
            if matches!(&*call.func, syn::Expr::Path(p)
                if p.path.segments.last().is_some_and(|s| s.ident == "Err")));
        if !err {
            self.0.push(r.return_token.span.start().line);
        }
        visit::visit_expr_return(self, r);
    }

    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}

    fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}

    fn visit_item(&mut self, _: &'ast syn::Item) {}
}
