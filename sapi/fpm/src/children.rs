//! `fpm_children.c`: children made for a pool up to what its process
//! manager asks, each forked with its pipes and its scoreboard slot, and
//! buried when it exits: its exit logged, its slot freed, what it still
//! had to say read, and another made in its place unless the process
//! manager ended it.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::Instant;

use nix::sys::signal::{SigmaskHow, Signal, sigprocmask};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use php_sapi::Forked;

use crate::conf::Pm;
use crate::master::{Became, Child, Fpm, State, Step};
use crate::pctl::signal_name;
use crate::zlog::Level;
use crate::{zlog, zlog_sys};

/// What `fpm_children_make` returned in the master.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Made {
    /// 1: done.
    Done,
    /// 2: a child could not be made.
    Failed,
}

/// `fpm_stdio_prepare_pipes`: a pipe for the child's stdout and one for
/// its stderr, their read ends non-blocking, where the pool catches its
/// workers' output.
fn prepare_pipes(catch: bool) -> Result<Option<[(OwnedFd, OwnedFd); 2]>, ()> {
    if !catch {
        return Ok(None);
    }
    let pipe = |what: &str, line: u32| {
        nix::unistd::pipe().map_err(|e| {
            zlog_sys!(
                Error @ (crate::zlog::Site::at("fpm_stdio_prepare_pipes", line)),
                e,
                "failed to prepare the {what} pipe"
            );
        })
    };
    let out = pipe("stdout", 295)?;
    let err = pipe("stderr", 300)?;
    for read in [&out.0, &err.0] {
        let nonblocking = nix::fcntl::fcntl(
            read.as_fd(),
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        );
        if nonblocking.is_err() {
            zlog_sys!(Error @ fpm_stdio_prepare_pipes:307, nix::errno::Errno::last(), "failed to unblock pipes");
            return Err(());
        }
    }
    Ok(Some([out, err]))
}

/// `fpm_children_make`: children made for the pool at `pool` while the
/// master may, the pool is below what its process manager asks, and the
/// master below `process.max`.
#[allow(clippy::too_many_lines)] // `fpm_children_make`, php's one function, kept as one
pub(crate) fn make(
    fpm: &mut Fpm,
    pool: usize,
    in_event_loop: bool,
    nb_to_spawn: i32,
    is_debug: bool,
) -> Step<Made> {
    let config = &fpm.pools[pool].config;
    let running = fpm.pools[pool].running();
    let max = match fpm.pools[pool].pm() {
        Pm::Dynamic if !in_event_loop => config.pm_start_servers,
        Pm::Ondemand if !in_event_loop => 0,
        Pm::Dynamic | Pm::Ondemand => running + nb_to_spawn,
        Pm::Static => config.pm_max_children,
    };
    let process_max = fpm.global.process_max;
    while fpm.state == State::Normal
        && fpm.pools[pool].running() < max
        && (process_max < 1 || fpm.running_children() < process_max)
    {
        fpm.warned = false;
        let Ok(pipes) = prepare_pipes(fpm.pools[pool].config.catch_workers_output) else {
            return Ok(Made::Failed);
        };
        let Some(scoreboard) = fpm.pools[pool].scoreboard else {
            return Ok(Made::Failed);
        };
        let Some(slot) = scoreboard.alloc() else {
            zlog!(
                Error @ fpm_scoreboard_proc_alloc:493,
                "[pool {}] no free scoreboard slot",
                String::from_utf8_lossy(&scoreboard.pool())
            );
            return Ok(Made::Failed);
        };
        zlog!(Debug @ fpm_children_make:449, "blocking signals before child birth");
        if sigprocmask(
            SigmaskHow::SIG_BLOCK,
            Some(&crate::master::block_set(true)),
            None,
        )
        .is_err()
        {
            zlog!(Warning @ fpm_children_make:451, "child may miss signals");
        }
        match php_sapi::fork() {
            Ok(Forked::Child) => {
                fpm.leave_events();
                crate::zlog::set_child();
                scoreboard.child_use(slot, nix::unistd::getpid().as_raw());
                crate::stdio::child_use_pipes(pipes);
                return Err(Became::Child(pool, slot));
            }
            Err(e) => {
                zlog!(Debug @ fpm_children_make:465, "unblocking signals");
                crate::master::unblock_all();
                zlog_sys!(Error @ fpm_children_make:467, crate::conf::errno(&e), "fork() failed");
                scoreboard.free(slot);
                return Ok(Made::Failed);
            }
            Ok(Forked::Parent { child }) => {
                zlog!(Debug @ fpm_children_make:473, "unblocking signals, child born");
                crate::master::unblock_all();
                let (stdout, stderr) = match pipes {
                    Some([(out_read, _), (err_read, _)]) => (Some(out_read), Some(err_read)),
                    None => (None, None),
                };
                let (out_fd, err_fd) = (
                    stdout.as_ref().map(AsRawFd::as_raw_fd),
                    stderr.as_ref().map(AsRawFd::as_raw_fd),
                );
                fpm.pools[pool].children.insert(
                    0,
                    Child {
                        pid: child.as_raw(),
                        started: Instant::now(),
                        scoreboard_i: slot,
                        stdout,
                        stderr,
                        log_stream: None,
                        idle_kill: false,
                        slow_logged: None,
                        traced: false,
                    },
                );
                fpm.watch_output(child.as_raw(), out_fd, err_fd);
                let level = if is_debug {
                    Level::Debug
                } else {
                    Level::Notice
                };
                let line = format!(
                    "[pool {}] child {} started",
                    fpm.pools[pool].name(),
                    child.as_raw()
                );
                crate::zlog::log(
                    level,
                    crate::zlog::Site::at("fpm_children_make", 479),
                    line.as_bytes(),
                    None,
                );
            }
        }
    }
    if !fpm.warned
        && process_max > 0
        && fpm.running_children() >= process_max
        && fpm.pools[pool].running() < max
    {
        fpm.warned = true;
        zlog!(
            Warning @ fpm_children_make:487,
            "The maximum number of processes has been reached. Please review your configuration and consider raising 'process.max'"
        );
    }
    Ok(Made::Done)
}

/// `fpm_children_create_initial`: an ondemand pool's socket watched, any
/// other pool's first children made.
pub(crate) fn create_initial(fpm: &mut Fpm, pool: usize) -> Step<Made> {
    if fpm.pools[pool].pm() == Pm::Ondemand {
        fpm.watch_ondemand(pool);
        return Ok(Made::Done);
    }
    make(fpm, pool, false, 0, true)
}

/// `fpm_children_bury`: every child that ended, reaped and logged.
#[allow(clippy::too_many_lines)] // `fpm_children_bury`, kept as one
pub(crate) fn bury(fpm: &mut Fpm) -> Step {
    loop {
        let status = match waitpid(
            Pid::from_raw(-1),
            Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED),
        ) {
            Ok(WaitStatus::StillAlive) | Err(_) => return Ok(()),
            Ok(status) => status,
        };
        let Some(pid) = status.pid().map(Pid::as_raw) else {
            return Ok(());
        };
        let found = fpm.child(pid);
        let mut severity = Level::Notice;
        let mut restart = true;
        let mut signal = None;
        let how = match status {
            WaitStatus::Exited(_, code) => {
                if found.is_some_and(|(p, c)| fpm.pools[p].children[c].idle_kill) {
                    restart = false;
                }
                if code != 0 {
                    severity = Level::Warning;
                }
                format!("with code {code}")
            }
            WaitStatus::Signaled(_, sig, core) => {
                signal = Some(sig);
                if found.is_some_and(|(p, c)| fpm.pools[p].children[c].idle_kill)
                    && sig == Signal::SIGQUIT
                {
                    restart = false;
                }
                if sig != Signal::SIGQUIT {
                    severity = Level::Warning;
                }
                format!(
                    "on signal {} ({}{})",
                    sig as i32,
                    signal_name(sig as i32),
                    if core { " - core dumped" } else { "" }
                )
            }
            WaitStatus::Stopped(..) => {
                zlog!(Notice @ fpm_children_bury:275, "child {pid} stopped for tracing");
                if let Some((p, c)) = found
                    && fpm.pools[p].children[c].traced
                {
                    crate::slowlog::trace(fpm, p, c);
                }
                continue;
            }
            _ => continue,
        };
        let Some((p, c)) = found else {
            if fpm.parent_pid == 1 {
                zlog!(
                    Debug @ fpm_children_bury:343,
                    "unknown child ({pid}) exited {how} - most likely an orphan process (master process is the init process)"
                );
            } else {
                zlog!(
                    Warning @ fpm_children_bury:345,
                    "unknown child ({pid}) exited {how} - potentially a bug or pre exec child (e.g. s6-notifyoncheck)"
                );
            }
            continue;
        };
        let mut child = fpm.pools[p].children.remove(c);
        if let Some(scoreboard) = fpm.pools[p].scoreboard {
            scoreboard.free(child.scoreboard_i);
        }
        let lived = child.started.elapsed();
        let name = fpm.pools[p].name();
        if restart {
            if fpm.state != State::Normal {
                severity = Level::Debug;
            }
            let line = format!(
                "[pool {name}] child {pid} exited {how} after {}.{:06} seconds from start",
                lived.as_secs(),
                lived.subsec_micros()
            );
            crate::zlog::log(
                severity,
                crate::zlog::Site::at("fpm_children_bury", 300),
                line.as_bytes(),
                None,
            );
        } else {
            zlog!(
                Debug @ fpm_children_bury:302,
                "[pool {name}] child {pid} has been killed by the process management after {}.{:06} seconds from start",
                lived.as_secs(),
                lived.subsec_micros()
            );
        }
        close(fpm, p, &mut child);
        fpm.child_exited()?;
        if matches!(signal, Some(Signal::SIGSEGV | Signal::SIGBUS))
            && fpm.fault(crate::scoreboard::epoch())
        {
            zlog!(
                Warning @ fpm_children_bury:329,
                "failed processes threshold ({} in {} sec) is reached, initiating reload",
                fpm.global.emergency_restart_threshold,
                fpm.global.emergency_restart_interval
            );
            fpm.pctl(State::Reloading, crate::pctl::Action::Set)?;
        }
        if restart {
            make(fpm, p, true, 1, false)?;
        }
    }
}

/// `fpm_child_close` in the loop for the child that ran in the pool at
/// `pool`: each pipe read to its end as its event would be
/// (`fpm_event_fire`), then out of the loop and closed, as the pipe that
/// ended is; one still open, where something the child started holds its
/// writing end, out of the loop before it closes too.
fn close(fpm: &mut Fpm, pool: usize, child: &mut Child) {
    for stdout in [true, false] {
        let Some(fd) = crate::stdio::pipe(child, stdout).take() else {
            continue;
        };
        let mut stream = child.log_stream.take();
        let open = crate::stdio::pipe_said(fpm, pool, child.pid, &mut stream, fd, stdout);
        child.log_stream = stream;
        if let Some(fd) = open {
            fpm.unwatch(fd.as_fd());
        }
    }
    if let Some(stream) = &mut child.log_stream {
        stream.finish();
    }
}
