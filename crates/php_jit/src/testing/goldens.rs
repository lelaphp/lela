//! The programs `types` and `lower` are tested over: the four the scalar
//! core is measured on (`jit-design.md` §11, days 39–45), each with the
//! plan the recording tier records it under, and the golden files
//! `types` and `lower` hold what they make of them to; and any other a
//! test records under a plan of its own ([`traced`]). A golden's record is `types/goldens/<name>.trace`; with
//! `PHP_JIT_BLESS=1` set, a test writes its goldens instead of reading
//! them.

use std::path::{Path, PathBuf};

use crate::record::{Recorded, TraceRecord};
use crate::testing::records::recording;
use crate::types::{TraceIr, build};

/// One program of `types/goldens` and where the recording tier records it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fixture {
    pub(crate) name: &'static str,
    pub(crate) plan: &'static [(&'static str, &'static str)],
}

/// `fib`, entered: a call into its own root, inlined to the recursive
/// call.
pub(crate) const FIB: Fixture = Fixture {
    name: "fib",
    plan: &[("record.function", "fib"), ("record.start", "enter")],
};

/// `mandelbrot`'s escape loop, in the file's own code.
pub(crate) const MANDELBROT: Fixture = Fixture {
    name: "mandelbrot",
    plan: &[
        ("record.function", ""),
        ("record.start", "loop"),
        ("record.headers", "2"),
    ],
};

/// `nbody`'s inner loop of `advance`.
pub(crate) const NBODY: Fixture = Fixture {
    name: "nbody",
    plan: &[
        ("record.function", "advance"),
        ("record.start", "loop"),
        ("record.headers", "1"),
    ],
};

/// `spectralnorm`'s inner loop of `eval_A_times_u`, which inlines its call
/// to `eval_A`.
pub(crate) const SPECTRALNORM: Fixture = Fixture {
    name: "spectralnorm",
    plan: &[
        ("record.function", "eval_A_times_u"),
        ("record.start", "loop"),
        ("record.headers", "1"),
    ],
};

/// Where `types`' goldens are, the programs and their records among them.
pub(crate) fn types() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/types/goldens")
}

/// Whether the tests write their goldens rather than read them.
fn blessing() -> bool {
    std::env::var_os("PHP_JIT_BLESS").is_some_and(|on| on == "1")
}

/// `text` held to the golden at `path`, or written to it.
pub(crate) fn held(path: &Path, text: &str) -> Result<(), String> {
    if blessing() {
        return std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()));
    }
    let golden = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if golden == text {
        return Ok(());
    }
    Err(format!(
        "{} is not what was made; with PHP_JIT_BLESS=1 the test writes it\n--- golden\n{golden}\n--- made\n{text}",
        path.display()
    ))
}

/// The fixture's program, less its open tag, which the scratch file the
/// tests serve writes itself.
fn program(fixture: Fixture) -> Result<String, String> {
    let path = types().join(format!("{}.php", fixture.name));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text.strip_prefix("<?php\n").unwrap_or(&text).to_owned())
}

/// The fixture recorded under its plan: the recording, whose view
/// `build` reads through, since a `UnitView` is built by a request alone
/// (`jit-design.md` §5.3).
pub(crate) fn recorded(fixture: Fixture) -> Result<Recorded, String> {
    recording(fixture.plan, &program(fixture)?).map(|(recorded, _)| recorded)
}

/// The fixture's golden record, `<name>.trace`, parsed.
pub(crate) fn record(fixture: Fixture) -> Result<TraceRecord, String> {
    let path = types().join(format!("{}.trace", fixture.name));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    TraceRecord::parse(&text).map_err(|e| format!("{}.trace: {e:?}", fixture.name))
}

/// The trace the fixture's golden record builds, read through the view
/// of `recorded`.
pub(crate) fn built(fixture: Fixture, recorded: &Recorded) -> Result<TraceIr, String> {
    build(&record(fixture)?, &recorded.view).map_err(|abort| format!("{}: {abort:?}", fixture.name))
}

/// The trace `program` builds when recorded under `plan`, from its
/// record's text.
pub(crate) fn traced(plan: &[(&str, &str)], program: &str) -> Result<TraceIr, String> {
    let (recorded, _) = recording(plan, program)?;
    let record = TraceRecord::parse(&recorded.record.dump())
        .map_err(|e| format!("the record's text: {e:?}"))?;
    build(&record, &recorded.view).map_err(|abort| format!("{abort:?}"))
}
