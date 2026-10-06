//! The dispatch of design §4.3 seam 7, generated from `ops.toml` at the
//! workspace root over `php_unit::Opcode`: one `match` arm per opcode the
//! engine has a handler for, and one arm for every other opcode, which
//! ends the request in the internal-error fatal that names it.
//!
//! The spec is `ops.toml` and the bodies are `src/handlers.rs`, and this
//! build joins them, so a handler whose name is no opcode of the spec is
//! a build error rather than a body nothing dispatches to (design §5.8).
//! The handler for `ZEND_INIT_FCALL` is `fn init_fcall`, php's name in
//! snake case, in a raw identifier where Rust needs one.
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
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
    let mut arms = String::new();
    for (name, handler) in &handlers {
        let Some(variant) = opcodes.get(name) else {
            return Err(format!(
                "{}: `fn {handler}` is no opcode of ops.toml; a handler is one opcode's, named as php names it",
                bodies.display()
            ));
        };
        writeln!(
            arms,
            "        Opcode::{variant} => crate::handlers::{handler}::<T>(req, frame, code, op),"
        )
        .ok();
    }
    Ok(format!(
        "/// The one dispatch (design §4.3 seam 7, §5.8), generated from\n\
         /// `ops.toml` and the handlers beside it: the handler at the frame's\n\
         /// opline, or the fatal for an opcode this engine does not serve yet.\n\
         /// A handler that leaves a throwable pending says so with\n\
         /// [`Stepped::Threw`], which is the pending test the loop performs.\n\
         // One arm per opcode served: the length of this function is the\n\
         // number of handlers and says nothing about the code, so the line\n\
         // count is no bound on it. Every crate lints under `-D warnings`\n\
         // (harness §4.2), which reaches generated code too.\n\
         #[allow(clippy::too_many_lines)]\n\
         pub(crate) fn dispatch<T: Tier>(\n\
         \x20   req: &mut Request,\n\
         \x20   frame: FrameRef,\n\
         \x20   code: &OpArray,\n\
         \x20   op: &Opline,\n\
         ) -> Result<Stepped, Fatal> {{\n\
         \x20   match op.opcode {{\n\
         {arms}\
         \x20       other => Err(Fatal::Error(format!(\n\
         \x20           \"InternalError: {{}} has no handler in this engine yet\",\n\
         \x20           other.name()\n\
         \x20       ))),\n\
         \x20   }}\n\
         }}\n"
    ))
}

/// A body named after an opcode that the dispatch does not reach: a
/// handler indented into another item, or one whose visibility keeps the
/// generated `match` from naming it. Either way it is a body nothing
/// dispatches to, which is what this build exists to refuse.
fn undispatched(
    bodies: &Path,
    opcodes: &BTreeMap<String, String>,
    handlers: &[(String, String)],
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
        if handlers.iter().any(|(dispatched, _)| dispatched == name) {
            continue;
        }
        return Err(format!(
            "{}: `fn {name}` is the handler for an opcode of ops.toml and the dispatch does not reach it; a handler is `pub(crate) fn` at the top of the file",
            bodies.display()
        ));
    }
    Ok(())
}

/// php's opcode names against the enum variant `php_unit` generates.
fn opcodes(path: &Path) -> Result<BTreeMap<String, String>, String> {
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
        out.insert(snake(name), camel(name));
    }
    Ok(out)
}

/// The handlers `src/handlers.rs` declares: php's name in snake case
/// against the identifier to call, which is the same with the `r#` of a
/// raw identifier kept.
fn handlers(path: &Path) -> Result<Vec<(String, String)>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("pub(crate) fn ") else {
            continue;
        };
        let handler = rest.split(['<', '(']).next().unwrap_or_default().trim();
        if handler.is_empty() {
            continue;
        }
        out.push((
            handler.trim_start_matches("r#").to_owned(),
            handler.to_owned(),
        ));
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
