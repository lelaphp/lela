//! Seam 1 of #947's Testing Decisions: an extension declared with the
//! attributes alone, held to the oracle's bytes.
//!
//! - **Renderings.** Each mirror's Reflection signature lines (headers,
//!   parameters, returns, properties, constants), the extension's name
//!   taken out, are the oracle's for the same php declaration. A shape no
//!   php declaration has is held to the rendering of `parity/prelude.php`,
//!   the same declaration written in PHP.
//! - **Behaviour.** `parity/behaviour.php` prints the same bytes on the
//!   engine as on the oracle, which runs it with the prelude on its first
//!   line, so every line number stands.
//!
//! The `.expect` files are the pinned oracle's bytes, written by the
//! ignored `recapture` test and read here, never typed.

#[path = "parity/extension.rs"]
mod extension;

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use php_ext::test::{Capture, Engine};

const PRELUDE: &str = include_str!("parity/prelude.php");
const BEHAVIOUR: &str = include_str!("parity/behaviour.php");
const BEHAVIOUR_EXPECT: &str = include_str!("parity/behaviour.expect");
const RENDERINGS_EXPECT: &str = include_str!("parity/renderings.expect");

/// Declarations php makes, which the oracle renders as it has them.
const MIRRORS: &[&str] = &[
    "new ReflectionFunction('strspn')",
    "new ReflectionFunction('explode')",
    "new ReflectionFunction('htmlspecialchars')",
    "new ReflectionFunction('round')",
    "new ReflectionFunction('trim')",
    "new ReflectionFunction('mkdir')",
    "new ReflectionFunction('str_replace')",
    "new ReflectionFunction('in_array')",
    "new ReflectionFunction('array_keys')",
    "new ReflectionFunction('array_push')",
    "new ReflectionFunction('sscanf')",
    "new ReflectionFunction('array_multisort')",
    "new ReflectionFunction('preg_match')",
    "new ReflectionFunction('wordwrap')",
    "new ReflectionFunction('implode')",
    "new ReflectionFunction('join')",
    "new ReflectionFunction('utf8_encode')",
    "new ReflectionFunction('password_verify')",
    "new ReflectionClass('RoundingMode')",
    "new ReflectionClass('php_user_filter')",
    "new ReflectionMethod('SplFileInfo', '__construct')",
    "new ReflectionMethod('SplFileInfo', 'setFileClass')",
    "new ReflectionMethod('SplFileObject', '__construct')",
    "new ReflectionMethod('SplFileObject', 'fgets')",
    "new ReflectionMethod('SplFileObject', 'getCurrentLine')",
    "new ReflectionMethod('SplTempFileObject', '__construct')",
    "new ReflectionClass('JsonException')",
    "new ReflectionProperty('DOMNode', 'nodeName')",
    "new ReflectionClass('Dom\\AdjacentPosition')",
    "new ReflectionMethod('Dom\\Element', 'insertAdjacentText')",
];

/// Shapes no php declaration has, which the oracle renders from the
/// prelude. An enum is not among them: php lists a user enum's
/// `UnitEnum` first and an internal one's `BackedEnum` first, and
/// `Dom\AdjacentPosition` holds the internal order.
const SHAPES: &[&str] = &[
    "new ReflectionFunction('proto_seen')",
    "new ReflectionFunction('proto_call')",
    "new ReflectionFunction('proto_count')",
    "new ReflectionFunction('proto_manual')",
    "new ReflectionFunction('proto_describe')",
    "new ReflectionFunction('proto_never')",
    "new ReflectionFunction('proto_false')",
    "new ReflectionClass('ProtoCounter')",
    "new ReflectionClass('ProtoBase')",
    "new ReflectionClass('ProtoBox')",
    "new ReflectionClass('ProtoTally')",
    "new ReflectionClass('ProtoLaid')",
];

fn engine(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[extension::extension(), php_ext_reflection::extension()])
        .ini("html_errors", "0")
        .build();
    let served = engine.serve_capture(source, Capture::default());
    let mut out = String::from_utf8_lossy(&served.stdout).into_owned();
    if let Some(fatal) = served.fatal {
        out.push_str("\nFATAL: ");
        out.push_str(&fatal);
    }
    out
}

/// A program that renders each expression under a `== expr` line.
fn rendering_script(expressions: &[&str]) -> String {
    let mut script = String::from("<?php\n");
    for expression in expressions {
        let quoted = expression.replace('\\', "\\\\").replace('\'', "\\'");
        writeln!(script, "echo '== {quoted}', \"\\n\", {expression};").ok();
    }
    script
}

/// The declaration lines of a rendering, each trimmed, with where it was
/// declared taken out: `<internal:standard>` and `<user>` alike are `<X`.
fn signatures(rendering: &str) -> Vec<String> {
    const KEPT: &[&str] = &[
        "== ",
        "Parameter #",
        "- Return",
        "- Tentative return",
        "Property [",
        "Constant [",
        "Method [",
        "Function [",
        "Class [",
        "Enum [",
        "Interface [",
    ];
    rendering
        .lines()
        .map(str::trim)
        .filter(|line| KEPT.iter().any(|kept| line.starts_with(kept)))
        .map(|line| {
            let Some(at) = line.find(" [ <") else {
                return line.to_owned();
            };
            let (head, tail) = line.split_at(at + 4);
            let origin = tail.find(['>', ',']).unwrap_or(0);
            let tail = &tail[origin..];
            match tail.find("deprecated:") {
                Some(at) => {
                    let name = tail[at..].find('>').unwrap_or(0);
                    format!("{head}X{}deprecated:X{}", &tail[..at], &tail[at + name..])
                }
                None => format!("{head}X{tail}"),
            }
        })
        .collect()
}

fn engine_renderings() -> Vec<String> {
    let mut lines = signatures(&engine(&rendering_script(MIRRORS)));
    lines.extend(signatures(&engine(&rendering_script(SHAPES))));
    lines
}

#[test]
fn renderings_match_the_oracle() {
    let expected: Vec<&str> = RENDERINGS_EXPECT.lines().collect();
    let engine = engine_renderings();
    let differ: Vec<String> = expected
        .iter()
        .zip(engine.iter())
        .filter(|(oracle, engine)| *oracle != engine)
        .map(|(oracle, engine)| format!("  php:    {oracle}\n  engine: {engine}"))
        .collect();
    assert!(
        differ.is_empty() && expected.len() == engine.len(),
        "{} of {} lines differ ({} against {}):\n{}",
        differ.len(),
        expected.len(),
        expected.len(),
        engine.len(),
        differ.join("\n")
    );
}

#[test]
fn behaviour_matches_the_oracle() {
    let engine = engine(BEHAVIOUR);
    if engine != BEHAVIOUR_EXPECT {
        let at = engine
            .lines()
            .zip(BEHAVIOUR_EXPECT.lines())
            .position(|(e, o)| e != o)
            .unwrap_or(0);
        let window = |text: &str| text.lines().skip(at).take(6).collect::<Vec<_>>().join("\n");
        panic!(
            "behaviour differs from line {}:\n--- php\n{}\n--- engine\n{}",
            at + 1,
            window(BEHAVIOUR_EXPECT),
            window(&engine)
        );
    }
}

/// A constant an extension adds to a class another declares, which no
/// prelude can write: `class_constants` on Core's `ArithmeticError`, and
/// inherited by its child as a declared one is.
#[test]
fn an_extern_class_takes_the_constants_of_its_impl() {
    let out =
        engine("<?php var_dump(ArithmeticError::PROTO_ADDED, DivisionByZeroError::PROTO_ADDED);");
    assert_eq!(out, "int(42)\nint(42)\n");
}

/// A native's frameless arities are its own name's and never its
/// alias's: php registers `join` as a `ZEND_RAW_FENTRY` with no
/// frameless infos, so `zend_flf_functions` holds `implode`'s two rows
/// and none for `join`, and a call of `join` is `INIT_FCALL`.
#[test]
fn an_alias_declares_no_frameless_entry() {
    use php_ext::__private::Frameless;
    assert!(matches!(
        extension::implode::DECLARED.frameless,
        [Frameless::One(_), Frameless::Two(_)]
    ));
    assert!(extension::join::DECLARED.frameless.is_empty());
}

/// This crate's directory as cargo sets `CARGO_MANIFEST_DIR` when it runs
/// the test, never the path `env!` compiled in: cargo reuses a test binary
/// across two checkouts that share a target directory (#2790).
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("cargo sets CARGO_MANIFEST_DIR for a test it runs")
}

/// The oracle's binary, `[oracle] path` in the one file that pins it,
/// read as TOML; a pin that cannot be read or names no path fails with
/// why, rather than running a binary named nothing.
fn oracle() -> PathBuf {
    let pin = manifest_dir().join("../../oracle.toml");
    let table: toml::Table = std::fs::read_to_string(&pin)
        .map_err(|e| e.to_string())
        .and_then(|text| toml::from_str(&text).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| panic!("{} is read as TOML: {e}", pin.display()));
    let path = table
        .get("oracle")
        .and_then(|oracle| oracle.get("path"))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("{} names no `[oracle] path`", pin.display()));
    PathBuf::from(path)
}

/// What the oracle prints for `source` under `-n`; an oracle that does
/// not start or answer fails with why, so `recapture` never pins nothing.
fn run_oracle(source: &str) -> String {
    let oracle = oracle();
    let mut child = Command::new(&oracle)
        .arg("-n")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{} starts: {e}", oracle.display()));
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(source.as_bytes())
            .unwrap_or_else(|e| panic!("{} reads the program: {e}", oracle.display()));
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("{} answers: {e}", oracle.display()));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The prelude folded onto the program's first line, after its `<?php`.
fn with_prelude(script: &str) -> String {
    let body = script.strip_prefix("<?php\n").unwrap_or(script);
    format!("<?php {}\n{body}", PRELUDE.replace('\n', " "))
}

/// Write the `.expect` files from the pinned oracle, as `xtask witness
/// --accept` writes a witness's: `cargo test -p php_bundle --test parity
/// -- --ignored recapture`, then read the diff.
#[test]
#[ignore = "runs the oracle and rewrites the pinned output"]
fn recapture() {
    let dir = manifest_dir().join("tests/parity");
    let mut renderings = signatures(&run_oracle(&rendering_script(MIRRORS)));
    renderings.extend(signatures(&run_oracle(&with_prelude(&rendering_script(
        SHAPES,
    )))));
    let renderings = renderings.join("\n") + "\n";
    assert!(std::fs::write(dir.join("renderings.expect"), renderings).is_ok());
    let behaviour = run_oracle(&with_prelude(BEHAVIOUR));
    assert!(std::fs::write(dir.join("behaviour.expect"), behaviour).is_ok());
}
