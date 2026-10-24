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
    /// The program it records, `types/goldens/<program>.php`, which
    /// fixtures recording other traces of one program share.
    pub(crate) program: &'static str,
    pub(crate) plan: &'static [(&'static str, &'static str)],
}

/// `fib`, entered: a call into its own root, inlined to the recursive
/// call.
pub(crate) const FIB: Fixture = Fixture {
    name: "fib",
    program: "fib",
    plan: &[("record.function", "fib"), ("record.start", "enter")],
};

/// `mandelbrot`'s escape loop, in the file's own code.
pub(crate) const MANDELBROT: Fixture = Fixture {
    name: "mandelbrot",
    program: "mandelbrot",
    plan: &[
        ("record.function", ""),
        ("record.start", "loop"),
        ("record.headers", "2"),
    ],
};

/// `nbody`'s inner loop of `advance`.
pub(crate) const NBODY: Fixture = Fixture {
    name: "nbody",
    program: "nbody",
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
    program: "spectralnorm",
    plan: &[
        ("record.function", "eval_A_times_u"),
        ("record.start", "loop"),
        ("record.headers", "1"),
    ],
};

/// `array_write` of the bench scripts: a packed array written by a long key
/// in a loop (#3228).
pub(crate) const ARRAY_WRITE: Fixture = Fixture {
    name: "array_write",
    program: "array_write",
    plan: &[
        ("record.function", "k"),
        ("record.start", "loop"),
        ("record.headers", "1"),
    ],
};

/// `sieve` of the bench scripts: its inner loop, a packed array of booleans
/// written `false` by a long key (#3228).
pub(crate) const SIEVE: Fixture = Fixture {
    name: "sieve",
    program: "sieve",
    plan: &[
        ("record.function", "k"),
        ("record.start", "loop"),
        ("record.headers", "1"),
    ],
};

/// A native's call in a loop, `strcmp`, a Core native: its `INIT_FCALL`
/// pushed in place and its `DO_ICALL` served by the handler, as nbody's
/// `sqrt` is (#2963).
pub(crate) const NATIVE_CALL: Fixture = Fixture {
    name: "native_call",
    program: "native_call",
    plan: &[
        ("record.function", "k"),
        ("record.start", "loop"),
        ("record.headers", "1"),
    ],
};

/// A loop whose callee's variables own an array and a string as it
/// returns: the `RETURN` left in place, freeing each (#3313).
pub(crate) const OWNING_LEAVE: Fixture = Fixture {
    name: "owning_leave",
    program: "owning_leave",
    plan: &[
        ("record.function", "k"),
        ("record.start", "loop"),
        ("record.headers", "0"),
    ],
};

/// A loop over strings, as `string_munge`'s line loop: a strict comparison
/// of a string with the empty string fused to its jump, one with another
/// string written as a boolean, a string's length and a cast to a
/// boolean, each inline (#3321).
pub(crate) const STRING_MATCH: Fixture = Fixture {
    name: "string_match",
    program: "string_match",
    plan: &[
        ("record.function", "k"),
        ("record.start", "loop"),
        ("record.headers", "0"),
    ],
};

/// A blob built in a loop, as `string_munge`'s first loop: strings joined
/// by `CONCAT`, a line interpolated by `ROPE_*` and appended by `.=`,
/// each string piece compiled inline (#3321).
pub(crate) const STRING_BUILD: Fixture = Fixture {
    name: "string_build",
    program: "string_build",
    plan: &[
        ("record.function", "k"),
        ("record.start", "loop"),
        ("record.headers", "0"),
    ],
};

/// A method's loop over `$this`, as `Zend/micro_bench.php`'s are: each
/// property opline on a published class's declared slot in place, under
/// one class guard (#3337).
pub(crate) const OBJECT_THIS: Fixture = Fixture {
    name: "object_this",
    program: "object_this",
    plan: &[
        ("record.function", "run"),
        ("record.start", "loop"),
        ("record.headers", "0"),
    ],
};

/// A function's loop over an object it holds: each untyped property's
/// opline in place under one class guard on the variable, the typed
/// property's write the handler's (#3337).
pub(crate) const OBJECT_VALUE: Fixture = Fixture {
    name: "object_value",
    program: "object_value",
    plan: &[
        ("record.function", "k"),
        ("record.start", "loop"),
        ("record.headers", "0"),
    ],
};

/// `fannkuch`'s first loop, recorded for the view it holds of
/// `fannkuch()`, which a side trace's record written out by hand is built
/// through (#3270).
pub(crate) const FANNKUCH: Fixture = Fixture {
    name: "fannkuch",
    program: "fannkuch",
    plan: &[
        ("record.function", "fannkuch"),
        ("record.start", "loop"),
        ("record.headers", "0"),
    ],
};

/// `binary_trees`' `check_tree`, entered: a node's elements read by a
/// constant key, held to `null`, and sent by value to the recursive
/// calls by name, the `_FUNC_ARG` family (#2963).
pub(crate) const BINARY_TREES_CHECK: Fixture = Fixture {
    name: "binary_trees_check",
    program: "binary_trees",
    plan: &[("record.function", "check_tree"), ("record.start", "enter")],
};

/// `binary_trees`' `make_tree`, from the landing of its first recursive
/// call's return: the node's array built and returned, recursively
/// (#2963).
pub(crate) const BINARY_TREES_MAKE: Fixture = Fixture {
    name: "binary_trees_make",
    program: "binary_trees",
    plan: &[("record.function", "make_tree"), ("record.start", "return")],
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
    let path = types().join(format!("{}.php", fixture.program));
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
