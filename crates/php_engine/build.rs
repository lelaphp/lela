//! The dispatch of design §4.3 seam 7, generated from `ops.toml` at the
//! workspace root over `php_unit::Handler`, the handler each opline is
//! resolved to where its unit is published: one `match` arm per handler
//! the engine serves, and one arm naming every other, which ends the
//! request in the internal-error fatal that names the opcode, so that
//! the `match` names every variant and its table needs no range test.
//!
//! The spec is `ops.toml` and the bodies are `src/handlers.rs`, and this
//! build joins them, so a handler whose name is no opcode of the spec is
//! a build error rather than a body nothing dispatches to (design §5.8).
//! The handler for `ZEND_INIT_FCALL` is `fn init_fcall`, php's name in
//! snake case, in a raw identifier where Rust needs one.
//!
//! A handler of an opcode `ops.toml` marks `specialise` may be generic
//! over the kinds of its operands, one `Operand` parameter for `op1` or
//! two for `op1` and `op2` after the tier's: each variant of the
//! opcode's, one per pair of kinds the spec lists, then names the
//! handler at those kinds (`crate::operand`), as `zend_vm_gen.php` emits
//! one handler per `SPEC` pair (design §5.8), and the variants that name
//! one call are one arm. A
//! handler generic over kinds the spec does not specialise is a build
//! error. Such a handler is handed the opline's position after the four
//! every handler takes, since its arm writes nothing to the frame before
//! it; every other arm writes the position first, php's `SAVE_OPLINE()`
//! (`crate::tier::Next`).
//!
//! The arms of the variants [`IN_THE_LOOP`] names call their handlers in
//! the loop's body, and every other arm calls its handler out of line
//! (#3285).
//!
//! Written to `OUT_DIR/dispatch.rs` and included, never committed (design
//! §10).
//!
//! Beside it, the build's identity, `PHP_ENGINE_BUILD`: a digest of every
//! source that decides what a unit is and what running one does, the
//! syntax, the compiler, the unit format, the heap and the engine, with
//! `ops.toml`. The file cache writes it into every unit it keeps on disk
//! and reads back only a unit that carries it, so a unit another build
//! wrote is never read (design §6.3), as php's `zend_system_id` keeps one
//! build's file cache from another's.
//!
//! And the build's facts, php's `main/build-defs.h` and `php_build_date`,
//! each stamped here once for every host, every constant and phpinfo to
//! read ([`build_defs`]), the install layout among them ([`layout`]).

#[path = "src/api/request/build_defs/layout.rs"]
mod layout;
#[path = "build/rustflags.rs"]
mod rustflags;

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Design §10 Profiles: a release-like build without the LLVM option
    // every profile passes is refused where the build asks (#3372).
    rustflags::hold();
    build_defs();
    // Read when `build.rs` runs, not when it compiles: cargo runs one
    // compiled `build.rs` for every checkout sharing a target directory
    // (#2628).
    let Some(dir) = env::var_os("CARGO_MANIFEST_DIR") else {
        println!("cargo:warning=CARGO_MANIFEST_DIR is unset; cargo sets it for every `build.rs`");
        std::process::exit(1);
    };
    let root = Path::new(&dir);
    let spec = root.join("../../ops.toml");
    let bodies = root.join("src/handlers.rs");
    println!("cargo:rerun-if-changed={}", spec.display());
    println!("cargo:rerun-if-changed={}", bodies.display());
    // Design §5.8: the dispatch threaded by tail calls, nightly's alone.
    println!("cargo::rustc-check-cfg=cfg(dispatch, values(\"tailcall\"))");
    let sources = [
        "../php_syntax/src",
        "../php_compile/src",
        "../php_unit/src",
        "../php_heap/src",
        "src",
    ];
    let mut files = vec![spec.clone()];
    for dir in sources {
        let dir = root.join(dir);
        println!("cargo:rerun-if-changed={}", dir.display());
        walk(&dir, &mut files);
    }
    files.sort();
    println!("cargo:rustc-env=PHP_ENGINE_BUILD={:016x}", digest(&files));
    let out = PathBuf::from(env::var("OUT_DIR").unwrap_or_default()).join("dispatch.rs");
    match generate(&spec, &bodies) {
        Ok(text) => {
            if let Err(e) = fs::write(&out, text) {
                println!("cargo:warning=writing {}: {e}", out.display());
                std::process::exit(1);
            }
        }
        Err(e) => {
            println!("cargo:warning={e}");
            std::process::exit(1);
        }
    }
}

/// The build's facts as `configure` and `main.c` fix php's: the Unix
/// seconds `php_build_date` renders, `SOURCE_DATE_EPOCH`'s where it is
/// set so that a reproducible build stays one and the moment this file
/// ran otherwise, which is when the engine's sources last changed, as
/// `__DATE__` moves when `main.c` is compiled again; the target triple the
/// engine is built for, `PHP_BUILD_SYSTEM` (never the building host's
/// `uname -a`, which would carry its name into the binary); and
/// `PHP_BUILD_PROVIDER` only where the environment names one, as
/// `configure` defines it only then (`Z/main/main_arginfo.h:13`), the
/// cfg `php_build_provider` saying whether it does; and the install
/// layout, each value under its macro's name ([`layout::layout`]).
fn build_defs() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=PHP_BUILD_PROVIDER");
    println!("cargo::rustc-check-cfg=cfg(php_build_provider)");
    let epoch = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs())
        });
    println!("cargo:rustc-env=PHP_BUILD_EPOCH={epoch}");
    println!(
        "cargo:rustc-env=PHP_BUILD_SYSTEM={}",
        env::var("TARGET").unwrap_or_default()
    );
    if let Some(provider) = env::var("PHP_BUILD_PROVIDER")
        .ok()
        .filter(|provider| !provider.is_empty())
    {
        println!("cargo:rustc-env=PHP_BUILD_PROVIDER={provider}");
        println!("cargo:rustc-cfg=php_build_provider");
    }
    let var = |name: &str| env::var(name).ok();
    for (name, value) in layout::layout(&var) {
        println!("cargo:rerun-if-env-changed={name}");
        println!("cargo:rustc-env={name}={value}");
    }
}

/// Every file under `dir`, into `files`.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, files);
        } else {
            files.push(path);
        }
    }
}

/// FNV-1a over each file's bytes in order, the build's identity.
fn digest(files: &[PathBuf]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in files {
        for byte in fs::read(file).unwrap_or_default() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

fn generate(spec: &Path, bodies: &Path) -> Result<String, String> {
    let opcodes = opcodes(spec)?;
    let handlers = handlers(bodies)?;
    undispatched(bodies, &opcodes, &handlers)?;
    // Each call the dispatch makes, with every `php_unit::Handler` it
    // serves, in the order first met: one arm per call, so a handler that
    // serves two variants, a `SEND_VAL` whose handler is generic over
    // `op1` alone, is one body under both.
    let mut calls: Vec<(String, Vec<String>)> = Vec::new();
    // The inline path a variant in the loop's body serves first, by the
    // variant: one call may serve two, an arithmetic alone and fused to
    // the `ASSIGN` after it ([`ASSIGNED`]), each by a path of its own.
    let mut inlines: BTreeMap<String, Inlined> = BTreeMap::new();
    let mut serve = |variant: String, call: String| match calls
        .iter_mut()
        .find(|(served, _)| *served == call)
    {
        Some((_, variants)) => variants.push(variant),
        None => calls.push((call, vec![variant])),
    };
    for handler in &handlers {
        let Some(spec) = opcodes.get(&handler.name) else {
            return Err(format!(
                "{}: `fn {}` is no opcode of ops.toml; a handler is one opcode's, named as php names it",
                bodies.display(),
                handler.ident
            ));
        };
        for (variant, call, inline) in
            arms(handler, spec).map_err(|e| format!("{}: {e}", bodies.display()))?
        {
            if let Some(inline) = inline {
                inlines.insert(variant.clone(), inline);
            }
            serve(variant, call);
        }
    }
    // Every handler of an opcode the engine serves none of, named in one
    // arm, so that the `match` names every variant and its jump table
    // needs no test of the handler's range ahead of it.
    for spec in opcodes
        .iter()
        .filter(|(name, _)| !handlers.iter().any(|handler| &handler.name == *name))
        .map(|(_, spec)| spec)
    {
        for Variant { name: variant, .. } in variants(spec)? {
            serve(
                variant,
                "Err(crate::tier::dispatch::refused(op, \"has no handler in this engine yet\"))"
                    .to_owned(),
            );
        }
    }
    // `Unresolved`, which no opline the dispatch is handed holds: every
    // frame's op array is read through a `php_unit::PublishedUnit`, which
    // only `Unit::published` makes, and it resolves every opline's handler.
    // The arm stays because `Unresolved` stays a variant, what an opline
    // holds before publication, and it is still the fatal, never a panic.
    serve(
        "Unresolved".to_owned(),
        "Err(crate::tier::dispatch::refused(\n\
         \x20           op,\n\
         \x20           \"is served from a unit never published, so no handler is resolved for it\",\n\
         \x20       ))"
            .to_owned(),
    );
    if let Some(stray) = IN_THE_LOOP.iter().find(|name| {
        !calls.iter().any(|(call, variants)| {
            !call.starts_with("Err(") && variants.iter().any(|variant| variant == *name)
        })
    }) {
        return Err(format!(
            "`{stray}` is in the loop's body by `IN_THE_LOOP` and is no variant a handler of {} serves",
            bodies.display()
        ));
    }
    // `become` is refused by a stable parser even where a `cfg` leaves it
    // out, so the threaded calls are written only into the build that sets
    // the `cfg`, which cargo names to a build script in its environment.
    let tails = if env::var("CARGO_CFG_DISPATCH").is_ok_and(|dispatch| dispatch == "tailcall") {
        tails(&calls)
    } else {
        String::new()
    };
    let (arms, held) = arms_of(&calls, &inlines);
    let held = held_macro(&held);
    Ok(format!(
        "/// The one dispatch (design §4.3 seam 7, §5.8), generated from\n\
         /// `ops.toml` and the handlers beside it: the handler the opline at\n\
         /// the frame's `at` was resolved to where its unit was published\n\
         /// (`php_unit::Handler`), one jump on one field as php's\n\
         /// `ZEND_VM_DISPATCH` is, or the fatal for an opcode this engine does\n\
         /// not serve yet. A handler that leaves a throwable pending says so\n\
         /// with [`crate::tier::Stepped::Threw`], which is the pending test\n\
         /// the loop performs. The loop holds `at` in its own hand ([`Next`]):\n\
         /// a handler generic over its operands' kinds is handed it and\n\
         /// writes it to the frame before anything it calls out to, and every\n\
         /// other handler is entered with it written, as php's handler is\n\
         /// entered past its `SAVE_OPLINE()`.\n\
         ///\n\
         /// The `Handler::Unresolved` arm is unreachable by construction:\n\
         /// `code` is the op array of a frame, read through the\n\
         /// `php_unit::PublishedUnit` the frame's annex or the function table\n\
         /// holds (`Request::unit_of`), which `Unit::published` alone makes\n\
         /// and which resolves every opline's handler, and a published unit\n\
         /// is read and never written (#2907). It stays because `Unresolved`\n\
         /// stays a `php_unit::Handler`, the value the compiler writes and\n\
         /// the unit cache's file reads back before publication, so a `match`\n\
         /// that names every variant names it; it is the fatal it always\n\
         /// was, never a panic.\n\
         // One arm per handler served: the length of this function is the\n\
         // number of handlers and says nothing about the code, so the line\n\
         // count is no bound on it. Every crate lints under `-D warnings`\n\
         // (harness §4.2), which reaches generated code too.\n\
         // Inlined into the loop, so that an opline costs one indirect\n\
         // jump to its handler and no call of its own (design §5.8);\n\
         // the arm of a variant outside `IN_THE_LOOP` is one call.\n\
         #[allow(clippy::too_many_lines, clippy::match_same_arms, clippy::inline_always, clippy::match_wildcard_for_single_variants)]\n\
         #[inline(always)]\n\
         pub(crate) fn dispatch<T: Tier>(\n\
         \x20   req: &mut Request,\n\
         \x20   frame: FrameRef,\n\
         \x20   code: &OpArray,\n\
         \x20   op: &Opline,\n\
         \x20   at: u32,\n\
         ) -> Result<Next, Fatal> {{\n\
         \x20   match op.handler {{\n\
         {arms}\
         \x20   }}\n\
         }}\n\
         {held}\
         {tails}"
    ))
}

/// The arms of `dispatch`'s `match`, one per call, and of `held!`'s, one
/// per call and inline path among its variants (`inlines`, by the
/// variant), so that an arithmetic alone and fused to the `ASSIGN` after
/// it ([`ASSIGNED`]), which make one call, are two arms of the loop's.
fn arms_of(
    calls: &[(String, Vec<String>)],
    inlines: &BTreeMap<String, Inlined>,
) -> (String, String) {
    let mut arms = String::new();
    let mut held = String::new();
    let pattern = |variants: &[&String]| {
        variants
            .iter()
            .map(|variant| format!("Handler::{variant}"))
            .collect::<Vec<_>>()
            .join("\n        | ")
    };
    for (call, variants) in calls {
        let all: Vec<&String> = variants.iter().collect();
        writeln!(arms, "        {} => {call},", pattern(&all)).ok();
        // The loop's arms, one per inline path among the call's variants.
        let mut paths: Vec<Option<&Inlined>> = Vec::new();
        for variant in variants {
            let path = inlines.get(variant);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        for path in paths {
            let served: Vec<&String> = variants
                .iter()
                .filter(|variant| inlines.get(*variant) == path)
                .collect();
            held.push_str(&held_arm(&pattern(&served), call, path));
        }
    }
    (arms, held)
}

/// One arm of [`held_macro`]'s `match`: `pattern`'s variants served by
/// `call`, and first by the inline path `inline` where it has one, over
/// the slots the loop holds.
fn held_arm(pattern: &str, call: &str, inline: Option<&Inlined>) -> String {
    let pattern = pattern.replace("Handler::", "php_unit::Handler::");
    let cold = format!(
        "{{\n\
         \x20               $slots = &mut [];\n\
         \x20               #[allow(unused_variables)]\n\
         \x20               let req: &mut crate::api::request::Request = &mut *$req;\n\
         \x20               crate::tier::Dispatched::Cold({call})\n\
         \x20           }}"
    );
    // A fused path serves the `ASSIGN` after its opline unseen, so the
    // loop of an engine whose observers declared `ASSIGN` serves the
    // opline alone, through its handler, and tells them of the `ASSIGN`.
    let guarded = |Inlined { path, assigned }: &Inlined| {
        if *assigned {
            format!(
                "if $declared.tells(php_unit::Opcode::Assign) {{ None }} else {{ {path}(&mut *$slots, code, op, at) }}"
            )
        } else {
            format!("{path}(&mut *$slots, code, op, at)")
        }
    };
    match inline.map(guarded) {
        Some(inline) => format!(
            "            {pattern} => match {inline} {{\n\
             \x20               Some(inline) => crate::tier::Dispatched::Inline(inline),\n\
             \x20               None => {cold},\n\
             \x20           }},\n"
        ),
        None => format!("            {pattern} => {cold},\n"),
    }
}

/// `dispatch::held!`, the macro over `held`'s arms.
fn held_macro(held: &str) -> String {
    format!(
        "\n\
         /// The same dispatch over the frame's slots the loop holds\n\
         /// (design §5.8): `held!(req, slots, frame, code, op, at, declared)`, a\n\
         /// `match` written into the loop's own body, where the borrow\n\
         /// checker reads every arm with the loop. The arm of a handler\n\
         /// with an inline path (`crate::handlers::inline_path`) serves it\n\
         /// over `slots`, with no request in hand; every other arm, and one\n\
         /// whose inline path stands aside, lets the slots go, an empty\n\
         /// frame left in their place, and calls the handler as\n\
         /// [`dispatch`] calls it, and the loop takes them up again for the\n\
         /// frame it goes on in. The borrow checker refuses any other order:\n\
         /// no arm reads the slots after a handler that can move them.\n\
         #[cfg(not(dispatch = \"tailcall\"))]\n\
         macro_rules! held {{\n\
         \x20   ($req:ident, $slots:ident, $frame:expr, $code:expr, $op:expr, $at:expr, $declared:expr) => {{{{\n\
         \x20       let (frame, code, op, at): (php_heap::FrameRef, &php_unit::OpArray, &php_unit::Opline, u32) =\n\
         \x20           ($frame, $code, $op, $at);\n\
         \x20       match op.handler {{\n\
         {held}\
         \x20       }}\n\
         \x20   }}}};\n\
         }}\n\
         #[cfg(not(dispatch = \"tailcall\"))]\n\
         pub(crate) use held;\n"
    )
}

/// The same calls threaded, `crate::tier::tail` (design §5.8): under
/// `cfg(dispatch = "tailcall")`, one function per call, which serves its
/// opline, finds the next and becomes its handler, and the table of them
/// by `php_unit::Handler`, one entry per variant, so that an index past
/// it is a build error in the table's constant and an entry no variant
/// names is the unresolved handler's internal error.
fn tails(calls: &[(String, Vec<String>)]) -> String {
    let handlers: usize = calls.iter().map(|(_, variants)| variants.len()).sum();
    // An entry the table's assignments leave is the unresolved handler's,
    // whose internal error names the opcode, never another opcode's body.
    let unfilled = calls
        .iter()
        .position(|(_, variants)| variants.iter().any(|variant| variant == "Unresolved"))
        .unwrap_or_default();
    let mut table = String::new();
    let mut bodies = String::new();
    for (number, (call, variants)) in calls.iter().enumerate() {
        for variant in variants {
            writeln!(
                table,
                "        table[Handler::{variant} as usize] = tail_{number}::<T, D>;"
            )
            .ok();
        }
        // A call that is the internal error reads nothing it is handed.
        let (unread, body) = if call.starts_with("Err(") {
            ("#[allow(unused_variables)]\n", format!("    {call}\n"))
        } else {
            (
                "",
                format!(
                    "    let next = {call}?;\n\
                     \x20   #[cfg(debug_assertions)]\n\
                     \x20   req.sensed_after(code, op)?;\n\
                     \x20   let (op, at) = match onward(req, frame, oplines, at, next, told)? {{\n\
                     \x20       ControlFlow::Continue(on) => on,\n\
                     \x20       ControlFlow::Break(next) => return Ok(next),\n\
                     \x20   }};\n\
                     \x20   become (table.of(op.handler))(req, frame, code, oplines, op, at, told, table)\n"
                ),
            )
        };
        write!(
            bodies,
            "\n{unread}\
             extern \"rust-preserve-none\" fn tail_{number}<T: Tier, D: Telling>(\n\
             \x20   req: &mut Request,\n\
             \x20   frame: FrameRef,\n\
             \x20   code: &OpArray,\n\
             \x20   oplines: &[Opline],\n\
             \x20   op: &Opline,\n\
             \x20   at: u32,\n\
             \x20   told: &D,\n\
             \x20   table: &Table<D>,\n\
             ) -> Result<Next, Fatal> {{\n\
             {body}\
             }}\n"
        )
        .ok();
    }
    format!(
        "\n/// The calls of [`dispatch`] threaded (`crate::tier::tail`).\n\
         #[cfg(dispatch = \"tailcall\")]\n\
         pub(super) mod tails {{\n\
         use std::marker::PhantomData;\n\
         use std::ops::ControlFlow;\n\
         \n\
         use php_heap::{{Fatal, FrameRef}};\n\
         use php_unit::{{Handler, OpArray, Opline}};\n\
         \n\
         use crate::api::request::Request;\n\
         use crate::observer::Telling;\n\
         use crate::tier::tail::{{Tail, Table, onward}};\n\
         use crate::tier::{{Next, Tier}};\n\
         \n\
         /// How many `php_unit::Handler`s there are, one entry of the table\n\
         /// each.\n\
         pub(crate) const HANDLERS: usize = {handlers};\n\
         \n\
         /// Each handler's function by its `php_unit::Handler`.\n\
         pub(crate) struct Tails<T, D>(PhantomData<(T, D)>);\n\
         \n\
         impl<T: Tier, D: Telling> Tails<T, D> {{\n\
         \x20   pub(crate) const TABLE: &Table<D> = &Table({{\n\
         \x20       let mut table = [tail_{unfilled}::<T, D> as Tail<D>; HANDLERS];\n\
         {table}\
         \x20       table\n\
         \x20   }});\n\
         }}\n\
         {bodies}\
         }}\n"
    )
}

/// One `php_unit::Handler`: its name, the kinds of `op1` and `op2` it is
/// served at, and of the `OP_DATA` after it where the row specialises
/// that, `None` for the variant of the kinds a row does not list,
/// and the branch of a comparison whose line says `SMART_BRANCH`, the
/// type in `crate::handlers::branch`.
struct Variant {
    name: String,
    kinds: Option<Vec<String>>,
    branch: Option<&'static str>,
    /// Whether the `out` operand is read, the type in `crate::operand` the
    /// handler is served at, where the row says `RETVAL`.
    retval: Option<&'static str>,
    /// Whether the variant is the one fused to the `ASSIGN` of its
    /// temporary to a CV after it ([`ASSIGNED`]).
    assigned: bool,
}

/// The two ways of a `RETVAL` opcode's `out`, each the part of the
/// variant's name `php_unit` gives it and the type in `crate::operand`
/// that serves it, which are one name.
const RETVALS: [&str; 2] = ["RetvalUnused", "RetvalUsed"];

/// The arithmetic `php_unit` resolves over whether the opline after it
/// is the `ASSIGN` of its temporary to a CV, which its `build.rs` names
/// in the same list: each such variant's call is its unfused one's, so
/// that `dispatch`, which a stepping host and the tier's recorder serve
/// an opline through, serves the arithmetic alone and the `ASSIGN` after
/// it as php does, and the loop's arm serves both by the fused inline
/// path where [`IN_THE_LOOP`] names the variant ([`inline_of`]).
const ASSIGNED: [&str; 4] = ["ADD", "SUB", "MUL", "DIV"];

/// The two ways of an arithmetic of [`ASSIGNED`], each the part of the
/// variant's name `php_unit` gives it and whether it is fused.
const ASSIGNS: [(&str, bool); 2] = [("", false), ("AssignCv", true)];

/// The branches of a `SMART_BRANCH` comparison, each the part of the
/// variant's name `php_unit` gives it and the type the handler is served
/// at.
const BRANCHES: [(&str, &str); 5] = [
    ("", "Unfused"),
    ("Jmpz", "OnJump<false, false>"),
    ("Jmpnz", "OnJump<true, false>"),
    ("JmpzBack", "OnJump<false, true>"),
    ("JmpnzBack", "OnJump<true, true>"),
];

/// Every `php_unit::Handler` of one opcode, as `php_unit`'s `build.rs`
/// names them from the same row, each with the kinds of `op1` and `op2`
/// it is served at: the opcode's own for one `ops.toml` does not
/// specialise, at the one kind each operand's row lists, and for one it
/// does a variant for every pair of the kinds listed for the operands it
/// specialises, the others at their one kind, and one, `None`, for the
/// kinds it does not list.
fn variants(spec: &Spec) -> Result<Vec<Variant>, String> {
    let lone = |kinds: &[String]| match kinds {
        [kind] => Some(kind.clone()),
        _ => None,
    };
    let ways: &[(&str, &str)] = if spec.smart { &BRANCHES } else { &[("", "")] };
    // One of [`ASSIGNED`]'s arithmetic, over which `php_unit` resolves
    // each handler twice, alone and fused to the `ASSIGN` after it.
    let assigns: &[(&str, bool)] = if ASSIGNED.iter().any(|name| camel(name) == spec.variant) {
        &ASSIGNS
    } else {
        &[("", false)]
    };
    let branched = |name: String, kinds: Option<Vec<String>>, retval: Option<&'static str>| {
        ways.iter().flat_map(move |(part, ty)| {
            let (name, kinds) = (name.clone(), kinds.clone());
            assigns.iter().map(move |(fused, assigned)| Variant {
                name: format!("{name}{part}{fused}"),
                kinds: kinds.clone(),
                branch: (!ty.is_empty()).then_some(*ty),
                retval,
                assigned: *assigned,
            })
        })
    };
    let [spec1, spec2] = spec.zval_shaped.map(|bit| spec.specialise && bit);
    if !spec1 && !spec2 {
        let kinds = lone(&spec.op1)
            .zip(lone(&spec.op2))
            .map(|(a, b)| vec![a, b]);
        return Ok(branched(spec.variant.clone(), kinds, None).collect());
    }
    let side = |listed: bool, kinds: &[String]| -> Result<Vec<(String, String)>, String> {
        if listed {
            return kinds
                .iter()
                .map(|kind| {
                    kind_type(kind)
                        .map(|name| (name.to_owned(), kind.clone()))
                        .ok_or_else(|| {
                            format!("{}: {kind} is no kind of crate::operand", spec.variant)
                        })
                })
                .collect();
        }
        Ok(vec![(String::new(), lone(kinds).unwrap_or_default())])
    };
    // The `OP_DATA` after the opline, where the row's `SPEC(OP_DATA=...)`
    // specialises it: each kind, and its part of the name.
    let data = match &spec.op_data {
        Some(kinds) => side(true, kinds)?
            .into_iter()
            .map(|(name, kind)| (format!("OpData{name}"), Some(kind)))
            .collect(),
        None => vec![(String::new(), None)],
    };
    // `SPEC(RETVAL)`: each kind's variant twice, its result unused and
    // used, after the kinds' parts of the name, as `php_unit` names them.
    let retvals: &[&str] = if spec.retval { &RETVALS } else { &[""] };
    let mut out = Vec::new();
    for (a, ka) in side(spec1, &spec.op1)? {
        for (b, kb) in side(spec2, &spec.op2)? {
            for (d, kd) in &data {
                for r in retvals {
                    let mut kinds = vec![ka.clone(), kb.clone()];
                    kinds.extend(kd.clone());
                    let retval = (!r.is_empty()).then_some(*r);
                    out.extend(branched(
                        format!("{}{a}{b}{d}{r}", spec.variant),
                        Some(kinds),
                        retval,
                    ));
                }
            }
        }
    }
    out.push(Variant {
        name: format!("{}Unlisted", spec.variant),
        kinds: None,
        branch: None,
        retval: None,
        assigned: false,
    });
    Ok(out)
}

/// One opcode's arms, each a `php_unit::Handler` and the call that
/// serves it: the handler at the tier for an opcode it serves at any
/// kinds, and, for a handler generic over its operands' kinds, the
/// handler at the kinds each variant names, with the internal-error
/// fatal for the kinds the spec does not list, which no unit the
/// compiler wrote has.
fn arms(handler: &Handler, spec: &Spec) -> Result<Vec<(String, String, Option<Inlined>)>, String> {
    let unlisted =
        "Err(crate::tier::dispatch::refused(op, \"over operand kinds ops.toml does not list\"))";
    // The branch a handler generic over it is served at, after its kinds.
    let branch = |variant: &Variant| -> Result<String, String> {
        match (handler.branched, variant.branch) {
            (false, _) => Ok(String::new()),
            (true, Some(ty)) => Ok(format!(", crate::handlers::branch::{ty}")),
            (true, None) => Err(format!(
                "`fn {}` is generic over a comparison's branch and ops.toml does not say `SMART_BRANCH` for the opcode",
                handler.ident
            )),
        }
    };
    if handler.operands == 0 {
        let mut out = Vec::new();
        for variant in variants(spec)? {
            if variant.kinds.is_none() && spec.specialise {
                out.push((variant.name, unlisted.to_owned(), None));
                continue;
            }
            let generics = branch(&variant)?;
            let call = call(handler, &variant.name, &generics);
            let inline = inline_of(handler, &variant, &generics);
            out.push((variant.name, call, inline));
        }
        return Ok(out);
    }
    if !spec.specialise {
        return Err(format!(
            "`fn {}` is generic over its operands' kinds and ops.toml does not mark the opcode `specialise`",
            handler.ident
        ));
    }
    let operands = 2 + usize::from(spec.op_data.is_some());
    if handler.operands > operands {
        return Err(format!(
            "`fn {}` is generic over {} operands; an opline of its opcode has {operands}, `OP_DATA`'s counted where ops.toml specialises it",
            handler.ident, handler.operands
        ));
    }
    let mut out = Vec::new();
    for variant in variants(spec)? {
        let Some(kinds) = &variant.kinds else {
            out.push((variant.name, unlisted.to_owned(), None));
            continue;
        };
        let mut params = String::new();
        for kind in &kinds[..handler.operands] {
            let Some(ty) = kind_type(kind) else {
                return Err(format!(
                    "`fn {}` is generic over its operands' kinds and ops.toml lists `{kind}` among them, which crate::operand has no type for",
                    handler.ident
                ));
            };
            write!(params, ", crate::operand::{ty}").ok();
        }
        params.push_str(&branch(&variant)?);
        match (handler.retval, variant.retval) {
            (false, _) => {}
            (true, Some(ty)) => {
                write!(params, ", crate::operand::{ty}").ok();
            }
            (true, None) => {
                return Err(format!(
                    "`fn {}` is generic over whether its `out` is read and ops.toml does not say `RETVAL` for the opcode",
                    handler.ident
                ));
            }
        }
        let call = call(handler, &variant.name, &params);
        let inline = inline_of(handler, &variant, &params);
        out.push((variant.name, call, inline));
    }
    Ok(out)
}

/// The variants whose arms are in the loop's body (design §5.8): every
/// variant met in more than a quarter of a percent of the oplines
/// dispatched by any program of `docs/perf/workloads`, by php-src's
/// `Zend/micro_bench.php` or by the four counting loops of #3273, as
/// a counting build of the loop told them over each (#3285). The arm of
/// every other variant calls its handler out of line, through a call the
/// optimiser reads as cold, so that the loop's body is the arms programs
/// meet and the register allocator, which weighs every arm of the
/// `match` alike, places the loop's values for them: with all 526
/// generic and `#[inline(always)]` arms in its body, `serve_frame` was
/// 270 KB and the arms programs meet paid for the ones they do not.
/// A name that is no variant the dispatch serves is a build error.
const IN_THE_LOOP: &[&str] = &[
    "AddArrayElement",
    "AddConstVar",
    "AddCvConst",
    "AddCvCv",
    "AddCvCvAssignCv",
    "AddTmpConst",
    "AddTmpCv",
    "AddTmpCvAssignCv",
    "AddTmpTmp",
    "AddTmpTmpAssignCv",
    "AddTmpVar",
    "AddVarVar",
    "ArrayKeyExists",
    "AssignCvConstRetvalUnused",
    "AssignCvCvRetvalUnused",
    "AssignCvTmpRetvalUnused",
    "AssignCvVarRetvalUnused",
    "AssignDimCvConstOpDataTmp",
    "AssignDimCvConstOpDataVar",
    "AssignDimCvCvOpDataConst",
    "AssignDimCvCvOpDataCv",
    "AssignDimCvCvOpDataTmp",
    "AssignDimCvTmpOpDataCv",
    "AssignDimOpCvCv",
    "AssignObjOp",
    "AssignObjUnusedConstOpDataConst",
    "AssignOpCvConst",
    "AssignOpCvCv",
    "AssignOpCvTmp",
    "AssignOpCvVar",
    "AssignStaticProp",
    "Bool",
    "BoolNot",
    "BwAnd",
    "Cast",
    "CheckFuncArg",
    "Coalesce",
    "ConcatConstTmp",
    "ConcatCvCv",
    "ConcatTmpConst",
    "ConcatTmpTmp",
    "Div",
    "DivAssignCv",
    "DoFcall",
    "DoFcallByName",
    "DoIcall",
    "DoUcall",
    "Echo",
    "FeFetchRVar",
    "FeFree",
    "FeResetR",
    "FetchClass",
    "FetchClassConstant",
    "FetchConstant",
    "FetchDimFuncArgCvConst",
    "FetchDimIs",
    "FetchDimRCvConst",
    "FetchDimRCvCv",
    "FetchDimRCvTmp",
    "FetchDimRTmpCv",
    "FetchDimRwCvCv",
    "FetchObjRCvConst",
    "FetchObjRUnusedConst",
    "FetchR",
    "FetchStaticPropR",
    "FetchThis",
    "FramelessIcall1",
    "FramelessIcall2",
    "FramelessIcall3",
    "Free",
    "FuncNumArgs",
    "InArray",
    "InitArray",
    "InitDynamicCall",
    "InitFcallByName",
    "InitFcallConst",
    "InitMethodCall",
    "InitStaticMethodCall",
    "IsEqualCvConstJmpz",
    "IsIdenticalCvConst",
    "IsIdenticalCvCv",
    "IsIdenticalTmpConst",
    "IsNotIdentical",
    "IssetIsemptyPropObj",
    "IssetIsemptyStaticProp",
    "IsSmallerConstCv",
    "IsSmallerConstTmpJmpz",
    "IsSmallerCvConst",
    "IsSmallerCvConstJmpnzBack",
    "IsSmallerCvConstJmpz",
    "IsSmallerCvCvJmpnzBack",
    "IsSmallerCvCvJmpz",
    "IsSmallerCvTmpJmpz",
    "IsSmallerOrEqualJmpnzBack",
    "IsSmallerTmpConst",
    "Jmp",
    "JmpnzConst",
    "JmpnzEx",
    "JmpnzTmp",
    "JmpSet",
    "JmpzCv",
    "JmpzEx",
    "JmpzTmp",
    "Mod",
    "MulCvConst",
    "MulCvCv",
    "MulCvCvAssignCv",
    "MulCvTmp",
    "MulCvTmpAssignCv",
    "MulCvVar",
    "MulTmpConst",
    "MulTmpTmp",
    "MulVarTmp",
    "New",
    "PreDecCvRetvalUnused",
    "PreDecObj",
    "PreDecVarRetvalUnused",
    "PreIncCvRetvalUnused",
    "PreIncObj",
    "QmAssign",
    "ReturnConst",
    "ReturnCv",
    "ReturnTmp",
    "RopeAddTmpConst",
    "RopeAddTmpCv",
    "RopeEndTmpConst",
    "RopeInitUnusedConst",
    "SendFuncArg",
    "SendRef",
    "SendValConstUnused",
    "SendValExConstUnused",
    "SendValExTmpUnused",
    "SendValTmpUnused",
    "SendVarCvUnused",
    "SendVarEx",
    "Strlen",
    "SubCvConst",
    "SubCvCv",
    "SubTmpConst",
    "SubTmpTmp",
    "SubTmpTmpAssignCv",
    "TypeCheck",
    "VerifyReturnType",
];

/// The handlers with an inline path, a function of
/// `crate::handlers::inline_path` named as the handler and generic over
/// what it is generic over but its tier, which the loop serves over the
/// slots it holds ([`inline_of`]). A name here that has no such function
/// is a build error of the crate, an unresolved path; a function there
/// that no name here reaches is one only where its handler does not
/// serve it too, an unused item (`div`, `jmp`), so a name left out of
/// the list leaves every other path served by its handler alone.
const INLINE_PATHS: &[&str] = &[
    "add",
    "add_assign_cv",
    "assign",
    "assign_op",
    "div",
    "div_assign_cv",
    "fetch_dim_r",
    "is_equal",
    "is_identical",
    "is_not_equal",
    "is_not_identical",
    "is_smaller",
    "is_smaller_or_equal",
    "jmp",
    "jmpnz",
    "jmpnz_ex",
    "jmpz",
    "jmpz_ex",
    "mul",
    "mul_assign_cv",
    "pre_dec",
    "pre_inc",
    "qm_assign",
    "sub",
    "sub_assign_cv",
];

/// An inline path the loop's arm serves first: the function's path, and
/// whether it serves the `ASSIGN` after its opline too ([`ASSIGNED`]).
#[derive(PartialEq)]
struct Inlined {
    path: String,
    assigned: bool,
}

/// The inline path the loop's arm for `variant` serves first, over the
/// slots it holds: `handler`'s, at `generics` after the tier, where the
/// variant is in the loop's body ([`IN_THE_LOOP`]) and the handler has
/// one ([`INLINE_PATHS`]), and its `_assign_cv` path for a variant fused
/// to the `ASSIGN` after it ([`ASSIGNED`]).
fn inline_of(handler: &Handler, variant: &Variant, generics: &str) -> Option<Inlined> {
    let path = if variant.assigned {
        format!("{}_assign_cv", handler.ident)
    } else {
        handler.ident.clone()
    };
    if !IN_THE_LOOP.contains(&variant.name.as_str()) || !INLINE_PATHS.contains(&path.as_str()) {
        return None;
    }
    let generics = generics.trim_start_matches(", ");
    let turbofish = if generics.is_empty() {
        String::new()
    } else {
        format!("::<{generics}>")
    };
    Some(Inlined {
        path: format!("crate::handlers::inline_path::{path}{turbofish}"),
        assigned: variant.assigned,
    })
}

/// The call one arm makes to `handler` at `generics` for `variant`:
/// in the loop's body for a variant [`IN_THE_LOOP`] names, and out of
/// line through the dispatch's `aside` for every other. A handler that
/// takes the opline's place stands the frame there itself, where
/// something outside the loop can read it; every other is entered with
/// the frame stood there, php's `SAVE_OPLINE()`.
fn call(handler: &Handler, variant: &str, generics: &str) -> String {
    let path = format!("crate::handlers::{}::<T{generics}>", handler.ident);
    match (IN_THE_LOOP.contains(&variant), handler.at) {
        (true, true) => format!("{path}(req, frame, code, op, at)"),
        (true, false) => format!(
            "{{\n\
             \x20           req.vm_mut().stand(frame, at);\n\
             \x20           {path}(req, frame, code, op)\n\
             \x20       }}"
        ),
        (false, true) => format!("crate::tier::dispatch::aside(req, frame, code, op, at, {path})"),
        (false, false) => format!(
            "crate::tier::dispatch::aside(req, frame, code, op, at, |req, frame, code, op, at| {{\n\
             \x20           req.vm_mut().stand(frame, at);\n\
             \x20           {path}(req, frame, code, op)\n\
             \x20       }})"
        ),
    }
}

/// The operand kind's type in `crate::operand`, as `ops.toml` names it.
fn kind_type(kind: &str) -> Option<&'static str> {
    match kind {
        "CONST" => Some("Const"),
        "TMP" => Some("Tmp"),
        "VAR" => Some("Var"),
        "CV" => Some("Cv"),
        "UNUSED" => Some("Unused"),
        _ => None,
    }
}

/// A body named after an opcode that the dispatch does not reach: a
/// handler indented into another item, or one whose visibility keeps the
/// generated `match` from naming it. Either way it is a body nothing
/// dispatches to, which is what this build exists to refuse.
fn undispatched(
    bodies: &Path,
    opcodes: &BTreeMap<String, Spec>,
    handlers: &[Handler],
) -> Result<(), String> {
    let text =
        fs::read_to_string(bodies).map_err(|e| format!("reading {}: {e}", bodies.display()))?;
    for line in text.lines() {
        let Some((_, rest)) = line.split_once("fn ") else {
            continue;
        };
        let name = rest
            .split(['<', '('])
            .next()
            .unwrap_or_default()
            .trim()
            .trim_start_matches("r#");
        if !opcodes.contains_key(name) {
            continue;
        }
        if handlers.iter().any(|dispatched| dispatched.name == name) {
            continue;
        }
        return Err(format!(
            "{}: `fn {name}` is the handler for an opcode of ops.toml and the dispatch does not reach it; a handler is `pub(crate) fn` at the top of the file",
            bodies.display()
        ));
    }
    Ok(())
}

/// What the dispatch reads of one `[[op]]` row: the enum variant
/// `php_unit` generates, whether the design's hot list names the opcode
/// (`specialise`), and the kinds each operand may be.
struct Spec {
    variant: String,
    specialise: bool,
    op1: Vec<String>,
    op2: Vec<String>,
    /// Whether each operand's shape, `op1`'s then `op2`'s, carries a zval
    /// kind with php's `SPEC` bit, which is what `php_unit` specialises a
    /// handler over: not for `ANY`, nor for a field such as `JMP_ADDR`.
    zval_shaped: [bool; 2],
    /// `SMART_BRANCH` in the handler line's `SPEC(...)`, over which
    /// `php_unit` resolves three handlers per kind.
    smart: bool,
    /// The kinds of `SPEC(OP_DATA=...)` on an opcode the hot list
    /// specialises, which `php_unit` resolves the handler over too.
    op_data: Option<Vec<String>>,
    /// `RETVAL` in the handler line's `SPEC(...)` on an opcode the hot
    /// list specialises by kind, over which `php_unit` resolves each
    /// kind's handler twice.
    retval: bool,
}

/// php's opcode names against what the dispatch reads of their rows.
fn opcodes(path: &Path) -> Result<BTreeMap<String, Spec>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let table: toml::Table = toml::from_str(&text).map_err(|e| e.to_string())?;
    let rows = table
        .get("op")
        .and_then(toml::Value::as_array)
        .ok_or("ops.toml has no [[op]] rows")?;
    let mut out = BTreeMap::new();
    for row in rows {
        let name = row
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or("an [[op]] row without a name")?;
        let kinds = |field: &str| -> Vec<String> {
            row.get(field)
                .and_then(toml::Value::as_array)
                .map(|kinds| {
                    kinds
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let specialised = |field: &str| {
            row.get(field)
                .and_then(toml::Value::as_str)
                .unwrap_or("ANY")
                .split('|')
                .any(|word| {
                    matches!(
                        word,
                        "CONST" | "TMP" | "VAR" | "CV" | "UNUSED" | "TMPVAR" | "TMPVARCV"
                    )
                })
        };
        let words: Vec<&str> = row
            .get("spec")
            .and_then(toml::Value::as_str)
            .map(|spec| spec.split(',').collect())
            .unwrap_or_default();
        let specialise = row
            .get("specialise")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        let zval_shaped = [specialised("op1_raw"), specialised("op2_raw")];
        let op_data: Option<Vec<String>> = words
            .iter()
            .find_map(|word| word.strip_prefix("OP_DATA="))
            .filter(|_| specialise)
            .map(|kinds| kinds.split('|').map(str::to_owned).collect());
        // `php_unit` resolves `RETVAL` where it resolves a kind of an
        // operand or of the `OP_DATA` after it, and only there.
        let retval = specialise
            && words.contains(&"RETVAL")
            && (zval_shaped.contains(&true) || op_data.is_some());
        out.insert(
            snake(name),
            Spec {
                variant: camel(name),
                specialise,
                op1: kinds("op1"),
                op2: kinds("op2"),
                zval_shaped,
                smart: words.contains(&"SMART_BRANCH"),
                op_data,
                retval,
            },
        );
    }
    Ok(out)
}

/// One handler `src/handlers.rs` declares: php's name in snake case, the
/// identifier to call, which is the same with the `r#` of a raw
/// identifier kept, and how many of its generic parameters are operand
/// kinds.
struct Handler {
    name: String,
    ident: String,
    operands: usize,
    /// Whether the handler is generic over its comparison's branch, the
    /// `S: Branch` after its kinds (`crate::handlers::branch`).
    branched: bool,
    /// Whether the handler is generic over whether its `out` is read,
    /// the `R: Retval` after its kinds (`crate::operand::Retval`).
    retval: bool,
    /// Whether the handler takes the place of its opline, `at: u32`, and
    /// stands the frame there itself where something outside the loop
    /// reads it; a handler generic over operand kinds always takes it.
    at: bool,
}

/// The handlers `src/handlers.rs` declares, each read off the line that
/// opens it and its generic parameters, on that line or the lines down to
/// the one that closes them.
fn handlers(path: &Path) -> Result<Vec<Handler>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (at, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("pub(crate) fn ") else {
            continue;
        };
        let ident = rest.split(['<', '(']).next().unwrap_or_default().trim();
        if ident.is_empty() {
            continue;
        }
        // The generic parameters, on the opening line or, where rustfmt
        // wraps them, on the lines down to the one that closes them.
        let mut opening = (*rest).to_owned();
        for next in lines.iter().skip(at + 1) {
            if opening.contains(">(") || !opening.contains('<') || opening.contains('(') {
                break;
            }
            opening.push_str(next.trim());
        }
        let generics = opening
            .strip_prefix(ident)
            .and_then(|rest| rest.strip_prefix('<'))
            .and_then(|rest| rest.split_once(">("))
            .map_or("", |(generics, _)| generics);
        // The parameters, down to the line that closes them.
        let at_param = lines
            .iter()
            .skip(at)
            .take_while(|line| !line.contains(") -> "))
            .chain(lines.get(at))
            .any(|line| line.contains(" at: u32"));
        out.push(Handler {
            at: at_param,
            name: ident.trim_start_matches("r#").to_owned(),
            ident: ident.to_owned(),
            operands: generics.matches(": Operand").count(),
            branched: generics.contains(": Branch"),
            retval: generics.contains(": Retval"),
        });
    }
    Ok(out)
}

/// `INIT_FCALL` as `init_fcall`, which is how a handler is named.
fn snake(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// `INIT_FCALL` as `InitFcall`, the variant `php_unit`'s `build.rs` emits.
fn camel(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first
                    .to_uppercase()
                    .chain(chars.map(|c| c.to_ascii_lowercase()))
                    .collect(),
                None => String::new(),
            }
        })
        .collect()
}
