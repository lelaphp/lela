//! `fpm_scoreboard.c`: each pool's scoreboard in memory the master maps
//! shared before it forks, a header of the pool's counts and a slot per
//! child it may run, which the children write their request's stage and
//! details into and the master and the status page read. Every field is
//! an atomic, and the locks are php's: the header's reader count and
//! writer flag, each changed under a short guard, and a spinning flag per
//! slot. A process can be killed holding any of them, so the master never
//! waits on one without end: it frees a dead child's slot without the
//! lock, as php zeroes it, reads a slot by trying its lock once, and
//! waits a bounded while on the header's, as php does (#2777).

use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::conf::Pm;
use crate::zlog;

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
    /// The guard the two below change under.
    lock: AtomicU32,
    /// A writer between `fpm_scoreboard_update_begin` and `_commit`.
    writer_active: AtomicU32,
    /// The readers between `fpm_scoreboard_acquire` and `_release`.
    reader_count: AtomicU32,
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

/// How a spinning flag is taken: `fpm_spinlock` trying once or looping
/// until it holds, and `fpm_spinlock_with_max_retries`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    Once,
    Hang,
    Retries,
}

/// A spinning flag taken as `wait` says: whether it is held.
fn lock(flag: &AtomicU32, wait: Wait) -> bool {
    let mut retries = 0;
    loop {
        if flag
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
        match wait {
            Wait::Once => return false,
            Wait::Hang => {}
            Wait::Retries => {
                retries += 1;
                if retries > MAX_RETRIES {
                    return false;
                }
            }
        }
        std::thread::yield_now();
    }
}

fn unlock(flag: &AtomicU32) {
    flag.store(0, Ordering::Release);
}

impl Board {
    /// The guard over the reader count and the writer flag. php tries it
    /// once and goes on whether it took it or not; here it is waited for
    /// a bounded while and gone on past the same way, so a process killed
    /// holding it costs the next one a wait and never the master its loop,
    /// and the unlock after lets it go for good.
    fn guard(&self) {
        lock(&self.lock, Wait::Retries);
    }

    /// `fpm_scoreboard_readers_decrement`.
    fn readers_decrement(&self) {
        self.guard();
        let readers = self.reader_count.load(Ordering::Relaxed);
        self.reader_count
            .store(readers.saturating_sub(1), Ordering::Relaxed);
        unlock(&self.lock);
    }

    /// `fpm_scoreboard_update_begin`: the writer flag held once no reader
    /// is counted, a reader that stays counted past the retries taken for
    /// one that crashed, and a writer that holds the flag past them for
    /// one that did, whose flag this writer then goes on under.
    fn update_begin(&self) {
        let mut retries = 0;
        loop {
            self.guard();
            if self.reader_count.load(Ordering::Relaxed) == 0 {
                if !lock(&self.writer_active, Wait::Retries) {
                    zlog!(
                        Warning @ fpm_scoreboard_update_begin:118,
                        "scoreboard: writer {} waited too long for another writer to release lock.",
                        std::process::id()
                    );
                }
                unlock(&self.lock);
                return;
            }
            unlock(&self.lock);
            retries += 1;
            if retries > MAX_RETRIES {
                self.readers_decrement();
                zlog!(
                    Warning @ fpm_scoreboard_update_begin:133,
                    "scoreboard: writer detected a potential crashed reader, decrementing reader count."
                );
                retries = 0;
            }
            std::thread::yield_now();
        }
    }

    /// `fpm_scoreboard_acquire` with `FPM_SCOREBOARD_LOCK_NOHANG`: this
    /// reader counted once no writer is active, and none past the retries.
    /// php then counts a reader down that it never counted; this counts
    /// down nothing, since what it waited for was a writer.
    fn acquire(&self) -> bool {
        let mut retries = 0;
        loop {
            self.guard();
            if self.writer_active.load(Ordering::Relaxed) == 0 {
                self.reader_count.fetch_add(1, Ordering::Relaxed);
                unlock(&self.lock);
                return true;
            }
            unlock(&self.lock);
            std::thread::yield_now();
            retries += 1;
            if retries > MAX_RETRIES {
                zlog!(
                    Warning @ fpm_scoreboard_acquire:310,
                    "scoreboard: reader waited too long for writer to release lock."
                );
                return false;
            }
        }
    }
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
        b.update_begin();
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
        unlock(&b.writer_active);
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

    /// `fpm_scoreboard_copy`: the header read as a counted reader and,
    /// where `procs` says, every slot, each read whole under its lock;
    /// none where a writer stays active past the retries.
    pub(crate) fn copy(&self, procs: bool) -> Option<BoardView> {
        if !self.board.acquire() {
            zlog!(Error @ fpm_scoreboard_copy:359, "scoreboard: failed to lock (already locked)");
            return None;
        }
        let mut view = self.header();
        if procs {
            view.procs = self.procs.iter().map(Proc::view).collect();
        }
        self.board.readers_decrement();
        Some(view)
    }

    /// The header as it stands, read as php's access log reads
    /// `fpm_scoreboard_get()`, under no lock.
    pub(crate) fn header(&self) -> BoardView {
        let b = self.board;
        BoardView {
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
        }
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
    /// Zeroed whole, its lock with it, as `fpm_scoreboard_proc_free`'s
    /// `memset` zeroes it: the master frees the slot of a child it has
    /// buried, which may have died holding the lock, so it never waits for
    /// it (#2777).
    fn clear(&self) {
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

    /// The slot read whole, under its lock, waited for as a child waits
    /// for its own (`FPM_SCOREBOARD_LOCK_HANG`).
    pub(crate) fn view(&self) -> ProcView {
        lock(&self.lock, Wait::Hang);
        self.read()
    }

    /// The slot read whole, its lock tried once, as the master tries a
    /// child's (`FPM_SCOREBOARD_LOCK_NOHANG`): none where it is held.
    pub(crate) fn view_once(&self) -> Option<ProcView> {
        lock(&self.lock, Wait::Once).then(|| self.read())
    }

    /// The slot's fields, its lock held, and the lock let go.
    fn read(&self) -> ProcView {
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
        lock(&self.lock, Wait::Hang);
        self.stage.store(stage as u8, Ordering::Relaxed);
        self.tv_us.store(now_us, Ordering::Relaxed);
        unlock(&self.lock);
    }

    /// `fpm_request_reading_headers`'s slot: the stage, the time and CPU
    /// the request was accepted at, one more request, every detail of
    /// the last one cleared.
    pub(crate) fn reading_headers(&self, now_us: i64, cpu_us: i64) {
        lock(&self.lock, Wait::Hang);
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
        lock(&self.lock, Wait::Hang);
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
        lock(&self.lock, Wait::Hang);
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "a test holds the board it mapped and its slots to exist, and waits for a lock on a thread of its own so one waited for without end fails the test rather than hanging it (seams.toml rule fork.threads)"
)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use super::{Action, Scoreboard, Update};
    use crate::conf::Pm;

    /// `f` on a thread of its own, its answer within ten seconds, or the
    /// test failed: a lock waited for without end spins past them.
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || sent.send(f()).ok());
        received
            .recv_timeout(Duration::from_secs(10))
            .expect("a lock a dead process held was waited for without end")
    }

    fn board() -> Scoreboard {
        Scoreboard::new(b"www", Pm::Dynamic, 2).expect("a scoreboard mapped")
    }

    /// A child killed while it held its slot's lock, as `SIGTERM` finds one
    /// writing its request's stage (#2777): the master buries it and frees
    /// the slot without the lock, as `fpm_scoreboard_proc_free` zeroes it
    /// whole, lock and all, so the slot serves the next child.
    #[test]
    fn a_slot_whose_dead_child_held_its_lock_is_freed() {
        let board = board();
        let at = board.alloc().expect("a free slot");
        board.child_use(at, 4242);
        let proc = board.proc(at).expect("the slot");
        proc.lock.store(1, Ordering::Relaxed);
        within(move || board.free(at));
        assert_eq!(proc.lock.load(Ordering::Relaxed), 0);
        assert_eq!(proc.view_once().map(|view| view.pid), Some(0));
    }

    /// The master reads a slot whose lock is held once, as
    /// `fpm_request_check_timed_out` tries it, and answers none.
    #[test]
    fn a_held_slot_is_read_once_and_answers_none() {
        let board = board();
        let proc = board.proc(0).expect("the first slot");
        proc.lock.store(1, Ordering::Relaxed);
        assert!(within(move || proc.view_once().is_none()));
    }

    /// A writer that died between its update's begin and its commit leaves
    /// `writer_active` set: the next writer waits a bounded while, warns,
    /// and writes, and a reader gives up and answers no copy, as php's
    /// `fpm_scoreboard_update_begin` and `fpm_scoreboard_acquire` do.
    #[test]
    fn a_dead_writer_neither_stops_the_next_writer_nor_hangs_a_reader() {
        let board = board();
        board.board.writer_active.store(1, Ordering::Relaxed);
        let idle = within(move || {
            board.update(
                Update {
                    idle: 3,
                    ..Update::NONE
                },
                Action::Set,
            );
            board.board.idle.load(Ordering::Relaxed)
        });
        assert_eq!(idle, 3);
        let board = self::board();
        board.board.writer_active.store(1, Ordering::Relaxed);
        assert!(within(move || board.copy(false).is_none()));
    }

    /// A reader that died holding its count leaves it raised: a writer
    /// takes it for a crashed reader after its retries, as php's does,
    /// counts it down and writes.
    #[test]
    fn a_dead_reader_does_not_stop_a_writer() {
        let board = board();
        board.board.reader_count.store(1, Ordering::Relaxed);
        let (idle, readers) = within(move || {
            board.update(
                Update {
                    idle: 2,
                    ..Update::NONE
                },
                Action::Set,
            );
            (
                board.board.idle.load(Ordering::Relaxed),
                board.board.reader_count.load(Ordering::Relaxed),
            )
        });
        assert_eq!((idle, readers), (2, 0));
    }

    /// A copy counts itself in as a reader and out again.
    #[test]
    fn a_copy_leaves_no_reader_counted() {
        let board = board();
        let copy = board.copy(true).expect("a copy with no writer active");
        assert_eq!(copy.pool, b"www");
        assert_eq!(copy.procs.len(), 2);
        assert_eq!(board.board.reader_count.load(Ordering::Relaxed), 0);
    }
}
