//! The debugging host's own stepping held to the loop's observer events
//! (design §4.3 rows 8 and 9; #2009): a program served by this crate's
//! `phpdbg`, its `Debugger` tier bound and driven by the prompt's
//! commands, raises the begins, ends and errors the plain loop raises
//! for the same program, in the same order, with two recording
//! observers in the bundle. The engine's tests hold a copy of the
//! stepping loop (`php_engine`'s `tests/stepped.rs`); this holds the
//! host itself, so a change to how it unwinds, what it hands back or
//! which tier call it makes is measured here.
//!
//! The recording extension is the engine tests' `TestExt`, compiled
//! here against `php_ext` as `php_ext`'s lifecycle test compiles it. Its
//! log is the process's, so the host runs in a process of its own: this
//! test binary spawned again with the ignored test `child` selected,
//! which runs `php_sapi_phpdbg::main_with` with the observers beside the
//! bundle, the commands on its stdin, and writes what it recorded to a
//! file for the test that spawned it.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use php_ext as api;
use php_sapi::{
    Aborted, Engine, HostServes, OutputSink, Primary, RequestDescription, ResponseHead,
};

#[path = "../../../crates/php_engine/src/tests/test_ext.rs"]
#[allow(dead_code, reason = "the engine's own tests read the rest")]
mod test_ext;

use test_ext::Phase::{Begin, End, Error, ObserverInit, Rinit};
use test_ext::{Event, Events, Recording, TestExt};

/// Where the child writes what it recorded; set only for the child.
const EVENTS_TO: &str = "PHPDBG_OBSERVED_EVENTS_TO";
/// The program the child debugs.
const PROGRAM: &str = "PHPDBG_OBSERVED_PROGRAM";

/// The extensions both runs carry beside the bundle.
fn observers() -> Vec<api::abi::Extension> {
    vec![TestExt::<0>::observing(), TestExt::<1>::observing()]
}

/// What a test compares: each request's observer events, a request
/// begun by the first extension's `rinit`, every one `Debug`-written.
fn requests(events: &Events) -> Vec<Vec<String>> {
    let kept = events.only(&[
        Rinit(None),
        ObserverInit(String::new()),
        Begin(String::new()),
        End(String::new(), None),
        Error(String::new()),
    ]);
    let mut requests: Vec<Vec<String>> = Vec::new();
    for event in kept.0 {
        match event {
            Event::Ext(0, Rinit(_)) => requests.push(Vec::new()),
            Event::Ext(_, Rinit(_)) => {}
            event => match requests.last_mut() {
                Some(request) => request.push(format!("{event:?}")),
                None => requests.push(vec![format!("{event:?}")]),
            },
        }
    }
    requests
}

/// The child: phpdbg over the program `PROGRAM` names, the observers
/// beside the bundle, the commands on stdin; what it recorded, a request
/// to a line of its events joined by tabs, written where `EVENTS_TO`
/// names. A run of the test binary that sets neither does nothing.
#[test]
#[ignore = "the phpdbg the other tests spawn, run by them alone"]
fn child() {
    if let (Some(to), Some(program)) = (std::env::var_os(EVENTS_TO), std::env::var_os(PROGRAM)) {
        recorded(&to, &program);
    }
}

/// The child's run, over `program` and into the file `to`.
fn recorded(to: &OsStr, program: &OsStr) {
    let rec = Recording::start();
    let argv = [
        b"phpdbg".to_vec(),
        b"-qIb".to_vec(),
        b"-n".to_vec(),
        program.as_encoded_bytes().to_vec(),
    ];
    php_sapi_phpdbg::main_with(&argv, &observers());
    let written: Vec<String> = requests(&rec.events())
        .iter()
        .map(|request| request.join("\t"))
        .collect();
    std::fs::write(to, written.join("\n")).unwrap();
}

/// A directory for the test `name`, with `source` as `t.php`.
fn laid(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sapi-phpdbg-observed-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(dir.join("t.php"), source).unwrap();
    dir
}

/// Each request's events the debugging host raised over `t.php` of
/// `dir`, driven by `commands`, and what it wrote.
fn debugged(dir: &Path, commands: &str) -> (Vec<Vec<String>>, String) {
    let to = dir.join("events");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(EVENTS_TO, &to)
        .env(PROGRAM, dir.join("t.php"))
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let written = std::fs::read_to_string(&to).unwrap_or_default();
    let requests = written
        .lines()
        .map(|line| {
            line.split('\t')
                .filter(|e| !e.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .collect();
    (requests, String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What the request's output and log were handed.
#[derive(Default)]
struct Kept(Vec<u8>);

impl OutputSink for Kept {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn flush(&mut self) {}
    fn send_headers(&mut self, _: &ResponseHead) {}
    fn log(&mut self, entry: &[u8]) {
        self.0.extend_from_slice(entry);
    }
    fn write_stderr(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

/// Each request's events the plain loop raised over `t.php` of `dir`,
/// served as phpdbg's run serves it, bare and under phpdbg's name, with
/// the bundle and the observers.
fn plain(dir: &Path) -> Vec<Vec<String>> {
    let rec = Recording::start();
    let mut extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    extensions.extend(observers());
    let engine = Engine::builder()
        .sapi_name("phpdbg")
        .extensions(&extensions)
        .build();
    let file = dir.join("t.php").into_os_string().into_encoded_bytes();
    engine.serve(
        RequestDescription {
            argv: vec![file.clone()],
            cwd: dir.to_path_buf(),
            sapi_name: String::from("phpdbg"),
            script_name: Some(file),
            primary: Primary::Bare,
            ..RequestDescription::default()
        },
        Box::new(Kept::default()),
        |_| HostServes::Program,
    );
    drop(engine);
    requests(&rec.events())
}

/// The run's request: the first the host served that observed anything,
/// since the session's own requests before it run no program. The cycle
/// after it, held at the context's first opline until `q`, observes
/// nothing, as php executes nothing there (#2359).
fn the_run(requests: &[Vec<String>]) -> &[String] {
    requests
        .iter()
        .find(|request| !request.is_empty())
        .map_or(&[], Vec::as_slice)
}

/// `source` debugged under each of `sessions`, the commands the prompt
/// reads, each run's events held to the plain loop's, and each session's
/// output to hold every line of `shown` among its own.
fn held_to_the_loop(name: &str, source: &str, sessions: &[(&str, &[&str])]) {
    let dir = laid(name, source);
    let plain = plain(&dir);
    let [loop_run] = plain.as_slice() else {
        panic!("{name}: the plain loop served one request: {plain:#?}");
    };
    assert!(
        loop_run.iter().any(|event| event.contains("Begin(")),
        "{name}: the loop observed nothing: {loop_run:#?}"
    );
    for (commands, shown) in sessions {
        let (requests, out) = debugged(&dir, commands);
        for line in *shown {
            assert!(
                out.contains(line),
                "{name}, {commands:?}: the session wrote {line:?}:\n{out}"
            );
        }
        assert_eq!(
            the_run(&requests),
            loop_run.as_slice(),
            "{name}, {commands:?}: the debugged run's events (left), the loop's (right):\n{out}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Calls two deep, run whole, and stepped into and over from a
/// breakpoint: every begin and end the loop raises, each end with what
/// the frame returned.
#[test]
fn calls_raise_the_loop_s_events() {
    held_to_the_loop(
        "calls",
        "<?php\nfunction g($y) {\n  return $y * 2;\n}\nfunction f($x) {\n  $r = g($x);\n  return $r + 1;\n}\necho f(1), f(2), \"\\n\";\n",
        &[
            ("r\nq\n", &["35\n[Script ended normally]"]),
            (
                "b f\nr\ns\ns\nn\nc\nc\nq\n",
                &[">00006:   $r = g($x);", "[Script ended normally]"],
            ),
            (
                "b 6\nr\nn\nn\nn\nc\nc\nc\nq\n",
                &[">00007:   return $r + 1;", "[Script ended normally]"],
            ),
        ],
    );
}

/// A throwable a caller catches, through a `finally`: the frames it
/// leaves end with nothing returned, run whole and stepped through.
#[test]
fn a_caught_unwind_raises_the_loop_s_events() {
    held_to_the_loop(
        "caught",
        "<?php\nfunction b() {\n  throw new Exception('x');\n}\nfunction a() {\n  b();\n}\ntry {\n  a();\n} catch (Exception $e) {\n  echo \"c\\n\";\n} finally {\n  echo \"f\\n\";\n}\n",
        &[
            ("r\nq\n", &["c\nf\n[Script ended normally]"]),
            (
                "b a\nr\ns\ns\ns\ns\ns\nc\nq\n",
                &[">00006:   b();", "[Script ended normally]"],
            ),
        ],
    );
}

/// A throwable nothing catches: the host stops at it, says so, and the
/// run ends in it; every frame it leaves ends with nothing returned.
#[test]
fn an_uncaught_unwind_raises_the_loop_s_events() {
    held_to_the_loop(
        "uncaught",
        "<?php\nfunction b() {\n  throw new Exception('x');\n}\nfunction a() {\n  b();\n}\na();\n",
        &[
            ("r\nc\nq\n", &["[Uncaught Exception in "]),
            (
                "b a\nr\ns\ns\nc\nc\nq\n",
                &[">00006:   b();", "[Uncaught Exception in "],
            ),
        ],
    );
}

/// A fatal inside a call: the error is told to the observers, and the
/// frames it ends end with nothing returned, the host's prompt at the
/// fatal's log between.
#[test]
fn a_fatal_raises_the_loop_s_events() {
    held_to_the_loop(
        "fatal",
        "<?php\nini_set('memory_limit', '2M');\nfunction big() {\n  return str_repeat('x', 4 << 20);\n}\nfunction a() {\n  return big();\n}\na();\n",
        &[
            (
                "r\nc\nq\n",
                &["Allowed memory size of 2097152 bytes exhausted"],
            ),
            (
                "b big\nr\ns\nc\nq\n",
                &[
                    ">00004:   return str_repeat('x', 4 << 20);",
                    "Allowed memory size",
                ],
            ),
        ],
    );
}

/// The execution context held at the prompt is compiled and not
/// executed, as php's `phpdbg_compile` leaves it until `run`'s
/// `zend_execute` (#2359): a session that quits there hands an observer
/// nothing of the main file, nor does the cycle held again after a run;
/// and the run's begin of the main file comes after what the prompt
/// served before `r`, here an `ev`'s code.
#[test]
fn a_held_context_raises_nothing_until_run() {
    let dir = laid(
        "held",
        "<?php\nfunction g($y) {\n  return $y * 2;\n}\n$f = new Fiber(function () { return g(2); });\n$f->start();\necho g(1), \"\\n\";\n",
    );
    let plain = plain(&dir);
    let [loop_run] = plain.as_slice() else {
        panic!("the plain loop served one request: {plain:#?}");
    };
    let (requests, out) = debugged(&dir, "q\n");
    assert!(
        requests.iter().all(Vec::is_empty),
        "`q` at the held prompt observed something:\n{requests:#?}\n{out}"
    );
    for (commands, ran) in [("r\nq\n", 1), ("r\nr\nq\n", 2)] {
        let (requests, out) = debugged(&dir, commands);
        let runs: Vec<_> = requests.iter().filter(|r| !r.is_empty()).collect();
        assert_eq!(
            runs,
            vec![loop_run; ran],
            "{commands:?}: each run's events (left), the loop's (right), and no other request's:\n{out}"
        );
    }
    let (requests, out) = debugged(&dir, "ev 7\nr\nq\n");
    let runs: Vec<_> = requests.iter().filter(|r| !r.is_empty()).collect();
    let [run] = runs.as_slice() else {
        panic!("one request observed anything: {requests:#?}\n{out}");
    };
    let main = run
        .iter()
        .position(|event| event.contains("Begin(\"file 't.php'\")"))
        .unwrap_or_else(|| panic!("the run began the main file: {run:#?}"));
    assert!(
        main > 0 && run[..main].iter().any(|event| event.contains("Begin(")),
        "the main file began before the `ev` the prompt read first: {run:#?}"
    );
    assert_eq!(
        &run[main..],
        &loop_run[loop_run
            .iter()
            .position(|event| event.contains("Begin(\"file 't.php'\")"))
            .unwrap_or_default()..],
        "the run from its begin of the main file (left), the loop's (right)"
    );
    std::fs::remove_dir_all(&dir).ok();
}
