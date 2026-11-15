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
//! An inherent impl is attached to the type its self type resolves to from
//! the module the impl is in: a plain name is the type defined there or
//! brought in by a `use` there, a glob included, and `crate::`, `self::`
//! and `super::` paths are followed, so two types of one name in one crate
//! each list their own methods alone.
//!
//! Each line is also known by the module that defines it
//! ([`Inventory::defining_modules`]): an item's by the module it is
//! written in, after every `pub use` is followed; an inherent impl's item
//! by the module the impl block is written in, which need not be the
//! type's; a trait's item by the trait's; a re-export from outside the
//! workspace by the module whose `pub use` writes it. A type whose
//! methods are spread over one file per area is counted per file.
//!
//! A module that `include!`s a file generated into `OUT_DIR`, and so not
//! read, is taken to define every name a path asks of it by name that
//! nothing there defines or may import, never one a glob through it asks: `pub use opcode::Opcode` beside
//! `mod opcode { include!(...) }` names a type of that module, and the
//! inherent impls written on `Opcode` elsewhere in the crate list under
//! it.
//!
//! Known limits: `#[path]` on a `mod` is not read; `cfg` is not evaluated,
//! so a `pub` item behind a feature is always listed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use syn::visit::Visit;
use syn::{ImplItem, Item, TraitItem, Type, UseTree, Visibility};

use super::workspace::{Crates, Present};
use crate::reading::joined;
use crate::rust::parse_file;

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

/// The inherent-impl surface of one crate: the `pub` associated items
/// under the module and name of the type each impl resolves to, each with
/// the module its impl block is written in.
type AssocTable = HashMap<(ModId, String), BTreeMap<String, ModId>>;

/// Lock lines, each with the module that defines it; the first path that
/// reaches a line names its module.
type Lines = BTreeMap<String, ModId>;

/// A module's items, and where a `mod x;` inside it is looked for.
struct Module {
    items: Vec<Item>,
    dir: PathBuf,
}

impl Module {
    /// The paths of the module's `use` items, flattened; its `pub use`
    /// items' alone with `public`.
    fn uses(&self, public: bool) -> Vec<UsePath> {
        (self.items.iter())
            .filter_map(|item| match item {
                Item::Use(u) if !public || is_pub(&u.vis) => Some(flatten(&u.tree)),
                _ => None,
            })
            .flatten()
            .collect()
    }
}

/// What a `use` path names.
enum Target {
    Module(ModId),
    Item {
        module: ModId,
        name: String,
    },
    /// A child module of `module` and an item there of one name, which
    /// rustc keeps in two namespaces, `mod x;` beside `pub fn x`:
    /// a path goes on through the module, and a `use` of the name brings
    /// in the item and, where it is `pub`, the module.
    Both {
        module: ModId,
        name: String,
        public: bool,
    },
    /// Outside the workspace, or not resolvable from the sources.
    Opaque,
}

impl Target {
    /// The module a path goes on through, or a glob reads.
    fn module(self) -> Option<ModId> {
        match self {
            Target::Module(m) => Some(m),
            Target::Both { module, name, .. } => Some(module.child(&name)),
            Target::Item { .. } | Target::Opaque => None,
        }
    }
}

/// The reader of public items over one workspace, caching the modules it
/// has parsed.
pub struct Inventory<'a> {
    crates: &'a Crates,
    modules: HashMap<ModId, Option<Rc<Module>>>,
    /// Per crate: the `pub` associated items of inherent impls, by the
    /// module and name of the type each resolves to.
    assoc: HashMap<String, Rc<AssocTable>>,
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
        Ok(self.lines(krate)?.into_keys().collect())
    }

    /// The public items of `krate`, as lock lines, each with the module
    /// that defines it as a path from its crate's name:
    /// `php_engine::api::request::hooks` for a method of `Request` written
    /// in that file.
    pub fn defining_modules(&mut self, krate: &Present) -> Result<BTreeMap<String, String>> {
        Ok(self
            .lines(krate)?
            .into_iter()
            .map(|(line, module)| (line, self.shown(&module)))
            .collect())
    }

    /// Whether `segs`, a path from the root of `krate`, names a module:
    /// the root itself where `segs` is empty.
    pub fn names_module(&mut self, krate: &Present, segs: &[String]) -> Result<bool> {
        let path: Vec<String> = std::iter::once("self".to_owned())
            .chain(segs.iter().cloned())
            .collect();
        let root = ModId::root(&krate.name);
        Ok(self.resolve(&root, &path, 0)?.module().is_some())
    }

    fn lines(&mut self, krate: &Present) -> Result<Lines> {
        let mut out = Lines::new();
        let mut seen = HashSet::new();
        self.collect(&ModId::root(&krate.name), "", &mut out, &mut seen)?;
        Ok(out)
    }

    /// A module as a path from its crate's name as code spells it.
    fn shown(&self, id: &ModId) -> String {
        let krate = self
            .crates
            .named(&id.krate)
            .map_or_else(|| id.krate.clone(), Present::ident);
        joined(std::iter::once(krate).chain(id.path.iter().cloned()), "::")
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
                        if let Some((kind, name, ..)) = plain_item(other) {
                            out.push(format!("{kind} {name}"));
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
        out: &mut Lines,
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
                        list_first(out, join(prefix, &ident.to_string()), id);
                    }
                }
                other => {
                    if let Some((_, name, vis, _)) = plain_item(other)
                        && is_pub(vis)
                    {
                        self.collect_item(id, &name, &join(prefix, &name), out)?;
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
        out: &mut Lines,
        seen: &mut HashSet<(ModId, String)>,
    ) -> Result<()> {
        let target = self.resolve(from, &path.segs, 0)?;
        match (&path.leaf, target) {
            (Leaf::Glob, Target::Opaque) => list_first(out, join(prefix, "*"), from),
            (Leaf::Glob, target) => {
                if let Some(m) = target.module() {
                    self.collect(&m, prefix, out, seen)?;
                }
            }
            (Leaf::Name(alias), Target::Module(m)) => {
                self.collect(&m, &join(prefix, alias), out, seen)?;
            }
            (Leaf::Name(alias), Target::Item { module, name }) => {
                self.collect_item(&module, &name, &join(prefix, alias), out)?;
            }
            (
                Leaf::Name(alias),
                Target::Both {
                    module,
                    name,
                    public,
                },
            ) => {
                let line = join(prefix, alias);
                self.collect_item(&module, &name, &line, out)?;
                if public {
                    self.collect(&module.child(&name), &line, out, seen)?;
                }
            }
            (Leaf::Name(alias), Target::Opaque) => list_first(out, join(prefix, alias), from),
        }
        Ok(())
    }

    /// The item `name` defined in `module` listed as `line`, after its
    /// associated items ([`Inventory::surface`]).
    fn collect_item(
        &mut self,
        module: &ModId,
        name: &str,
        line: &str,
        out: &mut Lines,
    ) -> Result<()> {
        for (suffix, at) in self.surface(module, name)? {
            list_first(out, format!("{line}::{suffix}"), &at);
        }
        list_first(out, line.to_owned(), module);
        Ok(())
    }

    /// The associated items of the item `name` defined in `module`: a
    /// trait's items, and the `pub` items of the inherent impls that
    /// resolve to that type; each with the module that defines it, the
    /// trait's or the impl block's.
    fn surface(&mut self, module: &ModId, name: &str) -> Result<Vec<(String, ModId)>> {
        let mut out = Vec::new();
        if let Some(m) = self.module(module)? {
            for item in &m.items {
                if let Item::Trait(t) = item
                    && t.ident == name
                {
                    out.extend(
                        t.items
                            .iter()
                            .filter_map(|i| match i {
                                TraitItem::Const(c) => Some(c.ident.to_string()),
                                TraitItem::Fn(f) => Some(f.sig.ident.to_string()),
                                TraitItem::Type(t) => Some(t.ident.to_string()),
                                _ => None,
                            })
                            .map(|item| (item, module.clone())),
                    );
                }
            }
        }
        let key = (module.clone(), name.to_owned());
        if let Some(items) = self.assoc(&module.krate)?.get(&key) {
            out.extend(items.iter().map(|(item, at)| (item.clone(), at.clone())));
        }
        Ok(out)
    }

    /// The inherent-impl surface of one crate, by the module and name of
    /// the type each impl resolves to, read once.
    fn assoc(&mut self, krate: &str) -> Result<Rc<AssocTable>> {
        if let Some(found) = self.assoc.get(krate) {
            return Ok(Rc::clone(found));
        }
        let mut table = AssocTable::new();
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
                        let Some(segs) = self_type_path(&i.self_ty) else {
                            continue;
                        };
                        let key = self.self_type(&id, &module, &segs)?;
                        let entry = table.entry(key).or_default();
                        for assoc in &i.items {
                            let name = match assoc {
                                ImplItem::Const(c) if is_pub(&c.vis) => &c.ident,
                                ImplItem::Fn(f) if is_pub(&f.vis) => &f.sig.ident,
                                ImplItem::Type(t) if is_pub(&t.vis) => &t.ident,
                                _ => continue,
                            };
                            entry.entry(name.to_string()).or_insert_with(|| id.clone());
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

    /// The module and name of the type an inherent impl in `module` (`id`)
    /// resolves to, from its self type `segs`: the first segment is what a
    /// `use` there brings in under that name, of any visibility, else an
    /// item defined there or brought in by a glob for a plain name, else a
    /// path followed from `crate`, `self`, `super` or a child module. A
    /// self type that resolves to nothing is keyed by the module the impl
    /// is in and the last segment, which no listed item asks for: its
    /// methods are not listed.
    fn self_type(
        &mut self,
        id: &ModId,
        module: &Module,
        segs: &[String],
    ) -> Result<(ModId, String)> {
        let last = segs.last().cloned().unwrap_or_default();
        let fallback = (id.clone(), last.clone());
        let Some((first, rest)) = segs.split_first() else {
            return Ok(fallback);
        };
        let uses = module.uses(false);
        let named = uses
            .iter()
            .find(|p| matches!(&p.leaf, Leaf::Name(alias) if alias == first));
        let mut target = match named {
            Some(path) => {
                let mut full = path.segs.clone();
                full.extend(rest.iter().cloned());
                self.resolve(id, &full, 0)?
            }
            None if rest.is_empty() => self.lookup(id, first, 0)?,
            None => self.resolve(id, segs, 0)?,
        };
        if matches!(target, Target::Opaque) && rest.is_empty() {
            for path in uses.iter().filter(|p| p.leaf == Leaf::Glob) {
                target = self.through_glob(id, path, first, 0)?;
                if !matches!(target, Target::Opaque) {
                    break;
                }
            }
        }
        Ok(match target {
            Target::Item { module, name } => (module, name),
            Target::Module(_) | Target::Both { .. } | Target::Opaque => fallback,
        })
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
            match target.module() {
                Some(m) => cur = m,
                None => return Ok(Target::Opaque),
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

    /// What `name` is in module `id`, asked of that module by path: what
    /// [`Inventory::find`] answers, else, in a module that `include!`s a
    /// file generated into `OUT_DIR`, an item that file defines, the code
    /// compiling. A name a `use` there may bring in, a glob's
    /// included, is never taken for the file's, and a glob through the
    /// module asks [`Inventory::find`] alone, so the module does not
    /// answer for names it does not define.
    fn lookup(&mut self, id: &ModId, name: &str, depth: usize) -> Result<Target> {
        let found = self.find(id, name, depth)?;
        if matches!(found, Target::Opaque) && self.module(id)?.is_some_and(|m| generates(&m, name))
        {
            return Ok(Target::Item {
                module: id.clone(),
                name: name.to_owned(),
            });
        }
        Ok(found)
    }

    /// What `name` is in module `id` as its sources write it: a child
    /// module, an item defined there, the two where both are ([`Target::Both`]),
    /// or what a `pub use` there brings in under that name.
    fn find(&mut self, id: &ModId, name: &str, depth: usize) -> Result<Target> {
        if depth > 32 {
            return Ok(Target::Opaque);
        }
        let Some(module) = self.module(id)? else {
            return Ok(Target::Opaque);
        };
        let child = module.items.iter().find_map(|item| match item {
            Item::Mod(m) if m.ident == name => Some(is_pub(&m.vis)),
            _ => None,
        });
        let defined = module
            .items
            .iter()
            .any(|item| plain_item(item).is_some_and(|(_, n, ..)| n == name));
        match (child, defined) {
            (Some(_), false) => return Ok(Target::Module(id.child(name))),
            (None, false) => {}
            (child, true) => {
                let (module, name) = (id.clone(), name.to_owned());
                return Ok(match child {
                    Some(public) => Target::Both {
                        module,
                        name,
                        public,
                    },
                    None => Target::Item { module, name },
                });
            }
        }
        for path in &module.uses(true) {
            match &path.leaf {
                Leaf::Name(alias) if alias == name => {
                    return self.resolve(id, &path.segs, depth + 1);
                }
                Leaf::Glob => {
                    let found = self.through_glob(id, path, name, depth + 1)?;
                    if !matches!(found, Target::Opaque) {
                        return Ok(found);
                    }
                }
                Leaf::Name(_) => {}
            }
        }
        Ok(Target::Opaque)
    }

    /// What `name` is in the module the glob `path`, written in module
    /// `id`, reads; nothing where it reads `id` itself or no module.
    fn through_glob(
        &mut self,
        id: &ModId,
        path: &UsePath,
        name: &str,
        depth: usize,
    ) -> Result<Target> {
        match self.resolve(id, &path.segs, depth)?.module() {
            Some(m) if m != *id => self.find(&m, name, depth),
            _ => Ok(Target::Opaque),
        }
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

/// List `line` as defined in `module`, unless a path read before listed
/// it.
fn list_first(out: &mut Lines, line: String, module: &ModId) {
    out.entry(line).or_insert_with(|| module.clone());
}

/// Whether `name`, which nothing `module` writes defines, is taken for
/// an item of the file it `include!`s from `OUT_DIR`, which the
/// instrument does not read since nothing is built: the module writes
/// such an `include!` at item level, and no `use` there names `name` or
/// is a glob that may bring it in.
fn generates(module: &Module, name: &str) -> bool {
    let includes = module.items.iter().any(|item| {
        matches!(item, Item::Macro(m)
            if m.mac.path.segments.last().is_some_and(|s| s.ident == "include")
                && m.mac.tokens.to_string().contains("OUT_DIR"))
    });
    includes
        && !module.uses(false).iter().any(|p| match &p.leaf {
            Leaf::Glob => true,
            Leaf::Name(alias) => alias == name,
        })
}

fn parse(file: &Path) -> Result<Vec<Item>> {
    Ok(parse_file(file)?.items)
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

/// The kind, name, visibility and attributes of an item that is one lock
/// line: not a module, a `use`, an impl, a macro or an extern block.
pub(super) fn plain_item(
    item: &Item,
) -> Option<(&'static str, String, &Visibility, &[syn::Attribute])> {
    let (kind, ident, vis, attrs) = match item {
        Item::Const(i) => ("const", &i.ident, &i.vis, &i.attrs),
        Item::Enum(i) => ("enum", &i.ident, &i.vis, &i.attrs),
        Item::Fn(i) => ("fn", &i.sig.ident, &i.vis, &i.attrs),
        Item::Static(i) => ("static", &i.ident, &i.vis, &i.attrs),
        Item::Struct(i) => ("struct", &i.ident, &i.vis, &i.attrs),
        Item::Trait(i) => ("trait", &i.ident, &i.vis, &i.attrs),
        Item::TraitAlias(i) => ("trait alias", &i.ident, &i.vis, &i.attrs),
        Item::Type(i) => ("type", &i.ident, &i.vis, &i.attrs),
        Item::Union(i) => ("union", &i.ident, &i.vis, &i.attrs),
        _ => return None,
    };
    Some((kind, ident.to_string(), vis, attrs))
}

/// The segments of an inherent impl's self type: `[Cache]` for
/// `impl Cache<'a>`, `[crate, request, Request]` for
/// `impl crate::request::Request`.
pub(super) fn self_type_path(ty: &Type) -> Option<Vec<String>> {
    match ty {
        Type::Path(p) => Some(
            p.path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect(),
        ),
        Type::Reference(r) => self_type_path(&r.elem),
        Type::Group(g) => self_type_path(&g.elem),
        Type::Paren(p) => self_type_path(&p.elem),
        _ => None,
    }
}

/// One path a `use` item imports, with its tree flattened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UsePath {
    /// The path as written, `self` and `super` included; for a name or a
    /// rename, the last segment is the name imported.
    segs: Vec<String>,
    leaf: Leaf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leaf {
    /// The name the import is known by in the importing module.
    Name(String),
    Glob,
}

/// One path a file reaches a crate or a module by ([`uses`]): a `use`
/// item's, or one its code writes, read as the import it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The path from where it starts, `crate`, `self` and `super`
    /// included: a `use` item's as written, and a written path's with
    /// its first segment replaced by the path of the `use` that names it.
    pub segs: Vec<String>,
    pub written: Written,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    /// A `use` item's path, ending in the name it imports or a glob.
    Use(Leaf),
    /// A path of two or more segments written in an expression, a type, a
    /// pattern, a macro's name or an attribute, whose first `import`
    /// segments are the path of the `use` in scope that names its first
    /// segment, `tier::room` read as `php_engine::tier::room` beside `use
    /// php_engine::tier`; 1 for a path no `use` names, written from a
    /// crate's name, `crate`, `self` or `super`.
    Path { import: usize },
}

impl Import {
    /// Whether this is a path the code writes through a `use` in scope,
    /// which reaches into nothing that `use` does not import already.
    pub fn through_use(&self) -> bool {
        matches!(self.written, Written::Path { import } if import > 1)
    }

    /// The path relative to the crate root for a `crate::`, `self::` or
    /// `super::` path written in the module `at`, else `None`.
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

/// Every path a file reaches a crate or a module by, inline modules and
/// function bodies included, in one walk ([`Walk`]): each `use` item's,
/// and each path its code writes through a `use` in scope or from a
/// crate's name, so `tier::room(..)` beside `use php_engine::tier` is the
/// import `php_engine::tier::room`.
///
/// A `use` is in scope in the module or block that writes it and the
/// blocks inside, never inside an inline `mod`, as rustc reads it; an
/// item of the type namespace a module or block defines (`mod`, a type,
/// a trait) shadows an outer `use` of its name, and a `let` binding never
/// does, a path of two segments never starting from one. Not read: an
/// `extern crate` rename, a name a glob
/// brings in, a `use` whose path starts from another `use`'s name, a
/// generic parameter shadowing a `use`, and the paths inside a macro's
/// tokens, `format!`'s arguments and a `macro_rules!` body among them,
/// `kernels!` lists being read by [`kernels_named`].
pub fn uses(file: &Path) -> Result<Vec<Import>> {
    Ok(Walk::of(&parse_file(file)?))
}

/// The `php_engine` paths the `kernels!` lists in a file name, each
/// line's after its `=` (`jit-design.md` §5.3): what `tier.lock`'s kernels
/// section is generated from.
pub fn kernels_named(file: &Path) -> Result<Vec<String>> {
    let mut lists = Lists(Vec::new());
    lists.visit_file(&parse_file(file)?);
    let mut out = Vec::new();
    for list in lists.0 {
        let named = list
            .parse_body_with(served_through)
            .with_context(|| format!("reading a kernels! list in {}", file.display()))?;
        out.extend(named);
    }
    Ok(out)
}

/// Every `kernels!` invocation in a file.
struct Lists(Vec<syn::Macro>);

impl<'ast> Visit<'ast> for Lists {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.is_ident("kernels") {
            self.0.push(mac.clone());
        }
    }
}

/// One list, `Request => mod wrapped { .. }`, read for the path each line
/// names after its `=`; a line that names none adds nothing.
fn served_through(input: syn::parse::ParseStream<'_>) -> syn::Result<Vec<String>> {
    input.parse::<Type>()?;
    input.parse::<syn::Token![=>]>()?;
    input.parse::<syn::Token![mod]>()?;
    input.parse::<syn::Ident>()?;
    let lines;
    syn::braced!(lines in input);
    let mut out = Vec::new();
    while !lines.is_empty() {
        if lines.peek(syn::Token![=]) && !lines.peek(syn::Token![=>]) {
            lines.parse::<syn::Token![=]>()?;
            let path: syn::Path = lines.parse()?;
            let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
            out.push(segs.join("::"));
        } else {
            lines.step(|cursor| {
                let (_, next) = cursor
                    .token_tree()
                    .ok_or_else(|| cursor.error("a kernels! line ends early"))?;
                Ok(((), next))
            })?;
        }
    }
    Ok(out)
}

/// The walk [`uses`] takes, with the scopes the paths it meets are read
/// through, the innermost last.
struct Walk {
    out: Vec<Import>,
    scopes: Vec<Scope>,
}

/// The names one module or block brings into scope. A module's scope is
/// `closed`: a name it does not bring in is not looked for in the scopes
/// around it.
struct Scope {
    names: HashMap<String, Bound>,
    closed: bool,
}

/// What a scope binds a name of the type namespace to.
enum Bound {
    /// A `use`, by its path.
    Use(Vec<String>),
    /// An item the scope defines, a `mod`, a type or a trait.
    Item,
}

impl Scope {
    fn of<'i>(items: impl IntoIterator<Item = &'i Item>, closed: bool) -> Scope {
        let mut names = HashMap::new();
        for item in items {
            let defined = match item {
                Item::Use(u) => {
                    for path in flatten(&u.tree) {
                        if let Leaf::Name(alias) = path.leaf {
                            names.insert(alias, Bound::Use(path.segs));
                        }
                    }
                    continue;
                }
                Item::Mod(i) => &i.ident,
                Item::Struct(i) => &i.ident,
                Item::Enum(i) => &i.ident,
                Item::Union(i) => &i.ident,
                Item::Trait(i) => &i.ident,
                Item::Type(i) => &i.ident,
                _ => continue,
            };
            names.entry(defined.to_string()).or_insert(Bound::Item);
        }
        Scope { names, closed }
    }
}

impl Walk {
    fn of(file: &syn::File) -> Vec<Import> {
        let mut walk = Walk {
            out: Vec::new(),
            scopes: vec![Scope::of(&file.items, true)],
        };
        walk.visit_file(file);
        walk.out
    }

    /// What `name` is in the innermost scope that brings it in, `None`
    /// where no scope it is looked for in names it.
    fn named(&self, name: &str) -> Option<&Bound> {
        for scope in self.scopes.iter().rev() {
            if let Some(found) = scope.names.get(name) {
                return Some(found);
            }
            if scope.closed {
                break;
            }
        }
        None
    }

    fn within(&mut self, scope: Scope, walk: impl FnOnce(&mut Walk)) {
        self.scopes.push(scope);
        walk(self);
        self.scopes.pop();
    }
}

impl<'ast> Visit<'ast> for Walk {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.out
            .extend(flatten(&item.tree).into_iter().map(|p| Import {
                segs: p.segs,
                written: Written::Use(p.leaf),
            }));
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        match &item.content {
            Some((_, items)) => self.within(Scope::of(items, true), |walk| {
                syn::visit::visit_item_mod(walk, item);
            }),
            None => syn::visit::visit_item_mod(self, item),
        }
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        let items = block.stmts.iter().filter_map(|stmt| match stmt {
            syn::Stmt::Item(item) => Some(item),
            _ => None,
        });
        self.within(Scope::of(items, false), |walk| {
            syn::visit::visit_block(walk, block);
        });
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if let [first, rest @ ..] = segs.as_slice()
            && !rest.is_empty()
        {
            let named = path.leading_colon.is_none().then(|| self.named(first));
            let import = match named.flatten() {
                Some(Bound::Use(import)) => Some(import.iter().chain(rest).cloned().collect()),
                Some(Bound::Item) => None,
                None => Some(segs.clone()),
            };
            if let Some(import) = import {
                let written = Written::Path {
                    import: import.len() - rest.len(),
                };
                self.out.push(Import {
                    segs: import,
                    written,
                });
            }
        }
        syn::visit::visit_path(self, path);
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

#[cfg(test)]
mod tests {

    use super::*;
    use crate::manifest::Manifest;

    /// The walk reads a written path through the `use` in scope where it
    /// is written: a block's `use` inside the block alone, a type
    /// the block defines shadowing the file's `use` of its name, an inline
    /// `mod` seeing none of the file's, and `::`-rooted and in-crate paths
    /// as written; only a path through a `use` is `through_use`.
    #[test]
    fn a_written_path_is_read_through_the_use_in_scope_where_it_is_written() {
        let file = syn::parse_str::<syn::File>(
            "use php_engine::tier;
             fn a() { tier::room(); { struct tier; tier::new(); } }
             fn b() { use php_engine::api as tier; tier::Request::state(); }
             mod own { fn c() { tier::mine(); } }
             fn d() { ::php_engine::tier::stand(); crate::serve::go(); }",
        )
        .unwrap();
        let written: Vec<(String, bool)> = Walk::of(&file)
            .into_iter()
            .filter(|i| matches!(i.written, Written::Path { .. }))
            .map(|i| (i.segs.join("::"), i.through_use()))
            .collect();
        let expected = [
            ("php_engine::tier::room", true),
            ("php_engine::api::Request::state", true),
            ("tier::mine", false),
            ("php_engine::tier::stand", false),
            ("crate::serve::go", false),
        ];
        let expected: Vec<(String, bool)> = expected
            .iter()
            .map(|(s, t)| ((*s).to_owned(), *t))
            .collect();
        assert_eq!(written, expected);
    }

    /// The `same-name` fixture: `reading::Outcome` and `work::Outcome`, with
    /// `work.rs` writing impls as a plain name, `self::`, `crate::` and a
    /// module imported by `use`, and `glob.rs` through a glob and `super::`.
    /// The crate is named `php_heap` for `Crates::scan` alone, which
    /// finds a crate through a manifest row.
    #[test]
    fn an_inherent_impl_lands_under_the_type_it_is_written_on_alone() {
        let root = crate::package_dir().join("fixtures/api-check/same-name");
        let manifest = Manifest::named("php_heap");
        let crates = Crates::scan(&manifest, &root).unwrap();
        let mut inventory = Inventory::new(&crates);
        let items = inventory
            .public_items(crates.named("php_heap").unwrap())
            .unwrap();
        let lines: Vec<&str> = items.iter().map(String::as_str).collect();
        assert_eq!(
            lines,
            [
                "reading::Outcome",
                "reading::Outcome::globbed",
                "reading::Outcome::of",
                "reading::Outcome::render",
                "work::Outcome",
                "work::Outcome::above",
                "work::Outcome::exit",
                "work::Outcome::status",
                "work::Outcome::worked",
            ]
        );
    }

    /// The `include` fixture: `Kind`, `include!`d from `OUT_DIR`,
    /// lists the methods `kind.rs` writes on it, and `Plain`, reached
    /// through a glob after a glob of the including module, lists its
    /// own; the including module answers for no name a glob asks.
    #[test]
    fn a_type_included_from_out_dir_lists_its_impls_and_a_glob_through_it_answers_nothing() {
        let root = crate::package_dir().join("fixtures/api-check/include");
        let manifest = Manifest::named("php_heap");
        let crates = Crates::scan(&manifest, &root).unwrap();
        let mut inventory = Inventory::new(&crates);
        let krate = crates.named("php_heap").unwrap();
        let modules = inventory.defining_modules(krate).unwrap();
        let lines: Vec<(&str, &str)> = modules
            .iter()
            .map(|(line, module)| (line.as_str(), module.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                ("Kind", "php_heap::generated"),
                ("Kind::COUNT", "php_heap::kind"),
                ("Kind::name", "php_heap::kind"),
                ("Plain", "php_heap::plain"),
                ("Plain::new", "php_heap::kind"),
            ]
        );
    }

    /// Each line is defined where its item is written: a method by the
    /// module its impl block is in, which for `reading::Outcome` is three
    /// modules, and a type by its own. Every line has one module,
    /// so the per-module counts sum to the lock's.
    #[test]
    fn a_method_is_defined_by_the_module_its_impl_block_is_written_in() {
        let root = crate::package_dir().join("fixtures/api-check/same-name");
        let manifest = Manifest::named("php_heap");
        let crates = Crates::scan(&manifest, &root).unwrap();
        let mut inventory = Inventory::new(&crates);
        let krate = crates.named("php_heap").unwrap();
        let modules = inventory.defining_modules(krate).unwrap();
        let of = |line: &str| modules[line].as_str();
        assert_eq!(of("reading::Outcome"), "php_heap::reading");
        assert_eq!(of("reading::Outcome::render"), "php_heap::reading");
        assert_eq!(of("reading::Outcome::globbed"), "php_heap::glob");
        assert_eq!(of("reading::Outcome::of"), "php_heap::work");
        assert_eq!(of("work::Outcome::above"), "php_heap::glob");
        assert_eq!(of("work::Outcome::worked"), "php_heap::work");
        let lines: BTreeSet<String> = modules.keys().cloned().collect();
        assert_eq!(lines, inventory.public_items(krate).unwrap());
    }
}
