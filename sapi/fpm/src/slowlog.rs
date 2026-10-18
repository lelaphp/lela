//! `fpm_request_check_timed_out` and the slow log: each child's request
//! held by the master to `request_slowlog_timeout`, which writes the
//! request to the pool's slow log once, and to
//! `request_terminate_timeout`, which ends the child with `SIGTERM`.

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::master::Fpm;
use crate::scoreboard::{Action, Stage, Update};
use crate::zlog;

/// `str_purify_filename`: each byte outside printable ASCII a `.`.
fn purify(name: &[u8]) -> String {
    name.iter()
        .map(|&b| {
            if (b' '..=0x7f).contains(&b) {
                char::from(b)
            } else {
                '.'
            }
        })
        .collect()
}

/// `fpm_request_check_timed_out` for child `c` of pool `p`.
pub(crate) fn check_timed_out(
    fpm: &mut Fpm,
    p: usize,
    c: usize,
    now: i64,
    terminate: i32,
    slowlog: i32,
    track_finished: bool,
) {
    let Some(scoreboard) = fpm.pools[p].scoreboard else {
        return;
    };
    let child = &fpm.pools[p].children[c];
    let Some(proc) = scoreboard
        .proc(child.scoreboard_i)
        .and_then(crate::scoreboard::Proc::view_once)
    else {
        zlog!(Notice @ fpm_request_check_timed_out:232, "failed to acquire scoreboard");
        return;
    };
    if child.slow_logged.is_some_and(|at| at != proc.accepted_us) {
        fpm.pools[p].children[c].slow_logged = None;
    }
    let stage = proc.stage();
    if stage <= Stage::Accepting || !(stage < Stage::End || track_finished) {
        return;
    }
    let elapsed = now - proc.accepted_us;
    let (secs, micros) = (elapsed / 1_000_000, elapsed % 1_000_000);
    let child = &fpm.pools[p].children[c];
    let pid = child.pid;
    let name = fpm.pools[p].name();
    let request = format!(
        "\"{} {}{}{}\"",
        String::from_utf8_lossy(&proc.request_method),
        String::from_utf8_lossy(&proc.request_uri),
        if proc.query_string.is_empty() {
            ""
        } else {
            "?"
        },
        String::from_utf8_lossy(&proc.query_string)
    );
    let script = purify(&proc.script_filename);
    if child.slow_logged.is_none()
        && slowlog != 0
        && stage == Stage::Executing
        && secs >= i64::from(slowlog)
    {
        fpm.pools[p].children[c].slow_logged = Some(proc.accepted_us);
        fpm.pools[p].children[c].traced = true;
        crate::ffi::trace::signal(pid);
        zlog!(
            Warning @ fpm_request_check_timed_out:268,
            "[pool {name}] child {pid}, script '{script}' (request: {request}) executing too slow ({secs}.{micros:06} sec), logging"
        );
    } else if terminate != 0 && secs >= i64::from(terminate) {
        kill(Pid::from_raw(pid), Signal::SIGTERM).ok();
        zlog!(
            Warning @ fpm_request_check_timed_out:279,
            "[pool {name}] child {pid}, script '{script}' (request: {request}) execution timed out ({secs}.{micros:06} sec), terminating"
        );
    }
}

/// `fpm_php_trace`, the tracer of child `c` of pool `p` once it stopped:
/// the slow request counted, and where the pool's slow log opens and the
/// child's memory is readable, the entry `fpm_php_trace_dump` writes, its
/// time, pool and pid and the program it serves; the child let go on
/// after, whatever came of it.
pub(crate) fn trace(fpm: &mut Fpm, p: usize, c: usize) {
    if let Some(scoreboard) = fpm.pools[p].scoreboard {
        scoreboard.update(
            Update {
                slow_rq: 1,
                ..Update::ZERO
            },
            Action::Inc,
        );
    }
    let pid = fpm.pools[p].children[c].pid;
    zlog!(Notice @ fpm_php_trace:195, "about to trace {pid}");
    let path = fpm.pools[p].config.slowlog.clone().unwrap_or_default();
    // `fopen(slowlog, "a+")`.
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(crate::conf::path(&path));
    match opened {
        Ok(mut file) => {
            if crate::ffi::trace::ready(pid) {
                std::io::Write::write_all(&mut file, &dump(fpm, p, c)).ok();
                crate::ffi::trace::close(pid);
            }
        }
        Err(e) => {
            crate::zlog_sys!(
                Error @ fpm_php_trace:200,
                crate::conf::errno(&e),
                "unable to open slowlog ({})",
                String::from_utf8_lossy(&path)
            );
        }
    }
    crate::ffi::trace::resume(pid);
    fpm.pools[p].children[c].traced = false;
    zlog!(Notice @ fpm_php_trace:223, "finished trace of {pid}");
}

/// `fpm_php_trace_dump`'s head: the time, the pool and the pid, and the
/// program the request runs as the scoreboard holds it.
fn dump(fpm: &Fpm, p: usize, c: usize) -> Vec<u8> {
    let child = &fpm.pools[p].children[c];
    let script = fpm.pools[p]
        .scoreboard
        .and_then(|scoreboard| scoreboard.proc(child.scoreboard_i))
        .and_then(crate::scoreboard::Proc::view_once)
        .map(|proc| proc.script_filename)
        .unwrap_or_default();
    let mut entry = b"\n".to_vec();
    entry.extend_from_slice(&crate::zlog::time_now());
    entry.extend_from_slice(
        format!(
            "[pool {}] pid {}\nscript_filename = ",
            fpm.pools[p].name(),
            child.pid
        )
        .as_bytes(),
    );
    entry.extend_from_slice(&script);
    entry.push(b'\n');
    entry
}
