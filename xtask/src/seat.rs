//! A seat (#2982): one of the few places on the machine a building
//! instrument runs in. Five worktrees each building under
//! `CARGO_BUILD_JOBS=3` on twelve cores measured a load of 32 at the
//! median and 100 at the most on 2026-10-18, and every build and test ran
//! slower for it; so `clippy`, `test`, `miri`, `phpt`, `compile-identity`,
//! `crosscheck`, `fpm-compare`, `soak`, `witness` and `bench` each take a
//! seat before they build, and one that finds every seat held waits for
//! one, saying so once. A seat is an exclusive lock on one of `XTASK_SEATS`
//! files (3 unless set; 0 seats no one) in `XTASK_SEAT_DIR` (`/tmp/xtask-seats`
//! unless set), so it is the machine's and not one checkout's, and the
//! operating system gives it back however its holder ends. The holder's
//! children are seated with it: [`crate::batch::without_git`] marks every
//! command an instrument spawns, so a test that runs `xtask` inside the
//! seated `test` takes no second seat and cannot wait on its own parent.
//! A seat that cannot be taken at all, a directory that cannot be made, is
//! no seat and never a failed run. A seat is also a share of the machine:
//! a seated instrument runs [`jobs`] at once, its cores divided among the
//! seats, and hands cargo that share as `CARGO_BUILD_JOBS`, so three seats
//! ask for the machine once and not three times (#3624). A bench on a
//! dedicated machine takes every seat, so nothing else seated builds while
//! it measures, and builds with the whole machine (#3767, #3774).

use std::fs::File;
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Set on a seated instrument's children, which take no seat of their own.
const SEATED: &str = "XTASK_SEATED";

/// The share of the machine cargo builds with, which a seated instrument
/// hands its children.
const JOBS: &str = "CARGO_BUILD_JOBS";

/// How many seats this process holds, so what it spawns is marked and
/// builds with their share.
static HELD: AtomicUsize = AtomicUsize::new(0);

/// How long a waiting instrument sleeps between looks at the seats.
const LOOK: Duration = Duration::from_secs(2);

/// The machine's seats: how many, and the directory their files are in.
#[derive(Debug, Clone)]
pub(crate) struct Seats {
    dir: PathBuf,
    count: usize,
}

/// A seat held until it drops, or every seat; one that holds no file
/// seats no one.
#[derive(Debug)]
#[must_use = "the seat is given back when it drops"]
pub(crate) struct Seat {
    held: Vec<File>,
}

impl Drop for Seat {
    /// The lock is given back by name, not by closing the file: `flock`
    /// is the open file description's, and a child another thread forks
    /// holds a duplicate of the descriptor until its `exec` closes it, so
    /// a close alone leaves the seat held for that window (#3004).
    fn drop(&mut self) {
        for file in self.held.drain(..) {
            file.unlock().ok();
            HELD.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Seats {
    /// The seats the environment names, or none where this process is a
    /// seated instrument's child.
    pub(crate) fn of_env() -> Seats {
        let count = machine_seats();
        let dir = std::env::var_os("XTASK_SEAT_DIR")
            .map_or_else(|| PathBuf::from("/tmp/xtask-seats"), PathBuf::from);
        let seated = std::env::var_os(SEATED).is_some();
        Seats {
            dir,
            count: if seated { 0 } else { count },
        }
    }

    /// One seat for `instrument`, waited for while every one is held.
    pub(crate) fn take(&self, instrument: &str) -> Seat {
        self.wait(instrument, || self.try_take(instrument))
    }

    /// Every seat for `instrument`, each waited for in turn while it is
    /// held, the lowest first, so two takers of every seat never hold one
    /// each and wait on each other.
    pub(crate) fn take_every(&self, instrument: &str) -> Seat {
        let mut held = Vec::new();
        for n in 0..self.count {
            let seat = self.wait(instrument, || {
                Ok(self
                    .try_seat(n, instrument)?
                    .map(|file| Seat { held: vec![file] }))
            });
            if seat.held.is_empty() {
                return seat;
            }
            held.push(seat);
        }
        Seat {
            held: held
                .into_iter()
                .flat_map(|mut seat| std::mem::take(&mut seat.held))
                .collect(),
        }
    }

    /// The seat `look` finds, looked for again while it finds none; a seat
    /// that cannot be taken at all is none, and the instrument runs.
    fn wait(
        &self,
        instrument: &str,
        mut look: impl FnMut() -> std::io::Result<Option<Seat>>,
    ) -> Seat {
        let started = Instant::now();
        let mut said = false;
        loop {
            match look() {
                Ok(Some(seat)) => {
                    if said {
                        let waited = started.elapsed().as_secs();
                        eprintln!("xtask {instrument}: seated after {waited}s");
                    }
                    return seat;
                }
                Ok(None) => {
                    if !said {
                        eprintln!(
                            "xtask {instrument}: waiting for one of {} seats, held by: {}",
                            self.count,
                            self.holders().join("; ")
                        );
                        said = true;
                    }
                    std::thread::sleep(LOOK);
                }
                Err(err) => {
                    eprintln!("xtask {instrument}: running unseated: {err}");
                    return Seat { held: Vec::new() };
                }
            }
        }
    }

    /// A free seat for `instrument`, `None` while every one is held.
    fn try_take(&self, instrument: &str) -> std::io::Result<Option<Seat>> {
        if self.count == 0 {
            return Ok(Some(Seat { held: Vec::new() }));
        }
        for n in 0..self.count {
            if let Some(file) = self.try_seat(n, instrument)? {
                return Ok(Some(Seat { held: vec![file] }));
            }
        }
        Ok(None)
    }

    /// Seat `n` for `instrument`, its file saying who holds it; `None`
    /// while another holds it.
    fn try_seat(&self, n: usize, instrument: &str) -> std::io::Result<Option<File>> {
        std::fs::create_dir_all(&self.dir)?;
        let mut file = File::options()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(n.to_string()))?;
        if file.try_lock().is_err() {
            return Ok(None);
        }
        file.set_len(0)?;
        let cwd = std::env::current_dir().unwrap_or_default();
        write!(
            file,
            "{instrument} pid {} in {}",
            std::process::id(),
            cwd.display()
        )?;
        HELD.fetch_add(1, Ordering::Relaxed);
        Ok(Some(file))
    }

    /// What each held seat's file says of its holder.
    fn holders(&self) -> Vec<String> {
        (0..self.count)
            .filter_map(|n| std::fs::read_to_string(self.dir.join(n.to_string())).ok())
            .filter(|said| !said.is_empty())
            .collect()
    }
}

/// The seat `command` builds in, taken before it runs: every building
/// instrument's but `clippy`'s and `test`'s, which take theirs once they
/// know they have a crate to build; and every seat for a bench on a
/// dedicated machine (#3767).
pub(crate) fn of_command(command: &crate::cli::Command) -> Option<Seat> {
    use crate::cli::Command;
    if let Command::Bench(ask) = command
        && ask.machine == crate::bench::Machine::Dedicated
    {
        return Some(Seats::of_env().take_every("bench"));
    }
    let instrument = match command {
        Command::Miri { .. } => "miri",
        Command::Phpt(_) => "phpt",
        Command::CompileIdentity(_) => "compile-identity",
        Command::Crosscheck { .. } => "crosscheck",
        Command::TraceDump(_) => "trace-dump",
        Command::Hostile => "hostile",
        Command::Fold(_) => "fold",
        Command::FpmCompare { .. } | Command::Soak { .. } => "makala",
        Command::Witness(_) => "witness",
        Command::Bench(_) => "bench",
        _ => return None,
    };
    Some(Seats::of_env().take(instrument))
}

/// How many seats the machine has, `XTASK_SEATS` or three, whether or not
/// this process is a seated instrument's child.
fn machine_seats() -> usize {
    std::env::var("XTASK_SEATS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(3)
}

/// How many jobs a seated instrument runs at once unless told: the share
/// `CARGO_BUILD_JOBS` names, which a seated parent hands its children, and
/// otherwise the machine's cores divided among its seats, times the seats
/// it holds (#3624).
pub(crate) fn jobs() -> NonZeroUsize {
    let cores = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let held = HELD.load(Ordering::Relaxed).max(1);
    share(
        std::env::var(JOBS).ok().as_deref(),
        cores,
        (machine_seats(), held),
    )
}

/// The share of `cores` that `held` of `seats` run, or the one `named`
/// says; zero seats divide nothing, and a share is never less than one job.
fn share(named: Option<&str>, cores: NonZeroUsize, (seats, held): (usize, usize)) -> NonZeroUsize {
    named.and_then(|n| n.parse().ok()).unwrap_or_else(|| {
        let share = cores.get() * held / seats.max(1);
        NonZeroUsize::new(share.min(cores.get())).unwrap_or(NonZeroUsize::MIN)
    })
}

/// `command` marked as a seated instrument's child, where this process
/// holds a seat, and given the seat's share of the machine to build with.
pub(crate) fn lend(command: &mut Command) {
    if HELD.load(Ordering::Relaxed) > 0 {
        command.env(SEATED, "1").env(JOBS, jobs().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seats(name: &str, count: usize) -> (crate::scratch::Tree, Seats) {
        let tree = crate::scratch::Tree::new(name).unwrap();
        let dir = tree.path().join("seats");
        (tree, Seats { dir, count })
    }

    #[test]
    fn a_seat_is_held_until_it_drops_and_then_another_takes_it() {
        let (_tree, seats) = seats("seat-one", 1);
        let first = seats.try_take("test").unwrap().unwrap();
        assert_eq!(first.held.len(), 1);
        assert!(seats.try_take("clippy").unwrap().is_none());
        assert_eq!(seats.holders().len(), 1);
        assert!(seats.holders()[0].starts_with("test pid "));
        // A duplicate of the descriptor, as a child forked by a sibling
        // test holds one until its `exec` (#3004): the seat comes back
        // when it drops all the same.
        let forked = first.held[0].try_clone().unwrap();
        drop(first);
        assert!(seats.try_take("clippy").unwrap().is_some());
        drop(forked);
    }

    #[test]
    fn as_many_take_a_seat_at_once_as_there_are_seats() {
        let (_tree, seats) = seats("seat-two", 2);
        let held = [seats.try_take("a").unwrap(), seats.try_take("b").unwrap()];
        assert!(held.iter().all(Option::is_some));
        assert!(seats.try_take("c").unwrap().is_none());
    }

    /// A bench on a dedicated machine holds every seat, so no other
    /// instrument is seated until it drops (#3767).
    #[test]
    fn every_seat_taken_leaves_none_until_it_drops() {
        let (_tree, seats) = seats("seat-every", 3);
        let every = seats.take_every("bench");
        assert_eq!(every.held.len(), 3);
        assert!(seats.try_take("test").unwrap().is_none());
        assert_eq!(seats.holders().len(), 3);
        assert!(seats.holders().iter().all(|h| h.starts_with("bench pid ")));
        drop(every);
        assert!(seats.try_take("test").unwrap().is_some());
    }

    #[test]
    fn no_seats_seat_everyone_without_a_file() {
        let (_tree, seats) = seats("seat-none", 0);
        let seat = seats.try_take("test").unwrap().unwrap();
        assert!(seat.held.is_empty());
        assert!(seats.take_every("bench").held.is_empty());
        assert!(!seats.dir.exists());
    }

    #[test]
    fn a_seat_runs_its_share_of_the_cores_unless_cargo_is_given_one() {
        let cores = NonZeroUsize::new(12).unwrap();
        let share = |named, seats| share(named, cores, (seats, 1)).get();
        assert_eq!(share(None, 3), 4);
        assert_eq!(share(None, 5), 2);
        assert_eq!(share(None, 24), 1);
        assert_eq!(share(None, 0), 12);
        assert_eq!(share(Some("6"), 3), 6);
        assert_eq!(share(Some("0"), 3), 4);
        assert_eq!(share(Some("many"), 3), 4);
        // Every seat held is the whole machine.
        assert_eq!(super::share(None, cores, (3, 3)).get(), 12);
        assert_eq!(super::share(None, cores, (5, 5)).get(), 12);
    }

    #[test]
    fn a_seat_dir_that_cannot_be_made_runs_unseated() {
        let (tree, mut seats) = seats("seat-file", 1);
        let file = tree.path().join("a-file");
        std::fs::write(&file, "").unwrap();
        seats.dir = file.join("seats");
        assert!(seats.take("test").held.is_empty());
    }
}
