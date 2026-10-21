//! Seam 9 under the compiled tier (`jit-design.md` §4.3, *Observed calls*
//! and *Observed opcodes*), served through engines whose extensions
//! declare an observer: a call a compiled trace makes is begun and ended
//! as the interpreter begins and ends it, since compiled code serves
//! every call and return through the interpreter's handler, and an
//! extension observing an opcode past the two silence opcodes refuses
//! the tier with php's warning.

use php_engine::api::abi::{Beneath, Observed, ObserverHandlers, Opcodes, chain};
use php_engine::api::values::Value;
use php_engine::test::Engine;
use php_engine::tier::{ExtensionBuilder, Start};

use crate::testing::{Lane, Scratch, stdout};

/// The name `observed` serves, `{main}` for a file's code.
fn name(observed: &Observed<'_>) -> String {
    observed
        .function()
        .and_then(|function| observed.symbols().function_decl(function))
        .map_or_else(
            || "{main}".to_owned(),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        )
}

/// Every call observed, begun and ended.
fn every_call(_: &mut Observed<'_>) -> ObserverHandlers {
    ObserverHandlers {
        begin: Some(begin),
        end: Some(end),
    }
}

fn begin(observed: &mut Observed<'_>) {
    let line = format!("<{}>\n", name(observed));
    observed.response_mut().write_stderr(line.as_bytes());
}

fn end(observed: &mut Observed<'_>, _: Option<&Value>) {
    let line = format!("</{}>\n", name(observed));
    observed.response_mut().write_stderr(line.as_bytes());
}

/// `ZEND_ECHO`, which compiled code would serve with no telling of its
/// own where it served it inline.
fn echo(_: &Start) -> Opcodes {
    let mut opcodes = Opcodes::default();
    opcodes.insert(b"ZEND_ECHO");
    opcodes
}

/// The two opcodes php's tier leaves observed.
fn silence(_: &Start) -> Opcodes {
    let mut opcodes = Opcodes::default();
    opcodes.insert(b"ZEND_BEGIN_SILENCE");
    opcodes.insert(b"ZEND_END_SILENCE");
    opcodes
}

/// An opline told of, by the opcode the frame stands at.
fn told(observed: &mut Observed<'_>, beneath: Beneath<'_, chain::Opcode>) {
    let line = format!("<told {}>\n", beneath.opcode());
    observed.response_mut().write_stderr(line.as_bytes());
}

/// An engine bound to the compiled tier under `ini`, with `observing`
/// in its bundle.
fn observing(observing: ExtensionBuilder, ini: &[(&str, &str)]) -> Engine {
    let mut builder = Engine::builder()
        .tier::<Lane>()
        .extensions([observing.build("observing")])
        .ini("opcache.file_update_protection", "0");
    for (entry, value) in ini {
        builder = builder.ini(entry, value);
    }
    builder.build()
}

/// The tier's force lane with the `TRACE` lines a compile and an exit
/// write, `compiled` (1<<14) and `exit` (1<<15), opcache on for the
/// command line, as php starts its tier only there (#3126), and
/// diagnostics displayed as the command line displays them.
const FORCE: [(&str, &str); 5] = [
    ("opcache.enable_cli", "1"),
    ("opcache.jit", "tracing"),
    ("jit.mode", "force"),
    ("opcache.jit_debug", "0xc000"),
    ("html_errors", "0"),
];

/// Loops that call a user function, a function it calls and a native,
/// and a silenced function.
const CALLS: &str = "function f($x) { return $x + 1; }
    function g($x) { return f($x) * 2; }
    $s = 0;
    for ($i = 0; $i < 5; $i++) { $s = g($s) + strcmp('b', 'a'); }
    for ($i = 0; $i < 3; $i++) { $s += @f($i); }
    echo $s, \"\\n\";";

/// The lines an observer wrote to `stderr`, the tier's own aside.
fn observed_lines(stderr: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| line.starts_with('<'))
        .map(str::to_owned)
        .collect()
}

/// What the observer wrote between trace `n`'s compile and its first
/// exit: what compiled code made it see, since a forced trace is entered
/// at the visit after its compile and runs until it leaves.
fn between(stderr: &str, n: u32) -> Result<Vec<String>, String> {
    let compiled = stderr
        .find(&format!("---- TRACE {n} compiled"))
        .ok_or_else(|| format!("trace {n} compiles:\n{stderr}"))?;
    let exited = stderr
        .get(compiled..)
        .and_then(|after| after.find(&format!("     TRACE {n} exit")))
        .ok_or_else(|| format!("trace {n} is entered and left:\n{stderr}"))?;
    let inside = stderr.get(compiled..compiled + exited).unwrap_or_default();
    Ok(observed_lines(inside.as_bytes()))
}

/// A call a compiled trace makes is begun and ended as the interpreter
/// begins and ends it, a native's among them: the observer sees the
/// same events under `jit.mode=force` as with the tier off, and sees
/// calls between a trace's compile and its exit, which compiled code
/// made through the interpreter's handler.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_a_compiled_trace_makes_is_begun_and_ended_as_the_interpreter_does() -> Result<(), String>
{
    let scratch = Scratch::new();
    let off = scratch.serve(
        &observing(
            ExtensionBuilder::default().observer(every_call),
            &[("opcache.jit", "disable")],
        ),
        CALLS,
    );
    let forced = scratch.serve(
        &observing(ExtensionBuilder::default().observer(every_call), &FORCE),
        CALLS,
    );
    assert_eq!(stdout(&forced), stdout(&off));
    assert_eq!(stdout(&off), "99\n");
    assert_eq!(observed_lines(&forced.stderr), observed_lines(&off.stderr));
    let stderr = String::from_utf8_lossy(&forced.stderr);
    let inside = between(&stderr, 1)?;
    for call in ["<g>", "<f>", "</f>", "</g>", "<strcmp>", "</strcmp>"] {
        assert!(
            inside.iter().any(|line| line == call),
            "{call} inside the compiled loop:\n{stderr}"
        );
    }
    Ok(())
}

/// php's `zend_jit_check_support`: an extension observing an opcode past
/// the two silence opcodes runs the tier `Off`, so nothing is recorded
/// or compiled and no `TRACE` line is written whatever
/// `opcache.jit_debug` asks, and the engine's first request raises php's
/// startup warning, once; an engine observing the silence opcodes alone
/// compiles its traces, and their observers are told from inside a
/// compiled loop, before the opline's handler, as the interpreter tells
/// them.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_observed_opcode_past_the_silence_opcodes_refuses_the_tier_with_phps_warning()
-> Result<(), String> {
    let scratch = Scratch::new();
    let refused = observing(ExtensionBuilder::default().opcodes(echo, told), &FORCE);
    let first = scratch.serve(&refused, CALLS);
    assert_eq!(
        stdout(&first),
        "\nWarning: JIT is incompatible with third party extensions that setup user opcode \
         handlers. JIT disabled. in Unknown on line 0\n99\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&first.stderr),
        "<told ZEND_ECHO>\n<told ZEND_ECHO>\n"
    );
    let second = scratch.serve(&refused, CALLS);
    assert_eq!(stdout(&second), "99\n");

    let silenced = observing(ExtensionBuilder::default().opcodes(silence, told), &FORCE);
    let served = scratch.serve(&silenced, CALLS);
    assert_eq!(stdout(&served), "99\n");
    let stderr = String::from_utf8_lossy(&served.stderr);
    assert!(!stderr.contains("Warning"), "{stderr}");
    let inside = between(&stderr, 2)?;
    for opcode in ["<told ZEND_BEGIN_SILENCE>", "<told ZEND_END_SILENCE>"] {
        assert!(
            inside.iter().any(|line| line == opcode),
            "{opcode} inside the compiled loop:\n{stderr}"
        );
    }
    Ok(())
}
