//! What a lane's programs leave of System V IPC, removed after them: a
//! phpt subset's tests (#1412) and a witness run's fences (#2115). A test
//! that creates a shared memory segment, a semaphore set or a message
//! queue and ends before it removes it, as `shmop`'s private
//! segments do under php itself, leaves the object standing after its
//! process is gone, and the kernel's tables are small: macOS allows 32
//! segments (`kern.sysv.shmmni`) and about 40 queues, so a day of runs
//! fills them and every later `shmop_open`, `shm_attach` or
//! `msg_get_queue` fails with "No space left on device".
//!
//! **Whose an object is.** An object is a subset's when the kernel names a
//! process the run spawned on it, and never by its key, so a database or
//! any other program on the machine is never touched: a segment by its
//! creator and the last process to attach or detach it, a queue by its
//! last sender or receiver, and a semaphore set by the last process to
//! operate on any of its semaphores, the kernel keeping no creator for
//! those two. Beside that it is new since the subset began,
//! so an object a recycled process id created before the run is not the
//! run's; nothing is attached to it; and no process the kernel names on it
//! is alive, so one another program still holds is left to it. The
//! processes are the ones [`Oracle`] spawned, every program's own; a
//! process a test forked is not among them, and what only it touched is
//! left standing.
//!
//! **An object that names no process.** A queue nobody sent to or read
//! from, as php's `msg_get_queue(0)` leaves one of mode 0 that nobody can
//! use, a semaphore set nobody operated on, or one whose semaphores this
//! user may not read: the kernel names no process on it. Such an object is
//! the run's, and removed, when all three of these hold, as Mitchell ruled
//! on #1469 (#1482): its creator is the runner's own user; its key is
//! `IPC_PRIVATE`, which no database or other program can look up; and the
//! kernel's whole-second `CTIME` falls inside the run of one of the
//! subset's own programs, the oracle's or the engine's, from spawn to exit
//! rounded out to whole seconds. The only object of another program the
//! rule can take is one of this user's, private, made in the same second
//! as one of the subset's programs ran, which nothing but its maker could
//! ever address. Any other new object that names no process stands, and
//! the run counts it apart from those it removed.
//!
//! **A witness run's.** The witness lane serves most fences in the
//! harness's own process, through the linked engine, and the oracle's
//! beside it, so the processes a fence's objects name are not the lane's
//! to list ahead. A witness run instead holds a [`Claim`]: a file in the
//! user's claims directory, locked for as long as the run lives and
//! written to as each fence starts and ends. An object is the run's when
//! it is private, the runner's, attached to nothing, names no process
//! still alive ([`forsaken`]), and was made inside the claim's window,
//! from the claim's making to its settling. A run killed before it
//! settles leaves its claim with the lock released, however it ended, as
//! a scratch tree's lock is ([`crate::scratch`]); the next witness run
//! takes it before it starts ([`reclaim`]), its window ending a recipe's
//! timeout past the claim's last write, since a fence in flight may run
//! that long after it. The same sweep takes every private segment of the
//! runner's that is forsaken and names a process, a dead creator's that
//! nothing attaches, which a phpt run killed before its sweep leaves too:
//! the segments are the table that fills first (#2115).
//!
//! **On Linux.** The System V subsets run in a private IPC namespace where
//! `unshare` permits, so nothing they make outlives its program, and the
//! sweep there is a check that finds nothing ([`Namespace`]).
//!
//! Where `unshare` is refused, as Docker's default seccomp profile refuses
//! it, they run in the machine's own, and the sweep removes what they left
//! as it does on macOS (#1643).
//!
//! **How the kernel is read.** On Linux, `/proc/sysvipc/shm`, `msg` and
//! `sem` list the objects with their processes, keys, creators' uids and
//! `ctime` as seconds since the epoch; util-linux's `ipcs` prints none of
//! that in the table macOS's does. Elsewhere `ipcs -a` lists them with
//! their processes, keys, creators and `CTIME`, which it prints as the
//! local time of day alone, so the sweep reads the zone's offset from
//! `date +%z` and takes the latest such second not after now, the object
//! being new since the subset began. `ipcrm` removes them on both. Neither
//! names a semaphore's process, so `perl`'s `semctl(GETPID)` reads it. A
//! machine without them reads no object, and the subset's sweep says so
//! rather than failing the run.
//!
//! [`Oracle`]: crate::oracle::Oracle

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

use crate::oracle::{Programs, Ran};

/// One kind of System V object, as `ipcs` letters it and `ipcrm` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Memory,
    Semaphores,
    Queue,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Memory, Kind::Semaphores, Kind::Queue];

    fn of(letter: &str) -> Option<Kind> {
        match letter {
            "m" => Some(Kind::Memory),
            "s" => Some(Kind::Semaphores),
            "q" => Some(Kind::Queue),
            _ => None,
        }
    }

    /// The `ipcrm` flag that removes one by its id.
    fn flag(self) -> &'static str {
        match self {
            Kind::Memory => "-m",
            Kind::Semaphores => "-s",
            Kind::Queue => "-q",
        }
    }

    fn noun(self, n: usize) -> &'static str {
        match (self, n) {
            (Kind::Memory, 1) => "shared memory segment",
            (Kind::Memory, _) => "shared memory segments",
            (Kind::Semaphores, 1) => "semaphore set",
            (Kind::Semaphores, _) => "semaphore sets",
            (Kind::Queue, 1) => "message queue",
            (Kind::Queue, _) => "message queues",
        }
    }
}

/// One object as the kernel describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Object {
    pub kind: Kind,
    pub id: u64,
    /// The processes the kernel names on it, none of them zero: a
    /// segment's creator and the last to attach or detach it, a queue's
    /// last sender and receiver, the last process to operate on each
    /// semaphore of a set.
    pub named: BTreeSet<u32>,
    /// The processes attached to a segment; none for the other kinds.
    pub attached: u64,
    /// The key it was made under; `IPC_PRIVATE` is 0.
    pub key: u64,
    /// The user who made it, as `ipcs` prints the creator: a name, or the
    /// number where the user database has none, which `/proc/sysvipc`
    /// always prints.
    pub creator: String,
    /// When it was made or last set.
    pub ctime: Option<Ctime>,
}

/// When an object was made or last set, as the kernel's listing gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ctime {
    /// The local second of the day, all `ipcs` prints.
    Day(u32),
    /// Seconds since the epoch, as `/proc/sysvipc` prints it.
    Epoch(i64),
}

/// Where Linux lists each kind's objects.
const PROC: &str = "/proc/sysvipc";

/// The objects standing on the machine at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Standing(pub Vec<Object>);

impl Standing {
    /// The machine's objects now, read from `/proc/sysvipc` where the
    /// machine has it and through `ipcs` where it has not, and, for each
    /// semaphore set's processes, through `perl`.
    pub(crate) fn now() -> Result<Standing> {
        let mut standing = Standing::proc(Path::new(PROC))?.map_or_else(Standing::ipcs, Ok)?;
        let sets: Vec<String> = standing
            .0
            .iter()
            .filter(|o| o.kind == Kind::Semaphores)
            .map(|o| o.id.to_string())
            .collect();
        if !sets.is_empty() {
            let pids = semaphore_pids(&sets)?;
            for object in &mut standing.0 {
                if object.kind == Kind::Semaphores {
                    object.named = pids.get(&object.id).cloned().unwrap_or_default();
                }
            }
        }
        Ok(standing)
    }

    /// The machine's objects as `ipcs -a` lists them.
    fn ipcs() -> Result<Standing> {
        let listed = Command::new("ipcs")
            .arg("-a")
            .output()
            .context("running ipcs -a")?;
        if !listed.status.success() {
            bail!(
                "ipcs -a exited {}: {}",
                listed.status,
                String::from_utf8_lossy(&listed.stderr).trim()
            );
        }
        Ok(Standing::parse(&String::from_utf8_lossy(&listed.stdout)))
    }

    /// What `ipcs -a` printed, read by each section's header line: an
    /// object's processes are a segment's `CPID` and `LPID` and a queue's
    /// `LSPID` and `LRPID`, its attached count a segment's `NATTCH`; a semaphore set
    /// names none here, `ipcs` printing none, until [`Standing::now`]
    /// reads them. Every kind's `KEY`, `CREATOR` and `CTIME` are read
    /// alike. A line of no section is not an object.
    pub(crate) fn parse(ipcs: &str) -> Standing {
        let mut header: Vec<&str> = Vec::new();
        let mut objects = Vec::new();
        for line in ipcs.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.first() {
                Some(&"T") => header = fields,
                Some(letter) => {
                    let Some(kind) = Kind::of(letter) else {
                        continue;
                    };
                    let column = |name: &str| {
                        header
                            .iter()
                            .position(|h| *h == name)
                            .and_then(|at| fields.get(at))
                            .and_then(|v| v.parse::<u64>().ok())
                    };
                    let text = |name: &str| {
                        header
                            .iter()
                            .position(|h| *h == name)
                            .and_then(|at| fields.get(at))
                            .copied()
                    };
                    let Some(id) = column("ID") else {
                        continue;
                    };
                    let key = text("KEY")
                        .and_then(|k| u64::from_str_radix(k.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(u64::MAX);
                    let creator = text("CREATOR").unwrap_or_default().to_owned();
                    // `CTIME` is the last column, and a time `ipcs` leaves
                    // unset is the one word `no-entry`, so the field is the
                    // line's last.
                    let ctime = fields.last().and_then(|t| second_of_day(t)).map(Ctime::Day);
                    let (named, attached): (&[&str], _) = match kind {
                        Kind::Memory => (&["CPID", "LPID"], column("NATTCH").unwrap_or(0)),
                        Kind::Queue => (&["LSPID", "LRPID"], 0),
                        Kind::Semaphores => (&[], 0),
                    };
                    let named = named
                        .iter()
                        .filter_map(|name| column(name))
                        .filter(|pid| *pid != 0)
                        .filter_map(|pid| u32::try_from(pid).ok())
                        .collect();
                    objects.push(Object {
                        kind,
                        id,
                        named,
                        attached,
                        key,
                        creator,
                        ctime,
                    });
                }
                None => {}
            }
        }
        Standing(objects)
    }

    /// The objects `dir`, Linux's `/proc/sysvipc`, lists; none where the
    /// machine has no such directory.
    fn proc(dir: &Path) -> Result<Option<Standing>> {
        if !dir.is_dir() {
            return Ok(None);
        }
        let read = |name: &str| {
            let file = dir.join(name);
            std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))
        };
        Ok(Some(Standing::of_proc(
            &read("shm")?,
            &read("msg")?,
            &read("sem")?,
        )))
    }

    /// What `/proc/sysvipc/shm`, `msg` and `sem` print, each read by its
    /// header line as [`Standing::parse`] reads `ipcs`: a segment's `cpid`
    /// and `lpid` and a queue's `lspid` and `lrpid` are its processes, a
    /// segment's `nattch` its attached count; the key is the signed
    /// decimal the kernel prints, the creator the `cuid`, and `ctime`
    /// seconds since the epoch. A semaphore set names no process here either.
    pub(crate) fn of_proc(shm: &str, msg: &str, sem: &str) -> Standing {
        let mut objects = Vec::new();
        for (kind, listed, id, named) in [
            (Kind::Queue, msg, "msqid", &["lspid", "lrpid"][..]),
            (Kind::Memory, shm, "shmid", &["cpid", "lpid"][..]),
            (Kind::Semaphores, sem, "semid", &[][..]),
        ] {
            let mut lines = listed.lines();
            let header: Vec<&str> = lines
                .next()
                .map(|line| line.split_whitespace().collect())
                .unwrap_or_default();
            for line in lines {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let text = |name: &str| {
                    header
                        .iter()
                        .position(|h| *h == name)
                        .and_then(|at| fields.get(at))
                        .copied()
                };
                let column = |name: &str| text(name).and_then(|v| v.parse::<u64>().ok());
                let Some(id) = column(id) else {
                    continue;
                };
                // A `key_t` is an `int`, which the kernel prints signed.
                let key = text("key")
                    .and_then(|k| k.parse::<i32>().ok())
                    .map_or(u64::MAX, |k| u64::from(u32::from_ne_bytes(k.to_ne_bytes())));
                let named = named
                    .iter()
                    .filter_map(|name| column(name))
                    .filter(|pid| *pid != 0)
                    .filter_map(|pid| u32::try_from(pid).ok())
                    .collect();
                objects.push(Object {
                    kind,
                    id,
                    named,
                    attached: if kind == Kind::Memory {
                        column("nattch").unwrap_or(0)
                    } else {
                        0
                    },
                    key,
                    creator: text("cuid").unwrap_or_default().to_owned(),
                    ctime: text("ctime")
                        .and_then(|t| t.parse::<i64>().ok())
                        .map(Ctime::Epoch),
                });
            }
        }
        Standing(objects)
    }
}

/// Each semaphore set's processes, by `perl`'s `semctl(GETPID)` over its
/// semaphores in turn until the kernel refuses the next number; `sets`
/// are the ids. A set this user may not read names none.
fn semaphore_pids(sets: &[String]) -> Result<BTreeMap<u64, BTreeSet<u32>>> {
    const READ: &str = "use IPC::SysV qw(GETPID); for my $id (@ARGV) { for (my $i = 0; defined(my $pid = semctl($id, $i, GETPID, 0)); $i++) { print \"$id $pid\\n\" if $pid + 0 > 0; } }";
    let read = Command::new("perl")
        .arg("-e")
        .arg(READ)
        .args(sets)
        .output()
        .context("running perl for the semaphores' processes")?;
    if !read.status.success() {
        bail!(
            "perl reading the semaphores' processes exited {}: {}",
            read.status,
            String::from_utf8_lossy(&read.stderr).trim()
        );
    }
    let mut pids: BTreeMap<u64, BTreeSet<u32>> = BTreeMap::new();
    for line in String::from_utf8_lossy(&read.stdout).lines() {
        if let Some((id, pid)) = line.split_once(' ')
            && let (Ok(id), Ok(pid)) = (id.parse(), pid.parse())
        {
            pids.entry(id).or_default().insert(pid);
        }
    }
    Ok(pids)
}

/// The objects of `after` the run left: new since `before`, nothing
/// attached, a process of `spawned` named on it, and no process named on
/// it that `alive` says still runs.
pub(crate) fn left<'a>(
    before: &Standing,
    after: &'a Standing,
    spawned: &BTreeSet<u32>,
    alive: impl Fn(u32) -> bool,
) -> Vec<&'a Object> {
    let stood: BTreeSet<(Kind, u64)> = before.0.iter().map(|o| (o.kind, o.id)).collect();
    after
        .0
        .iter()
        .filter(|o| !stood.contains(&(o.kind, o.id)))
        .filter(|o| o.attached == 0)
        .filter(|o| o.named.iter().any(|pid| spawned.contains(pid)))
        .filter(|o| !o.named.iter().any(|pid| alive(*pid)))
        .collect()
}

/// `H:MM:SS` as the second of the day it names; anything else, `ipcs`'s
/// `no-entry` among it, none.
fn second_of_day(time: &str) -> Option<u32> {
    let mut parts = time.split(':').map(str::parse::<u32>);
    let (Some(Ok(h)), Some(Ok(m)), Some(Ok(s)), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    (h < 24 && m < 60 && s < 61).then_some(h * 3600 + m * 60 + s)
}

/// The user the run runs as: who an object without a process must have
/// been made by to be the run's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Runner {
    pub name: String,
    pub uid: u32,
}

impl Runner {
    /// The user this process runs as.
    pub(crate) fn now() -> Runner {
        let uid = nix::unistd::getuid();
        let name = nix::unistd::User::from_uid(uid)
            .ok()
            .flatten()
            .map(|user| user.name)
            .unwrap_or_default();
        Runner {
            name,
            uid: uid.as_raw(),
        }
    }

    /// Whether `ipcs`'s creator is this user, by name or by number.
    fn made(&self, creator: &str) -> bool {
        (!self.name.is_empty() && creator == self.name) || creator == self.uid.to_string()
    }
}

/// The wall clock when the machine was read, and the zone `ipcs` prints
/// its times in, as seconds east of UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Clock {
    pub now: i64,
    pub east: i64,
}

impl Clock {
    /// Now, in the zone `date +%z` names, which is the one `ipcs` prints
    /// in: both read this process's environment.
    pub(crate) fn now() -> Result<Clock> {
        let zone = Command::new("date")
            .arg("+%z")
            .output()
            .context("running date +%z")?;
        let zone = String::from_utf8_lossy(&zone.stdout);
        let east =
            offset(zone.trim()).with_context(|| format!("date +%z printed {:?}", zone.trim()))?;
        Ok(Clock {
            now: seconds(SystemTime::now(), false),
            east,
        })
    }

    /// When `ctime` was, as seconds since the epoch.
    fn at(&self, ctime: Ctime) -> i64 {
        match ctime {
            Ctime::Day(second) => self.when(second),
            Ctime::Epoch(at) => at,
        }
    }

    /// The latest instant, as seconds since the epoch, not after now
    /// whose local time of day is `second`.
    fn when(&self, second: u32) -> i64 {
        let local = self.now + self.east;
        let mut made = local - local.rem_euclid(86_400) + i64::from(second);
        if made > local {
            made -= 86_400;
        }
        made - self.east
    }
}

/// `+HHMM` or `-HHMM` as seconds east of UTC.
fn offset(zone: &str) -> Option<i64> {
    let (sign, digits) = match zone.split_at_checked(1)? {
        ("+", digits) => (1, digits),
        ("-", digits) => (-1, digits),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// `time` as whole seconds since the epoch, rounded down or, with `up`,
/// up.
fn seconds(time: SystemTime, up: bool) -> i64 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let whole = i64::try_from(since.as_secs()).unwrap_or(i64::MAX);
    if up && since.subsec_nanos() > 0 {
        whole + 1
    } else {
        whole
    }
}

/// Whether `object` is `runner`'s, made under `IPC_PRIVATE`, attached to
/// nothing, and names no process `alive` says still runs: one nothing but
/// its id reaches, and that no living process the kernel names holds.
pub(crate) fn forsaken(object: &Object, runner: &Runner, alive: impl Fn(u32) -> bool) -> bool {
    object.key == 0
        && object.attached == 0
        && runner.made(&object.creator)
        && !object.named.iter().any(|pid| alive(*pid))
}

/// Whether `clock` reads `object`'s `CTIME` as a second inside one of
/// `windows`, each from and to in whole seconds since the epoch.
fn made_within(object: &Object, windows: &[(i64, i64)], clock: &Clock) -> bool {
    object.ctime.is_some_and(|ctime| {
        let made = clock.at(ctime);
        windows
            .iter()
            .any(|(from, to)| *from <= made && made <= *to)
    })
}

/// `from` to `to` rounded out to whole seconds since the epoch, a `to` of
/// none lasting until `clock`'s now.
fn window(from: SystemTime, to: Option<SystemTime>, clock: &Clock) -> (i64, i64) {
    (
        seconds(from, false),
        to.map_or(clock.now, |to| seconds(to, true)),
    )
}

/// The objects of `after` that name no process and are the run's by the
/// rule Mitchell ruled on #1469 (#1482): new since `before`, forsaken by
/// `runner`, at a second inside one of `runs`, each rounded out to whole
/// seconds, a run not seen to end lasting until `clock`'s now.
pub(crate) fn unnamed_left<'a>(
    before: &Standing,
    after: &'a Standing,
    runs: &[Ran],
    (runner, clock): (&Runner, &Clock),
) -> Vec<&'a Object> {
    let stood: BTreeSet<(Kind, u64)> = before.0.iter().map(|o| (o.kind, o.id)).collect();
    let windows: Vec<(i64, i64)> = runs
        .iter()
        .map(|ran| window(ran.from, ran.to, clock))
        .collect();
    after
        .0
        .iter()
        .filter(|o| !stood.contains(&(o.kind, o.id)))
        .filter(|o| o.named.is_empty() && forsaken(o, runner, |_| false))
        .filter(|o| made_within(o, &windows, clock))
        .collect()
}

/// The objects of `standing` a witness run's claims over `windows` take
/// (#2115): each forsaken by `runner`, as `alive` reads the processes it
/// names, and made inside one of the windows.
pub(crate) fn claimed<'a>(
    standing: &'a Standing,
    windows: &[(i64, i64)],
    (runner, clock): (&Runner, &Clock),
    alive: impl Fn(u32) -> bool,
) -> Vec<&'a Object> {
    standing
        .0
        .iter()
        .filter(|o| forsaken(o, runner, &alive) && made_within(o, windows, clock))
        .collect()
}

/// The segments of `standing` forsaken by `runner` that name a process:
/// private ones nothing attaches whose creator and last user are dead,
/// whatever run made them (#2115).
pub(crate) fn abandoned<'a>(
    standing: &'a Standing,
    runner: &Runner,
    alive: impl Fn(u32) -> bool,
) -> Vec<&'a Object> {
    standing
        .0
        .iter()
        .filter(|o| o.kind == Kind::Memory && !o.named.is_empty() && forsaken(o, runner, &alive))
        .collect()
}

/// What one sweep did: the objects it removed, each kind's ids in the
/// order removed, how many of those named no process, and the new ones it
/// left because the kernel names no process on them and the rule for
/// those does not take them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Swept {
    pub removed: BTreeMap<Kind, Vec<u64>>,
    pub unnamed_removed: usize,
    pub unnamed: usize,
}

impl Swept {
    pub(crate) fn removed(&self) -> usize {
        self.removed.values().map(Vec::len).sum()
    }
}

impl fmt::Display for Swept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<String> = Kind::ALL
            .iter()
            .filter_map(|kind| {
                let ids = self.removed.get(kind)?;
                let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
                Some(format!("{} {}", kind.noun(ids.len()), ids.join(" ")))
            })
            .collect();
        write!(
            f,
            "removed {} System V object{}",
            self.removed(),
            crate::reading::plural(self.removed())
        )?;
        if !kinds.is_empty() {
            write!(f, " ({})", kinds.join(", "))?;
        }
        if self.unnamed_removed > 0 {
            write!(
                f,
                ", {} of them naming no process, the runner's, private and made while one of the run's programs ran",
                self.unnamed_removed
            )?;
        }
        if self.unnamed > 0 {
            write!(
                f,
                "; {} new one{} name{} no process and stand{}, since nothing tells the run's from another program's",
                self.unnamed,
                crate::reading::plural(self.unnamed),
                if self.unnamed == 1 { "s" } else { "" },
                if self.unnamed == 1 { "s" } else { "" },
            )?;
        }
        Ok(())
    }
}

/// Remove what the programs of `spawned` left since `before`, read again
/// now, and say what was done.
pub(crate) fn sweep(before: &Standing, spawned: &Programs) -> Result<Swept> {
    let clock = Clock::now()?;
    let after = Standing::now()?;
    let mut gone = left(before, &after, &spawned.pids, alive);
    let unnamed_gone = unnamed_left(before, &after, &spawned.runs, (&Runner::now(), &clock));
    let stood: BTreeSet<(Kind, u64)> = before.0.iter().map(|o| (o.kind, o.id)).collect();
    let unnamed = after
        .0
        .iter()
        .filter(|o| o.named.is_empty() && !stood.contains(&(o.kind, o.id)))
        .count();
    let mut swept = Swept {
        unnamed: unnamed - unnamed_gone.len(),
        ..Swept::default()
    };
    gone.extend(unnamed_gone);
    remove(gone, &mut swept)?;
    Ok(swept)
}

/// Each of `objects` removed by an `ipcrm` of its own and counted into
/// `swept`, so one that another run's sweep took in the moment between is
/// passed over and not the whole sweep refused (#1582).
fn remove(objects: Vec<&Object>, swept: &mut Swept) -> Result<()> {
    let mut refused = Vec::new();
    for object in objects {
        let removed = Command::new("ipcrm")
            .arg(object.kind.flag())
            .arg(object.id.to_string())
            .output()
            .context("running ipcrm")?;
        if removed.status.success() {
            swept
                .removed
                .entry(object.kind)
                .or_default()
                .push(object.id);
            swept.unnamed_removed += usize::from(object.named.is_empty());
        } else {
            refused.push((object, removed));
        }
    }
    if refused.is_empty() {
        return Ok(());
    }
    // An object `ipcrm` refused that is no longer standing was removed in
    // the moment between: by another run's sweep on the machine, which the
    // rule for an object naming no process lets take one of this user's
    // private objects of the same second (#1582). It is that run's to
    // count, and this sweep goes on; one still standing is a refusal.
    let now = Standing::now()?;
    for (object, removed) in refused {
        if now
            .0
            .iter()
            .any(|o| o.kind == object.kind && o.id == object.id)
        {
            bail!(
                "ipcrm {} {} exited {}: {}",
                object.kind.flag(),
                object.id,
                removed.status,
                String::from_utf8_lossy(&removed.stderr).trim()
            );
        }
    }
    Ok(())
}

/// Where the user's claims stand: one directory under the temporary one,
/// read by every run that shares it.
fn claims() -> PathBuf {
    std::env::temp_dir().join("xtask-sysv-claims")
}

/// What a claim is named while its lock is being taken, a name
/// [`reclaim`] passes over, as a scratch tree's is ([`crate::scratch`]).
const TAKING: &str = "taking";

/// A witness run's claim on the System V objects made while it runs
/// (#2115): a file of the claims directory, locked for as long as this
/// value lives, which the run writes to as it goes. Settled, what the run
/// left is removed and the file goes; a run that ends without settling
/// leaves the file with its lock released, for the next run's
/// [`reclaim`].
#[derive(Debug)]
pub(crate) struct Claim {
    path: PathBuf,
    _lock: File,
    from: SystemTime,
}

impl Claim {
    /// A claim of `lane`'s, from now.
    pub(crate) fn new(lane: &str) -> Result<Claim> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = claims();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!(
            "{lane}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let taking = path.with_extension(TAKING);
        let from = SystemTime::now();
        let lock = File::create(&taking)
            .with_context(|| format!("creating the claim {}", taking.display()))?;
        lock.try_lock()
            .map_err(|_| anyhow!("the claim {} is held by another run", taking.display()))?;
        fs::rename(&taking, &path)
            .with_context(|| format!("naming the claim {}", path.display()))?;
        Ok(Claim {
            path,
            _lock: lock,
            from,
        })
    }

    /// The claim's file, which the run writes to as it goes.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// What the run left removed: every object forsaken by the runner and
    /// made from the claim's making until now. The claim goes with it.
    pub(crate) fn settle(self) -> Result<Swept> {
        let clock = Clock::now()?;
        let standing = Standing::now()?;
        let windows = [window(self.from, None, &clock)];
        let mut swept = Swept::default();
        remove(
            claimed(&standing, &windows, (&Runner::now(), &clock), alive),
            &mut swept,
        )?;
        fs::remove_file(&self.path).ok();
        Ok(swept)
    }
}

/// What runs that ended without settling their claims left, removed
/// before a run starts (#2115): each such claim's window from its making
/// to `grace` past its last write, and every segment [`abandoned`]. The
/// claims go with it; a claim whose lock will not come is a run still
/// going, and is passed over.
pub(crate) fn reclaim(grace: Duration) -> Result<Swept> {
    let mut ended = Vec::new();
    for entry in fs::read_dir(claims()).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == TAKING) {
            continue;
        }
        let Ok(file) = File::options().write(true).open(&path) else {
            continue;
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if file.try_lock().is_err() {
            continue;
        }
        let Ok(last) = meta.modified() else {
            continue;
        };
        ended.push((path, meta.created().unwrap_or(last), last + grace, file));
    }
    let clock = Clock::now()?;
    let standing = Standing::now()?;
    let runner = Runner::now();
    let windows: Vec<(i64, i64)> = ended
        .iter()
        .map(|(_, from, to, _)| window(*from, Some(*to), &clock))
        .collect();
    let mut gone = claimed(&standing, &windows, (&runner, &clock), alive);
    gone.extend(abandoned(&standing, &runner, alive));
    let mut seen = BTreeSet::new();
    gone.retain(|o| seen.insert((o.kind, o.id)));
    let mut swept = Swept::default();
    remove(gone, &mut swept)?;
    for (path, ..) in ended {
        fs::remove_file(path).ok();
    }
    Ok(swept)
}

/// The subsets whose tests make System V objects, the four extensions of
/// System V IPC.
pub(crate) const SUBSETS: [&str; 4] = ["ext/shmop", "ext/sysvmsg", "ext/sysvsem", "ext/sysvshm"];

/// The private IPC namespace a System V subset's programs run in on Linux
/// where `unshare` permits (#1482): each program in one of its own, made
/// by `unshare` around the binary, so whatever it makes goes with it and
/// what a test forks is in it too. Tried first as it is, which a root
/// user or a container may, then in a user namespace of its own mapped to
/// the runner's own user, which an unprivileged user may where the kernel
/// allows it; the runner's user, and not root, since php's tests read
/// who runs them. None elsewhere, and none where neither is permitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Namespace(pub Vec<String>);

impl Namespace {
    /// The namespace `scope` runs in, where it is a System V subset and
    /// this machine has one to give.
    pub(crate) fn of(scope: &str) -> Option<Namespace> {
        if !SUBSETS.contains(&scope) {
            return None;
        }
        Namespace::here()
    }

    #[cfg(target_os = "linux")]
    fn here() -> Option<Namespace> {
        static HERE: std::sync::OnceLock<Option<Namespace>> = std::sync::OnceLock::new();
        HERE.get_or_init(|| {
            let unshare = std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join("unshare"))
                    .find(|candidate| candidate.is_file())
            })?;
            Namespace::chosen(&unshare.to_string_lossy(), Namespace::permitted)
        })
        .clone()
    }

    #[cfg(not(target_os = "linux"))]
    fn here() -> Option<Namespace> {
        None
    }

    /// The first of the ways `unshare` at `path` may make one that
    /// `permitted` lets run, in the order [`Namespace::tries`] lists them;
    /// none where it lets neither.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn chosen(path: &str, permitted: impl Fn(&Namespace) -> bool) -> Option<Namespace> {
        Namespace::tries(path).into_iter().find(|ns| permitted(ns))
    }

    /// The two ways `unshare` at `path` may make one, in the order tried.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn tries(path: &str) -> [Namespace; 2] {
        let words = |flags: &[&str]| {
            let mut words = vec![path.to_owned()];
            words.extend(flags.iter().map(|flag| (*flag).to_owned()));
            words.push("--".to_owned());
            Namespace(words)
        };
        [
            words(&["--ipc"]),
            words(&["--user", "--map-current-user", "--ipc"]),
        ]
    }

    /// Whether `true` runs under it.
    #[cfg(target_os = "linux")]
    fn permitted(&self) -> bool {
        let Some((program, args)) = self.0.split_first() else {
            return false;
        };
        Command::new(program)
            .args(args)
            .arg("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let flags: Vec<&str> = self
            .0
            .iter()
            .skip(1)
            .map(String::as_str)
            .filter(|word| *word != "--")
            .collect();
        write!(
            f,
            "each program runs in a private IPC namespace (unshare {})",
            flags.join(" ")
        )
    }
}

/// Whether a process runs under this id now, as `kill(pid, 0)` answers: one
/// this user may not signal runs too.
pub(crate) fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(errno) => errno != nix::errno::Errno::ESRCH,
    }
}

#[cfg(test)]
mod tests;
