//! The in-process adapter of design §6.1: the compiler linked into the
//! harness, reached the way `witness` reaches the engine, through
//! `php_bundle` (design §4.2). [`Engine::unit`] compiles one program of
//! the corpus and hands back the unit; `Unit::dump` prints its op arrays
//! in php's own format, which `unit.lock` names. Nothing stands between
//! the compiler and the diff but the bytes it printed: no binary is
//! built, no crate joins the `xtask` edge of the manifest, and the lane
//! runs from design §11's days 1–2, red on every file while the compiler
//! emits what php does not.
//!
//! **One engine for the run.** A unit is a value of the compiler's and
//! carries nothing of a request, so the engine the bundle builds is made
//! once and every file of the corpus is compiled through it; the
//! fresh-engine-per-program isolation the witness lane wants is a
//! property of serving, not of compiling. The ini is the pin's recipe and
//! its `dump` regime, laid over the engine's defaults as a host's `-d`
//! lays it, so one recipe stands behind both dumps; an entry the engine
//! has no name for is carried and read by nobody, which is what php does
//! with `opcache.*` where opcache is not loaded.
//!
//! **A panic is a failure, not the lane's end** (design §8). The compile
//! is caught, and a panic comes back as the trouble one file borks on,
//! naming nothing of the other 6287.

use std::panic::{self, AssertUnwindSafe};

use anyhow::{Result, bail};

use php_bundle::test::Engine;

use super::dump::Dumped;
use crate::oracle::{Normalise, normalise};

/// A fresh engine of the bundle's enabled set under `ini`, each entry as
/// the recipe writes it, `name=value`.
pub(super) fn engine(ini: &[String]) -> Engine {
    let entries: Vec<(&str, &str)> = ini
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .collect();
    php_bundle::test::engine(&entries)
}

/// What the linked compiler dumps for one program: the unit's op arrays
/// in php's format, trimmed by the recipe's rules as the oracle's own
/// stream is trimmed, so a trailing newline is no difference. A source
/// the compiler refuses is no dump at all, which is what php's empty
/// second stream is, and the verdict is what reads the pair.
pub(super) fn dump(
    engine: &Engine,
    rules: &Normalise,
    source: &[u8],
    filename: &[u8],
) -> Result<Dumped> {
    let Ok(compiled) = panic::catch_unwind(AssertUnwindSafe(|| {
        engine.unit(source, filename).map(|unit| unit.dump())
    })) else {
        bail!("the linked compiler panicked; the panic itself is on the harness's second stream");
    };
    Ok(Dumped {
        arrays: normalise(rules, compiled.unwrap_or_default().into_bytes()),
        said: Vec::new(),
    })
}
