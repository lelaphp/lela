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
//! ([`Inventory::defining_modules`], #1795): an item's by the module it is
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
//! it (#2025).
//!
//! Known limits: `#[path]` on a `mod` is not read; `cfg` is not evaluated,
//! so a `pub` item behind a feature is always listed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

/// What a `use` path names.
enum Target {
    Module(ModId),
    Item {
        module: ModId,
        name: String,
    },
    /// A child module of `module` and an item there of one name, which
    /// rustc keeps in two namespaces, `mod x;` beside `pub fn x` (#2791):
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
    /// in that file (#1795).
    pub fn defining_modules(&mut self, krate: &Present) -> Result<BTreeMap<String, String>> {
        Ok(self
            .lines(krate)?
            .into_iter()
            .map(|(line, module)| (line, self.shown(&module)))
            .collect())
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
        std::iter::once(krate)
            .chain(id.path.iter().cloned())
            .collect::<Vec<_>>()
            .join("::")
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
                    if let Some((name, vis)) = plain_item(other)
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
        let uses: Vec<UsePath> = module
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Use(u) => Some(flatten(&u.tree)),
                _ => None,
            })
            .flatten()
            .collect();
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
                if let Some(m) = self.resolve(id, &path.segs, 0)?.module()
                    && m != *id
                {
                    target = self.find(&m, first, 0)?;
                    if !matches!(target, Target::Opaque) {
                        break;
                    }
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
    /// compiling (#2025). A name a `use` there may bring in, a glob's
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
            .any(|item| plain_item(item).is_some_and(|(n, _)| n == name));
        let (module_id, name) = (id.clone(), name.to_owned());
        match (child, defined) {
            (Some(public), true) => {
                return Ok(Target::Both {
                    module: module_id,
                    name,
                    public,
                });
            }
            (Some(_), false) => return Ok(Target::Module(module_id.child(&name))),
            (None, true) => {
                return Ok(Target::Item {
                    module: module_id,
                    name,
                });
            }
            (None, false) => {}
        }
        let name = name.as_str();
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
                    if let Some(m) = self.resolve(id, &path.segs, depth + 1)?.module()
                        && m != *id
                    {
                        let found = self.find(&m, name, depth + 1)?;
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
        && !module.items.iter().any(|item| {
            matches!(item, Item::Use(u) if flatten(&u.tree).iter().any(|p| match &p.leaf {
                Leaf::Glob => true,
                Leaf::Name(alias) => alias == name,
            }))
        })
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

/// The segments of an inherent impl's self type: `[Cache]` for
/// `impl Cache<'a>`, `[crate, request, Request]` for
/// `impl crate::request::Request`.
fn self_type_path(ty: &Type) -> Option<Vec<String>> {
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

#[cfg(test)]
mod tests {

    use super::*;
    use crate::manifest::Manifest;

    /// The `same-name` fixture: `reading::Outcome` and `work::Outcome`, with
    /// `work.rs` writing impls as a plain name, `self::`, `crate::` and a
    /// module imported by `use`, and `glob.rs` through a glob and `super::`
    /// (#79). The crate is named `php_heap` for `Crates::scan` alone, which
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

    /// The `include` fixture (#2025): `Kind`, `include!`d from `OUT_DIR`,
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
    /// modules, and a type by its own (#1795). Every line has one module,
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
