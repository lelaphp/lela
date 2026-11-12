//! `fpm_request_check_timed_out` and the slow log: each child's request
//! held by the master to `request_slowlog_timeout`, which writes the
//! request to the pool's slow log once, and to
//! `request_terminate_timeout`, which ends the child with `SIGTERM`.
//!
//! The request's fields, the method, path and query of the line naming a
//! slow or timed-out request, and the slow log's `script_filename` and
//! each frame's name and file, which hold the program's path, have each
//! `\r` and `\n` written as two bytes ([`php_sapi::log_field`], #4355), so
//! no request writes a line of its own into either log. A slow log the
//! trace finds gone is made again owner-only, `0600`, as the
//! configuration makes it at the start, so only its owner reads what a
//! request left in it.

use std::os::unix::fs::OpenOptionsExt as _;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use php_sapi::log_field as field;

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
    // The request's method, path and query are its own, each with a line
    // break escaped (#4355).
    let (method, uri, query) = (
        field(&proc.request_method),
        field(&proc.request_uri),
        field(&proc.query_string),
    );
    let request = crate::zformat!(
        "\"{} {}{}{}\"",
        method.as_ref(),
        uri.as_ref(),
        if query.is_empty() { "" } else { "?" },
        query.as_ref()
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
            bytes "[pool {}] child {}, script '{}' (request: {}) executing too slow ({} sec), logging",
            name,
            pid,
            script,
            request,
            format!("{secs}.{micros:06}")
        );
    } else if terminate != 0 && secs >= i64::from(terminate) {
        kill(Pid::from_raw(pid), Signal::SIGTERM).ok();
        zlog!(
            Warning @ fpm_request_check_timed_out:279,
            bytes "[pool {}] child {}, script '{}' (request: {}) execution timed out ({} sec), terminating",
            name,
            pid,
            script,
            request,
            format!("{secs}.{micros:06}")
        );
    }
}

/// `fpm_php_trace`, the tracer of child `c` of pool `p` once it stopped:
/// the slow request counted, and where the pool's slow log opens and the
/// child's memory is readable, the entry `fpm_php_trace_dump` writes, its
/// time, pool and pid, the program it serves and the request's frames;
/// the child let go on after, whatever came of it. php's order holds by
/// scope: the memory closes (`fpm_trace_close`, the master detached on
/// Linux) as the `if let` that opened it ends, the slow log is closed as
/// its arm ends (`fclose`), and only then is the child sent `SIGCONT`.
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
    // `fopen(slowlog, "a+")`; the trace makes a slow log it finds gone
    // `0600`, as the configuration's open made it (#4355).
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(crate::conf::path(&path));
    match opened {
        Ok(mut file) => {
            if let Some(memory) = crate::ffi::trace::ready(pid) {
                std::io::Write::write_all(&mut file, &dump(fpm, p, c, &memory)).ok();
            }
        }
        Err(e) => {
            crate::zlog_sys!(
                Error @ fpm_php_trace:200,
                crate::conf::errno(&e),
                bytes "unable to open slowlog ({})",
                path
            );
        }
    }
    crate::ffi::trace::resume(pid);
    fpm.pools[p].children[c].traced = false;
    zlog!(Notice @ fpm_php_trace:223, "finished trace of {pid}");
}

/// `fpm_php_trace_dump`: the head, the time, the pool and the pid and
/// the program the request runs as the `scoreboard` holds it, then the
/// request's frames read out of the stopped child ([`frames`]), and
/// `+++ dump failed` where a read of them failed.
fn dump(fpm: &Fpm, p: usize, c: usize, memory: &crate::ffi::trace::Memory) -> Vec<u8> {
    let child = &fpm.pools[p].children[c];
    let (script, word) = fpm.pools[p]
        .scoreboard
        .and_then(|scoreboard| scoreboard.proc(child.scoreboard_i))
        .and_then(crate::scoreboard::Proc::view_once)
        .map(|proc| (proc.script_filename, proc.chain_word))
        .unwrap_or_default();
    let mut entry = b"\n".to_vec();
    entry.extend_from_slice(&crate::zlog::time_now());
    entry.extend_from_slice(&crate::zformat!(
        "[pool {}] pid {}\nscript_filename = ",
        fpm.pools[p].name(),
        child.pid
    ));
    entry.extend_from_slice(&field(&script));
    entry.push(b'\n');
    let depth = fpm.pools[p].config.request_slowlog_trace_depth;
    let read = |at: u64, into: &mut [u8]| memory.read(at, into);
    if frames(&read, word, depth, &mut entry).is_none() {
        entry.extend_from_slice(b"+++ dump failed\n");
    }
    entry
}

/// The bytes `fpm_trace_get_strz` reads into php's 1024-byte buffer:
/// at most 1023, and none past a NUL.
const STRZ: u64 = 1023;

/// `valid_ptr`: an address that is not zero and is a word's.
fn valid(at: u64) -> bool {
    at != 0 && at.is_multiple_of(8)
}

/// `fpm_php_trace_dump`'s walk of `EG(current_execute_data)`, out of the
/// child through `read`, from the word at `word` that names the innermost
/// room, into `out`: per frame, up to `depth` of them, one line, the
/// room's address, the name of what it serves, `[INCLUDE_OR_EVAL]` for a
/// file's included or evaluated, `???` where it names nothing, then the
/// file and the line of the nearest user code it was called from, its
/// own caller's or further down, `unknown:0` where none is; the main op
/// array's frame ends the walk. A room the engine laid that php's chain
/// does not hold, a call still being built or a frame being popped, is
/// passed over. None where a read failed, what was written standing.
///
/// A sleep two calls down reads, where the child's memory opens to the
/// master:
///
/// ```text
/// [0x000000010c8b4130] usleep() /srv/slow.php:2
/// [0x000000010c8b40d0] f() /srv/slow.php:3
/// ```
fn frames(
    read: &dyn Fn(u64, &mut [u8]) -> bool,
    word: u64,
    depth: i32,
    out: &mut Vec<u8>,
) -> Option<()> {
    use php_sapi::readable::{Callee, Room, Standing};
    let room = |at: u64| {
        let mut bytes = [0u8; Room::BYTES];
        read(at, &mut bytes).then(|| Room::read(&bytes)).flatten()
    };
    let callee = |at: u64| {
        let mut bytes = [0u8; Callee::BYTES];
        read(at, &mut bytes).then(|| Callee::read(&bytes)).flatten()
    };
    let strz = |at: u64, len: u64| {
        let mut bytes = vec![0u8; usize::try_from(len.min(STRZ)).ok()?];
        read(at, &mut bytes).then_some(())?;
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        bytes.truncate(end);
        Some(bytes)
    };
    if word == 0 {
        return Some(());
    }
    let mut at = {
        let mut bytes = [0u8; 8];
        read(word, &mut bytes).then_some(())?;
        u64::from_ne_bytes(bytes)
    };
    let mut limit = depth;
    while at != 0 {
        let frame = room(at)?;
        if frame.standing == Standing::Passed {
            at = frame.below;
            continue;
        }
        let name = if valid(frame.callee) {
            let served = callee(frame.callee)?;
            if served.name.len == 0 {
                if frame.standing == Standing::Top {
                    return Some(());
                }
                b"[INCLUDE_OR_EVAL]".to_vec()
            } else {
                strz(served.name.at, served.name.len)?
            }
        } else {
            b"???".to_vec()
        };
        out.extend_from_slice(format!("[0x{at:016x}] ").as_bytes());
        out.extend_from_slice(&field(&name));
        out.extend_from_slice(b"()");
        let (mut file, mut lineno) = (Vec::new(), 0u32);
        let mut below = frame.below;
        while below != 0 {
            let down = room(below)?;
            if down.standing == Standing::Passed {
                below = down.below;
                continue;
            }
            if !valid(down.callee) {
                break;
            }
            if let Some(code) = callee(down.callee)?.code {
                file = strz(code.file.at, code.file.len)?;
                if let Some(line) = code.lineno_at(down.opline) {
                    let mut bytes = [0u8; 4];
                    read(line, &mut bytes).then_some(())?;
                    lineno = u32::from_ne_bytes(bytes);
                }
                break;
            }
            below = down.below;
        }
        out.push(b' ');
        let file = if file.is_empty() {
            std::borrow::Cow::Borrowed(&b"unknown"[..])
        } else {
            field(&file)
        };
        out.extend_from_slice(&file);
        out.extend_from_slice(format!(":{lineno}\n").as_bytes());
        at = frame.below;
        limit -= 1;
        if limit == 0 {
            break;
        }
    }
    Some(())
}
