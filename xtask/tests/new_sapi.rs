//! `xtask new-sapi` over the fixture of `xtask/fixtures/new-sapi`
//! (ADR-0008): the host it lays down has the topology of the CLI and CGI
//! hosts, passes `budget`, `unsafe-audit`, `api-check` and `clippy`
//! untouched, the last against the fixture's stand-ins for the facade and
//! the bundle, which carry the items the template names, joins
//! the workspace members, and a second run for the same name refuses, as
//! one for a name without its row does. The fixture is copied to a scratch
//! directory first; nothing is generated in place, and the workspace's own
//! `sapi/` gains no crate. The stand-ins are held to the real crates: each
//! public item of one is a public item or a re-export of the workspace's
//! crate of that name, read from the sources as a lock is, and in
//! its shape: each field a `pub` field of the real item, each function as
//! many parameters as the real one and each written alike where it names
//! no generic, and each method the real `OutputSink` requires declared.
#![cfg(tracked)]

use std::path::{Path, PathBuf};

use quote::ToTokens;
use syn::{FnArg, ImplItem, Item, ReceiverKind, Signature, TraitItem, Type};
use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status};
use xtask::scratch::Tree;
use xtask::{api_check, budget, clippy, new_sapi, unsafe_audit};

mod common {
    pub mod manifest;
    pub mod repo;
}
use common::manifest::Tracked;
use common::repo::Repo;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture() -> PathBuf {
    xtask::package_dir().join("fixtures/new-sapi")
}

/// A scratch copy of the fixture with one commit, since a reading's stamp
/// names `HEAD`.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("new-sapi-{name}")).unwrap();
    copy(&fixture(), &dir);
    // The fixture names the pin by a path relative to its place in the
    // tree; the copy names the workspace's.
    Tracked::of(&dir).with_the_workspace_pin().write(&dir);
    Repo::init(&dir).commit("fixture");
    dir
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn failed(readings: &[Reading]) -> Vec<String> {
    readings
        .iter()
        .filter(|r| r.status == Status::Fail)
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect()
}

fn of<'a>(readings: &'a [Reading], scope: &str) -> &'a Reading {
    readings
        .iter()
        .find(|r| r.scope == scope)
        .unwrap_or_else(|| panic!("no reading of scope {scope}"))
}

#[test]
fn the_host_laid_down_has_the_topology_of_the_cli_and_the_cgi() {
    let root = scratch("topology");
    let manifest = Manifest::load(&root).unwrap();
    let laid = new_sapi::run(&root, &manifest, "demo", None).unwrap();
    assert_eq!(laid.dir, root.join("sapi/demo"));

    // Two dependencies, the facade and the bundle (ADR-0008), and the
    // binary under php's name for the host.
    let cargo: toml::Table = toml::from_str(&read(&root, "sapi/demo/Cargo.toml")).unwrap();
    assert_eq!(cargo["package"]["name"].as_str(), Some("php_sapi_demo"));
    let deps = cargo["dependencies"].as_table().unwrap();
    let mut names: Vec<&str> = deps.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["php_bundle", "php_sapi"]);
    assert_eq!(
        deps["php_sapi"]["path"].as_str(),
        Some("../../crates/php_sapi")
    );
    let bins = cargo["bin"].as_array().unwrap();
    assert_eq!(bins.len(), 1, "{bins:?}");
    assert_eq!(bins[0]["name"].as_str(), Some("php-demo"));
    assert_eq!(cargo["lints"]["workspace"].as_bool(), Some(true));

    // `main`, the host's name, the facade alone, the lints the manifest
    // denies under sapi/*, and no unsafe.
    let lib = read(&root, "sapi/demo/src/lib.rs");
    assert!(lib.contains("pub fn main() -> ExitCode"), "{lib}");
    assert!(lib.contains("pub const NAME: &str = \"demo\";"), "{lib}");
    assert!(lib.contains("use php_sapi::{"), "{lib}");
    assert!(!lib.contains("php_engine"), "the facade alone: {lib}");
    assert!(lib.contains("#![forbid(unsafe_code)]"), "{lib}");
    let main = read(&root, "sapi/demo/src/main.rs");
    assert!(main.contains("php_sapi_demo::main()"), "{main}");
    // Both crate roots, the library's and the binary's.
    for lint in manifest
        .rule("lints.no-panic")
        .unwrap()
        .predicate
        .deny_lints
        .as_ref()
        .unwrap()
    {
        for root in [&lib, &main] {
            assert!(
                root.contains(lint),
                "{lint} is denied from the manifest: {root}"
            );
        }
    }
    // No build script: the build's facts are the engine's own stamp, and
    // the bundle's `configure` switches are handed to it.
    assert!(!root.join("sapi/demo/build.rs").exists());
    assert!(lib.contains(".configure(php_bundle::CONFIGURE)"), "{lib}");
    let test = read(&root, "sapi/demo/tests/serve.rs");
    assert!(test.contains("CARGO_BIN_EXE_php-demo"), "{test}");

    // The workspace member, where the root lists them one by one.
    let members: toml::Table = toml::from_str(&read(&root, "Cargo.toml")).unwrap();
    let members = members["workspace"]["members"].as_array().unwrap();
    assert!(
        members.iter().any(|m| m.as_str() == Some("sapi/demo")),
        "{members:?}"
    );

    // The note names the subset the row owns, read from the manifest.
    let note = laid
        .notes
        .iter()
        .find(|n| n.starts_with("phpt subset:"))
        .unwrap_or_else(|| panic!("{:#?}", laid.notes));
    assert!(note.contains("php-src/sapi/cli/tests"), "{note}");
}

#[test]
fn the_binary_is_named_as_the_line_names_it() {
    let root = scratch("binary");
    let manifest = Manifest::load(&root).unwrap();
    new_sapi::run(&root, &manifest, "demo", Some("demodbg")).unwrap();
    let cargo = read(&root, "sapi/demo/Cargo.toml");
    assert!(cargo.contains("name = \"demodbg\""), "{cargo}");
    let test = read(&root, "sapi/demo/tests/serve.rs");
    assert!(test.contains("CARGO_BIN_EXE_demodbg"), "{test}");
}

#[test]
fn the_host_laid_down_passes_the_four_instruments_untouched() {
    let root = scratch("instruments");
    let manifest = Manifest::load(&root).unwrap();
    new_sapi::run(&root, &manifest, "demo", None).unwrap();

    let readings = budget::run(&root, &manifest).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    assert_eq!(of(&readings, "sapi/demo").status, Status::Pass);
    // None of the three Rust files states a licence, as no file the
    // rule's list does not name does, and the crate inherits a `license`
    // that covers them.
    let licence = of(&readings, "licence");
    assert_eq!(licence.status, Status::Pass, "{}", licence.message);
    assert_eq!(licence.numbers["files"], 3.into(), "{}", licence.message);

    let readings = unsafe_audit::run(&root, &manifest).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    assert_eq!(of(&readings, "sapi/demo").status, Status::Pass);

    let readings = api_check::hold(&manifest, &root, false).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    let demo = of(&readings, "sapi/demo");
    assert_eq!(demo.status, Status::Pass, "{}", demo.message);

    // Every target, warnings denied, and the `deny_lints` rules at both
    // crate roots: a `let _ =` over an `io::Result` is `let_underscore_drop`.
    let readings = clippy::run(&root, &manifest, &["sapi/demo".to_owned()]).unwrap();
    let demo = of(&readings, "sapi/demo");
    assert_eq!(demo.status, Status::Pass, "{}", demo.message);
}

/// The public items of the fixture's stand-in for `name` that the
/// workspace's crate of that name lacks, as lock lines; the sources are
/// read, nothing is built.
fn beyond_the_real(fixture: &Path, name: &str) -> Vec<String> {
    let real = defined(workspace(), name);
    let stand_in = defined(fixture, name);
    assert!(
        !stand_in.is_empty(),
        "the stand-in for {name} declares nothing"
    );
    (stand_in.keys())
        .filter(|line| !real.contains_key(*line))
        .cloned()
        .collect()
}

/// The public items of the crate `name` under `root` as lock lines, each
/// with the module that defines it.
fn defined(root: &Path, name: &str) -> std::collections::BTreeMap<String, String> {
    api_check::defining_modules(&Manifest::load(root).unwrap(), root, name).unwrap()
}

/// The items of `module`, a path from its crate's name
/// (`php_engine::engine`), read from the crate under `root`'s `crates/`:
/// a module of its own file, or one written inline in its parent's.
fn module_items(root: &Path, module: &str) -> Vec<Item> {
    let parse = |file: &Path| {
        let text =
            std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        syn::parse_file(&text).unwrap().items
    };
    let mut segments = module.split("::");
    let mut dir = root
        .join("crates")
        .join(segments.next().unwrap())
        .join("src");
    let mut items = parse(&dir.join("lib.rs"));
    for segment in segments {
        let found = items.iter().find_map(|item| match item {
            Item::Mod(m) if m.ident == segment => Some(m.content.clone()),
            _ => None,
        });
        dir = dir.join(segment);
        items = match found.unwrap_or_else(|| panic!("{module}: no mod {segment}")) {
            Some((_, inline)) => inline,
            None => parse(
                &[dir.with_extension("rs"), dir.join("mod.rs")]
                    .into_iter()
                    .find(|file| file.is_file())
                    .unwrap_or_else(|| panic!("{module}: no file for {segment}")),
            ),
        };
    }
    items
}

/// What a public item is, as far as a host's call of it can tell.
enum Shape {
    /// A struct's named fields, each with whether it is `pub`.
    Struct(Vec<(String, bool)>),
    Fn(Box<Signature>),
    /// A trait's methods, each with whether an implementor must write it.
    Trait(Vec<(String, bool)>),
    Other,
}

/// The shape of the lock line `line` among the items of its module: a
/// method of the type or trait its last but one segment names, or an item
/// of the last segment's name.
fn shape(items: &[Item], line: &str) -> Option<Shape> {
    let mut parts = line.rsplit("::");
    let name = parts.next()?;
    let owner = parts.next();
    let method = items.iter().find_map(|item| match item {
        Item::Impl(i) if i.trait_.is_none() => {
            let Type::Path(ty) = &*i.self_ty else {
                return None;
            };
            (ty.path.segments.last()?.ident == owner?).then_some(())?;
            i.items.iter().find_map(|item| match item {
                ImplItem::Fn(f) if f.sig.ident == name => Some(f.sig.clone()),
                _ => None,
            })
        }
        Item::Trait(t) if t.ident == owner? => t.items.iter().find_map(|item| match item {
            TraitItem::Fn(f) if f.sig.ident == name => Some(f.sig.clone()),
            _ => None,
        }),
        _ => None,
    });
    if let Some(sig) = method {
        return Some(Shape::Fn(Box::new(sig)));
    }
    items.iter().find_map(|item| match item {
        Item::Struct(s) if s.ident == name => Some(Shape::Struct(
            (s.fields.iter())
                .filter_map(|f| {
                    Some((
                        f.ident.as_ref()?.to_string(),
                        matches!(f.vis, syn::Visibility::Public(_)),
                    ))
                })
                .collect(),
        )),
        Item::Fn(f) if f.sig.ident == name => Some(Shape::Fn(Box::new(f.sig.clone()))),
        Item::Trait(t) if t.ident == name => Some(Shape::Trait(
            (t.items.iter())
                .filter_map(|item| match item {
                    TraitItem::Fn(f) => Some((f.sig.ident.to_string(), f.default.is_none())),
                    _ => None,
                })
                .collect(),
        )),
        Item::Const(c) if c.ident == name => Some(Shape::Other),
        Item::Type(t) if t.ident == name => Some(Shape::Other),
        Item::Enum(e) if e.ident == name => Some(Shape::Other),
        _ => None,
    })
}

/// A parameter as written: its type, or the receiver as a call sees it,
/// `self`, `&self` or `&mut self`.
fn written(arg: &FnArg) -> String {
    match arg {
        FnArg::Receiver(r) => match &r.kind {
            ReceiverKind::Value => "self".to_owned(),
            ReceiverKind::Reference(_, _, Some(_)) => "&mut self".to_owned(),
            ReceiverKind::Reference(..) => "&self".to_owned(),
            _ => r.to_token_stream().to_string(),
        },
        FnArg::Typed(t) => t.ty.to_token_stream().to_string(),
    }
}

/// Where the stand-in's signature of `line` is not the real one's: another
/// number of parameters, or a parameter it writes concretely, naming none
/// of its generics and no `impl`, written otherwise than the real one.
fn signature_differs(line: &str, ours: &Signature, theirs: &Signature) -> Vec<String> {
    let (n, m) = (ours.inputs.len(), theirs.inputs.len());
    if n != m {
        return vec![format!(
            "{line} takes {n} parameters in the stand-in and {m} in the real crate"
        )];
    }
    let generics: Vec<String> = ours
        .generics
        .type_params()
        .map(|p| p.ident.to_string())
        .collect();
    let words = |text: &str| -> Vec<String> {
        (text.split(|c: char| !c.is_alphanumeric() && c != '_'))
            .map(str::to_owned)
            .collect()
    };
    (ours.inputs.iter().zip(&theirs.inputs).enumerate())
        .filter_map(|(at, (a, b))| {
            let (a, b) = (written(a), written(b));
            let generic = words(&a)
                .iter()
                .any(|w| w == "impl" || generics.contains(w));
            (!generic && a != b).then(|| {
                format!(
                    "{line}: parameter {} is `{a}` in the stand-in and `{b}` in the real crate",
                    at + 1
                )
            })
        })
        .collect()
}

/// Each stand-in item of the crate `name` under `root` whose shape is not
/// the real one's: a field the real item has not as `pub`, a signature
/// [`signature_differs`] names, a method the real trait requires that the
/// stand-in does not declare. Sources are read; nothing is built.
fn misshapen(root: &Path, name: &str) -> Vec<String> {
    let real = defined(workspace(), name);
    let mut out = Vec::new();
    for (line, module) in defined(root, name) {
        let Some(theirs) = real.get(&line) else {
            continue;
        };
        let ours = shape(&module_items(root, &module), &line);
        let theirs = shape(&module_items(workspace(), theirs), &line);
        match (ours, theirs) {
            (Some(Shape::Struct(ours)), Some(Shape::Struct(theirs))) => {
                for (field, _) in ours {
                    if !theirs.contains(&(field.clone(), true)) {
                        out.push(format!("{line}.{field} is no pub field of the real {line}"));
                    }
                }
            }
            (Some(Shape::Fn(ours)), Some(Shape::Fn(theirs))) => {
                out.extend(signature_differs(&line, &ours, &theirs));
            }
            (Some(Shape::Trait(ours)), Some(Shape::Trait(theirs))) => {
                for (method, required) in theirs {
                    if required && !ours.iter().any(|(own, _)| *own == method) {
                        out.push(format!(
                            "{line} requires {method}, which the stand-in does not declare"
                        ));
                    }
                }
            }
            (Some(Shape::Other), Some(Shape::Other)) => {}
            _ => out.push(format!(
                "{line}: the stand-in and the real item are not of one kind"
            )),
        }
    }
    out
}

/// The stand-ins hold the shapes the template calls: every field,
/// every parameter and every method the real facade requires; and the
/// three negative controls each fail it, a field the real item lacks, a
/// parameter more and one written otherwise, and a required method left
/// out of the sink.
#[test]
fn the_stand_ins_are_held_to_the_real_facade_s_shapes() {
    for name in ["php_sapi", "php_bundle"] {
        let wrong = misshapen(&fixture(), name);
        assert!(wrong.is_empty(), "{name}: {wrong:#?}");
    }
    let mutated = |name: &str, from: &str, to: &str| {
        let root = scratch(name);
        let lib = root.join("crates/php_sapi/src/lib.rs");
        let text = std::fs::read_to_string(&lib).unwrap();
        assert!(text.contains(from), "{from}");
        std::fs::write(&lib, text.replacen(from, to, 1)).unwrap();
        misshapen(&root, "php_sapi")
    };
    assert_eq!(
        mutated(
            "field",
            "    pub cwd: PathBuf,",
            "    pub cwd: PathBuf,\n    pub workdir: PathBuf,"
        ),
        ["RequestDescription.workdir is no pub field of the real RequestDescription"]
    );
    assert_eq!(
        mutated(
            "arity",
            "_log: fn(&[u8])) -> Engine",
            "_log: fn(&[u8]), _more: u8) -> Engine"
        ),
        ["start takes 4 parameters in the stand-in and 3 in the real crate"]
    );
    assert_eq!(
        mutated("written", "_name: &'static str", "_name: String"),
        [
            "EngineBuilder::sapi_name: parameter 2 is `String` in the stand-in and `& 'static str` in the real crate"
        ]
    );
    assert_eq!(
        mutated(
            "sink",
            "    /// The error stream.\n    fn write_stderr(&mut self, bytes: &[u8]);\n",
            ""
        ),
        ["OutputSink requires write_stderr, which the stand-in does not declare"]
    );
}

#[test]
fn the_stand_ins_declare_only_what_the_real_facade_and_bundle_declare() {
    for name in ["php_sapi", "php_bundle"] {
        let beyond = beyond_the_real(&fixture(), name);
        assert!(
            beyond.is_empty(),
            "{name}: not public in the real crate: {beyond:?}"
        );
    }

    // The negative control: an item the real facade lacks, a method of a
    // type it has, and a function of the bundle it lacks.
    let root = scratch("stand-ins");
    for (name, extra) in [
        (
            "php_sapi",
            "pub struct Served;\nimpl Engine {\n    pub fn served(&self) {}\n}\n",
        ),
        ("php_bundle", "pub fn registered() {}\n"),
    ] {
        let lib = root.join(format!("crates/{name}/src/lib.rs"));
        let text = std::fs::read_to_string(&lib).unwrap();
        std::fs::write(&lib, text + extra).unwrap();
    }
    assert_eq!(
        beyond_the_real(&root, "php_sapi"),
        ["Engine::served", "Served"]
    );
    assert_eq!(beyond_the_real(&root, "php_bundle"), ["registered"]);
}

#[test]
fn a_second_run_and_a_host_without_its_row_are_refused_writing_nothing() {
    let root = scratch("refused");
    let manifest = Manifest::load(&root).unwrap();
    new_sapi::run(&root, &manifest, "demo", None).unwrap();
    let again = new_sapi::run(&root, &manifest, "demo", None).unwrap_err();
    assert!(format!("{again:#}").contains("exists"), "{again:#}");

    let rowless = new_sapi::run(&root, &manifest, "fpm", None).unwrap_err();
    assert!(
        format!("{rowless:#}").contains("no [[crate]] row for sapi/fpm"),
        "{rowless:#}"
    );
    assert!(!root.join("sapi/fpm").exists());

    let named = new_sapi::run(&root, &manifest, "Demo", None).unwrap_err();
    assert!(format!("{named:#}").contains("lowercase"), "{named:#}");
}
