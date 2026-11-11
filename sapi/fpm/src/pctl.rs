//! `fpm_process_ctl.c`: the master's state and what it does to its
//! children to reach it — `SIGQUIT`, then `SIGTERM`, then `SIGKILL`, a
//! step per `process_control_timeout` — and, once none is left, the exit
//! or the exec a reload is; the idle servers kept between the pool's
//! bounds each second; the request timeouts checked each heartbeat; and
//! an ondemand pool's child made when a connection waits.

use std::time::Instant;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::conf::Pm;
use crate::master::{Fpm, State, Step, Timer};
use crate::scoreboard::{Action as Board, Stage, Update};
use crate::{zlog, zlog_sys};

/// `FPM_IDLE_SERVER_MAINTENANCE_HEARTBEAT`, in milliseconds.
pub(crate) const MAINTENANCE_HEARTBEAT: u64 = 1000;
/// `FPM_PCTL_MIN_HEARTBEAT`, in milliseconds.
pub(crate) const MIN_HEARTBEAT: i32 = 130;

/// `FPM_PCTL_ACTION_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Set,
    Timeout,
    LastChildExited,
}

/// `fpm_signal_names`.
pub(crate) fn signal_name(signo: i32) -> &'static str {
    Signal::try_from(signo).map_or("", |signal| signal.as_str())
}

impl Fpm {
    /// `fpm_pctl`.
    pub(crate) fn pctl(&mut self, new_state: State, action: Action) -> Step {
        match action {
            Action::Set => {
                if self.state == new_state {
                    return Ok(());
                }
                let overridable = match self.state {
                    State::Normal => true,
                    State::Reloading => matches!(new_state, State::Finishing | State::Terminating),
                    State::Finishing => new_state == State::Terminating,
                    State::Terminating => false,
                };
                if !overridable {
                    zlog!(
                        Debug @ fpm_pctl:231,
                        bytes "not switching to '{}' state, because already in '{}' state",
                        new_state.name(),
                        self.state.name()
                    );
                    return Ok(());
                }
                self.signal_sent = None;
                self.state = new_state;
                zlog!(Debug @ fpm_pctl:239, bytes "switching to '{}' state", self.state.name());
                self.action_next()
            }
            Action::Timeout => self.action_next(),
            Action::LastChildExited => self.action_last(),
        }
    }

    /// `fpm_pctl_action_next`: the next signal to every child, and the
    /// time it has.
    fn action_next(&mut self) -> Step {
        if self.running_children() == 0 {
            self.action_last()?;
        }
        let (signal, timeout) = match self.signal_sent {
            None => (
                if self.state == State::Terminating {
                    Signal::SIGTERM
                } else {
                    Signal::SIGQUIT
                },
                self.global.process_control_timeout,
            ),
            Some(Signal::SIGQUIT) => (Signal::SIGTERM, 1),
            Some(_) => (Signal::SIGKILL, 1),
        };
        self.kill_all(signal);
        self.signal_sent = Some(signal);
        self.add_timer(
            Timer::Pctl,
            u64::try_from(timeout).unwrap_or(0) * 1000,
            false,
        );
        Ok(())
    }

    /// `fpm_pctl_action_last`: the exec of a reload, or the exit.
    fn action_last(&mut self) -> Step {
        match self.state {
            State::Reloading => self.exec(),
            State::Finishing | State::Terminating => self.exit(),
            State::Normal => Ok(()),
        }
    }

    /// `fpm_pctl_exit`.
    fn exit(&mut self) -> Step {
        zlog!(Notice @ fpm_pctl_exit:69, "exiting, bye-bye!");
        self.unlink_pid();
        self.sockets.unlink_all();
        // `fpm_php_cleanup`, the last of the cleanups to run, as the
        // first registered: the engine ended.
        drop(self.engine.take());
        php_sapi::exit(0);
    }

    /// `fpm_pctl_exec`: the master's own command line exec'd, its
    /// listening sockets handed on through `FPM_SOCKETS`, the signals
    /// blocked until the new master handles them.
    fn exec(&mut self) -> Step {
        zlog!(Debug @ fpm_pctl_exec:80, "Blocking some signals before reexec");
        if nix::sys::signal::sigprocmask(
            nix::sys::signal::SigmaskHow::SIG_BLOCK,
            Some(&crate::master::block_set(false)),
            None,
        )
        .is_err()
        {
            zlog!(Warning @ fpm_pctl_exec:82, "concurrent reloads may be unstable");
        }
        let first = self.argv.first().map(Vec::as_slice).unwrap_or_default();
        let rest: Vec<u8> = (1..=10)
            .filter_map(|i| self.argv.get(i))
            .flat_map(|a| crate::zformat!(", \"{}\"", a))
            .collect();
        zlog!(
            Notice @ fpm_pctl_exec:100,
            bytes "reloading: execvp(\"{}\", {\"{}\"{}})",
            first,
            first,
            rest
        );
        for (name, value) in self.sockets.for_exec() {
            if value.is_empty() {
                crate::ffi::unset_env(name.as_bytes());
            } else {
                crate::ffi::set_env(name.as_bytes(), &value);
            }
        }
        // `fpm_cleanups_run(FPM_CLEANUP_PARENT_EXEC)`: the engine ended
        // before the exec, as `fpm_php_cleanup` ends it.
        drop(self.engine.take());
        crate::stdio::restore_original_stderr(self, true);
        crate::ffi::restore_defaults().ok();
        let argv: Vec<std::ffi::CString> = self
            .argv
            .iter()
            .filter_map(|a| std::ffi::CString::new(a.clone()).ok())
            .collect();
        if let Some(program) = argv.first()
            && let Err(e) = nix::unistd::execvp(program, &argv)
        {
            zlog_sys!(Error @ fpm_pctl_exec:107, e, "failed to reload: execvp() failed");
        }
        php_sapi::exit(i32::from(crate::master::EXIT_SOFTWARE));
    }

    /// `fpm_pctl_kill_all`: `signal` to every child.
    pub(crate) fn kill_all(&self, signal: Signal) {
        let mut alive = 0;
        for pool in &self.pools {
            for child in &pool.children {
                let sent = kill(Pid::from_raw(child.pid), signal);
                zlog!(
                    Debug @ fpm_pctl_kill_all:165,
                    bytes "[pool {}] sending signal {} {} to child {}",
                    pool.name(),
                    signal as i32,
                    signal.as_str(),
                    child.pid
                );
                if sent.is_ok() {
                    alive += 1;
                }
            }
        }
        if alive > 0 {
            zlog!(Debug @ fpm_pctl_kill_all:174, "{alive} child(ren) still alive");
        }
    }

    /// `fpm_pctl_child_exited`.
    pub(crate) fn child_exited(&mut self) -> Step {
        if self.state == State::Normal {
            return Ok(());
        }
        if self.running_children() == 0 {
            self.pctl(State::Normal, Action::LastChildExited)?;
        }
        Ok(())
    }
}

/// `fpm_pctl_check_request_timeout`.
pub(crate) fn check_request_timeout(fpm: &mut Fpm) {
    let now = crate::master::clock_us();
    for p in 0..fpm.pools.len() {
        let config = &fpm.pools[p].config;
        let (terminate, slowlog, track) = (
            config.request_terminate_timeout,
            config.request_slowlog_timeout,
            config.request_terminate_timeout_track_finished,
        );
        if terminate == 0 && slowlog == 0 {
            continue;
        }
        for c in 0..fpm.pools[p].children.len() {
            crate::slowlog::check_timed_out(fpm, p, c, now, terminate, slowlog, track);
        }
    }
}

/// `fpm_pctl_kill_idle_child`: `SIGQUIT`, and `SIGKILL` for one that was
/// asked before.
fn kill_idle_child(fpm: &mut Fpm, pool: usize, child: usize) {
    let target = &mut fpm.pools[pool].children[child];
    let signal = if target.idle_kill {
        Signal::SIGKILL
    } else {
        Signal::SIGQUIT
    };
    target.idle_kill = true;
    kill(Pid::from_raw(target.pid), signal).ok();
}

/// `fpm_pctl_perform_idle_server_maintenance`: each pool's idle and
/// active children counted into its `scoreboard`; an ondemand pool's
/// oldest idle child ended once idle past `pm.process_idle_timeout`; a
/// dynamic pool's spare children kept between `pm.min_spare_servers` and
/// `pm.max_spare_servers`, spawning twice as many each second it falls
/// short, up to `pm.max_spawn_rate`.
#[allow(clippy::too_many_lines)] // `fpm_pctl_perform_idle_server_maintenance`, kept as one
pub(crate) fn perform_idle_server_maintenance(fpm: &mut Fpm) -> Step {
    for p in 0..fpm.pools.len() {
        let (mut idle, mut active) = (0, 0);
        let mut last_idle: Option<(usize, Instant)> = None;
        let Some(scoreboard) = fpm.pools[p].scoreboard else {
            continue;
        };
        for (c, child) in fpm.pools[p].children.iter().enumerate() {
            if is_idle(scoreboard, child.scoreboard_i) {
                if last_idle.is_none_or(|(_, started)| child.started < started) {
                    last_idle = Some((c, child.started));
                }
                idle += 1;
            } else {
                active += 1;
            }
        }
        scoreboard.update(
            Update {
                idle,
                active,
                lq: 0,
                lq_len: -1,
                requests: -1,
                max_children_reached: -1,
                slow_rq: -1,
                memory_peak: 0,
            },
            Board::Set,
        );
        let pool = &fpm.pools[p];
        let config = pool.config.clone();
        if pool.pm() == Pm::Ondemand {
            zlog!(
                Debug @ fpm_pctl_perform_idle_server_maintenance:382,
                bytes "[pool {}] currently {} active children, {} spare children",
                pool.name(),
                active,
                idle
            );
            let Some((c, _)) = last_idle else {
                continue;
            };
            let last = scoreboard
                .proc(pool.children[c].scoreboard_i)
                .map_or(0, super::scoreboard::Proc::last_activity_us);
            let now = crate::master::clock_us();
            if last / 1_000_000 < now / 1_000_000 - i64::from(config.pm_process_idle_timeout) {
                kill_idle_child(fpm, p, c);
            }
            continue;
        }
        if pool.pm() != Pm::Dynamic {
            continue;
        }
        zlog!(
            Debug @ fpm_pctl_perform_idle_server_maintenance:398,
            bytes "[pool {}] currently {} active children, {} spare children, {} running children. Spawning rate {}",
            pool.name(),
            active,
            idle,
            pool.running(),
            pool.idle_spawn_rate
        );
        if idle > config.pm_max_spare_servers
            && let Some((c, _)) = last_idle
        {
            kill_idle_child(fpm, p, c);
            fpm.pools[p].idle_spawn_rate = 1;
            continue;
        }
        if idle < config.pm_min_spare_servers {
            let running = fpm.pools[p].running();
            if running >= config.pm_max_children {
                reached_max_children(fpm, p, "pm.max_children", 410);
                fpm.pools[p].idle_spawn_rate = 1;
                continue;
            }
            let rate = fpm.pools[p].idle_spawn_rate;
            if rate >= 8 {
                zlog!(
                    Warning @ fpm_pctl_perform_idle_server_maintenance:418,
                    bytes "[pool {}] seems busy (you may need to increase pm.start_servers, or pm.min/max_spare_servers), spawning {} children, there are {} idle, and {} total children",
                    fpm.pools[p].name(),
                    rate,
                    idle,
                    running
                );
            }
            let to_fork = rate
                .min(config.pm_min_spare_servers - idle)
                .min(config.pm_max_children - running);
            if to_fork <= 0 {
                reached_max_children(fpm, p, "pm.max_children", 429);
                fpm.pools[p].idle_spawn_rate = 1;
                continue;
            }
            fpm.pools[p].warn_max_children = false;
            crate::children::make(fpm, p, true, to_fork, true)?;
            zlog!(
                Debug @ fpm_pctl_perform_idle_server_maintenance:446,
                bytes "[pool {}] {} child(ren) have been created dynamically",
                fpm.pools[p].name(),
                to_fork
            );
            if fpm.pools[p].idle_spawn_rate < config.pm_max_spawn_rate {
                fpm.pools[p].idle_spawn_rate *= 2;
            }
            continue;
        }
        fpm.pools[p].idle_spawn_rate = 1;
    }
    Ok(())
}

/// The warning each time a pool first reaches its children's ceiling,
/// counted into its `scoreboard`, from the call at `line`:
/// `fpm_pctl_perform_idle_server_maintenance`'s two or
/// `fpm_pctl_on_socket_accept`'s.
fn reached_max_children(fpm: &mut Fpm, pool: usize, setting: &str, line: u32) {
    let p = &mut fpm.pools[pool];
    if !p.warn_max_children && p.shared.is_none() {
        if let Some(scoreboard) = p.scoreboard {
            scoreboard.update(
                Update {
                    max_children_reached: 1,
                    ..Update::ZERO
                },
                Board::Inc,
            );
        }
        let function = if line == 533 {
            "fpm_pctl_on_socket_accept"
        } else {
            "fpm_pctl_perform_idle_server_maintenance"
        };
        zlog!(
            Warning @ (crate::zlog::Site::at(function, line)),
            bytes "[pool {}] server reached {} setting ({}), consider raising it",
            p.name(),
            setting,
            p.config.pm_max_children
        );
        p.warn_max_children = true;
    }
}

/// `fpm_request_is_idle`.
pub(crate) fn is_idle(scoreboard: crate::scoreboard::Scoreboard, slot: usize) -> bool {
    scoreboard
        .proc(slot)
        .is_some_and(|proc| proc.stage() == Stage::Accepting)
}

/// `fpm_pctl_on_socket_accept`: a connection waits on an ondemand pool,
/// and a child is made for it where none is idle and the pool is below
/// its ceiling.
pub(crate) fn on_socket_accept(fpm: &mut Fpm, pool: usize) -> Step {
    if fpm.pools[pool].running() >= fpm.pools[pool].config.pm_max_children {
        reached_max_children(fpm, pool, "max_children", 533);
        return Ok(());
    }
    let Some(scoreboard) = fpm.pools[pool].scoreboard else {
        return Ok(());
    };
    if fpm.pools[pool]
        .children
        .iter()
        .any(|c| is_idle(scoreboard, c.scoreboard_i))
    {
        return Ok(());
    }
    fpm.pools[pool].warn_max_children = false;
    crate::children::make(fpm, pool, true, 1, true)?;
    zlog!(
        Debug @ fpm_pctl_on_socket_accept:553,
        bytes "[pool {}] got accept without idle child available .... I forked",
        fpm.pools[pool].name()
    );
    Ok(())
}
