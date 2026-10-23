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
//! no seat and never a failed run.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Set on a seated instrument's children, which take no seat of their own.
const SEATED: &str = "XTASK_SEATED";

/// Whether this process holds a seat, so what it spawns is marked.
static HELD: AtomicBool = AtomicBool::new(false);

/// How long a waiting instrument sleeps between looks at the seats.
const LOOK: Duration = Duration::from_secs(2);

/// The machine's seats: how many, and the directory their files are in.
#[derive(Debug, Clone)]
pub(crate) struct Seats {
    dir: PathBuf,
    count: usize,
}

/// A seat held until it drops; one that holds no file seats no one.
#[derive(Debug)]
#[must_use = "the seat is given back when it drops"]
pub(crate) struct Seat {
    held: Option<File>,
}

impl Drop for Seat {
    /// The lock is given back by name, not by closing the file: `flock`
    /// is the open file description's, and a child another thread forks
    /// holds a duplicate of the descriptor until its `exec` closes it, so
    /// a close alone leaves the seat held for that window (#3004).
    fn drop(&mut self) {
        if let Some(file) = self.held.take() {
            file.unlock().ok();
            HELD.store(false, Ordering::Relaxed);
        }
    }
}

impl Seats {
    /// The seats the environment names, or none where this process is a
    /// seated instrument's child.
    pub(crate) fn of_env() -> Seats {
        let count = std::env::var("XTASK_SEATS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(3);
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
        let started = Instant::now();
        let mut said = false;
        loop {
            match self.try_take(instrument) {
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
                    return Seat { held: None };
                }
            }
        }
    }

    /// A free seat for `instrument`, `None` while every one is held.
    fn try_take(&self, instrument: &str) -> std::io::Result<Option<Seat>> {
        if self.count == 0 {
            return Ok(Some(Seat { held: None }));
        }
        std::fs::create_dir_all(&self.dir)?;
        for n in 0..self.count {
            let mut file = crate::lock_file(&self.dir.join(n.to_string()))?;
            if file.try_lock().is_err() {
                continue;
            }
            file.set_len(0)?;
            let cwd = std::env::current_dir().unwrap_or_default();
            write!(
                file,
                "{instrument} pid {} in {}",
                std::process::id(),
                cwd.display()
            )?;
            HELD.store(true, Ordering::Relaxed);
            return Ok(Some(Seat { held: Some(file) }));
        }
        Ok(None)
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
/// know they have a crate to build.
pub(crate) fn of_command(command: &crate::cli::Command) -> Option<Seat> {
    use crate::cli::Command;
    let instrument = match command {
        Command::Miri { .. } => "miri",
        Command::Phpt(_) => "phpt",
        Command::CompileIdentity(_) => "compile-identity",
        Command::Crosscheck { .. } => "crosscheck",
        Command::TraceDump(_) => "trace-dump",
        Command::FpmCompare { .. } | Command::Soak { .. } => "makala",
        Command::Witness(_) => "witness",
        Command::Bench(_) => "bench",
        _ => return None,
    };
    Some(Seats::of_env().take(instrument))
}

/// `command` marked as a seated instrument's child, where this process
/// holds a seat.
pub(crate) fn lend(command: &mut Command) {
    if HELD.load(Ordering::Relaxed) {
        command.env(SEATED, "1");
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
        assert!(first.held.is_some());
        assert!(seats.try_take("clippy").unwrap().is_none());
        assert_eq!(seats.holders().len(), 1);
        assert!(seats.holders()[0].starts_with("test pid "));
        // A duplicate of the descriptor, as a child forked by a sibling
        // test holds one until its `exec` (#3004): the seat comes back
        // when it drops all the same.
        let forked = first.held.as_ref().unwrap().try_clone().unwrap();
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

    #[test]
    fn no_seats_seat_everyone_without_a_file() {
        let (_tree, seats) = seats("seat-none", 0);
        let seat = seats.try_take("test").unwrap().unwrap();
        assert!(seat.held.is_none());
        assert!(!seats.dir.exists());
    }

    #[test]
    fn a_seat_dir_that_cannot_be_made_runs_unseated() {
        let (tree, mut seats) = seats("seat-file", 1);
        let file = tree.path().join("a-file");
        std::fs::write(&file, "").unwrap();
        seats.dir = file.join("seats");
        assert!(seats.take("test").held.is_none());
    }
}
