//! What one extension declares it requires, read from its library's
//! sources as `api-check` reads the bundle's `cfg`s: the rows of php's
//! `zend_module_dep` list (`ZEND_MOD_REQUIRED`, `ZEND_MOD_OPTIONAL`,
//! `ZEND_MOD_CONFLICTS`) an extension hands `ExtensionBuilder::dependencies`
//! (#1915).
//!
//! A row is read when it is written as one of:
//!
//! - a `Dependency { name, kind, .. }` literal, its `name` holding the
//!   module's name as a string literal (`String::from("spl")`,
//!   `"spl".into()`) and its `kind` a `DependencyKind::...` path;
//! - a call `f("spl", DependencyKind::Required)`, or `f("spl")` to a
//!   function or closure of the crate whose body names one kind.
//!
//! The rows are read from the argument of every `.dependencies(...)` call
//! in the library: a `vec![...]` or an array of rows, or a call with no
//! argument to a function of the crate that returns one. Anything else is
//! named in the reading as a declaration the instrument does not read,
//! never taken for "requires nothing".

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Expr, Token};

use super::super::module_of;
use super::super::workspace::Present;

/// The rows of one extension's declared dependencies the bundle is held
/// to.
#[derive(Debug, Default)]
pub(super) struct Declared {
    /// Each module declared `Required`, as the manifest name of its
    /// extension's crate: `spl` is `ext/spl`.
    pub(super) required: BTreeSet<String>,
    /// Where a declaration stands that the instrument does not read, as
    /// the file relative to the crate.
    pub(super) unread: Vec<String>,
}

impl Declared {
    /// Read every `.dependencies(...)` call in `krate`'s library sources.
    pub(super) fn read(krate: &Present) -> Result<Declared> {
        let mut bodies: BTreeMap<String, Expr> = BTreeMap::new();
        let mut calls: Vec<(String, Expr)> = Vec::new();
        for file in krate.sources()? {
            if module_of(krate, &file).is_none() {
                continue;
            }
            let text = crate::read(&file)?;
            if !text.contains("dependencies(") && !text.contains("Dependency") {
                continue;
            }
            let parsed = crate::rust::parsed(&text, &file)?;
            let mut scan = Scan::default();
            scan.visit_file(&parsed);
            let at = krate.relative(&file).display().to_string();
            calls.extend(scan.calls.into_iter().map(|expr| (at.clone(), expr)));
            bodies.extend(scan.bodies);
        }
        let mut declared = Declared::default();
        for (at, argument) in calls {
            let Some(rows) = rows(&argument, &bodies, 0) else {
                declared.unread.push(at);
                continue;
            };
            for row in rows {
                match read_row(&row, &bodies) {
                    Some((name, Kind::Required)) => {
                        declared
                            .required
                            .insert(format!("ext/{}", name.to_ascii_lowercase()));
                    }
                    Some((_, Kind::Optional | Kind::Conflicts)) => {}
                    None => declared.unread.push(at.clone()),
                }
            }
        }
        declared.unread.dedup();
        Ok(declared)
    }
}

/// `DependencyKind`'s three values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Required,
    Optional,
    Conflicts,
}

impl Kind {
    /// The kind a path names, if it is `DependencyKind::<kind>`.
    fn of(path: &syn::Path) -> Option<Kind> {
        let mut segments = path.segments.iter().rev();
        let value = segments.next()?;
        if segments.next()?.ident != "DependencyKind" {
            return None;
        }
        match value.ident.to_string().as_str() {
            "Required" => Some(Kind::Required),
            "Optional" => Some(Kind::Optional),
            "Conflicts" => Some(Kind::Conflicts),
            _ => None,
        }
    }
}

/// How deep a `.dependencies(...)` argument is followed through the
/// crate's functions before it is named as unread.
const DEPTH: usize = 4;

/// The rows an argument of `.dependencies(...)` lists, or `None` for one
/// of a shape the instrument does not read.
fn rows(expr: &Expr, bodies: &BTreeMap<String, Expr>, depth: usize) -> Option<Vec<Expr>> {
    if depth > DEPTH {
        return None;
    }
    match expr {
        Expr::Macro(m) if m.mac.path.is_ident("vec") => m
            .mac
            .parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
            .ok()
            .map(|rows| rows.into_iter().collect()),
        Expr::Array(array) => Some(array.elems.iter().cloned().collect()),
        Expr::Paren(inner) => rows(&inner.expr, bodies, depth),
        Expr::Reference(inner) => rows(&inner.expr, bodies, depth),
        Expr::Block(block) => rows(tail(&block.block)?, bodies, depth + 1),
        Expr::Call(call) if call.args.is_empty() => {
            rows(bodies.get(&callee(call)?)?, bodies, depth + 1)
        }
        _ => None,
    }
}

/// The module's name and kind one row declares, or `None` for a row of a
/// shape the instrument does not read.
fn read_row(row: &Expr, bodies: &BTreeMap<String, Expr>) -> Option<(String, Kind)> {
    match row {
        Expr::Paren(inner) => read_row(&inner.expr, bodies),
        Expr::Struct(literal) => {
            let field = |name: &str| {
                literal.fields.iter().find_map(|f| match &f.member {
                    syn::Member::Named(ident) if ident == name => Some(&f.expr),
                    _ => None,
                })
            };
            let name = Found::in_expr(field("name")?).single_str()?;
            let kind = Found::in_expr(field("kind")?).single_kind()?;
            Some((name, kind))
        }
        Expr::Call(call) => {
            let mut found = Found::default();
            for arg in &call.args {
                found.visit_expr(arg);
            }
            let name = found.strs.first()?.clone();
            let kind = match found.single_kind() {
                Some(kind) => kind,
                None if found.kinds.is_empty() => {
                    Found::in_expr(bodies.get(&callee(call)?)?).single_kind()?
                }
                None => return None,
            };
            Some((name, kind))
        }
        _ => None,
    }
}

/// The name a call's function is, when it is one identifier.
fn callee(call: &syn::ExprCall) -> Option<String> {
    match &*call.func {
        Expr::Path(path) => path.path.get_ident().map(ToString::to_string),
        _ => None,
    }
}

/// A block's value: its last statement, an expression with no `;`.
fn tail(block: &syn::Block) -> Option<&Expr> {
    match block.stmts.last()? {
        syn::Stmt::Expr(expr, None) => Some(expr),
        _ => None,
    }
}

/// The string literals and `DependencyKind` paths inside one expression,
/// in the order written.
#[derive(Default)]
struct Found {
    strs: Vec<String>,
    kinds: Vec<Kind>,
}

impl Found {
    fn in_expr(expr: &Expr) -> Found {
        let mut found = Found::default();
        found.visit_expr(expr);
        found
    }

    /// The one string literal written, if there is exactly one.
    fn single_str(self) -> Option<String> {
        match <[String; 1]>::try_from(self.strs) {
            Ok([one]) => Some(one),
            Err(_) => None,
        }
    }

    /// The one kind named, however often, if exactly one is.
    fn single_kind(&self) -> Option<Kind> {
        let first = *self.kinds.first()?;
        self.kinds.iter().all(|k| *k == first).then_some(first)
    }
}

impl<'ast> Visit<'ast> for Found {
    fn visit_lit_str(&mut self, lit: &'ast syn::LitStr) {
        self.strs.push(lit.value());
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.kinds.extend(Kind::of(path));
        visit::visit_path(self, path);
    }
}

/// Of one file: each `.dependencies(...)` call's argument, and each
/// function or `let`-bound closure by name, its body as an expression.
#[derive(Default)]
struct Scan {
    calls: Vec<Expr>,
    bodies: BTreeMap<String, Expr>,
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "dependencies"
            && let [argument] = node.args.iter().collect::<Vec<_>>().as_slice()
        {
            self.calls.push((*argument).clone());
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.bodies.insert(
            node.sig.ident.to_string(),
            Expr::Block(syn::ExprBlock {
                attrs: Vec::new(),
                label: None,
                block: (*node.block).clone(),
            }),
        );
        visit::visit_item_fn(self, node);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let syn::Pat::Ident(name) = &node.pat
            && let Some(init) = &node.init
            && let Expr::Closure(closure) = &*init.expr
        {
            self.bodies
                .insert(name.ident.to_string(), (*closure.body).clone());
        }
        visit::visit_local(self, node);
    }
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {

    use super::*;

    /// What the tracked `ext/<name>` declares `Required`, and where it
    /// declares what the instrument does not read.
    fn declared(name: &str) -> (Vec<String>, Vec<String>) {
        let dir = crate::package_dir().to_path_buf().join("../ext").join(name);
        let krate = Present {
            name: format!("ext/{name}"),
            dir,
            package: format!("php_ext_{name}"),
            deps: Vec::new(),
            inherits_lints: true,
        };
        let read = Declared::read(&krate).unwrap();
        (read.required.into_iter().collect(), read.unread)
    }

    /// Each shape the tracked extensions write their rows in is read:
    /// a literal in a `vec!` (`ext/pdo_sqlite`), rows a function returns
    /// (standard), a closure taking the kind (phar), a function fixing it
    /// (mysqli, xsl), and Optional and Conflicts rows left out (dom).
    /// sqlite3 declares nothing, so links nothing into the bundle (#1915).
    #[test]
    fn the_tracked_extensions_declare_php_s_required_rows() {
        let none: Vec<String> = Vec::new();
        for (name, required) in [
            ("pdo_sqlite", &["ext/pdo"][..]),
            ("standard", &["ext/uri"]),
            ("phar", &["ext/hash", "ext/spl"]),
            ("mysqli", &["ext/mysqlnd", "ext/spl"]),
            ("xsl", &["ext/dom", "ext/libxml"]),
            ("dom", &["ext/lexbor", "ext/libxml"]),
            ("session", &[]),
            ("sqlite3", &[]),
        ] {
            assert_eq!(
                declared(name),
                (
                    required.iter().map(|s| (*s).to_owned()).collect(),
                    none.clone()
                ),
                "{name}"
            );
        }
    }
}
