//! The master: `fpm.c`'s `fpm_init` and `fpm_run`, `fpm_events.c`'s loop,
//! `fpm_signals.c`'s pipe, and the state the rest of the master reads,
//! php's `fpm_globals`, `fpm_global_config` and `fpm_worker_all_pools`
//! gathered in one [`Fpm`]. A child is made inside the loop, as php's
//! fork returns in the middle of it; here the child leaves every call
//! that got it there as [`Became::Child`], back to `main`, which then
//! serves requests ([`crate::worker`]).

use std::io::Read as _;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::time::Instant;

use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};

use crate::conf::{self, Global, Pm, PoolConfig};
use crate::events::Poller;
use crate::scoreboard::Scoreboard;
use crate::sockets::{Domain, Sockets};
use crate::unix::Credentials;
use crate::zlog::Stream;
use crate::{zlog, zlog_sys};

/// `FPM_EXIT_OK`, `_USAGE`, `_SOFTWARE`, `_CONFIG`: `<sysexits.h>`.
pub(crate) const EXIT_OK: u8 = 0;
pub(crate) const EXIT_USAGE: u8 = 64;
pub(crate) const EXIT_SOFTWARE: u8 = 70;
pub(crate) const EXIT_CONFIG: u8 = 78;

/// What a call of the master's became once it forked: this process is
/// now the child of the pool at the index.
#[derive(Debug)]
pub(crate) enum Became {
    /// The pool's index, and the child's scoreboard slot.
    Child(usize, usize),
}

/// A step of the master that may have forked.
pub(crate) type Step<T = ()> = Result<T, Became>;

/// `fpm_state`, the process manager's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Normal,
    Reloading,
    Terminating,
    Finishing,
}

impl State {
    /// `fpm_state_names`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            State::Normal => "normal",
            State::Reloading => "reloading",
            State::Terminating => "terminating",
            State::Finishing => "finishing",
        }
    }
}

/// One child the master made, `struct fpm_child_s`.
pub(crate) struct Child {
    pub(crate) pid: i32,
    pub(crate) started: Instant,
    pub(crate) scoreboard_i: usize,
    pub(crate) stdout: Option<OwnedFd>,
    pub(crate) stderr: Option<OwnedFd>,
    pub(crate) log_stream: Option<Stream>,
    pub(crate) idle_kill: bool,
    /// `slow_logged`: when the request logged slow was accepted.
    pub(crate) slow_logged: Option<i64>,
    /// `tracer`: the child was stopped for its slow request's trace.
    pub(crate) traced: bool,
}

/// One pool, `struct fpm_worker_pool_s`.
pub(crate) struct Pool {
    pub(crate) config: PoolConfig,
    /// The pool whose scoreboard this one's status page shows.
    pub(crate) shared: Option<usize>,
    pub(crate) domain: Domain,
    pub(crate) listening: Option<RawFd>,
    pub(crate) credentials: Credentials,
    /// Newest first, as `fpm_child_link` links them.
    pub(crate) children: Vec<Child>,
    pub(crate) idle_spawn_rate: i32,
    pub(crate) warn_max_children: bool,
    pub(crate) scoreboard: Option<Scoreboard>,
    pub(crate) access_log: Option<OwnedFd>,
    pub(crate) limit_extensions: Vec<Vec<u8>>,
}

impl Pool {
    pub(crate) fn name(&self) -> String {
        String::from_utf8_lossy(&self.config.name).into_owned()
    }

    pub(crate) fn pm(&self) -> Pm {
        self.config.pm.unwrap_or(Pm::Static)
    }

    pub(crate) fn running(&self) -> i32 {
        i32::try_from(self.children.len()).unwrap_or(i32::MAX)
    }
}

/// A timer of the loop, what `fpm_event_set_timer` sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Timer {
    /// `fpm_pctl_heartbeat`: the request timeouts checked.
    Heartbeat,
    /// `fpm_pctl_perform_idle_server_maintenance_heartbeat`.
    Maintenance,
    /// `pctl_event`: the process manager's next step.
    Pctl,
    /// `children_bury_timer`.
    Bury,
}

/// A queued timer: when it is due, and every how long it repeats.
struct Queued {
    timer: Timer,
    due: Instant,
    every: Option<std::time::Duration>,
}

/// What a ready descriptor of the loop is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watched {
    Signals,
    Output { pid: i32, pipe: crate::stdio::Pipe },
    Ondemand(usize),
}

/// php-fpm's state: `fpm_globals`, `fpm_global_config`, every pool, the
/// loop's queues, and what the command line gave.
#[allow(clippy::struct_excessive_bools)] // `fpm_globals`' flags, one field each
pub(crate) struct Fpm {
    pub(crate) global: Global,
    pub(crate) pools: Vec<Pool>,
    pub(crate) sockets: Sockets,
    /// `saved_argv`, what a reload execs.
    pub(crate) argv: Vec<Vec<u8>>,
    pub(crate) config_path: Vec<u8>,
    pub(crate) prefix: Option<Vec<u8>>,
    pub(crate) pid_path: Option<Vec<u8>>,
    pub(crate) log_level: i32,
    /// `fpm_globals.heartbeat`, in milliseconds.
    pub(crate) heartbeat: i32,
    pub(crate) run_as_root: bool,
    pub(crate) force_stderr: bool,
    pub(crate) parent_pid: i32,
    pub(crate) test_successful: bool,
    /// The calling process of a daemonizing start, once the master it
    /// forked acknowledged its start: it ends with `FPM_EXIT_OK` after
    /// `fpm_cleanups_run(FPM_CLEANUP_PARENT_EXIT)`, which ends its engine
    /// (`Z/sapi/fpm/fpm/fpm_unix.c:661-664`).
    acknowledged: bool,
    /// The master's engine, from the end of `fpm_init` until the master
    /// exits or execs a reload, each of which ends it first, as
    /// `fpm_php_cleanup` runs `php_module_shutdown` there
    /// (`Z/sapi/fpm/fpm/fpm_php.c:187-194`).
    pub(crate) engine: Option<php_sapi::Engine>,
    pub(crate) state: State,
    pub(crate) signal_sent: Option<Signal>,
    pub(crate) stderr_original: Option<OwnedFd>,
    pub(crate) send_config_pipe: Option<OwnedFd>,
    /// `fpm_children_make`'s `warned`.
    pub(crate) warned: bool,
    last_faults: Vec<i64>,
    fault: usize,
    poller: Option<Poller>,
    watched: Vec<(RawFd, Watched)>,
    timers: Vec<Queued>,
    signal_read: Option<OwnedFd>,
    signal_write: Option<OwnedFd>,
}

/// What `fpm_init` is handed from the command line.
pub(crate) struct Args {
    pub(crate) argv: Vec<Vec<u8>>,
    pub(crate) config: Option<Vec<u8>>,
    pub(crate) prefix: Option<Vec<u8>>,
    pub(crate) pid: Option<Vec<u8>>,
    pub(crate) test: u32,
    pub(crate) run_as_root: bool,
    /// `-D` or `-F`, over `daemonize`.
    pub(crate) daemonize: Option<bool>,
    pub(crate) force_stderr: bool,
}

/// What `fpm_init` came to.
pub(crate) enum Init {
    /// `FPM_INIT_CONTINUE`: the master runs.
    Continue(Box<Fpm>),
    /// `FPM_INIT_EXIT_OK`: `-t` found the configuration good, or this is
    /// the calling process of a daemonizing start and the master
    /// acknowledged its start.
    ExitOk,
    /// `FPM_INIT_ERROR`, with the pipe a daemonizing parent waits on.
    Error(Option<OwnedFd>),
}

/// `fpm_clock_get`: the monotonic time, in microseconds.
pub(crate) fn clock_us() -> i64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map_or(0, |t| t.tv_sec() * 1_000_000 + t.tv_nsec() / 1000)
}

/// The signals `fpm_signals_init_mask` blocks through early init and
/// around a fork or an exec; a child's set adds `SIGTERM` and `SIGQUIT`.
pub(crate) fn block_set(child: bool) -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGUSR1);
    set.add(Signal::SIGUSR2);
    set.add(Signal::SIGCHLD);
    if child {
        set.add(Signal::SIGTERM);
        set.add(Signal::SIGQUIT);
    }
    set
}

/// `fpm_signals_unblock`: every signal unblocked.
pub(crate) fn unblock_all() {
    sigprocmask(SigmaskHow::SIG_UNBLOCK, Some(&SigSet::all()), None).ok();
}

impl Fpm {
    /// `fpm_init`: each part's `init_main` in php's order, and then the
    /// pid file, stderr tied to the error log and `fpm is running`.
    pub(crate) fn init(engine: &php_sapi::Engine, args: Args) -> Init {
        let mut fpm = Fpm {
            global: Global::default(),
            pools: Vec::new(),
            sockets: Sockets::default(),
            argv: args.argv,
            config_path: Vec::new(),
            prefix: args.prefix,
            pid_path: args.pid,
            log_level: 0,
            heartbeat: 0,
            run_as_root: args.run_as_root,
            force_stderr: args.force_stderr,
            parent_pid: 0,
            test_successful: false,
            acknowledged: false,
            engine: None,
            state: State::Normal,
            signal_sent: None,
            stderr_original: None,
            send_config_pipe: None,
            warned: false,
            last_faults: Vec::new(),
            fault: 0,
            poller: None,
            watched: Vec::new(),
            timers: Vec::new(),
            signal_read: None,
            signal_write: None,
        };
        let config = args
            .config
            .filter(|c| !c.is_empty())
            .or_else(|| engine.ini_get(b"fpm.config").filter(|c| !c.is_empty()));
        let ok = crate::stdio::init_main()
            && fpm.conf_init_main(engine, config, args.test, args.daemonize)
            && fpm.unix_init_main()
            && fpm.scoreboard_init_main()
            && crate::env::init_main(&mut fpm)
            && fpm.signals_init_main()
            && fpm.children_init_main()
            && fpm.sockets_init_main()
            && fpm.event_init_main();
        if !ok {
            if fpm.test_successful || fpm.acknowledged {
                return Init::ExitOk;
            }
            zlog!(Error @ fpm_init:72, "FPM initialization failed");
            return Init::Error(fpm.send_config_pipe.take());
        }
        if !fpm.write_pid() {
            zlog!(Error @ fpm_init:78, "FPM initialization failed");
            return Init::Error(fpm.send_config_pipe.take());
        }
        crate::stdio::init_final(&fpm);
        zlog!(Notice @ fpm_init:83, "fpm is running, pid {}", fpm.parent_pid);
        Init::Continue(Box::new(fpm))
    }

    /// `fpm_conf_init_main`.
    fn conf_init_main(
        &mut self,
        engine: &php_sapi::Engine,
        config: Option<Vec<u8>>,
        test: u32,
        daemonize: Option<bool>,
    ) -> bool {
        if let Some(prefix) = self.prefix.as_deref().filter(|p| !p.is_empty())
            && !conf::is_dir(prefix)
        {
            zlog!(
                Error @ fpm_conf_init_main:1824,
                "the global prefix '{}' does not exist or is not a directory",
                String::from_utf8_lossy(prefix)
            );
            return false;
        }
        self.config_path = config.unwrap_or_else(|| conf::default_config(self.prefix.as_deref()));
        let Some(parsed) = conf::read(engine, &self.config_path, self.prefix.as_deref()) else {
            zlog!(
                Error @ fpm_conf_init_main:1859,
                "failed to load configuration file '{}'",
                String::from_utf8_lossy(&self.config_path)
            );
            return false;
        };
        self.global = parsed.global;
        if let Some(pid) = self.pid_path.clone().filter(|p| !p.is_empty()) {
            self.global.pid_file = Some(pid);
        }
        self.pools = parsed
            .pools
            .into_iter()
            .zip(parsed.shared)
            .map(|(config, shared)| Pool {
                config,
                shared,
                domain: Domain::Inet,
                listening: None,
                credentials: Credentials::default(),
                children: Vec::new(),
                idle_spawn_rate: 1,
                warn_max_children: false,
                scoreboard: None,
                access_log: None,
                limit_extensions: Vec::new(),
            })
            .collect();
        if !self.post_process(daemonize) {
            zlog!(Error @ fpm_conf_init_main:1864, "failed to post process the configuration");
            return false;
        }
        if test > 0 {
            for pool in &self.pools {
                if !crate::unix::test_config(&pool.config, pool.domain) {
                    return false;
                }
            }
            if test > 1 {
                let pools: Vec<(&PoolConfig, bool)> = self
                    .pools
                    .iter()
                    .map(|p| (&p.config, p.shared.is_some()))
                    .collect();
                conf::dump(
                    &self.global,
                    self.log_level,
                    crate::events::MECHANISM,
                    &pools,
                );
            }
            zlog!(
                Notice @ fpm_conf_init_main:1878,
                "configuration file {} test is successful",
                String::from_utf8_lossy(&self.config_path)
            );
            self.test_successful = true;
            return false;
        }
        true
    }

    /// `fpm_conf_post_process`.
    fn post_process(&mut self, daemonize: Option<bool>) -> bool {
        let local = Some(php_sapi::PHP_LOCALSTATEDIR.as_bytes());
        if let Some(pid) = self.global.pid_file.clone() {
            self.global.pid_file =
                conf::evaluate_full_path(&pid, None, self.prefix.as_deref(), local, false);
        }
        if let Some(daemonize) = daemonize {
            self.global.daemonize = daemonize;
        }
        self.log_level = self.global.log_level;
        if let Some(level) = level_of(self.log_level) {
            crate::zlog::set_level(level);
        }
        if self.global.log_limit < i32::try_from(crate::zlog::MIN_LIMIT).unwrap_or(512) {
            zlog!(
                Error @ fpm_conf_post_process:1293,
                "log_limit must be greater than {}",
                crate::zlog::MIN_LIMIT
            );
            return false;
        }
        crate::zlog::set_limit(usize::try_from(self.global.log_limit).unwrap_or(1024));
        crate::zlog::set_buffering(self.global.log_buffering);
        if self.global.process_max < 0 {
            zlog!(Error @ fpm_conf_post_process:1300, "process_max can't be negative");
            return false;
        }
        let priority = self.global.process_priority;
        if priority != 64 && !(-19..=20).contains(&priority) {
            zlog!(Error @ fpm_conf_post_process:1305, "process.priority must be included into [-19,20]");
            return false;
        }
        if self.global.error_log.is_none() {
            self.global.error_log = Some(b"log/php-fpm.log".to_vec());
        }
        if self.global.syslog_ident.is_none() {
            self.global.syslog_ident = Some(b"php-fpm".to_vec());
        }
        if self.global.syslog_facility < 0 {
            self.global.syslog_facility = libc::LOG_DAEMON;
        }
        let error_log = self.global.error_log.clone().unwrap_or_default();
        if !error_log.eq_ignore_ascii_case(b"syslog") {
            self.global.error_log =
                conf::evaluate_full_path(&error_log, None, self.prefix.as_deref(), local, false);
        }
        if !self.global.daemonize && !crate::stdio::save_original_stderr(self) {
            return false;
        }
        if !crate::stdio::open_error_log(self, false) {
            return false;
        }
        if let Some(mechanism) = &self.global.events_mechanism
            && !mechanism.eq_ignore_ascii_case(crate::events::MECHANISM.as_bytes())
        {
            zlog!(
                Error @ fpm_event_pre_init:298,
                "event mechanism '{}' is not available on this system",
                String::from_utf8_lossy(mechanism)
            );
            return false;
        }
        if !crate::pools::process_all(self) {
            return false;
        }
        if crate::access::open(self, false) < 0 {
            return false;
        }
        for pool in &self.pools {
            let (Some(log), Some(format)) = (&pool.config.access_log, &pool.config.access_format)
            else {
                continue;
            };
            if log.is_empty() {
                continue;
            }
            if !crate::access::check(format) {
                zlog!(
                    Error @ fpm_conf_post_process:1359,
                    "[pool {}] wrong format for access.format '{}'",
                    pool.name(),
                    String::from_utf8_lossy(format)
                );
                return false;
            }
        }
        true
    }

    /// `fpm_unix_init_main`: the master's limits, the daemonizing fork
    /// and its session, its priority, and each pool's credentials.
    fn unix_init_main(&mut self) -> bool {
        use nix::sys::resource::Resource;
        if self.global.rlimit_files != 0
            && let Err(e) =
                crate::unix::set_rlimit(Resource::RLIMIT_NOFILE, self.global.rlimit_files)
        {
            zlog_sys!(
                Error @ fpm_unix_init_main:578,
                e,
                "failed to set rlimit_files for this pool. Please check your system limits or decrease rlimit_files. setrlimit(RLIMIT_NOFILE, {})",
                self.global.rlimit_files
            );
            return false;
        }
        if self.global.rlimit_core != 0
            && let Err(e) = crate::unix::set_rlimit(Resource::RLIMIT_CORE, self.global.rlimit_core)
        {
            zlog_sys!(
                Error @ fpm_unix_init_main:589,
                e,
                "failed to set rlimit_core for this pool. Please check your system limits or decrease rlimit_core. setrlimit(RLIMIT_CORE, {})",
                self.global.rlimit_core
            );
            return false;
        }
        if self.global.daemonize && !self.daemonize() {
            return false;
        }
        nix::unistd::setsid().ok();
        if self.global.process_priority != 64 {
            if nix::unistd::Uid::effective().is_root() {
                if !crate::ffi::set_priority(self.global.process_priority) {
                    zlog_sys!(
                        Error @ fpm_unix_init_main:687,
                        nix::errno::Errno::last(),
                        "Unable to set priority for the master process"
                    );
                    return false;
                }
            } else {
                zlog!(
                    Notice @ fpm_unix_init_main:691,
                    "'process.priority' directive is ignored when FPM is not running as root"
                );
            }
        }
        self.parent_pid = nix::unistd::getpid().as_raw();
        for i in 0..self.pools.len() {
            match crate::unix::conf_pool(&self.pools[i].config, self.run_as_root) {
                Some(credentials) => self.pools[i].credentials = credentials,
                None => return false,
            }
        }
        true
    }

    /// `fpm_unix_init_main`'s fork under `daemonize`: the calling process
    /// waits up to ten seconds for the master to say through a pipe how
    /// its start went, and exits with that.
    fn daemonize(&mut self) -> bool {
        let (read, write) = match nix::unistd::pipe() {
            Ok(pair) => pair,
            Err(e) => {
                zlog_sys!(Error @ fpm_unix_init_main:613, e, "failed to create pipe");
                return false;
            }
        };
        match php_sapi::fork() {
            Err(e) => {
                zlog_sys!(Error @ fpm_unix_init_main:622, crate::conf::errno(&e), "failed to daemonize");
                false
            }
            Ok(php_sapi::Forked::Child) => {
                drop(read);
                self.send_config_pipe = Some(write);
                true
            }
            Ok(php_sapi::Forked::Parent { .. }) => {
                drop(write);
                zlog!(
                    Debug @ fpm_unix_init_main:643,
                    "The calling process is waiting for the master process to ping via fd={}",
                    read.as_raw_fd()
                );
                let mut fds = [nix::poll::PollFd::new(
                    read.as_fd(),
                    nix::poll::PollFlags::POLLIN,
                )];
                let ready = match nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(10_000u16))
                {
                    Ok(ready) => ready,
                    Err(e) => {
                        zlog_sys!(Error @ fpm_unix_init_main:646, e, "failed to select");
                        std::process::exit(i32::from(EXIT_SOFTWARE));
                    }
                };
                if ready == 0 {
                    zlog!(
                        Error @ fpm_unix_init_main:671,
                        "the master process didn't send back its status (via the pipe to the calling process)"
                    );
                    std::process::exit(i32::from(EXIT_SOFTWARE));
                }
                let mut file = std::fs::File::from(read);
                let mut value = [0u8; 4];
                match file.read(&mut value) {
                    Ok(0) => {
                        zlog!(Error @ fpm_unix_init_main:658, "no data have been read from pipe");
                        std::process::exit(i32::from(EXIT_SOFTWARE));
                    }
                    Ok(_) if i32::from_ne_bytes(value) == 1 => {
                        zlog!(
                            Debug @ fpm_unix_init_main:662,
                            "I received a valid acknowledge from the master process, I can exit without error"
                        );
                        // Returned up to `run`, which ends with
                        // `FPM_EXIT_OK` once its engine is ended.
                        self.acknowledged = true;
                        false
                    }
                    Ok(_) => {
                        zlog!(Debug @ fpm_unix_init_main:666, "The master process returned an error !");
                        std::process::exit(i32::from(EXIT_SOFTWARE))
                    }
                    Err(e) => {
                        zlog_sys!(Error @ fpm_unix_init_main:653, conf::errno(&e), "failed to read from pipe");
                        std::process::exit(i32::from(EXIT_SOFTWARE))
                    }
                }
            }
        }
    }

    /// `fpm_scoreboard_init_main`: each pool's scoreboard mapped, a
    /// status pool's pointing at the one it shows.
    fn scoreboard_init_main(&mut self) -> bool {
        zlog!(Debug @ fpm_scoreboard_init_main:38, "got clock tick '{}'", crate::scoreboard::clock_tick());
        for pool in &mut self.pools {
            let Ok(nprocs) = usize::try_from(pool.config.pm_max_children) else {
                zlog!(
                    Error @ fpm_scoreboard_init_main:47,
                    "[pool {}] Unable to create scoreboard SHM because max_client is not set",
                    pool.name()
                );
                return false;
            };
            if nprocs < 1 {
                zlog!(
                    Error @ fpm_scoreboard_init_main:47,
                    "[pool {}] Unable to create scoreboard SHM because max_client is not set",
                    pool.name()
                );
                return false;
            }
            let Some(scoreboard) = Scoreboard::new(&pool.config.name, pool.pm(), nprocs) else {
                zlog_sys!(
                    Error @ fpm_shm_alloc:32,
                    nix::errno::Errno::last(),
                    "unable to allocate {} bytes in shared memory",
                    nprocs
                );
                return false;
            };
            pool.scoreboard = Some(scoreboard);
        }
        true
    }

    /// `fpm_signals_init_main`: the pipe the handlers write into, and the
    /// handlers; every signal unblocked after.
    fn signals_init_main(&mut self) -> bool {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
        let (read, write) = match socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        ) {
            Ok(pair) => pair,
            Err(e) => {
                zlog_sys!(Error @ fpm_signals_init_main:190, e, "failed to init signals: socketpair()");
                return false;
            }
        };
        for end in [&read, &write] {
            use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
            if fcntl(end.as_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).is_err() {
                zlog_sys!(
                    Error @ fpm_signals_init_main:195,
                    nix::errno::Errno::last(),
                    "failed to init signals: fd_set_blocked()"
                );
                return false;
            }
            if fcntl(end.as_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).is_err() {
                zlog_sys!(
                    Error @ fpm_signals_init_main:200,
                    nix::errno::Errno::last(),
                    "failed to init signals: fcntl(F_SETFD, FD_CLOEXEC)"
                );
                return false;
            }
        }
        if let Err(e) = crate::ffi::master_signals(write.as_raw_fd(), self.parent_pid) {
            zlog_sys!(Error @ fpm_signals_init_main:215, e, "failed to init signals: sigaction()");
            return false;
        }
        self.signal_read = Some(read);
        self.signal_write = Some(write);
        zlog!(Debug @ fpm_signals_init_main:219, "Unblocking all signals");
        unblock_all();
        true
    }

    /// `fpm_children_init_main`: the ring of crash times where
    /// `emergency_restart_threshold` and `_interval` are both set.
    fn children_init_main(&mut self) -> bool {
        if self.global.emergency_restart_threshold > 0 && self.global.emergency_restart_interval > 0
        {
            self.last_faults =
                vec![0; usize::try_from(self.global.emergency_restart_threshold).unwrap_or(0)];
        }
        true
    }

    /// `fpm_sockets_init_main`: the inherited sockets, then each pool's.
    fn sockets_init_main(&mut self) -> bool {
        self.sockets.inherit();
        for i in 0..self.pools.len() {
            let config = self.pools[i].config.clone();
            let address = config.listen_address.clone().unwrap_or_default();
            let fd = match self.pools[i].domain {
                Domain::Inet => self
                    .sockets
                    .inet(&config.name, &address, config.listen_backlog),
                Domain::Unix => {
                    let Some(permissions) = crate::unix::socket_permissions(&config) else {
                        return false;
                    };
                    self.sockets
                        .unix(&config.name, &address, config.listen_backlog, permissions)
                }
            };
            let Some(fd) = fd else {
                return false;
            };
            self.pools[i].listening = Some(fd);
        }
        self.sockets.close_unused();
        true
    }

    /// `fpm_event_init_main`: the event module made, reserving one event
    /// for the signal pipe and two for each child of a pool that catches
    /// its workers' output.
    fn event_init_main(&mut self) -> bool {
        let max = 1 + self
            .pools
            .iter()
            .filter(|p| p.config.catch_workers_output && p.config.pm_max_children > 0)
            .map(|p| p.config.pm_max_children * 2)
            .sum::<i32>();
        if let Ok(poller) = Poller::new(usize::try_from(max).unwrap_or(1)) {
            self.poller = Some(poller);
            zlog!(
                Debug @ fpm_event_init_main:345,
                "event module is {} and {max} fds have been reserved",
                crate::events::MECHANISM
            );
            true
        } else {
            zlog!(
                Error @ fpm_event_init_main:341,
                "Unable to initialize the event module {}",
                crate::events::MECHANISM
            );
            false
        }
    }

    /// `fpm_conf_write_pid`.
    fn write_pid(&self) -> bool {
        let Some(pid_file) = &self.global.pid_file else {
            return true;
        };
        let path = conf::path(pid_file);
        std::fs::remove_file(path).ok();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(path);
        let mut file = match file {
            Ok(file) => file,
            Err(e) => {
                zlog_sys!(
                    Error @ fpm_conf_write_pid:1261,
                    conf::errno(&e),
                    "Unable to create the PID file ({}).",
                    String::from_utf8_lossy(pid_file)
                );
                return false;
            }
        };
        if let Err(e) = std::io::Write::write_all(&mut file, self.parent_pid.to_string().as_bytes())
        {
            zlog_sys!(Error @ fpm_conf_write_pid:1268, conf::errno(&e), "Unable to write to the PID file.");
            return false;
        }
        true
    }

    /// `fpm_conf_unlink_pid`.
    pub(crate) fn unlink_pid(&self) {
        if let Some(pid_file) = &self.global.pid_file
            && let Err(e) = std::fs::remove_file(conf::path(pid_file))
        {
            zlog_sys!(
                Error @ fpm_conf_unlink_pid:1242,
                conf::errno(&e),
                "Unable to remove the PID file ({}).",
                String::from_utf8_lossy(pid_file)
            );
        }
    }

    /// `fpm_run`: each pool's first children made, then the loop, until
    /// this process is one of the children.
    pub(crate) fn run(&mut self) -> Step<std::convert::Infallible> {
        for i in 0..self.pools.len() {
            if crate::children::create_initial(self, i)? == crate::children::Made::Failed {
                self.pctl(State::Terminating, crate::pctl::Action::Set)?;
                self.event_loop(true)?;
            }
        }
        self.event_loop(false)
    }

    /// Watch `fd` in the loop as `what`.
    fn watch(&mut self, fd: RawFd, what: Watched, edge: bool) {
        let (Some(poller), Some(borrowed)) = (&self.poller, self.borrow(fd, what)) else {
            return;
        };
        if poller.add(borrowed, edge).is_err() {
            zlog!(Error @ fpm_event_kqueue_add:167, "kevent: unable to add event");
            return;
        }
        self.watched.retain(|(had, _)| *had != fd);
        self.watched.push((fd, what));
    }

    /// The descriptor a watch names, borrowed from its owner.
    fn borrow(&self, fd: RawFd, what: Watched) -> Option<std::os::fd::BorrowedFd<'_>> {
        match what {
            Watched::Signals => self.signal_read.as_ref().map(AsFd::as_fd),
            Watched::Output { pid, pipe } => self
                .pools
                .iter()
                .flat_map(|p| p.children.iter())
                .find(|c| c.pid == pid)
                .and_then(|c| pipe.of(c))
                .map(AsFd::as_fd),
            Watched::Ondemand(_) => self.sockets.by_fd(fd).map(AsFd::as_fd),
        }
    }

    /// `fpm_event_del` of a descriptor still open.
    pub(crate) fn unwatch(&mut self, fd: std::os::fd::BorrowedFd<'_>) {
        if let Some(poller) = &self.poller {
            poller.remove(fd);
        }
        let raw = fd.as_raw_fd();
        self.watched.retain(|(had, _)| *had != raw);
    }

    /// Watch a child's two pipes, `fpm_stdio_parent_use_pipes`.
    pub(crate) fn watch_output(&mut self, pid: i32, stdout: Option<RawFd>, stderr: Option<RawFd>) {
        use crate::stdio::Pipe;
        for (fd, pipe) in [(stdout, Pipe::Stdout), (stderr, Pipe::Stderr)] {
            if let Some(fd) = fd {
                self.watch(fd, Watched::Output { pid, pipe }, false);
            }
        }
    }

    /// `fpm_children_create_initial`'s ondemand half: the listening socket
    /// watched, edge-triggered.
    pub(crate) fn watch_ondemand(&mut self, pool: usize) {
        if let Some(fd) = self.pools[pool].listening {
            self.watch(fd, Watched::Ondemand(pool), true);
        }
    }

    /// `fpm_event_set_timer` and `fpm_event_add`: `timer` due in `ms`,
    /// repeating where `persist` says; one queued already moves.
    pub(crate) fn add_timer(&mut self, timer: Timer, ms: u64, persist: bool) {
        let every = std::time::Duration::from_millis(ms);
        let due = Instant::now() + every;
        if let Some(queued) = self.timers.iter_mut().find(|q| q.timer == timer) {
            queued.due = due;
            queued.every = persist.then_some(every);
            return;
        }
        self.timers.insert(
            0,
            Queued {
                timer,
                due,
                every: persist.then_some(every),
            },
        );
    }

    /// `fpm_event_loop`.
    fn event_loop(&mut self, err: bool) -> Step<std::convert::Infallible> {
        if let Some(fd) = self.signal_read.as_ref().map(AsRawFd::as_raw_fd) {
            self.watch(fd, Watched::Signals, false);
        }
        if self.heartbeat > 0 {
            self.heartbeat = self.heartbeat.max(crate::pctl::MIN_HEARTBEAT);
            zlog!(
                Debug @ fpm_pctl_heartbeat:478,
                "heartbeat have been set up with a timeout of {}ms",
                self.heartbeat
            );
            self.add_timer(
                Timer::Heartbeat,
                u64::try_from(self.heartbeat).unwrap_or(1000),
                true,
            );
        }
        if !err {
            self.add_timer(Timer::Maintenance, crate::pctl::MAINTENANCE_HEARTBEAT, true);
            let reserved: usize = self
                .pools
                .iter()
                .filter_map(|p| p.scoreboard)
                .map(|s| s.shm_bytes())
                .sum();
            zlog!(Debug @ fpm_event_loop:373, "{reserved} bytes have been reserved in SHM");
            zlog!(Notice @ fpm_event_loop:374, "ready to handle connections");
        }
        loop {
            let now = Instant::now();
            let next = self.timers.iter().map(|q| q.due).min();
            let timeout = match next {
                Some(due) if due > now => {
                    u64::try_from((due - now).as_millis()).unwrap_or(1000) + 1
                }
                _ => 1000,
            };
            let ready = self
                .poller
                .as_ref()
                .map(|p| p.wait(timeout))
                .unwrap_or_default();
            let triggered = ready.len();
            for fd in ready {
                let Some(&(_, what)) = self.watched.iter().find(|(had, _)| *had == fd) else {
                    continue;
                };
                match what {
                    Watched::Signals => self.got_signal()?,
                    Watched::Output { pid, pipe } => crate::stdio::child_said(self, pid, pipe),
                    Watched::Ondemand(pool) => crate::pctl::on_socket_accept(self, pool)?,
                }
            }
            if triggered > 0 {
                zlog!(Debug @ fpm_event_loop:426, "event module triggered {triggered} events");
            }
            let mut at = 0;
            while at < self.timers.len() {
                let now = Instant::now();
                if now < self.timers[at].due {
                    at += 1;
                    continue;
                }
                let timer = self.timers[at].timer;
                match self.timers[at].every {
                    Some(every) => {
                        self.timers[at].due = now + every;
                        at += 1;
                    }
                    None => {
                        self.timers.remove(at);
                    }
                }
                self.fire(timer)?;
            }
        }
    }

    /// A timer's callback.
    fn fire(&mut self, timer: Timer) -> Step {
        match timer {
            Timer::Heartbeat => {
                crate::pctl::check_request_timeout(self);
                Ok(())
            }
            Timer::Maintenance => {
                if self.state == State::Normal {
                    crate::pctl::perform_idle_server_maintenance(self)?;
                }
                Ok(())
            }
            Timer::Pctl => self.pctl(State::Normal, crate::pctl::Action::Timeout),
            Timer::Bury => crate::children::bury(self),
        }
    }

    /// `fpm_got_signal`: each character the handlers wrote, acted on.
    fn got_signal(&mut self) -> Step {
        let mut byte = [0u8; 1];
        loop {
            let read = match &self.signal_read {
                Some(fd) => nix::unistd::read(fd, &mut byte),
                None => return Ok(()),
            };
            match read {
                Ok(1) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::EAGAIN) | Ok(_) => return Ok(()),
                Err(e) => {
                    zlog_sys!(Error @ fpm_got_signal:74, e, "unable to read from the signal pipe");
                    return Ok(());
                }
            }
            match byte[0] {
                b'C' => {
                    zlog!(Debug @ fpm_got_signal:81, "received SIGCHLD");
                    self.add_timer(Timer::Bury, 0, false);
                }
                b'I' => {
                    zlog!(Debug @ fpm_got_signal:90, "received SIGINT");
                    zlog!(Notice @ fpm_got_signal:91, "Terminating ...");
                    self.pctl(State::Terminating, crate::pctl::Action::Set)?;
                }
                b'T' => {
                    zlog!(Debug @ fpm_got_signal:95, "received SIGTERM");
                    zlog!(Notice @ fpm_got_signal:96, "Terminating ...");
                    self.pctl(State::Terminating, crate::pctl::Action::Set)?;
                }
                b'Q' => {
                    zlog!(Debug @ fpm_got_signal:100, "received SIGQUIT");
                    zlog!(Notice @ fpm_got_signal:101, "Finishing ...");
                    self.pctl(State::Finishing, crate::pctl::Action::Set)?;
                }
                b'1' => {
                    zlog!(Debug @ fpm_got_signal:105, "received SIGUSR1");
                    crate::stdio::reopen_logs(self);
                }
                b'2' => {
                    zlog!(Debug @ fpm_got_signal:130, "received SIGUSR2");
                    zlog!(Notice @ fpm_got_signal:131, "Reloading in progress ...");
                    self.pctl(State::Reloading, crate::pctl::Action::Set)?;
                }
                _ => {}
            }
        }
    }

    /// The child `pid`, in whichever pool it runs.
    pub(crate) fn child(&mut self, pid: i32) -> Option<(usize, usize)> {
        self.pools.iter().enumerate().find_map(|(p, pool)| {
            pool.children
                .iter()
                .position(|c| c.pid == pid)
                .map(|c| (p, c))
        })
    }

    /// `fpm_globals.running_children`.
    pub(crate) fn running_children(&self) -> i32 {
        self.pools.iter().map(Pool::running).sum()
    }

    /// A crash counted towards `emergency_restart_threshold`: whether it
    /// fills the interval, which starts a reload.
    pub(crate) fn fault(&mut self, now: i64) -> bool {
        if self.last_faults.is_empty() {
            return false;
        }
        self.last_faults[self.fault] = now;
        self.fault += 1;
        if self.fault == self.last_faults.len() {
            self.fault = 0;
        }
        let interval = i64::from(self.global.emergency_restart_interval);
        self.last_faults.iter().all(|at| now - at <= interval)
    }

    /// `fpm_event_cleanup` in a new child: the event queue given up. A
    /// kqueue is not inherited across `fork`, so its number is no
    /// descriptor of the child's and is left unclosed, lest it close
    /// whatever the child opens under that number next.
    pub(crate) fn leave_events(&mut self) {
        self.watched.clear();
        self.timers.clear();
        let poller = self.poller.take();
        if cfg!(target_os = "linux") {
            drop(poller);
        } else {
            #[expect(
                clippy::mem_forget,
                reason = "a kqueue is not inherited across `fork`, so its number is no descriptor of the child's and is left unclosed"
            )]
            std::mem::forget(poller);
        }
    }

    /// The read end of the signal pipe, closed in a child.
    pub(crate) fn close_signal_pipe(&mut self) {
        self.signal_read = None;
        self.signal_write = None;
    }
}

/// A `log_level` number as a level, where it is one.
pub(crate) fn level_of(n: i32) -> Option<crate::zlog::Level> {
    use crate::zlog::Level;
    Some(match n {
        1 => Level::Debug,
        2 => Level::Notice,
        3 => Level::Warning,
        4 => Level::Error,
        5 => Level::Alert,
        _ => return None,
    })
}
