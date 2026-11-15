//! What ext/pcntl does under Linux's `#ifdef`s, each program held
//! to what `php -n` of a Linux build of the pinned tag printed for it (the
//! `dax-php-oracle` image of `ci/oracle/Dockerfile`, aarch64). The oracle
//! of record is macOS's, so these are held on a Linux host alone, the CI
//! Linux lane among them. Each program stays inside the thread that
//! serves it: what forks or takes a process-directed signal is the `php`
//! binary's to serve, as the crate's witnesses are. A warning is held to
//! its text, without the file and line it names.
#![cfg(target_os = "linux")]

use php_ext::test::{Capture, Engine};

/// `source` served by an engine with this crate alone and `php -n`'s
/// plain-text errors: what it printed, each warning's ` in <file> on
/// line <n>` cut.
fn served(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_pcntl::extension()])
        .ini("html_errors", "0")
        .build();
    let out = engine.serve_capture(source, Capture::default()).stdout;
    String::from_utf8_lossy(&out)
        .lines()
        .map(|line| match line.rfind(" in ") {
            Some(at) if line.starts_with("Warning: ") && line.ends_with(char::is_numeric) => {
                &line[..at]
            }
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `pcntl_getcpuaffinity`, `pcntl_setcpuaffinity` and `pcntl_getcpu`, with
/// php's errors. Each id is paired with `-1` from 0 up, so the message
/// names the `-1` while the id is taken and the id once it is not: the
/// bound both name is the last id taken, `maxcpus - 1` as php 8.5.12
/// says it, read without the host's CPU count.
#[test]
fn the_cpu_affinity_reads_and_writes_as_php_s_does() {
    let source = r#"<?php
$cpus = pcntl_getcpuaffinity();
$n = 0; $ints = true; foreach ($cpus as $c) { $n++; $ints = $ints && $c === (int) $c; }
var_dump($n > 0, $ints, pcntl_setcpuaffinity(null, $cpus), pcntl_getcpuaffinity() === $cpus);
var_dump(pcntl_setcpuaffinity(null, ['0']));
try { pcntl_setcpuaffinity(null, []); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
try { pcntl_setcpuaffinity(null, ['x']); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
try { pcntl_setcpuaffinity(null, [1.5]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
$prefix = 'pcntl_setcpuaffinity(): Argument #2 ($cpu_ids) cpu id must be between 0 and ';
$taken = $refused = null;
for ($id = 0; $refused === null && $id < 65536; $id++) {
    try { pcntl_setcpuaffinity(null, [$id, -1]); } catch (ValueError $e) {
        if ($taken === null || $e->getMessage() === $taken) $taken = $e->getMessage(); else $refused = $e->getMessage();
    }
}
$last = $id - 2;
var_dump($taken === $prefix . $last . ' (-1)', $refused === $prefix . $last . ' (' . ($last + 1) . ')');
try { pcntl_getcpuaffinity(99999999); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
var_dump(pcntl_setcpuaffinity(null, $cpus));
$cpu = pcntl_getcpu();
var_dump($cpu === (int) $cpu && $cpu >= 0);
"#;
    let want = r"bool(true)
bool(true)
bool(true)
bool(true)
bool(true)
pcntl_setcpuaffinity(): Argument #2 ($cpu_ids) must not be empty
pcntl_setcpuaffinity(): Argument #2 ($cpu_ids) cpu id invalid value (x)
pcntl_setcpuaffinity(): Argument #2 ($cpu_ids) value must be of type int|string, float given
bool(true)
bool(true)
pcntl_getcpuaffinity(): Argument #1 ($process_id) invalid process (99999999)
bool(true)
bool(true)";
    assert_eq!(served(source), want);
}

/// the real-time range in the mask and the handler table, the waits'
/// argument checks and a wait that times out, and a number the C library
/// keeps for itself.
#[test]
fn the_real_time_signals_and_the_waits_are_php_s() {
    let source = r#"<?php
var_dump(SIGRTMIN, SIGRTMAX, P_PIDFD, CLD_EXITED, SI_KERNEL, POLL_HUP);
var_dump(pcntl_sigprocmask(SIG_BLOCK, [SIGRTMIN, SIGRTMAX], $old));
var_dump(pcntl_sigprocmask(SIG_SETMASK, $old, $was));
foreach ($was as $n) if ($n >= SIGRTMIN) echo $n, "\n";
var_dump(pcntl_sigtimedwait([SIGRTMIN], $info, 0, 1000), $info);
try { pcntl_sigtimedwait([SIGRTMIN], $info, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
try { pcntl_sigtimedwait([SIGRTMIN], $info, 0, 1000000000); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
try { pcntl_sigtimedwait([SIGRTMIN], $info); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
try { pcntl_sigwaitinfo([]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
try { pcntl_sigwaitinfo([65]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
var_dump(pcntl_sigprocmask(SIG_BLOCK, [32]), pcntl_get_last_error() === PCNTL_EINVAL);
var_dump(pcntl_signal(SIGRTMIN, SIG_IGN), pcntl_signal_get_handler(SIGRTMIN), pcntl_signal(SIGRTMIN, SIG_DFL));
try { pcntl_signal(65, SIG_DFL); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
"#;
    let want = r"int(34)
int(64)
int(3)
int(1)
int(128)
int(6)
bool(true)
bool(true)
34
64
bool(false)
NULL
pcntl_sigtimedwait(): Argument #3 ($seconds) must be greater than or equal to 0
pcntl_sigtimedwait(): Argument #4 ($nanoseconds) must be between 0 and 1e9
pcntl_sigtimedwait(): At least one of argument #3 ($seconds) or argument #4 ($nanoseconds) must be greater than 0
pcntl_sigwaitinfo(): Argument #1 ($signals) must not be empty
pcntl_sigwaitinfo(): Argument #1 ($signals) signals must be between 1 and 64

Warning: pcntl_sigprocmask(): Invalid argument
bool(false)
bool(true)
bool(true)
int(1)
bool(true)
pcntl_signal(): Argument #1 ($signal) must be less than 65";
    assert_eq!(served(source), want);
}
/// `pcntl_waitid`'s `$info` for a child's exit carries Linux's `utime` and
/// `stime` between `status` and `pid`, as floats. The child is the test's
/// own, spawned before the program is served and reaped by it.
#[test]
fn a_child_s_siginfo_carries_its_clock_ticks() -> std::io::Result<()> {
    let child = std::process::Command::new("sh")
        .args(["-c", "exit 3"])
        .spawn()?;
    let source = r"<?php
var_dump(pcntl_waitid(P_PID, CHILD, $info, WEXITED));
foreach ($info as $k => $v) echo $k, ' ', $v === (int) $v ? 'integer' : 'double', PHP_EOL;
var_dump($info['signo'] === SIGCHLD, $info['code'] === CLD_EXITED, $info['status'], $info['pid'] === CHILD);
"
    .replace("CHILD", &child.id().to_string());
    let want = "bool(true)
signo integer
errno integer
code integer
status integer
utime double
stime double
pid integer
uid integer
bool(true)
bool(true)
int(3)
bool(true)";
    assert_eq!(served(&source), want);
    Ok(())
}

/// `Pcntl\QosClass` is declared off Apple's platforms too, as php's MINIT
/// registers it there, without the two functions that take and give it.
#[test]
fn the_qos_enum_is_declared_without_its_functions() {
    let source = r"<?php
$cases = Pcntl\QosClass::cases();
var_dump(enum_exists('Pcntl\QosClass'), $cases[2], $cases[4], isset($cases[5]));
var_dump(function_exists('pcntl_getqos_class'), function_exists('pcntl_setqos_class'));
";
    let want = "bool(true)
enum(Pcntl\\QosClass::Default)
enum(Pcntl\\QosClass::Background)
bool(false)
bool(false)
bool(false)";
    assert_eq!(served(source), want);
}
