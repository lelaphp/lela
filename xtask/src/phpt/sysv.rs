//! What a subset's tests leave of System V IPC, removed after the subset
//! (#1412). A test that creates a shared memory segment, a semaphore set or
//! a message queue and ends before it removes it, as `shmop`'s private
//! segments do under php itself, leaves the object standing after its
//! process is gone, and the kernel's tables are small: macOS allows 32
//! segments (`kern.sysv.shmmni`) and about 40 queues, so a day of runs
//! fills them and every later `shmop_open`, `shm_attach` or
//! `msg_get_queue` fails with "No space left on device".
//!
//! **Whose an object is.** An object is the run's when the kernel names a
//! process the run spawned on it, and never by its key, so a database or
//! any other program on the machine is never touched: a segment by its
//! creator, a queue by its last sender or receiver, and a semaphore set by
//! the last process to operate on any of its semaphores, the kernel keeping
//! no creator for those two. Beside that it is new since the subset began,
//! so an object a recycled process id created before the run is not the
//! run's; nothing is attached to it; and no process the kernel names on it
//! is alive, so one another program still holds is left to it. The
//! processes are the ones [`Oracle`] spawned, every program's own; a
//! process a test forked is not among them, and what only it touched is
//! left standing.
//!
//! **What is never removed.** An object the kernel names no process on: a
//! queue nobody sent to or read from, a semaphore set nobody operated on,
//! or one whose semaphores this user may not read. Nothing tells the run's
//! from another program's, so it stays, and the run counts it.
//!
//! **How the kernel is read.** `ipcs -a` lists the objects with their
//! processes, and `ipcrm` removes them; neither prints a semaphore's
//! process, so `perl`'s `semctl(GETPID)` reads it. A machine without them
//! reads no object, and the subset's sweep says so rather than failing the
//! run.
//!
//! [`Oracle`]: crate::oracle::Oracle

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::process::Command;

use anyhow::{Context, Result, bail};

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
    /// segment's creator, a queue's last sender and receiver, the last
    /// process to operate on each semaphore of a set.
    pub named: BTreeSet<u32>,
    /// The processes attached to a segment; none for the other kinds.
    pub attached: u64,
}

/// The objects standing on the machine at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Standing(pub Vec<Object>);

impl Standing {
    /// The machine's objects now, read through `ipcs` and, for each
    /// semaphore set's processes, `perl`.
    pub(crate) fn now() -> Result<Standing> {
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
        let mut standing = Standing::parse(&String::from_utf8_lossy(&listed.stdout));
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

    /// What `ipcs -a` printed, read by each section's header line: an
    /// object's processes are a segment's `CPID` and a queue's `LSPID` and
    /// `LRPID`, its attached count a segment's `NATTCH`; a semaphore set
    /// names none here, `ipcs` printing none, until [`Standing::now`]
    /// reads them. A line of no section is not an object.
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
                    let Some(id) = column("ID") else {
                        continue;
                    };
                    let (named, attached): (&[&str], _) = match kind {
                        Kind::Memory => (&["CPID"], column("NATTCH").unwrap_or(0)),
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
                    });
                }
                None => {}
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

/// What one sweep did: the objects it removed by kind, and the new ones it
/// left because the kernel names no process on them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Swept {
    pub removed: BTreeMap<Kind, usize>,
    pub unnamed: usize,
}

impl Swept {
    pub(crate) fn removed(&self) -> usize {
        self.removed.values().sum()
    }
}

impl fmt::Display for Swept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<String> = Kind::ALL
            .iter()
            .filter_map(|kind| {
                let n = *self.removed.get(kind)?;
                Some(format!("{n} {}", kind.noun(n)))
            })
            .collect();
        write!(
            f,
            "removed {} System V object{} its tests left",
            self.removed(),
            crate::reading::plural(self.removed())
        )?;
        if !kinds.is_empty() {
            write!(f, " ({})", kinds.join(", "))?;
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

/// Remove what the processes of `spawned` left since `before`, read again
/// now, and say what was done.
pub(crate) fn sweep(before: &Standing, spawned: &BTreeSet<u32>) -> Result<Swept> {
    let after = Standing::now()?;
    let gone = left(before, &after, spawned, alive);
    let stood: BTreeSet<(Kind, u64)> = before.0.iter().map(|o| (o.kind, o.id)).collect();
    let unnamed = after
        .0
        .iter()
        .filter(|o| o.named.is_empty() && !stood.contains(&(o.kind, o.id)))
        .count();
    let mut swept = Swept {
        unnamed,
        ..Swept::default()
    };
    if gone.is_empty() {
        return Ok(swept);
    }
    let mut ipcrm = Command::new("ipcrm");
    for object in &gone {
        ipcrm.arg(object.kind.flag()).arg(object.id.to_string());
    }
    let removed = ipcrm.output().context("running ipcrm")?;
    if !removed.status.success() {
        bail!(
            "ipcrm exited {}: {}",
            removed.status,
            String::from_utf8_lossy(&removed.stderr).trim()
        );
    }
    for object in gone {
        *swept.removed.entry(object.kind).or_default() += 1;
    }
    Ok(swept)
}

/// Whether a process runs under this id now, as `kill(pid, 0)` answers: one
/// this user may not signal runs too.
fn alive(pid: u32) -> bool {
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
