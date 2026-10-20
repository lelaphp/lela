//! What the tier's tests serve a program through: a fresh engine bound
//! to the tier under test, with the program in a file of its own that
//! the request includes, so the epoch's cache holds its unit and its op
//! arrays carry words, as a held unit's do (`jit-design.md` §3.2).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use php_engine::test::{Capture, Engine, Served};
use php_engine::tier::{
    self, Extension, ExtensionBuilder, Flow, FrameRef, Request, Site, StartupIni, Tier,
};
use php_unit::decl::{IniDecl, Name};

use crate::policy::Policy;

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(miri)))]
pub(crate) mod bodies;
pub(crate) mod goldens;
pub(crate) mod records;

/// The request every program is served by: its file included, so the
/// file is held by the epoch's cache.
const MAIN: &str = "<?php include 'body.php';";

/// One directory per program served, so tests running side by side never
/// share a file.
static SERVED: AtomicU32 = AtomicU32::new(0);

/// A directory of its own, removed on drop.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new() -> Scratch {
        let n = SERVED.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("php_jit-{}-{n}", std::process::id()));
        let _made = std::fs::create_dir_all(&dir);
        Scratch(dir)
    }

    /// `program` as `body.php` here, and the request that includes it
    /// served on `engine`.
    pub(crate) fn serve(&self, engine: &Engine, program: &str) -> Served {
        self.write(program);
        engine.serve_capture_in(&self.0, b"main.php", MAIN, Capture::default())
    }

    /// [`Scratch::serve`], and the request state of type `K` the tier
    /// under test kept, as the request left it.
    pub(crate) fn serve_kept<K: Send + 'static>(
        &self,
        engine: &Engine,
        program: &str,
    ) -> (Served, Option<K>) {
        self.write(program);
        tier::serve_kept(engine, &self.0, b"main.php", MAIN)
    }
}

impl Scratch {
    /// `program` as `body.php`, left as it stands where it holds it
    /// already, so a second request finds the unit the cache holds.
    fn write(&self, program: &str) {
        let path = self.0.join("body.php");
        let text = format!("<?php\n{program}");
        if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            let _written = std::fs::write(path, text);
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _removed = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether this target maps a code arena, so a published trace keeps
/// its code; elsewhere a table numbers what it publishes (#2943).
pub(crate) const MAPS_CODE: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));

/// An engine bound to `T` under `ini`, the request's cache holding every
/// file the moment it is written.
pub(crate) fn engine<T: Tier>(ini: &[(&str, &str)]) -> Engine {
    let mut builder = Engine::builder()
        .tier::<T>()
        .ini("opcache.file_update_protection", "0");
    for (name, value) in ini {
        builder = builder.ini(name, value);
    }
    builder.build()
}

/// `program` served once on a fresh engine bound to `T`.
pub(crate) fn serve<T: Tier>(ini: &[(&str, &str)], program: &str) -> Served {
    Scratch::new().serve(&engine::<T>(ini), program)
}

/// The startup ini `entries` set.
pub(crate) fn ini(entries: &[(&str, &str)]) -> StartupIni {
    StartupIni {
        entries: entries
            .iter()
            .map(|(name, value)| (name.as_bytes().to_vec(), Some(value.as_bytes().to_vec())))
            .collect(),
    }
}

/// The `Policy` `entries` read to.
pub(crate) fn policy(entries: &[(&str, &str)]) -> Policy {
    Policy::from_ini(&ini(entries), &mut Vec::new())
}

/// The name of the function `frame` serves, none for a file's code.
pub(crate) fn function_of(req: &Request, frame: FrameRef) -> Option<Vec<u8>> {
    let function = tier::frame(req, frame)?.function()?;
    Some(req.symbols().function_decl(function)?.name.0.to_vec())
}

/// What a served program wrote to its first stream.
pub(crate) fn stdout(served: &Served) -> String {
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// System ini entries a test tier declares, each unset by default, so a
/// value the engine's builder sets reaches its engine start.
pub(crate) fn declared(names: &[&str]) -> Vec<IniDecl> {
    names
        .iter()
        .map(|name| IniDecl {
            name: Name::from(*name),
            default: None,
            user: false,
            per_dir: false,
            system: true,
        })
        .collect()
}

/// php's `opcache.jit*` directives, which ext/opcache declares in the
/// bundle and no engine of a tier's test has, declared by the test tier
/// instead so the tier's start reads what a test sets.
pub(crate) const OPCACHE_JIT: [&str; 19] = [
    "opcache.jit",
    "opcache.jit_buffer_size",
    "opcache.jit_debug",
    "opcache.jit_hot_loop",
    "opcache.jit_hot_func",
    "opcache.jit_hot_return",
    "opcache.jit_hot_side_exit",
    "opcache.jit_blacklist_root_trace",
    "opcache.jit_blacklist_side_trace",
    "opcache.jit_max_trace_length",
    "opcache.jit_max_root_traces",
    "opcache.jit_max_side_traces",
    "opcache.jit_max_exit_counters",
    "opcache.jit_max_loop_unrolls",
    "opcache.jit_max_recursive_calls",
    "opcache.jit_max_recursive_returns",
    "opcache.jit_max_polymorphic_calls",
    "opcache.jit_bisect_limit",
    "opcache.jit_prof_threshold",
];

/// The compiled tier as a test binds it: `Jit`, its state declared with
/// php's `opcache.jit*` beside its own two directives.
pub(crate) struct Lane;

impl Tier for Lane {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        crate::dispatch::safepoint(req, frame, site)
    }

    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(declared(&OPCACHE_JIT));
        Some(
            ExtensionBuilder::default()
                .ini(ini)
                .engine_state(crate::JitEngine::new)
                .request_state::<crate::JitRequest>()
                .rinit(crate::JitEngine::request_started)
                .build("jit"),
        )
    }
}
