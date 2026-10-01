//! `fpm_scoreboard.c`: each pool's scoreboard in memory the master maps
//! shared before it forks, a header of the pool's counts and a slot per
//! child it may run, which the children write their request's stage and
//! details into and the master and the status page read. php guards the
//! header with a reader and writer count and each slot with a spinlock;
//! here every field is an atomic and the same two locks are a spinning
//! flag each, so a reader sees one writer's whole update.

use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::conf::Pm;

/// `fpm_request_stage_e`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Stage {
    Creating = 0,
    Accepting = 1,
    ReadingHeaders = 2,
    Info = 3,
    Executing = 4,
    End = 5,
    Finished = 6,
}

impl Stage {
    fn of(byte: u8) -> Stage {
        match byte {
            1 => Stage::Accepting,
            2 => Stage::ReadingHeaders,
            3 => Stage::Info,
            4 => Stage::Executing,
            5 => Stage::End,
            6 => Stage::Finished,
            _ => Stage::Creating,
        }
    }

    /// `requests_stages`, what the status page names a stage.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Stage::Creating => "Creating",
            Stage::Accepting => "Idle",
            Stage::ReadingHeaders => "Reading headers",
            Stage::Info => "Getting request information",
            Stage::Executing => "Running",
            Stage::End => "Ending",
            Stage::Finished => "Finishing",
        }
    }
}

/// A fixed-length string of the scoreboard, as `strlcpy` fills it.
pub(crate) struct Text<const N: usize>([AtomicU8; N]);

impl<const N: usize> Text<N> {
    fn set(&self, bytes: &[u8]) {
        let len = bytes.len().min(N - 1);
        for (cell, byte) in self.0.iter().zip(bytes[..len].iter().copied().chain([0])) {
            cell.store(byte, Ordering::Relaxed);
        }
    }

    fn get(&self) -> Vec<u8> {
        self.0
            .iter()
            .map(|cell| cell.load(Ordering::Relaxed))
            .take_while(|b| *b != 0)
            .collect()
    }
}

/// `struct fpm_scoreboard_s`, less its slots.
pub(crate) struct Board {
    lock: AtomicU32,
    pool: Text<32>,
    pm: AtomicU8,
    start_epoch: AtomicI64,
    idle: AtomicI32,
    active: AtomicI32,
    active_max: AtomicI32,
    requests: AtomicU64,
    max_children_reached: AtomicU32,
    lq: AtomicI32,
    lq_max: AtomicI32,
    lq_len: AtomicU32,
    nprocs: AtomicU32,
    free_proc: AtomicI32,
    slow_rq: AtomicU64,
    memory_peak: AtomicU64,
}

/// `struct fpm_scoreboard_proc_s`: one child's slot.
pub(crate) struct Proc {
    lock: AtomicU32,
    used: AtomicU32,
    start_epoch: AtomicI64,
    pid: AtomicI32,
    requests: AtomicU64,
    stage: AtomicU8,
    accepted_us: AtomicI64,
    duration_us: AtomicI64,
    accepted_epoch: AtomicI64,
    tv_us: AtomicI64,
    request_uri: Text<512>,
    query_string: Text<2048>,
    request_method: Text<16>,
    content_length: AtomicU64,
    script_filename: Text<512>,
    auth_user: Text<32>,
    cpu_accepted_us: AtomicI64,
    cpu_duration_us: AtomicI64,
    last_request_cpu_us: AtomicI64,
    memory: AtomicU64,
}

/// A copy of a slot, `*proc` as `fpm_scoreboard_copy` takes it.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcView {
    pub(crate) used: bool,
    pub(crate) start_epoch: i64,
    pub(crate) pid: i32,
    pub(crate) requests: u64,
    pub(crate) stage: Option<Stage>,
    pub(crate) accepted_us: i64,
    pub(crate) duration_us: i64,
    pub(crate) accepted_epoch: i64,
    pub(crate) request_uri: Vec<u8>,
    pub(crate) query_string: Vec<u8>,
    pub(crate) request_method: Vec<u8>,
    pub(crate) content_length: u64,
    pub(crate) script_filename: Vec<u8>,
    pub(crate) auth_user: Vec<u8>,
    pub(crate) cpu_duration_us: i64,
    pub(crate) last_request_cpu_us: i64,
    pub(crate) memory: u64,
}

impl ProcView {
    /// The stage, `Creating` where none was written.
    pub(crate) fn stage(&self) -> Stage {
        self.stage.unwrap_or(Stage::Creating)
    }

    /// The status page's `last request cpu`: the CPU the request took
    /// over its wall time, in percent; zero where no time passed.
    pub(crate) fn cpu(&self) -> f64 {
        if self.cpu_duration_us == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)] // microseconds, well inside a double
        let ratio = self.last_request_cpu_us as f64 / self.cpu_duration_us as f64;
        ratio * 100.0
    }
}

/// A copy of the header, `*scoreboard` as `fpm_scoreboard_copy` takes it.
#[derive(Debug, Clone, Default)]
pub(crate) struct BoardView {
    pub(crate) pool: Vec<u8>,
    pub(crate) pm: Option<Pm>,
    pub(crate) start_epoch: i64,
    pub(crate) idle: i32,
    pub(crate) active: i32,
    pub(crate) active_max: i32,
    pub(crate) requests: u64,
    pub(crate) max_children_reached: u32,
    pub(crate) lq: i32,
    pub(crate) lq_max: i32,
    pub(crate) lq_len: u32,
    pub(crate) slow_rq: u64,
    pub(crate) memory_peak: u64,
    pub(crate) procs: Vec<ProcView>,
}

/// A pool's scoreboard: its header and its slots, in shared memory.
#[derive(Clone, Copy)]
pub(crate) struct Scoreboard {
    pub(crate) board: &'static Board,
    pub(crate) procs: &'static [Proc],
}

/// `sizeof(struct fpm_scoreboard_s)` and `sizeof(struct
/// fpm_scoreboard_proc_s)` on an LP64 target with `HAVE_TIMES`, as the
/// oracle's: what `fpm_shm_alloc` counts for a pool, which `%zu bytes
/// have been reserved in SHM` reports. This host lays its slots out in
/// atomics of its own, so the count is php's, not this layout's.
const BOARD_BYTES: usize = 144;
const PROC_BYTES: usize = 3344;

/// `fpm_scoreboard_tick`: `sysconf(_SC_CLK_TCK)`, as `%.0f` prints it.
pub(crate) fn clock_tick() -> i64 {
    nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .map_or(100, i64::from)
}

/// `FPM_SCOREBOARD_SPINLOCK_MAX_RETRIES`.
const MAX_RETRIES: u32 = 50_000;

/// A spinning flag held: `fpm_spinlock`, giving up after the retries php
/// allows where `nohang` says.
fn lock(flag: &AtomicU32, nohang: bool) -> bool {
    let mut tries = 0;
    while flag
        .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        tries += 1;
        if nohang && tries > MAX_RETRIES {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

fn unlock(flag: &AtomicU32) {
    flag.store(0, Ordering::Release);
}

/// Now, as `time(NULL)`.
pub(crate) fn epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// `FPM_SCOREBOARD_ACTION_SET` or `_INC`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Set,
    Inc,
}

/// `fpm_scoreboard_update_commit`'s arguments: `-1` leaves a count as it
/// stands under `Set`.
#[derive(Clone, Copy)]
pub(crate) struct Update {
    pub(crate) idle: i32,
    pub(crate) active: i32,
    pub(crate) lq: i32,
    pub(crate) lq_len: i32,
    pub(crate) requests: i32,
    pub(crate) max_children_reached: i32,
    pub(crate) slow_rq: i32,
    pub(crate) memory_peak: u64,
}

impl Update {
    /// Every count left as it stands.
    pub(crate) const NONE: Update = Update {
        idle: -1,
        active: -1,
        lq: -1,
        lq_len: -1,
        requests: -1,
        max_children_reached: -1,
        slow_rq: -1,
        memory_peak: 0,
    };

    /// Every count moved by nothing.
    pub(crate) const ZERO: Update = Update {
        idle: 0,
        active: 0,
        lq: 0,
        lq_len: 0,
        requests: 0,
        max_children_reached: 0,
        slow_rq: 0,
        memory_peak: 0,
    };
}

fn add_floor(cell: &AtomicI32, by: i32) {
    let now = cell.load(Ordering::Relaxed);
    cell.store(if now + by > 0 { now + by } else { 0 }, Ordering::Relaxed);
}

impl Scoreboard {
    /// `fpm_scoreboard_init_main` for one pool: the memory mapped and its
    /// pool, process manager, slot count and start time written.
    pub(crate) fn new(pool: &[u8], pm: Pm, nprocs: usize) -> Option<Scoreboard> {
        let board = crate::ffi::shared::<Board>(1)?.first()?;
        let procs = crate::ffi::shared::<Proc>(nprocs)?;
        board.pool.set(pool);
        board.pm.store(pm as u8, Ordering::Relaxed);
        board
            .nprocs
            .store(u32::try_from(nprocs).unwrap_or(0), Ordering::Relaxed);
        board.start_epoch.store(epoch(), Ordering::Relaxed);
        Some(Scoreboard { board, procs })
    }

    /// `fpm_scoreboard_update_begin` and `_commit` as one act.
    pub(crate) fn update(&self, update: Update, action: Action) {
        let b = self.board;
        lock(&b.lock, false);
        if action == Action::Set {
            let set = |cell: &AtomicI32, v: i32| {
                if v >= 0 {
                    cell.store(v, Ordering::Relaxed);
                }
            };
            set(&b.idle, update.idle);
            set(&b.active, update.active);
            set(&b.lq, update.lq);
            if update.lq_len >= 0 {
                b.lq_len
                    .store(u32::try_from(update.lq_len).unwrap_or(0), Ordering::Relaxed);
            }
            if update.requests >= 0 {
                b.requests.store(
                    u64::try_from(update.requests).unwrap_or(0),
                    Ordering::Relaxed,
                );
            }
            if update.max_children_reached >= 0 {
                let n = u32::try_from(update.max_children_reached).unwrap_or(0);
                b.max_children_reached.store(n, Ordering::Relaxed);
            }
            if update.slow_rq > 0 {
                b.slow_rq.store(
                    u64::try_from(update.slow_rq).unwrap_or(0),
                    Ordering::Relaxed,
                );
            }
        } else {
            add_floor(&b.idle, update.idle);
            add_floor(&b.active, update.active);
            let requests = i64::try_from(b.requests.load(Ordering::Relaxed)).unwrap_or(0)
                + i64::from(update.requests);
            b.requests.store(
                u64::try_from(requests.max(0)).unwrap_or(0),
                Ordering::Relaxed,
            );
            let reached = i64::from(b.max_children_reached.load(Ordering::Relaxed))
                + i64::from(update.max_children_reached);
            b.max_children_reached.store(
                u32::try_from(reached.max(0)).unwrap_or(0),
                Ordering::Relaxed,
            );
            let slow = i64::try_from(b.slow_rq.load(Ordering::Relaxed)).unwrap_or(0)
                + i64::from(update.slow_rq);
            b.slow_rq
                .store(u64::try_from(slow.max(0)).unwrap_or(0), Ordering::Relaxed);
        }
        let active = b.active.load(Ordering::Relaxed);
        if active > b.active_max.load(Ordering::Relaxed) {
            b.active_max.store(active, Ordering::Relaxed);
        }
        if b.memory_peak.load(Ordering::Relaxed) < update.memory_peak {
            b.memory_peak.store(update.memory_peak, Ordering::Relaxed);
        }
        unlock(&b.lock);
    }

    /// What `fpm_scoreboard_init_main` asked `fpm_shm_alloc` for, the
    /// header and its slots at php's sizes.
    pub(crate) fn shm_bytes(&self) -> usize {
        BOARD_BYTES + PROC_BYTES * self.procs.len()
    }

    /// The pool's name as the header carries it.
    pub(crate) fn pool(&self) -> Vec<u8> {
        self.board.pool.get()
    }

    /// `fpm_scoreboard_proc_alloc`: the slot `free_proc` names where it is
    /// free, else the first free one, marked used; `None` where none is.
    pub(crate) fn alloc(&self) -> Option<usize> {
        let free = self.board.free_proc.load(Ordering::Relaxed);
        let nprocs = self.procs.len();
        let at = usize::try_from(free)
            .ok()
            .filter(|&at| at < nprocs && self.procs[at].used.load(Ordering::Relaxed) == 0)
            .or_else(|| {
                crate::zlog!(
                    Debug @ fpm_scoreboard_proc_alloc:483,
                    "[pool {}] the proc->free_slot was not free. Let's search",
                    String::from_utf8_lossy(&self.pool())
                );
                self.procs
                    .iter()
                    .position(|p| p.used.load(Ordering::Relaxed) == 0)
            })?;
        self.procs[at].used.store(1, Ordering::Relaxed);
        let next = if at + 1 >= nprocs { 0 } else { at + 1 };
        self.board
            .free_proc
            .store(i32::try_from(next).unwrap_or(0), Ordering::Relaxed);
        Some(at)
    }

    /// `fpm_scoreboard_proc_free`: the slot zeroed, and named free.
    pub(crate) fn free(&self, at: usize) {
        let Some(proc) = self.procs.get(at) else {
            return;
        };
        if proc.used.load(Ordering::Relaxed) > 0 {
            proc.clear();
        }
        self.board
            .free_proc
            .store(i32::try_from(at).unwrap_or(0), Ordering::Relaxed);
    }

    /// `fpm_scoreboard_child_use`: the child's pid and start time.
    pub(crate) fn child_use(&self, at: usize, pid: i32) {
        if let Some(proc) = self.procs.get(at) {
            proc.pid.store(pid, Ordering::Relaxed);
            proc.start_epoch.store(epoch(), Ordering::Relaxed);
        }
    }

    /// The slot, where it is one of the board's.
    pub(crate) fn proc(&self, at: usize) -> Option<&'static Proc> {
        self.procs.get(at)
    }

    /// `fpm_scoreboard_copy`: the header and, where `procs` says, every
    /// slot, each read whole under its lock.
    pub(crate) fn copy(&self, procs: bool) -> BoardView {
        let b = self.board;
        lock(&b.lock, false);
        let mut view = BoardView {
            pool: b.pool.get(),
            pm: match b.pm.load(Ordering::Relaxed) {
                0 => Some(Pm::Static),
                1 => Some(Pm::Dynamic),
                2 => Some(Pm::Ondemand),
                _ => None,
            },
            start_epoch: b.start_epoch.load(Ordering::Relaxed),
            idle: b.idle.load(Ordering::Relaxed),
            active: b.active.load(Ordering::Relaxed),
            active_max: b.active_max.load(Ordering::Relaxed),
            requests: b.requests.load(Ordering::Relaxed),
            max_children_reached: b.max_children_reached.load(Ordering::Relaxed),
            lq: b.lq.load(Ordering::Relaxed),
            lq_max: b.lq_max.load(Ordering::Relaxed),
            lq_len: b.lq_len.load(Ordering::Relaxed),
            slow_rq: b.slow_rq.load(Ordering::Relaxed),
            memory_peak: b.memory_peak.load(Ordering::Relaxed),
            procs: Vec::new(),
        };
        unlock(&b.lock);
        if procs {
            view.procs = self.procs.iter().map(Proc::view).collect();
        }
        view
    }
}

/// What a child's request writes into its slot, `fpm_request_info`'s.
#[derive(Debug, Clone, Default)]
pub(crate) struct Info {
    pub(crate) request_uri: Option<Vec<u8>>,
    pub(crate) request_method: Option<Vec<u8>>,
    pub(crate) query_string: Option<Vec<u8>>,
    pub(crate) auth_user: Option<Vec<u8>>,
    pub(crate) script_filename: Option<Vec<u8>>,
    pub(crate) content_length: u64,
}

impl Proc {
    fn clear(&self) {
        lock(&self.lock, false);
        self.used.store(0, Ordering::Relaxed);
        for cell in [
            &self.start_epoch,
            &self.accepted_us,
            &self.duration_us,
            &self.accepted_epoch,
            &self.tv_us,
            &self.cpu_accepted_us,
            &self.cpu_duration_us,
            &self.last_request_cpu_us,
        ] {
            cell.store(0, Ordering::Relaxed);
        }
        self.pid.store(0, Ordering::Relaxed);
        self.requests.store(0, Ordering::Relaxed);
        self.stage.store(0, Ordering::Relaxed);
        self.content_length.store(0, Ordering::Relaxed);
        self.memory.store(0, Ordering::Relaxed);
        for text in [
            &self.request_uri.0[..],
            &self.query_string.0,
            &self.request_method.0,
            &self.script_filename.0,
            &self.auth_user.0,
        ] {
            for cell in text {
                cell.store(0, Ordering::Relaxed);
            }
        }
        unlock(&self.lock);
    }

    /// The slot read whole, under its lock.
    pub(crate) fn view(&self) -> ProcView {
        lock(&self.lock, false);
        let view = ProcView {
            used: self.used.load(Ordering::Relaxed) != 0,
            start_epoch: self.start_epoch.load(Ordering::Relaxed),
            pid: self.pid.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            stage: Some(Stage::of(self.stage.load(Ordering::Relaxed))),
            accepted_us: self.accepted_us.load(Ordering::Relaxed),
            duration_us: self.duration_us.load(Ordering::Relaxed),
            accepted_epoch: self.accepted_epoch.load(Ordering::Relaxed),
            request_uri: self.request_uri.get(),
            query_string: self.query_string.get(),
            request_method: self.request_method.get(),
            content_length: self.content_length.load(Ordering::Relaxed),
            script_filename: self.script_filename.get(),
            auth_user: self.auth_user.get(),
            cpu_duration_us: self.cpu_duration_us.load(Ordering::Relaxed),
            last_request_cpu_us: self.last_request_cpu_us.load(Ordering::Relaxed),
            memory: self.memory.load(Ordering::Relaxed),
        };
        unlock(&self.lock);
        view
    }

    /// The stage, read without the lock, as `fpm_request_is_idle` reads it.
    pub(crate) fn stage(&self) -> Stage {
        Stage::of(self.stage.load(Ordering::Relaxed))
    }

    /// `proc->tv`, the last activity.
    pub(crate) fn last_activity_us(&self) -> i64 {
        self.tv_us.load(Ordering::Relaxed)
    }

    /// A stage written with the time it began, under the slot's lock.
    pub(crate) fn set_stage(&self, stage: Stage, now_us: i64) {
        lock(&self.lock, false);
        self.stage.store(stage as u8, Ordering::Relaxed);
        self.tv_us.store(now_us, Ordering::Relaxed);
        unlock(&self.lock);
    }

    /// `fpm_request_reading_headers`'s slot: the stage, the time and CPU
    /// the request was accepted at, one more request, every detail of
    /// the last one cleared.
    pub(crate) fn reading_headers(&self, now_us: i64, cpu_us: i64) {
        lock(&self.lock, false);
        self.stage
            .store(Stage::ReadingHeaders as u8, Ordering::Relaxed);
        self.tv_us.store(now_us, Ordering::Relaxed);
        self.accepted_us.store(now_us, Ordering::Relaxed);
        self.accepted_epoch.store(epoch(), Ordering::Relaxed);
        self.cpu_accepted_us.store(cpu_us, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        for text in [
            &self.request_uri.0[..],
            &self.request_method.0,
            &self.script_filename.0,
            &self.query_string.0,
            &self.auth_user.0,
        ] {
            text[0].store(0, Ordering::Relaxed);
        }
        self.content_length.store(0, Ordering::Relaxed);
        unlock(&self.lock);
    }

    /// `fpm_request_info`'s slot.
    pub(crate) fn info(&self, info: &Info, now_us: i64) {
        lock(&self.lock, false);
        self.stage.store(Stage::Info as u8, Ordering::Relaxed);
        self.tv_us.store(now_us, Ordering::Relaxed);
        if let Some(uri) = &info.request_uri {
            self.request_uri.set(uri);
        }
        if let Some(method) = &info.request_method {
            self.request_method.set(method);
        }
        if let Some(query) = &info.query_string {
            self.query_string.set(query);
        }
        if let Some(user) = &info.auth_user {
            self.auth_user.set(user);
        }
        self.content_length
            .store(info.content_length, Ordering::Relaxed);
        if let Some(script) = &info.script_filename {
            self.script_filename.set(script);
        }
        unlock(&self.lock);
    }

    /// `fpm_request_end`'s slot: finishing, how long the request took in
    /// wall and CPU time, and its memory.
    pub(crate) fn end(&self, now_us: i64, cpu_us: i64, memory: u64) {
        lock(&self.lock, false);
        self.stage.store(Stage::Finished as u8, Ordering::Relaxed);
        self.tv_us.store(now_us, Ordering::Relaxed);
        let duration = now_us - self.accepted_us.load(Ordering::Relaxed);
        self.duration_us.store(duration, Ordering::Relaxed);
        self.cpu_duration_us.store(duration, Ordering::Relaxed);
        let cpu = cpu_us - self.cpu_accepted_us.load(Ordering::Relaxed);
        self.last_request_cpu_us.store(cpu, Ordering::Relaxed);
        self.memory.store(memory, Ordering::Relaxed);
        unlock(&self.lock);
    }
}
