//! The public items of a crate, as lock lines (design §4.5), and the `use`
//! paths of a file. Both are read from the sources with `syn`; nothing is
//! compiled.
//!
//! A lock line is an item's path from the crate root without the crate
//! name: `stack::push`, `abi::Cache`, `abi::Cache::new`. Listed: every
//! `pub` function, type, trait, constant and static reachable through
//! `pub` modules, the `pub` associated items of inherent impls under the
//! type they are for, a trait's items under the trait, and `#[macro_export]`
//! macros. Not listed: modules themselves, enum variants, struct fields,
//! trait impls, and anything `pub(crate)` or narrower.
//!
//! A `pub use` is followed: into this crate's modules, private ones
//! included, and into a workspace crate the `Cargo.toml` depends on, so the
//! facade's lock (`api.lock`, ADR-0004) lists what it re-exports from
//! `php_engine` under the facade's own names. A re-export from a crate
//! outside the workspace is one line, the re-exported name; a glob of one is
//! the line `<module>::*`.
//!
//! Known limits: `#[path]` on a `mod` is not read; an inherent impl is
//! attached to the type of the same name anywhere in its crate; `cfg` is
//! not evaluated, so a `pub` item behind a feature is always listed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use syn::visit::Visit;
use syn::{ImplItem, Item, TraitItem, Type, UseTree, Visibility};

use super::workspace::{Crates, Present};

/// One module of one crate: the crate's manifest name and the module path
/// from the root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModId {
    krate: String,
    path: Vec<String>,
}

impl ModId {
    fn root(krate: &str) -> ModId {
        ModId {
            krate: krate.to_owned(),
            path: Vec::new(),
        }
    }

    fn child(&self, name: &str) -> ModId {
        let mut path = self.path.clone();
        path.push(name.to_owned());
        ModId {
            krate: self.krate.clone(),
            path,
        }
    }

    fn parent(&self) -> ModId {
        let mut path = self.path.clone();
        path.pop();
        ModId {
            krate: self.krate.clone(),
            path,
        }
    }
}

/// A module's items, and where a `mod x;` inside it is looked for.
struct Module {
    items: Vec<Item>,
    dir: PathBuf,
}

/// What a `use` path names.
enum Target {
    Module(ModId),
    Item {
        module: ModId,
        name: String,
    },
    /// Outside the workspace, or not resolvable from the sources.
    Opaque,
}

/// The reader of public items over one workspace, caching the modules it
/// has parsed.
pub struct Inventory<'a> {
    crates: &'a Crates,
    modules: HashMap<ModId, Option<Rc<Module>>>,
    /// Per crate: the `pub` associated items of inherent impls, by the
    /// name of the type they are for.
    assoc: HashMap<String, Rc<HashMap<String, BTreeSet<String>>>>,
}

impl<'a> Inventory<'a> {
    pub fn new(crates: &'a Crates) -> Inventory<'a> {
        Inventory {
            crates,
            modules: HashMap::new(),
            assoc: HashMap::new(),
        }
    }

    /// The public items of `krate`, as lock lines. A crate without
    /// `src/lib.rs` has none.
    pub fn public_items(&mut self, krate: &Present) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        let mut seen = HashSet::new();
        self.collect(&ModId::root(&krate.name), "", &mut out, &mut seen)?;
        Ok(out)
    }

    /// Whether every item of `krate` is a `pub use` or a module of them:
    /// the shape of a facade (`reexports_only`, design §4.2). The
    /// offenders are returned as `kind name` pairs.
    pub fn defines_beyond_reexports(&mut self, krate: &Present) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut todo = vec![ModId::root(&krate.name)];
        while let Some(id) = todo.pop() {
            let Some(module) = self.module(&id)? else {
                continue;
            };
            for item in &module.items {
                match item {
                    Item::Use(_) => {}
                    Item::Mod(m) => todo.push(id.child(&m.ident.to_string())),
                    other => {
                        if let Some((name, _)) = plain_item(other) {
                            out.push(format!("{} {name}", item_kind(other)));
                        } else if let Item::Impl(_) = other {
                            out.push("an impl block".to_owned());
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn collect(
        &mut self,
        id: &ModId,
        prefix: &str,
        out: &mut BTreeSet<String>,
        seen: &mut HashSet<(ModId, String)>,
    ) -> Result<()> {
        if !seen.insert((id.clone(), prefix.to_owned())) {
            return Ok(());
        }
        let Some(module) = self.module(id)? else {
            return Ok(());
        };
        for item in &module.items {
            match item {
                Item::Mod(m) if is_pub(&m.vis) => {
                    let name = m.ident.to_string();
                    self.collect(&id.child(&name), &join(prefix, &name), out, seen)?;
                }
                Item::Use(u) if is_pub(&u.vis) => {
                    for path in flatten(&u.tree) {
                        self.collect_use(id, prefix, &path, out, seen)?;
                    }
                }
                Item::Macro(m) if m.attrs.iter().any(|a| a.path().is_ident("macro_export")) => {
                    if let Some(ident) = &m.ident {
                        out.insert(join(prefix, &ident.to_string()));
                    }
                }
                other => {
                    if let Some((name, vis)) = plain_item(other)
                        && is_pub(vis)
                    {
                        let line = join(prefix, &name);
                        for suffix in self.surface(id, &name)? {
                            out.insert(format!("{line}::{suffix}"));
                        }
                        out.insert(line);
                    }
                }
            }
        }
        Ok(())
    }

    fn collect_use(
        &mut self,
        from: &ModId,
        prefix: &str,
        path: &UsePath,
        out: &mut BTreeSet<String>,
        seen: &mut HashSet<(ModId, String)>,
    ) -> Result<()> {
        let target = self.resolve(from, &path.segs, 0)?;
        match (&path.leaf, target) {
            (Leaf::Glob, Target::Module(m)) => self.collect(&m, prefix, out, seen)?,
            (Leaf::Glob, Target::Item { .. }) => {}
            (Leaf::Glob, Target::Opaque) => {
                out.insert(join(prefix, "*"));
            }
            (Leaf::Name(alias), Target::Module(m)) => {
                self.collect(&m, &join(prefix, alias), out, seen)?;
            }
            (Leaf::Name(alias), Target::Item { module, name }) => {
                let line = join(prefix, alias);
                for suffix in self.surface(&module, &name)? {
                    out.insert(format!("{line}::{suffix}"));
                }
                out.insert(line);
            }
            (Leaf::Name(alias), Target::Opaque) => {
                out.insert(join(prefix, alias));
            }
        }
        Ok(())
    }

    /// The associated items of the item `name` defined in `module`: a
    /// trait's items, and the `pub` items of inherent impls for a type of
    /// that name in the crate.
    fn surface(&mut self, module: &ModId, name: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if let Some(m) = self.module(module)? {
            for item in &m.items {
                if let Item::Trait(t) = item
                    && t.ident == name
                {
                    out.extend(t.items.iter().filter_map(|i| match i {
                        TraitItem::Const(c) => Some(c.ident.to_string()),
                        TraitItem::Fn(f) => Some(f.sig.ident.to_string()),
                        TraitItem::Type(t) => Some(t.ident.to_string()),
                        _ => None,
                    }));
                }
            }
        }
        if let Some(items) = self.assoc(&module.krate)?.get(name) {
            out.extend(items.iter().cloned());
        }
        Ok(out)
    }

    /// The inherent-impl surface of one crate, by type name, read once.
    fn assoc(&mut self, krate: &str) -> Result<Rc<HashMap<String, BTreeSet<String>>>> {
        if let Some(found) = self.assoc.get(krate) {
            return Ok(Rc::clone(found));
        }
        let mut table: HashMap<String, BTreeSet<String>> = HashMap::new();
        let mut todo = vec![ModId::root(krate)];
        let mut visited = HashSet::new();
        while let Some(id) = todo.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            let Some(module) = self.module(&id)? else {
                continue;
            };
            for item in &module.items {
                match item {
                    Item::Mod(m) => todo.push(id.child(&m.ident.to_string())),
                    Item::Impl(i) if i.trait_.is_none() => {
                        let Some(type_name) = self_type_name(&i.self_ty) else {
                            continue;
                        };
                        let entry = table.entry(type_name).or_default();
                        for assoc in &i.items {
                            match assoc {
                                ImplItem::Const(c) if is_pub(&c.vis) => {
                                    entry.insert(c.ident.to_string());
                                }
                                ImplItem::Fn(f) if is_pub(&f.vis) => {
                                    entry.insert(f.sig.ident.to_string());
                                }
                                ImplItem::Type(t) if is_pub(&t.vis) => {
                                    entry.insert(t.ident.to_string());
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let table = Rc::new(table);
        self.assoc.insert(krate.to_owned(), Rc::clone(&table));
        Ok(table)
    }

    /// What `segs`, a `use` path written in module `from`, names.
    fn resolve(&mut self, from: &ModId, segs: &[String], depth: usize) -> Result<Target> {
        if depth > 32 || segs.is_empty() {
            return Ok(Target::Opaque);
        }
        let mut cur;
        let mut rest = segs;
        match segs[0].as_str() {
            "crate" => {
                cur = ModId::root(&from.krate);
                rest = &segs[1..];
            }
            "self" => {
                cur = from.clone();
                rest = &segs[1..];
            }
            "super" => {
                cur = from.clone();
                while rest.first().is_some_and(|s| s == "super") {
                    cur = cur.parent();
                    rest = &rest[1..];
                }
            }
            first => {
                let dep = self
                    .crates
                    .named(&from.krate)
                    .and_then(|c| c.dep_named(first))
                    .map(|d| d.name.clone());
                match dep {
                    Some(name) if self.crates.named(&name).is_some() => {
                        cur = ModId::root(&name);
                        rest = &segs[1..];
                    }
                    _ if self.is_child_module(from, first)? => {
                        // `use entry::*` beside `mod entry;` names the module in scope.
                        cur = from.clone();
                    }
                    _ => return Ok(Target::Opaque),
                }
            }
        }
        for (i, seg) in rest.iter().enumerate() {
            let target = self.lookup(&cur, seg, depth + 1)?;
            if i + 1 == rest.len() {
                return Ok(target);
            }
            match target {
                Target::Module(m) => cur = m,
                _ => return Ok(Target::Opaque),
            }
        }
        Ok(Target::Module(cur))
    }

    fn is_child_module(&mut self, id: &ModId, name: &str) -> Result<bool> {
        Ok(self.module(id)?.is_some_and(|m| {
            m.items
                .iter()
                .any(|i| matches!(i, Item::Mod(m) if m.ident == name))
        }))
    }

    /// What `name` is in module `id`: a child module, an item defined
    /// there, or what a `pub use` there brings in under that name.
    fn lookup(&mut self, id: &ModId, name: &str, depth: usize) -> Result<Target> {
        if depth > 32 {
            return Ok(Target::Opaque);
        }
        let Some(module) = self.module(id)? else {
            return Ok(Target::Opaque);
        };
        for item in &module.items {
            match item {
                Item::Mod(m) if m.ident == name => return Ok(Target::Module(id.child(name))),
                other => {
                    if plain_item(other).is_some_and(|(n, _)| n == name) {
                        return Ok(Target::Item {
                            module: id.clone(),
                            name: name.to_owned(),
                        });
                    }
                }
            }
        }
        let reexports: Vec<UsePath> = module
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Use(u) if is_pub(&u.vis) => Some(flatten(&u.tree)),
                _ => None,
            })
            .flatten()
            .collect();
        for path in &reexports {
            match &path.leaf {
                Leaf::Name(alias) if alias == name => {
                    return self.resolve(id, &path.segs, depth + 1);
                }
                Leaf::Glob => {
                    if let Target::Module(m) = self.resolve(id, &path.segs, depth + 1)?
                        && m != *id
                    {
                        let found = self.lookup(&m, name, depth + 1)?;
                        if !matches!(found, Target::Opaque) {
                            return Ok(found);
                        }
                    }
                }
                Leaf::Name(_) => {}
            }
        }
        Ok(Target::Opaque)
    }

    /// The module `id`, parsed once; `None` when the crate has no library
    /// or the module is not where `mod x;` says.
    fn module(&mut self, id: &ModId) -> Result<Option<Rc<Module>>> {
        if let Some(found) = self.modules.get(id) {
            return Ok(found.clone());
        }
        let loaded = self.load(id)?.map(Rc::new);
        self.modules.insert(id.clone(), loaded.clone());
        Ok(loaded)
    }

    fn load(&mut self, id: &ModId) -> Result<Option<Module>> {
        let Some(name) = id.path.last() else {
            let Some(lib) = self.crates.named(&id.krate).and_then(Present::lib) else {
                return Ok(None);
            };
            let dir = lib.parent().map(Path::to_path_buf).unwrap_or_default();
            return Ok(Some(Module {
                items: parse(&lib)?,
                dir,
            }));
        };
        let Some(parent) = self.module(&id.parent())? else {
            return Ok(None);
        };
        let dir = parent.dir.join(name);
        for item in &parent.items {
            if let Item::Mod(m) = item
                && m.ident == name
            {
                if let Some((_, items)) = &m.content {
                    return Ok(Some(Module {
                        items: items.clone(),
                        dir,
                    }));
                }
                for file in [parent.dir.join(format!("{name}.rs")), dir.join("mod.rs")] {
                    if file.is_file() {
                        return Ok(Some(Module {
                            items: parse(&file)?,
                            dir,
                        }));
                    }
                }
                return Ok(None);
            }
        }
        Ok(None)
    }
}

fn parse(file: &Path) -> Result<Vec<Item>> {
    let text = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let parsed = syn::parse_file(&text).with_context(|| format!("parsing {}", file.display()))?;
    Ok(parsed.items)
}

fn is_pub(vis: &Visibility) -> bool {
    matches!(vis, Visibility::Public(_))
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}::{name}")
    }
}

/// The name and visibility of an item that is one lock line: not a
/// module, a `use`, an impl, a macro or an extern block.
fn plain_item(item: &Item) -> Option<(String, &Visibility)> {
    let (ident, vis) = match item {
        Item::Const(i) => (&i.ident, &i.vis),
        Item::Enum(i) => (&i.ident, &i.vis),
        Item::Fn(i) => (&i.sig.ident, &i.vis),
        Item::Static(i) => (&i.ident, &i.vis),
        Item::Struct(i) => (&i.ident, &i.vis),
        Item::Trait(i) => (&i.ident, &i.vis),
        Item::TraitAlias(i) => (&i.ident, &i.vis),
        Item::Type(i) => (&i.ident, &i.vis),
        Item::Union(i) => (&i.ident, &i.vis),
        _ => return None,
    };
    Some((ident.to_string(), vis))
}

fn item_kind(item: &Item) -> &'static str {
    match item {
        Item::Const(_) => "const",
        Item::Enum(_) => "enum",
        Item::Fn(_) => "fn",
        Item::Static(_) => "static",
        Item::Struct(_) => "struct",
        Item::Trait(_) => "trait",
        Item::TraitAlias(_) => "trait alias",
        Item::Type(_) => "type",
        Item::Union(_) => "union",
        _ => "item",
    }
}

/// The last segment of an inherent impl's self type: `Cache` for
/// `impl Cache<'a>`, `Request` for `impl crate::request::Request`.
fn self_type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        Type::Reference(r) => self_type_name(&r.elem),
        Type::Group(g) => self_type_name(&g.elem),
        Type::Paren(p) => self_type_name(&p.elem),
        _ => None,
    }
}

/// One path a `use` item imports, with its tree flattened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsePath {
    /// The path as written, `self` and `super` included; for a name or a
    /// rename, the last segment is the name imported.
    pub segs: Vec<String>,
    pub leaf: Leaf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leaf {
    /// The name the import is known by in the importing module.
    Name(String),
    Glob,
}

impl UsePath {
    /// The path relative to the crate root for a `crate::`, `self::` or
    /// `super::` import written in the module `at`, else `None`.
    pub fn in_crate(&self, at: &[String]) -> Option<Vec<String>> {
        let mut base: Vec<String>;
        let mut rest = self.segs.as_slice();
        match rest.first().map(String::as_str) {
            Some("crate") => {
                base = Vec::new();
                rest = &rest[1..];
            }
            Some("self") => {
                base = at.to_vec();
                rest = &rest[1..];
            }
            Some("super") => {
                base = at.to_vec();
                while rest.first().is_some_and(|s| s == "super") {
                    base.pop();
                    rest = &rest[1..];
                }
            }
            _ => return None,
        }
        base.extend(rest.iter().cloned());
        Some(base)
    }
}

/// Every `use` path in a file, inline modules and function bodies
/// included.
pub fn uses(file: &Path) -> Result<Vec<UsePath>> {
    let text = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let parsed = syn::parse_file(&text).with_context(|| format!("parsing {}", file.display()))?;
    let mut collector = Collector(Vec::new());
    collector.visit_file(&parsed);
    Ok(collector.0)
}

struct Collector(Vec<UsePath>);

impl<'ast> Visit<'ast> for Collector {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.0.extend(flatten(&item.tree));
    }
}

fn flatten(tree: &UseTree) -> Vec<UsePath> {
    let mut out = Vec::new();
    walk_tree(tree, &mut Vec::new(), &mut out);
    out
}

fn walk_tree(tree: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<UsePath>) {
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            walk_tree(&p.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(n) => {
            let name = n.ident.to_string();
            let (segs, alias) = if name == "self" {
                (prefix.clone(), prefix.last().cloned().unwrap_or(name))
            } else {
                let mut segs = prefix.clone();
                segs.push(name.clone());
                (segs, name)
            };
            out.push(UsePath {
                segs,
                leaf: Leaf::Name(alias),
            });
        }
        UseTree::Rename(r) => {
            let name = r.ident.to_string();
            let mut segs = prefix.clone();
            if name != "self" {
                segs.push(name);
            }
            out.push(UsePath {
                segs,
                leaf: Leaf::Name(r.rename.to_string()),
            });
        }
        UseTree::Glob(_) => out.push(UsePath {
            segs: prefix.clone(),
            leaf: Leaf::Glob,
        }),
        UseTree::Group(g) => {
            for item in &g.items {
                walk_tree(item, prefix, out);
            }
        }
    }
}
