//! Statement events as `TestExt` records them (#1823): a Zend
//! extension's `statement_handler`, told at each `EXT_STMT` a compile
//! with extended info leaves, in the order a stepping debugger would
//! read them ([`crate::statement`]).
//!
//! The lines are php's. Where each `EXT_STMT` stands, and its line, is
//! what the oracle's `phpdbg -n -e '-p*'` prints of `main.php` and
//! `inc.php` (`L0006 0000 EXT_STMT`, …), and the compile-identity lane
//! holds the engine's oplines to the oracle's dump under the
//! extended-info regime (#1822). The order they are reached in is what the
//! zend-test oracle prints of `main.php` under `-e` with
//! `zend_test.observer.show_opcode_in_user_handler=ZEND_EXT_STMT`, one
//! line per `EXT_STMT` it reaches, with the frames it reaches them in.
//! No oracle loads a Zend extension with a statement handler, so the
//! sequence below is those two read together, not one the oracle
//! printed.

use std::path::Path;

use super::observer::serve_main;
use super::stepped::Stepper;
use super::test_ext::{Event, Events, Phase, Recording, TestExt, statement_name};
use crate::api::abi::Statement;
use crate::api::raising::Thrown;
use crate::engine::{Engine, EngineBuilder, RequestDescription};
use crate::probe::Probe;
use crate::test::{Capture, Files, handed_back};
use crate::{CommandLine, tier::Tier};

/// The stepped program: a loop that calls a function, an include, and
/// an `eval` that calls a function the include declared.
const MAIN: &str = "<?php
function f($x) {
    $y = $x + 1;
    return $y;
}
$a = 1;
for ($i = 0; $i < 2; $i++) {
    $a = f($a);
}
include __DIR__ . '/inc.php';
$a = eval('return g($a);');
if ($a > 2) {
    echo $a, \"\\n\";
}
";

/// What `MAIN` includes: a declaration, whose statement php hoists and
/// steps nowhere, and a body it steps.
const INC: &str = "<?php
function g($v) {
    return $v * 2;
}
";

/// Each statement `MAIN` runs, where its frame stands: the `for`'s
/// initialiser and then its condition on line 7 before each pass, as
/// php compiles the condition after the body with a statement of its
/// own (`L0007 0014 EXT_STMT`); `include`'s file runs nothing, its
/// function's statement hoisted; the `eval`'s one statement on its
/// line 1; and `echo`'s two arguments two statements of line 13.
const STEPPED: &[&str] = &[
    "main.php:6",
    "main.php:7",
    "main.php:7",
    "main.php:8",
    "main.php:3",
    "main.php:4",
    "main.php:7",
    "main.php:8",
    "main.php:3",
    "main.php:4",
    "main.php:7",
    "main.php:10",
    "main.php:11",
    "main.php(11) : eval()'d code:1",
    "inc.php:3",
    "main.php:12",
    "main.php:13",
    "main.php:13",
];

/// A generator's body and a fiber's, each left and resumed: a statement
/// is told in the frame that stands at it, whichever stack it is on.
const RESUMED: &str = "<?php
function g() {
    yield 1;
    yield 2;
}
foreach (g() as $v) {
    echo $v;
}
$f = new Fiber(function () {
    Fiber::suspend();
    echo \"f\\n\";
});
$f->start();
$f->resume();
";

/// Each statement `RESUMED` runs: the generator's on lines 3, 4 and its
/// closing brace's 5 between the loop's, and the fiber's closure's on
/// lines 10 to 12, left at its suspend for line 14's resume, as the
/// zend-test oracle reaches them.
const RESUMED_STEPPED: &[&str] = &[
    "main.php:6",
    "main.php:3",
    "main.php:7",
    "main.php:4",
    "main.php:7",
    "main.php:5",
    "main.php:9",
    "main.php:13",
    "main.php:10",
    "main.php:14",
    "main.php:11",
    "main.php:12",
];

/// The statements told, each as the extension it was told to and where
/// it stood.
fn statements(events: &Events) -> Vec<Event> {
    events.only(&[Phase::Statement(String::new())]).0
}

/// Each of `lines` told to each of `extensions`, in turn.
fn told(lines: &[&str], extensions: &[u8]) -> Vec<Event> {
    lines
        .iter()
        .flat_map(|line| {
            extensions
                .iter()
                .map(|&n| Event::Ext(n, Phase::Statement((*line).to_owned())))
        })
        .collect()
}

/// `main` and `INC` written into a directory of their own, `main`
/// served under the engine `builder` makes with two stepping Zend
/// extensions; the statements told and what it echoed.
fn stepped(name: &str, builder: EngineBuilder, main: &str) -> (Vec<Event>, String) {
    let rec = Recording::start();
    let engine = builder
        .extensions(&[TestExt::<0>::stepping(), TestExt::<1>::stepping()])
        .build();
    let files = Files::new(&format!("statements-{name}"), &[("inc.php", INC)]);
    rec.mark("built");
    let served = serve_main(&engine, &files, main);
    rec.mark("served");
    (
        statements(&rec.events().between("built", "served")),
        served.stdout_text(),
    )
}

/// Under extended info each Zend extension is told of each statement
/// the program runs, where php's `EXT_STMT` stands and in the order
/// php reaches them, every extension in the order they start, as
/// `ZEND_EXT_STMT` walks `zend_extensions`.
#[test]
fn each_zend_extension_is_told_each_statement_where_php_steps_it() {
    let (told_now, stdout) = stepped("loop", Engine::cli_builder().extended_info(true), MAIN);
    assert_eq!(stdout, "6\n");
    assert_eq!(told_now, told(STEPPED, &[0, 1]));
}

/// A generator resumed and a fiber switched to tell each statement where
/// its frame stands, under the loop and under the debugging host that
/// steps them.
#[test]
fn a_generator_and_a_fiber_tell_each_statement_where_they_resume() {
    let (told_now, stdout) = stepped(
        "resumed",
        Engine::cli_builder().extended_info(true),
        RESUMED,
    );
    assert_eq!(stdout, "12f\n");
    assert_eq!(told_now, told(RESUMED_STEPPED, &[0, 1]));
    let debugged = stepped(
        "resumed_debugger",
        Engine::cli_builder()
            .extended_info(true)
            .tier::<Stepper<false>>(),
        RESUMED,
    );
    assert_eq!(debugged, (told_now, stdout), "the debugging host");
}

/// Without extended info no `EXT_STMT` is compiled, so a Zend extension
/// with a statement handler is told of nothing.
#[test]
fn no_statement_is_told_without_extended_info() {
    let (told_now, stdout) = stepped("plain", Engine::cli_builder(), MAIN);
    assert_eq!(stdout, "6\n");
    assert_eq!(told_now, []);
}

/// A stepped program tells the statements the loop tells, under `Probe`
/// and under the debugging host and the recorder that drive `step` one
/// handler at a time: `EXT_STMT`'s handler is the loop's, whichever
/// drives it.
#[test]
fn a_program_a_tier_steps_tells_the_statements_the_loop_tells() {
    fn under<T: Tier>(name: &str) -> (Vec<Event>, String) {
        stepped(
            name,
            Engine::cli_builder().extended_info(true).tier::<T>(),
            MAIN,
        )
    }
    let plain = under::<crate::tier::Nop>("nop");
    assert_eq!(plain.0, told(STEPPED, &[0, 1]));
    assert_eq!(under::<Probe>("probe"), plain, "Probe");
    assert_eq!(
        under::<Stepper<false>>("debugger"),
        plain,
        "the debugging host"
    );
    assert_eq!(under::<Stepper<true>>("recorder"), plain, "the recorder");
}

/// Extension `N`'s statement handler that throws at `main.php:3`,
/// recording where it was told, and ` pending` where a throwable stood
/// pending as it was.
fn throwing<const N: u8>(statement: &mut Statement<'_>) -> Result<(), Thrown> {
    let at = statement_name(statement);
    let pending = if statement.is_pending() {
        " pending"
    } else {
        ""
    };
    super::test_ext::record(N, Phase::Statement(format!("{at}{pending}")));
    if at == "main.php:3" {
        let message = format!("stopped by {N}");
        return Err(crate::api::raising::throw_error(
            statement,
            "Exception",
            &message,
        ));
    }
    Ok(())
}

/// Two statement handlers that throw at `main.php:3`: the first's
/// throwable is raised where the statement stands, before it runs, so
/// `f`'s first statement never echoes, and the line it carries is the
/// statement's; the second is told after it with it standing pending,
/// as php's walk tests nothing between two handlers, and what it throws
/// takes the slot with the first as its `previous`. The catch's `echo`
/// of six arguments is six statements of line 5.
#[test]
fn what_a_statement_handler_throws_is_raised_where_the_statement_stands() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extended_info(true)
        .extensions(&[
            TestExt::<0>::builder()
                .zend_extension(TestExt::<0>::zend_row(throwing::<0>))
                .build("test_ext_0"),
            TestExt::<1>::builder()
                .zend_extension(TestExt::<1>::zend_row(throwing::<1>))
                .build("test_ext_1"),
        ])
        .build();
    let files = Files::new("statements-thrown", &[]);
    rec.mark("built");
    let served = serve_main(
        &engine,
        &files,
        "<?php\nfunction f() {\n    echo 'ran';\n}\ntry { f(); } catch (Exception $e) {\n    \
         echo $e->getMessage(), ' after ', $e->getPrevious()->getMessage(), ' at ', $e->getLine(), \"\\n\";\n}\n",
    );
    rec.mark("served");
    assert_eq!(
        served.stdout_text(),
        "stopped by 1 after stopped by 0 at 3\n"
    );
    assert_eq!(
        statements(&rec.events().between("built", "served")),
        [
            told(&["main.php:5"], &[0, 1]),
            told(&["main.php:3"], &[0]),
            told(&["main.php:3 pending"], &[1]),
            told(&["main.php:6"; 6], &[0, 1]),
        ]
        .concat()
    );
}

/// `-r`'s code under `-e`, as `zend_eval_stringl` serves it with
/// `EG(no_extensions)` set (`Z/Zend/zend_execute_API.c:1357-1382`):
/// nothing it reaches is told, an included file's function and an
/// `eval`'s code compiled with extended info among it; a shutdown
/// function served after it returned is told of what it reaches, its
/// `echo` of two arguments two statements of one line; and
/// after a fatal in the code, which leaves past the clearing, it is
/// told of nothing; a thrown `Error` left uncaught is no fatal, and
/// `zend_eval_string_ex` renders it after the clearing.
#[test]
fn a_string_the_host_evaluates_tells_no_statement_until_it_returns() {
    fn command_line(engine: &Engine, dir: &Path, code: &str) -> String {
        let description = RequestDescription {
            cwd: dir.to_path_buf(),
            sapi_name: String::from("cli"),
            ..RequestDescription::default()
        };
        let (_, sink) = engine.command_line(
            description,
            CommandLine::Direct(code.as_bytes().to_vec()),
            Box::new(Capture::default()),
        );
        let sink: Capture = handed_back(sink).unwrap_or_default();
        String::from_utf8_lossy(&sink.stdout).into_owned()
    }
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extended_info(true)
        .extensions(&[TestExt::<0>::stepping()])
        .build();
    let files = Files::new(
        "statements-host-string",
        &[
            ("inc.php", INC),
            ("down.php", "<?php\necho g(3), \"\\n\";\n"),
        ],
    );
    let dir = files.path();
    let inc = dir.join("inc.php");
    let down = dir.join("down.php");
    let shutdown = format!(
        "register_shutdown_function(function () {{ include_once '{}'; include '{}'; }});",
        inc.display(),
        down.display()
    );
    rec.mark("built");
    let stdout = command_line(
        &engine,
        dir,
        &format!(
            "{shutdown} include_once '{}'; echo g(2), \"\\n\"; eval('echo 1, \"\\n\";');",
            inc.display()
        ),
    );
    rec.mark("returned");
    assert_eq!(stdout, "4\n1\n6\n");
    let fatal = command_line(
        &engine,
        dir,
        &format!(
            "{shutdown} ini_set('memory_limit', '2M'); $s = '.'; while (true) {{ $s .= $s; }}"
        ),
    );
    rec.mark("fatal");
    assert!(fatal.ends_with("6\n"), "{fatal}");
    let events = rec.events();
    assert_eq!(
        statements(&events.between("built", "returned")),
        told(&["down.php:2", "inc.php:3", "down.php:2"], &[0])
    );
    assert_eq!(statements(&events.between("returned", "fatal")), []);
}
