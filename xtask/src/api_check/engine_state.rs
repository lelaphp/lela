//! The sensor of the manifest rule whose `check` names [`CHECK`] (#4348
//! ruling 2, ADR-0005): the seal on `EngineState`. The engine's four
//! entries that keep a value engine-wide are bounded by the trait, and
//! `#[derive(EngineState)]` holds every field of a type to it, so a type
//! holding a pointer into a request's heap does not compile there; what the
//! compiler cannot refuse is an impl written by hand, which holds no field
//! to anything. So a hand-written impl stands only where `unsafe` may, in
//! the homes the `unsafe.homes` rule names: `php_heap`, the trait's own
//! crate, for the primitives, the standard containers and its frozen
//! types, and an extension's `src/ffi/`. Anywhere else the rule's scope
//! covers, one fails the crate's reading, naming its line.
//!
//! The rule's `allow`, where it carries one, names the modules outside the
//! homes where a hand-written impl stands (#4633: `php_unit`'s `Frozen<T>`,
//! which only a constructor that walks every value vouches for). Each
//! entry is a module, `crate::path`, read by the [`Home`] that reads one
//! of `unsafe.homes`, so the module is its file and the directory beside
//! it; a crate or a directory is refused when the manifest is read
//! ([`admitted`]). An entry admits an impl whose trait the sensor reads as
//! this one, by name or by a rename, in that module of that crate, and
//! nothing else: there a line the sensor cannot read, an impl whose trait
//! is a metavariable and the trait handed to a macro whose body no source
//! holds fail the crate as anywhere, and the module is no home of
//! `unsafe`, which reads `unsafe.homes` alone. An entry naming a module no
//! crate has, or a crate the manifest does not list, covers no file and
//! admits nothing, and is not refused, so the row may stand before the
//! module it names is written. With no `allow` the sensor reads as it did
//! before the key.
//!
//! The sensor reads syntax, with `syn`, and follows what the compiler
//! follows: every source under the crate's `src/`, `tests/`, `benches/`
//! and `examples/` and its `build.rs` (a target path the crate's
//! `Cargo.toml` sets elsewhere is not walked), and each `mod` file,
//! `#[path]` and literal `include!` they reach, inside the crate directory
//! or out of it. A home covers the crate's own files only: a file reached
//! in another crate's `src/ffi/`, or outside every crate, is held like the
//! rest. An impl is one whose trait path ends in `EngineState` or in a
//! name a `use .. as` in the crate gives it, however the path is spaced,
//! in source or in a macro's tokens, a definition's or a call's. What the
//! sensor cannot read fails the crate, naming the line, in place of an
//! impl it might hide:
//!
//! - an `impl $t for` (or `impl #t for` in a `quote!`), whose trait is a
//!   metavariable or a path a repetition builds (`$($p)::+`, `#(#p)::*`),
//!   in a crate that names the trait;
//! - the trait handed to a macro the crate does not define, whose body is
//!   in no source it holds;
//! - a `#[path]` or `include!` naming a file that is not there, or not by
//!   a literal path (an `include!` other than the two-part `OUT_DIR`
//!   form), and a source that is not Rust; a `#[path]` inside an inline
//!   module resolves as in a `mod.rs` file, so under a file that is not
//!   one it may read the wrong file or refuse the line;
//! - a string literal naming the trait in a file of the build's closure,
//!   `build.rs` and what it reaches, where the crate includes a file from
//!   `OUT_DIR`, since what the build writes is in no source. A module of
//!   the crate the build reaches is judged by its strings, so a comment,
//!   a doc or a derive there does not fail it.
//!
//! The derive's own expansion, in [`DERIVE`], is exempt for every macro
//! token in it: an impl or a call any `quote!` there writes passes.
//!
//! What reading the source cannot hold, #4463 carries: a `build.rs`
//! that writes the impl without naming the trait (the name spelled in
//! pieces, or read from a file), an impl a proc macro parses from a
//! string, a local macro that forwards the trait to a macro outside the
//! workspace, a rename another crate exports, which is a line of that
//! crate's lock, and the homes themselves: in an extension's `src/ffi/` a
//! hand-written impl stands for any type, with no marker, and "a C
//! library's object" is the homes' prose, not a check.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use proc_macro2::{Delimiter, Spacing, Span, TokenStream, TokenTree};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Expr, Lit, Meta, Token};

use crate::manifest::{Crate, Manifest, Rule};
use crate::rust;
use crate::unsafe_audit::{self, Home};

/// The check the rule's `check` predicate names.
pub(crate) const CHECK: &str = "engine-state";

/// The trait the rule seals.
const TRAIT: &str = "EngineState";

/// The derive's expansion, whose `quote!` names the trait.
const DERIVE: &str = "crates/php_macros/src/engine_state.rs";

/// The modules `rule`'s `allow` names, where a hand-written impl stands
/// outside the homes; none where it carries no `allow`. An entry that is
/// not a module, `crate::path`, is refused naming the rule and the entry,
/// since a crate or a directory would admit an impl anywhere in it (#4633).
pub(crate) fn admitted(rule: &Rule) -> Result<Vec<Home>> {
    (rule.predicate.allow.iter().flatten())
        .map(|entry| match Home::parse(entry) {
            Ok(module @ Home::Module { .. }) => Ok(module),
            _ => bail!(
                "rule \"{}\" admits {entry:?}; an entry of the `allow` beside check = \"{CHECK}\" is one module, crate::path, as unsafe.homes names php_jit::enter, never a crate or a directory (#4633)",
                rule.id
            ),
        })
        .collect()
}

/// Each hand-written impl of the trait in `files`, the sources of
/// `krate` relative to the root, that `rule` covers and no home of
/// `unsafe.homes` does, less one in a module the rule's `allow` names
/// ([`admitted`]), and each place the sensor cannot read, as `path:line`
/// with what stops it after.
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
    let admitting = admitted(rule)?;
    let root = normal(root);
    let mut queue: Vec<(PathBuf, bool)> = (files.iter())
        .filter(|file| rule.covers(Path::new(file)) && !rule.exempts(Path::new(file)))
        .map(|file| {
            let build = Path::new(file) == krate.path().join("build.rs");
            (normal(&root.join(file)), build)
        })
        .collect();
    // The `build.rs` closure first, so a file it reaches is read as its.
    queue.sort_by_key(|(_, build)| *build);
    let mut sources: BTreeMap<PathBuf, Source> = BTreeMap::new();
    while let Some((path, build)) = queue.pop() {
        if sources.contains_key(&path) {
            continue;
        }
        let shown = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {shown}"))?;
        let read = Reader::read(&path, &text);
        queue.extend(read.follow.iter().map(|next| (next.clone(), build)));
        // A home covers the crate's own files: one it reaches in another
        // crate's `src/ffi/`, or outside every crate, is held.
        let own = Path::new(&shown).starts_with(krate.path());
        let standing = if own && homes.iter().any(|h| h.covers(krate, &shown)) {
            Standing::Home
        } else if own && admitting.iter().any(|m| m.covers(krate, &shown)) {
            Standing::Admitted
        } else {
            Standing::Held
        };
        sources.insert(
            path,
            Source {
                derive: shown == DERIVE,
                shown,
                standing,
                build,
                text,
                read,
            },
        );
    }
    Ok(found(&sources))
}

/// One source of the crate, read once.
struct Source {
    /// Its path relative to the root, or whole where it is outside it.
    shown: String,
    /// What its sites count as.
    standing: Standing,
    /// Whether it is the derive's own expansion.
    derive: bool,
    /// Whether the crate's `build.rs` reaches it.
    build: bool,
    text: String,
    read: Read,
}

/// What a source's sites count as, by what covers it among the crate's
/// own files.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// A home of `unsafe.homes`: nothing it writes counts.
    Home,
    /// A module the rule's `allow` names: an impl the sensor reads as the
    /// trait's passes, and every other site counts.
    Admitted,
    /// Neither, or a file of another crate or of none: every site counts.
    Held,
}

/// The sites in `sources` that fail the crate, each once, by file and line.
fn found(sources: &BTreeMap<PathBuf, Source>) -> Vec<String> {
    let mut names = BTreeSet::from([TRAIT.to_owned()]);
    while let Some(to) = (sources.values())
        .flat_map(|s| &s.read.renames)
        .find(|(from, to)| names.contains(from) && to != "_" && !names.contains(to))
        .map(|(_, to)| to.clone())
    {
        names.insert(to);
    }
    let mentioned = sources
        .values()
        .any(|s| word(&rust::without_comments_or_literals(&s.text), TRAIT));
    let defined: BTreeSet<&String> = sources.values().flat_map(|s| &s.read.defined).collect();
    let out_dir = sources.values().any(|s| s.read.out_dir);
    let mut sites: BTreeMap<(&str, usize), String> = BTreeMap::new();
    for s in sources.values().filter(|s| s.standing != Standing::Home) {
        let mut site = |line: usize, why: Option<&str>| {
            (sites.entry((s.shown.as_str(), line))).or_insert_with(|| match why {
                Some(why) => format!("{}:{line} ({why})", s.shown),
                None => format!("{}:{line}", s.shown),
            });
        };
        for (line, why) in &s.read.refused {
            site(*line, Some(why));
        }
        for imp in &s.read.impls {
            match &imp.trait_ {
                _ if imp.in_macro && s.derive => {}
                Trait::Named(name) if names.contains(name) && s.standing != Standing::Admitted => {
                    site(imp.line, None);
                }
                Trait::Meta if mentioned => site(imp.line, Some("its trait is a metavariable")),
                _ => {}
            }
        }
        for call in s.read.calls.iter().filter(|_| !s.derive) {
            let local = call.local.as_ref().is_some_and(|m| defined.contains(m));
            if !local && !call.idents.is_disjoint(&names) {
                site(
                    call.line,
                    Some("the trait handed to a macro whose body no source here holds"),
                );
            }
        }
        // A file of the build's closure, a module of the crate among them,
        // by its strings alone: what build.rs writes is text it holds.
        let strings = (s.read.strings.iter()).filter(|(_, lit)| names.iter().any(|n| word(lit, n)));
        for (line, _) in strings.filter(|_| s.build && out_dir) {
            site(
                *line,
                Some(
                    "read in build.rs's closure, a string names the trait, and what build.rs writes to OUT_DIR is in no source",
                ),
            );
        }
    }
    sites.into_values().collect()
}

/// What one source says that the sensor weighs.
#[derive(Default)]
struct Read {
    impls: Vec<Impl>,
    calls: Vec<Call>,
    /// Each `use .. as`, renamed from and to.
    renames: Vec<(String, String)>,
    /// The `macro_rules!` it defines.
    defined: Vec<String>,
    /// Each line it cannot be read past, and why.
    refused: Vec<(usize, &'static str)>,
    /// The files it reaches.
    follow: Vec<PathBuf>,
    /// Whether it includes a file from `OUT_DIR`.
    out_dir: bool,
    /// Each string literal outside a doc comment, with its line.
    strings: Vec<(usize, String)>,
}

/// An `impl .. X for`, by the last segment of `X`.
struct Impl {
    line: usize,
    trait_: Trait,
    in_macro: bool,
}

enum Trait {
    Named(String),
    /// A macro's `$t`, or a `quote!`'s `#t`.
    Meta,
}

/// A macro call other than `macro_rules!` and `include!`.
struct Call {
    line: usize,
    /// The macro's name where the path is that alone.
    local: Option<String>,
    /// Every identifier in its tokens.
    idents: BTreeSet<String>,
}

/// Reads one source: the file's directory, its stem, and the inline
/// modules it is inside, which a `mod` or `#[path]` resolves against.
struct Reader {
    dir: PathBuf,
    stem: String,
    inline: Vec<String>,
    read: Read,
}

impl Reader {
    fn read(path: &Path, text: &str) -> Read {
        let mut reader = Reader {
            dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            stem: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            inline: Vec::new(),
            read: Read::default(),
        };
        match syn::parse_file(text) {
            Ok(file) => reader.visit_file(&file),
            // An `include!`d expression, whose blocks may hold items.
            Err(err) => match syn::parse_str::<Expr>(text) {
                Ok(expr) => reader.visit_expr(&expr),
                Err(_) => reader.refuse(err.span(), "not read as Rust"),
            },
        }
        reader.read
    }

    fn refuse(&mut self, span: Span, why: &'static str) {
        self.read.refused.push((span.start().line, why));
    }

    /// Follow `path` where it is a file, or refuse the line that names it.
    fn reach(&mut self, path: &Path, span: Span) {
        if path.is_file() {
            self.read.follow.push(normal(path));
        } else {
            self.refuse(span, "a file the walk cannot follow");
        }
    }

    /// The directory an inline module's `mod` resolves in.
    fn module_dir(&self, mod_rs: bool) -> PathBuf {
        let mut dir = self.dir.clone();
        if !mod_rs {
            dir.push(&self.stem);
        }
        dir.extend(&self.inline);
        dir
    }

    fn include(&mut self, tokens: TokenStream, span: Span) {
        if let Ok(lit) = syn::parse2::<syn::LitStr>(tokens.clone()) {
            return self.reach(&self.dir.join(lit.value()), span);
        }
        let out_dir = syn::parse2::<syn::ExprMacro>(tokens).is_ok_and(|m| {
            let parts = (m.mac.path.is_ident("concat"))
                .then(|| m.mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated))
                .and_then(Result::ok);
            parts.is_some_and(|p| {
                p.len() == 2
                    && matches!(&p[0], Expr::Macro(env) if env.mac.path.is_ident("env")
                        && env.mac.parse_body::<syn::LitStr>().is_ok_and(|v| v.value() == "OUT_DIR"))
                    && matches!(&p[1], Expr::Lit(lit) if matches!(lit.lit, Lit::Str(_)))
            })
        });
        if out_dir {
            self.read.out_dir = true;
        } else {
            self.refuse(span, "an include! the walk cannot follow");
        }
    }

    /// The impls and includes a macro's tokens write, at any depth.
    fn scan(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (at, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(group) => self.scan(group.stream()),
                TokenTree::Ident(i) if i == "impl" => {
                    if let Some(trait_) = trait_after(&tokens[at + 1..]) {
                        let line = i.span().start().line;
                        self.read.impls.push(Impl {
                            line,
                            trait_,
                            in_macro: true,
                        });
                    }
                }
                TokenTree::Ident(i) if i == "include" => {
                    if let [TokenTree::Punct(bang), TokenTree::Group(args), ..] = &tokens[at + 1..]
                        && bang.as_char() == '!'
                    {
                        self.include(args.stream(), i.span());
                    }
                }
                TokenTree::Literal(lit) => {
                    (self.read.strings).push((lit.span().start().line, lit.to_string()));
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Reader {
    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if let Some(last) = node
            .trait_
            .as_ref()
            .and_then(|(path, _)| path.segments.last())
        {
            let (line, trait_) = (
                node.impl_token.span.start().line,
                Trait::Named(last.ident.to_string()),
            );
            self.read.impls.push(Impl {
                line,
                trait_,
                in_macro: false,
            });
        }
        visit::visit_item_impl(self, node);
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        self.read
            .defined
            .extend(node.ident.as_ref().map(ToString::to_string));
        visit::visit_item_macro(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let span = node.bang_token.span;
        if node.path.is_ident("include") {
            return self.include(node.tokens.clone(), span);
        }
        self.scan(node.tokens.clone());
        if !node.path.is_ident("macro_rules") {
            let mut idents = BTreeSet::new();
            idents_in(node.tokens.clone(), &mut idents);
            let local = node.path.get_ident().map(ToString::to_string);
            self.read.calls.push(Call {
                line: span.start().line,
                local,
                idents,
            });
        }
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let path = node.attrs.iter().find(|a| a.path().is_ident("path"));
        let conditional = node.attrs.iter().any(|a| {
            matches!(&a.meta, Meta::List(list) if list.path.is_ident("cfg_attr")
                && list.tokens.clone().into_iter().any(|t| matches!(t, TokenTree::Ident(i) if i == "path")))
        });
        let span = node.mod_token.span;
        if conditional {
            self.refuse(span, "a #[cfg_attr(.., path)] the walk cannot follow");
        }
        if node.content.is_some() {
            self.inline.push(node.ident.to_string());
            visit::visit_item_mod(self, node);
            self.inline.pop();
            return;
        }
        match path.map(|a| &a.meta) {
            Some(Meta::NameValue(nv)) => match &nv.value {
                Expr::Lit(syn::ExprLit {
                    lit: Lit::Str(lit), ..
                }) => {
                    let base = if self.inline.is_empty() {
                        self.dir.clone()
                    } else {
                        self.module_dir(true)
                    };
                    self.reach(&base.join(lit.value()), span);
                }
                _ => self.refuse(span, "a #[path] the walk cannot follow"),
            },
            Some(_) => self.refuse(span, "a #[path] the walk cannot follow"),
            None => {
                let name = node.ident.to_string();
                for dir in [self.module_dir(true), self.module_dir(false)] {
                    for file in [
                        dir.join(format!("{name}.rs")),
                        dir.join(&name).join("mod.rs"),
                    ] {
                        if file.is_file() {
                            self.read.follow.push(normal(&file));
                        }
                    }
                }
            }
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_use_rename(&mut self, node: &'ast syn::UseRename) {
        (self.read.renames).push((node.ident.to_string(), node.rename.to_string()));
    }

    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        (self.read.strings).push((node.span().start().line, node.value()));
    }

    fn visit_lit_byte_str(&mut self, node: &'ast syn::LitByteStr) {
        let text = String::from_utf8_lossy(&node.value()).into_owned();
        (self.read.strings).push((node.span().start().line, text));
    }

    /// A doc comment's text is no string the build can write.
    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        if !node.path().is_ident("doc") {
            visit::visit_attribute(self, node);
        }
    }
}

/// The trait of `impl <rest>`: the last name before `for` outside angle
/// brackets, past the impl's own generics; `None` for an inherent impl or
/// an `impl Trait` type.
fn trait_after(rest: &[TokenTree]) -> Option<Trait> {
    let (mut depth, mut last, mut prev) = (0u32, None, None::<&TokenTree>);
    for token in rest {
        let arrow = matches!(prev, Some(TokenTree::Punct(p)) if p.as_char() == '-' && p.spacing() == Spacing::Joint);
        match token {
            TokenTree::Punct(p) if p.as_char() == '<' => depth += 1,
            TokenTree::Punct(p) if p.as_char() == '>' && !arrow => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            TokenTree::Ident(i) if i == "for" => return last,
            TokenTree::Ident(i) if i == "where" => return None,
            TokenTree::Punct(p) if p.as_char() == ';' => return None,
            // A path a repetition builds, `$($p)::+` or `#(#p)::*`.
            TokenTree::Group(_) if matches!(prev, Some(TokenTree::Punct(p)) if matches!(p.as_char(), '$' | '#')) =>
            {
                last = Some(Trait::Meta);
            }
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => return None,
            TokenTree::Ident(i) => {
                let meta =
                    matches!(prev, Some(TokenTree::Punct(p)) if matches!(p.as_char(), '$' | '#'));
                last = Some(if meta {
                    Trait::Meta
                } else {
                    Trait::Named(i.to_string())
                });
            }
            _ => {}
        }
        prev = Some(token);
    }
    None
}

fn idents_in(tokens: TokenStream, out: &mut BTreeSet<String>) {
    for token in tokens {
        match token {
            TokenTree::Ident(i) => {
                out.insert(i.to_string());
            }
            TokenTree::Group(g) => idents_in(g.stream(), out),
            _ => {}
        }
    }
}

/// Whether `w` stands in `text` as a whole identifier.
fn word(text: &str, w: &str) -> bool {
    let ident = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    text.match_indices(w).any(|(at, _)| {
        !ident(at.checked_sub(1).and_then(|p| text.as_bytes().get(p)))
            && !ident(text.as_bytes().get(at + w.len()))
    })
}

/// `path` with its `.` and `..` resolved by the letter.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Trait, trait_after};
    use proc_macro2::{TokenStream, TokenTree};

    fn of(text: &str) -> Option<String> {
        let tokens: Vec<TokenTree> = text.parse::<TokenStream>().unwrap().into_iter().collect();
        trait_after(&tokens[1..]).map(|t| match t {
            Trait::Named(name) => name,
            Trait::Meta => "$".to_owned(),
        })
    }

    #[test]
    fn the_trait_of_an_impl_in_tokens_is_its_last_name_before_for() {
        assert_eq!(
            of("impl EngineState for A {}").as_deref(),
            Some("EngineState")
        );
        assert_eq!(
            of("impl<T: Fn() -> u8> a :: b :: EngineState for B<T> {}").as_deref(),
            Some("EngineState")
        );
        assert_eq!(
            of("impl<$($g)*> $crate::View<T> for $t {}").as_deref(),
            Some("View")
        );
        assert_eq!(
            of("impl #g #root::EngineState for #name {}").as_deref(),
            Some("EngineState")
        );
        assert_eq!(of("impl $t<X> for $x {}").as_deref(), Some("$"));
        // A path a repetition builds is a metavariable too (#4360).
        assert_eq!(of("impl $($p)::+ for $x {}").as_deref(), Some("$"));
        assert_eq!(of("impl #(#p)::* for #x {}").as_deref(), Some("$"));
        assert_eq!(of("impl $($t)* { fn f() {} }"), None);
    }

    #[test]
    fn an_inherent_impl_and_an_impl_trait_type_have_no_trait() {
        assert_eq!(of("impl $t { fn f() {} }"), None);
        assert_eq!(of("impl Iterator<Item = u8> { for x in y {} }"), None);
        assert_eq!(of("impl for<'a> Fn(&'a u8) -> u8; for"), None);
        assert_eq!(of("impl Display where T: Copy, for<'a> X: Y"), None);
    }
}
