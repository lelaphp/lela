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

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
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
        for (variant, call) in
            arms(handler, spec).map_err(|e| format!("{}: {e}", bodies.display()))?
        {
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
        for (variant, _) in variants(spec)? {
            serve(
                variant,
                "Err(Fatal::error(format!(\n\
                 \x20           \"InternalError: {} has no handler in this engine yet\",\n\
                 \x20           op.opcode.name()\n\
                 \x20       )))"
                    .to_owned(),
            );
        }
    }
    serve(
        "Unresolved".to_owned(),
        "Err(Fatal::error(format!(\n\
         \x20           \"InternalError: {} is served from a unit never published, so no handler is resolved for it\",\n\
         \x20           op.opcode.name()\n\
         \x20       )))"
            .to_owned(),
    );
    let mut arms = String::new();
    for (call, variants) in &calls {
        let pattern = variants
            .iter()
            .map(|variant| format!("Handler::{variant}"))
            .collect::<Vec<_>>()
            .join("\n        | ");
        writeln!(arms, "        {pattern} => {call},").ok();
    }
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
         // One arm per handler served: the length of this function is the\n\
         // number of handlers and says nothing about the code, so the line\n\
         // count is no bound on it. Every crate lints under `-D warnings`\n\
         // (harness §4.2), which reaches generated code too.\n\
         // Inlined into the loop, so that an opline costs one indirect\n\
         // jump to its handler and no call of its own (design §5.8).\n\
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
         }}\n"
    ))
}

/// One `php_unit::Handler`'s name, and the kinds of `op1` and `op2` it
/// is served at; `None` for the variant of the kinds a row does not list.
type Variant = (String, Option<[String; 2]>);

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
    let (spec1, spec2) = (spec.specialise && spec.spec1, spec.specialise && spec.spec2);
    if !spec1 && !spec2 {
        let kinds = lone(&spec.op1).zip(lone(&spec.op2)).map(|(a, b)| [a, b]);
        return Ok(vec![(spec.variant.clone(), kinds)]);
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
    let mut out = Vec::new();
    for (a, ka) in side(spec1, &spec.op1)? {
        for (b, kb) in side(spec2, &spec.op2)? {
            out.push((format!("{}{a}{b}", spec.variant), Some([ka.clone(), kb])));
        }
    }
    out.push((format!("{}Unlisted", spec.variant), None));
    Ok(out)
}

/// One opcode's arms, each a `php_unit::Handler` and the call that
/// serves it: the handler at the tier for an opcode it serves at any
/// kinds, and, for a handler generic over its operands' kinds, the
/// handler at the kinds each variant names, with the internal-error
/// fatal for the kinds the spec does not list, which no unit the
/// compiler wrote has.
fn arms(handler: &Handler, spec: &Spec) -> Result<Vec<(String, String)>, String> {
    if handler.operands == 0 {
        let call = format!(
            "{{\n\
             \x20           req.vm_mut().stand(frame, at);\n\
             \x20           crate::handlers::{}::<T>(req, frame, code, op)\n\
             \x20       }}",
            handler.ident
        );
        return Ok(variants(spec)?
            .into_iter()
            .map(|(variant, _)| (variant, call.clone()))
            .collect());
    }
    if !spec.specialise {
        return Err(format!(
            "`fn {}` is generic over its operands' kinds and ops.toml does not mark the opcode `specialise`",
            handler.ident
        ));
    }
    if handler.operands > 2 {
        return Err(format!(
            "`fn {}` is generic over {} operands; an opline has two",
            handler.ident, handler.operands
        ));
    }
    let unlisted = "Err(Fatal::error(format!(\n\
                    \x20           \"InternalError: {} over operand kinds ops.toml does not list\",\n\
                    \x20           op.opcode.name()\n\
                    \x20       )))";
    let mut out = Vec::new();
    for (variant, kinds) in variants(spec)? {
        let Some(kinds) = kinds else {
            out.push((variant, unlisted.to_owned()));
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
        out.push((
            variant,
            format!(
                "crate::handlers::{}::<T{params}>(req, frame, code, op, at)",
                handler.ident
            ),
        ));
    }
    Ok(out)
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
    /// Whether each operand's shape carries a zval kind with php's `SPEC`
    /// bit, which is what `php_unit` specialises a handler over: not for
    /// `ANY`, nor for a field such as `JMP_ADDR`.
    spec1: bool,
    spec2: bool,
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
        out.insert(
            snake(name),
            Spec {
                variant: camel(name),
                specialise: row
                    .get("specialise")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false),
                op1: kinds("op1"),
                op2: kinds("op2"),
                spec1: specialised("op1_raw"),
                spec2: specialised("op2_raw"),
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
}

/// The handlers `src/handlers.rs` declares, each read off the line that
/// opens it, whose generic parameters stand on it.
fn handlers(path: &Path) -> Result<Vec<Handler>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("pub(crate) fn ") else {
            continue;
        };
        let ident = rest.split(['<', '(']).next().unwrap_or_default().trim();
        if ident.is_empty() {
            continue;
        }
        let generics = rest
            .strip_prefix(ident)
            .and_then(|rest| rest.strip_prefix('<'))
            .and_then(|rest| rest.split_once(">("))
            .map_or("", |(generics, _)| generics);
        out.push(Handler {
            name: ident.trim_start_matches("r#").to_owned(),
            ident: ident.to_owned(),
            operands: generics.matches(": Operand").count(),
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
