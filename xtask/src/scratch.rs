//! A scratch tree that owns itself (#194): one directory under the
//! system's temporary directory, or under `/tmp` for a program the oracle
//! runs and the phpt run's copy of the pinned tree ([`Tree::short`]),
//! made by the constructor and taken away again when the value goes out
//! of scope, however it goes out of scope.
//!
//! The harness makes scratch trees in two places that cannot see each
//! other — the unit tests beside a module in `xtask/src/` and the
//! integration tests in `xtask/tests/`, which reach only what the library
//! publishes — so this is a `pub` module of `xtask` with its `xtask.lock`
//! lines, the harness holding its own interface rather than keeping two
//! copies of one idea. The instruments that write a tree while they run,
//! `oracle` and `compile-identity`, hold the same value.
//!
//! What the type is for is the failing run. A bare `PathBuf` owns nothing,
//! so a tree removed by a line at the bottom of a test body survives every
//! test that panics or returns early — which is the run whose leavings are
//! least wanted. Dropping is the one moment that happens either way.
//!
//! Except the one it is not. A process that is killed drops nothing: no
//! unwinding, no destructor, and the tree it was working in stands until
//! something else takes it away. A run interrupted at the terminal or cut
//! off by whatever started it leaves its whole tree behind, and a tree here
//! is a repository with a build in it, so the leavings are measured in
//! gigabytes and they accumulate for as long as nobody looks.
//!
//! So the tree does not own itself alone. Beside each one is a lock file,
//! opened and locked by the constructor and held for as long as the value
//! lives, and the first tree a process makes sweeps the temporary directory
//! before it makes it. A lock the sweep can take is a lock whose holder is
//! gone — the operating system releases it however the process ended, which
//! is the property `Drop` does not have — so the tree beside it is nobody's
//! and goes. A lock that will not come is a run still going, and its tree is
//! left where it is. Two mechanisms because there are two endings: dropping
//! is what makes a tree go promptly, the lock is what makes it go at all.
//!
//! To keep a tree and look at it, set [`KEEP`] to anything non-empty; the
//! constructor names each path it makes and nothing removes it afterwards.
//! A kept tree is made with no lock beside it, so the sweep does not look at
//! it either — being kept outlives the process that asked for it, which is
//! the whole point of asking.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};

use crate::git::git;
use crate::reading::{Reading, Status};

/// Set to anything non-empty and a scratch tree is made, named on stderr
/// and left standing: what a session debugging a failure sets, so the tree
/// the failure ran in is still there to read afterwards.
pub const KEEP: &str = "XTASK_KEEP_SCRATCH";

/// What every scratch path begins with, the tree and its lock alike: what
/// the sweep reads the temporary directory for, and what tells a tree of
/// this harness's from everything else living there.
const PREFIX: &str = "xtask-";

/// Where [`Tree::short`] lays a tree, whatever `TMPDIR` says: one
/// directory every session's runs share.
const SHORT: &str = "/tmp";

/// What a tree's lock is named: the tree's own name and this, so the two
/// are siblings and neither is inside the other. The lock is not in the
/// tree because the tree is handed to callers that lay a repository down
/// in it and then read back everything standing there; a file of ours
/// among theirs would be a file they have to know about.
const LOCK: &str = ".lock";

/// What a lock is named while it is being taken, a name the sweep does
/// not read: see [`hold`].
const TAKING: &str = ".lock-taking";

/// One scratch directory, removed when this value is dropped.
///
/// [`Tree::new`] takes a name for the caller — the test, or the lane — and
/// the path it makes is that name between `xtask-` and two numbers that
/// make it this one's alone, each at a fixed width ([`unique`]): the
/// process, and a counter of the trees this process has made. Two tests
/// naming themselves the same thing at the same time get two trees.
///
/// Deref, `AsRef<Path>` and `AsRef<OsStr>` are all here, so a call site
/// reads the way it read when the value was a `PathBuf`:
/// `tree.join("Cargo.toml")`, `fs::create_dir_all(&tree)`,
/// `command.arg(&tree)`, `tree.display()`.
pub struct Tree {
    path: PathBuf,
    /// The lock beside the tree: where it is, and the open file whose
    /// exclusive lock the operating system releases however this process
    /// ends. `None` under [`KEEP`], where there is nothing to release and
    /// nothing to sweep — a tree with no lock beside it is one the sweep
    /// never considers, which is what keeps a kept tree kept.
    ///
    /// This is also what says whether the tree is ours to remove: a value
    /// holding a lock removes both on drop, a value holding none removes
    /// nothing.
    held: Option<(PathBuf, File)>,
}

impl Tree {
    /// An empty directory named for `name`, made fresh: anything standing
    /// at the path is taken away first, so a tree an earlier crash left is
    /// cleared rather than added to.
    ///
    /// The path is canonical. A temporary directory is a symbolic link on
    /// macOS, and a repository laid down under one is printed by git and
    /// by php through the link it was resolved through, so a caller that
    /// compares paths would compare two spellings of one directory.
    pub fn new(name: &str) -> Result<Tree> {
        Tree::under(&std::env::temp_dir(), &unique(name))
    }

    /// The tree for `name` under `/tmp` and not the temporary directory,
    /// at a path of one length whatever process makes it, however many
    /// trees that process made before it and whatever `TMPDIR` the
    /// session has: where a program the oracle runs stands. php reads the
    /// path back, a unix socket's into its 104 bytes and a program file's into
    /// `$_SERVER`, whose strings a witness's memory count reads (#1730,
    /// #2523): under the temporary directory, the count moved with the
    /// session's `TMPDIR`.
    pub(crate) fn short(name: &str) -> Result<Tree> {
        Tree::under(Path::new(SHORT), &unique(name))
    }

    /// The tree `xtask-<name>` under `base`, swept as the temporary
    /// directory is.
    fn under(base: &Path, name: &str) -> Result<Tree> {
        static SWEPT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

        // Before this process's first tree under `base`, and so before
        // there is one of ours there to mistake for a dead run's.
        {
            let mut swept = SWEPT.lock().unwrap_or_else(PoisonError::into_inner);
            if !swept.iter().any(|done| done == base) {
                reap(base);
                swept.push(base.to_path_buf());
            }
        }

        let named = base.join(format!("{PREFIX}{name}"));
        let keep = std::env::var_os(KEEP).is_some_and(|v| !v.is_empty());

        // The lock is taken before the directory exists, so a sweep in
        // another process never meets a tree whose lock is not yet held
        // and reads it as a dead run's.
        let held = if keep { None } else { Some(hold(&named)?) };

        std::fs::remove_dir_all(&named).ok();
        std::fs::create_dir_all(&named)
            .map_err(|err| made(err, &named, "creating the scratch tree"))?;
        let path = std::fs::canonicalize(&named)
            .with_context(|| format!("resolving the scratch tree {}", named.display()))?;
        if keep {
            eprintln!("{KEEP}: keeping {}", path.display());
        }
        Ok(Tree { path, held })
    }

    /// The directory itself, where a caller needs the `&Path` by name.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `contents` written to `name` in this tree, and the path it stands
    /// at: a [`Full`] where the volume had no room for it ([`made`]).
    pub(crate) fn write(
        &self,
        name: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> Result<PathBuf> {
        let path = self.path.join(name);
        std::fs::write(&path, contents).map_err(|err| made(err, &path, "writing"))?;
        Ok(path)
    }
}

/// `name` made this tree's alone, at one length: the process id at the
/// widest any platform numbers one (Linux's `pid_max` reaches 4194304),
/// and a counter of the trees this process has made at a width no run
/// reaches. A number written at its own width moved the path by a byte
/// whenever the pid or the counter crossed a digit, and with it every
/// number php reads off the path (#2523).
fn unique(name: &str) -> String {
    const _: () = assert!(NUMBERED == "-0000000-000000".len());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let next = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{name}-{:07}-{next:06}", std::process::id())
}

/// How long the numbers [`unique`] puts after a tree's name are.
const NUMBERED: usize = 15;

/// A scratch tree, its lock, or a file a lane writes into one, that could
/// not be made for want of space (#644). The volume it would stand on is
/// full, so the run it was for was not measured: a lane reads this off the
/// error ([`Full::of`]) and says so as a skip naming the volume, where it
/// would otherwise be a witness that borked or a subset refused, a red line
/// a session reads as its own diff.
#[derive(Debug)]
pub(crate) struct Full {
    /// What could not be made.
    pub(crate) path: PathBuf,
    /// The mount point of the volume `path` stands on, as `df` names it,
    /// or the nearest directory of `path` that stands where `df` says
    /// nothing.
    pub(crate) volume: PathBuf,
}

impl Full {
    /// The number [`Full::skipped`] counts, one, on the reading of a run
    /// this kept from being measured.
    pub(crate) const NUMBER: &str = "full";

    /// The `Full` somewhere in `err`, under whatever context was added.
    pub(crate) fn of(err: &anyhow::Error) -> Option<&Full> {
        err.downcast_ref::<Full>()
    }

    /// `reading`, of the run of `name` that `why` ended, made the skip
    /// that says the volume was full and names it: no witness borked and
    /// no subset was refused, the run was not measured.
    pub(crate) fn skipped(
        mut reading: Reading,
        name: &str,
        why: &anyhow::Error,
    ) -> Result<Reading> {
        reading.status = Status::Skip;
        reading.message = format!("{name}: not measured for want of space: {why:#}");
        reading.count(Full::NUMBER, 1)?;
        Ok(reading)
    }
}

impl std::fmt::Display for Full {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no space left on {} for {}",
            self.volume.display(),
            self.path.display()
        )
    }
}

impl std::error::Error for Full {}

/// `err`, met making `path` in a scratch tree, under the context `<doing>
/// <path>`: a [`Full`] where the volume had no room for it, so a lane can
/// tell the disk from its own trouble, and the error as it came otherwise.
pub(crate) fn made(err: std::io::Error, path: &Path, doing: &str) -> anyhow::Error {
    use std::io::ErrorKind::{QuotaExceeded, StorageFull};
    let context = format!("{doing} {}", path.display());
    if matches!(err.kind(), StorageFull | QuotaExceeded) {
        let volume = volume(path);
        let path = path.to_path_buf();
        anyhow::Error::new(Full { path, volume }).context(context)
    } else {
        anyhow::Error::new(err).context(context)
    }
}

/// The mount point of the volume `path` stands on: `df -P`'s last column
/// for the nearest directory of `path` that stands, which reading needs no
/// room on the volume; that directory itself where `df` says nothing.
fn volume(path: &Path) -> PathBuf {
    let standing = path
        .ancestors()
        .find(|dir| dir.is_dir())
        .unwrap_or(Path::new("/"));
    std::process::Command::new("df")
        .arg("-P")
        .arg(standing)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let line = text.lines().last()?.to_owned();
            let mounted = line
                .split_whitespace()
                .skip(5)
                .collect::<Vec<_>>()
                .join(" ");
            (!mounted.is_empty()).then(|| PathBuf::from(mounted))
        })
        .unwrap_or_else(|| standing.to_path_buf())
}

/// `cargo` stood in at `dir/cargo`, and the `PATH` that finds it first.
///
/// `worktree add` ends in `cargo build -p xtask` in the checkout it lays
/// down, and a scratch clone has no `target/` for that build to start
/// from: every test through it compiled the harness and all its
/// dependencies cold, two minutes apiece on CI's runner and most of the
/// harness's test time.
/// What those tests hold is the command around the build, so the
/// stand-in answers `build -p xtask` by laying down `target/debug/xtask`
/// in its working directory as a shell file that runs `xtask`, the binary
/// under test, rooted at that directory: what a real build there would
/// answer for, in the time a file takes to write. `cargo xtask …`, which
/// `scripts/worktree-add.sh` and `worktree-retire.sh` fall back to in a
/// clone with no build and always call from its root, runs that binary
/// rooted there in the same way. Any other `cargo` call goes to the
/// `cargo` this process was started by. The one test that holds the
/// build itself, the seeded worktree's, runs without it.
pub fn cargo(dir: &Path, xtask: &Path) -> Result<std::ffi::OsString> {
    use std::os::unix::fs::PermissionsExt;

    let cargo = std::env::var_os("CARGO").context("no CARGO to hand other calls on to")?;
    let (xtask, cargo) = (
        xtask.display().to_string(),
        Path::new(&cargo).display().to_string(),
    );
    if xtask.contains('\'') || cargo.contains('\'') {
        anyhow::bail!("a quote in {xtask} or {cargo}, which the stand-in does not spell");
    }
    // A scratch tree's path is canonical, and `pwd -P` is the spelling a
    // real build's root would have.
    let body = format!(
        "#!/bin/sh\n\
         if [ \"$*\" = 'build -p xtask' ]; then\n\
         \x20   mkdir -p target/debug || exit 1\n\
         \x20   printf '#!/bin/sh\\nexec %s --root %s \"$@\"\\n' \"'{xtask}'\" \"'$(pwd -P)'\" > target/debug/xtask || exit 1\n\
         \x20   exec chmod +x target/debug/xtask\n\
         fi\n\
         if [ \"$1\" = xtask ]; then\n\
         \x20   shift\n\
         \x20   exec '{xtask}' --root \"$(pwd -P)\" \"$@\"\n\
         fi\n\
         exec '{cargo}' \"$@\"\n"
    );
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating the stand-in's directory {}", dir.display()))?;
    let file = dir.join("cargo");
    std::fs::write(&file, body)
        .with_context(|| format!("writing the cargo stand-in {}", file.display()))?;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("marking the cargo stand-in {} executable", file.display()))?;
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).context("joining the stand-in's directory onto PATH")
}

/// A scratch repository at `to` holding `workspace`'s files, tracked or
/// not yet but not ignored and standing in the working tree
/// ([`crate::git::files`]), as the one commit on `main`, with the hooks
/// path the clone's own is set to and that commit made unsigned and
/// unhooked: a person's `commit.gpgsign` is not the scratch repository's,
/// and the real pre-commit measuring the scratch's files is not what a
/// test holds. The harness a build there compiles is this working tree's,
/// uncommitted changes and all, so a test that builds `xtask` in a
/// worktree of it builds the harness under test.
///
/// The files are copied, which on APFS is a clone sharing the workspace's
/// blocks (`std::fs::copy` is `fclonefileat` there), and the objects are
/// borrowed: the scratch's object store names the workspace's as its
/// alternate, so `git add` writes only the blobs of what the workspace has
/// not committed, and the trees and the commit above them. Written out,
/// they were a scratch's real cost on disk, 36 MB of loose objects each,
/// and a suite lays a dozen at once (#644). What it borrows is reachable
/// in the workspace, or a loose object `git add` found there and freshened,
/// which `gc` keeps for two weeks from then; a scratch lives for one test,
/// so only a `git gc --prune=now` in the workspace during it can take a
/// borrowed object away.
pub fn repository(workspace: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for file in crate::git::files(workspace, Path::new("."))? {
        let target = to.join(&file);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::copy(workspace.join(&file), &target)
            .with_context(|| format!("copying {file} into {}", to.display()))?;
    }
    git(to, &["init", "-q", "-b", "main"], None)?;
    if let Some(common) = crate::git::common_dir(workspace) {
        let alternates = to.join(".git/objects/info/alternates");
        std::fs::write(
            &alternates,
            format!("{}\n", common.join("objects").display()),
        )
        .with_context(|| format!("writing {}", alternates.display()))?;
    }
    git(to, &["config", "core.hooksPath", "scripts/hooks"], None)?;
    git(to, &["add", "-A"], None)?;
    git(
        to,
        &[
            "-c",
            "user.name=xtask",
            "-c",
            "user.email=xtask@scratch",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/var/empty",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
        None,
    )?;
    Ok(())
}

/// `git` in `dir` as a person commits there: named, unsigned and
/// unhooked, since the real pre-commit measuring a scratch is not the
/// test.
#[cfg(test)]
pub(crate) fn committing(dir: &Path, args: &[&str]) {
    let unhooked = [
        "-c",
        "user.name=x",
        "-c",
        "user.email=x@x",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/var/empty",
    ];
    if let Err(err) = git(dir, &[&unhooked[..], args].concat(), None) {
        panic!("git {args:?} in {}: {err:#}", dir.display());
    }
}

/// The lock beside `tree`, opened and taken.
///
/// The file is made and locked under a name the sweep does not read, and
/// only then renamed to the lock's own, so a sweep in another process
/// never meets the lock before it is held. Made under its own name, it
/// stood unlocked between the open and the lock, and a sweep that opened
/// it there took it, read the tree as a dead run's and left this run
/// refusing its own lock, which two runs starting at once under load did
/// (#1252).
///
/// A lock that will not come says another live process is working at this
/// exact path, which the process id and the counter in the name are
/// supposed to make impossible; it is a failure rather than a thing to
/// work around, because the alternative is two runs in one directory.
fn hold(tree: &Path) -> Result<(PathBuf, File)> {
    let (taking, lock) = (beside(tree, TAKING), beside(tree, LOCK));
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&taking)
        .map_err(|err| made(err, &taking, "opening the scratch lock"))?;
    if file.try_lock().is_err() {
        anyhow::bail!(
            "the scratch lock {} is held: another run is working there",
            taking.display()
        );
    }
    std::fs::rename(&taking, &lock).map_err(|err| made(err, &lock, "naming the scratch lock"))?;
    Ok((lock, file))
}

/// The path `tree` names with `suffix` after it: its lock, or the name
/// the lock is taken under.
fn beside(tree: &Path, suffix: &str) -> PathBuf {
    let mut named = tree.to_path_buf().into_os_string();
    named.push(suffix);
    PathBuf::from(named)
}

/// Every scratch tree a dead process left in `base`, taken away.
///
/// The sweep reads the locks, not the trees. A lock it can take is one the
/// operating system released when its holder ended, however it ended, so
/// the tree beside it belongs to nobody and both go. A lock that will not
/// come belongs to a run still going: its tree is passed over, which is
/// what keeps concurrent runs — and, inside one test binary, the trees of
/// every test running beside the one that spawned a child — out of each
/// other's way. A tree with no lock beside it was kept on purpose and is
/// never a candidate.
///
/// Nothing here reports. A temporary directory this process may not read,
/// a lock another run took between the listing and the open, a tree
/// already gone: each is a reason to leave a path alone, and none is a
/// reason to fail the run that was only trying to make a directory.
fn reap(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let lock = entry.path();
        let Some(name) = lock.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(tree) = name.strip_suffix(LOCK) else {
            continue;
        };
        if !tree.starts_with(PREFIX) {
            continue;
        }
        let Ok(file) = File::options().read(true).write(true).open(&lock) else {
            continue;
        };
        if file.try_lock().is_err() || !names(&lock, &file) {
            continue;
        }
        std::fs::remove_dir_all(base.join(tree)).ok();
        std::fs::remove_file(&lock).ok();
    }
}

/// Whether `path` still names the file `file` has open: a lock renamed
/// over the one the sweep opened, a new run's at a path an old one left,
/// is that run's and not the sweep's to take away.
fn names(path: &Path, file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(path), file.metadata()) {
        (Ok(named), Ok(open)) => named.dev() == open.dev() && named.ino() == open.ino(),
        _ => false,
    }
}

/// A process a run left standing in its scratch tree (#2663): an orphan,
/// handed to init once its parent ended, whose command line names a path
/// inside a tree, as a test's php names its program in the run's copy of
/// the pinned tree and a php-fpm master that left its group by `setsid()`
/// names its ini there in its title. Its group goes with it where it leads
/// one, as a program's does (`oracle::Group`), and a php-fpm master's
/// workers are in its. A process whose parent lives is some program's
/// still, an agent's shell that names the tree among them, and is never
/// one; nor is one that names no tree, whatever number it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stray {
    pub pid: u32,
    pub group: u32,
    pub trees: Vec<PathBuf>,
}

impl Stray {
    /// The strays of `listed`, the lines `ps -o pid=,ppid=,pgid=,command=`
    /// printed, that name a tree made as `name` ([`unique`]).
    pub(crate) fn of(listed: &str, name: &str) -> Vec<Stray> {
        let marker = format!("/{PREFIX}{name}");
        let numbered = |n: &str| {
            n.bytes().enumerate().all(|(at, b)| match at {
                0 | 8 => b == b'-',
                _ => b.is_ascii_digit(),
            })
        };
        let mut strays = Vec::new();
        for line in listed.lines() {
            let mut words = line.split_whitespace();
            let mut number = || words.next().and_then(|word| word.parse::<u32>().ok());
            let (Some(pid), Some(INIT), Some(group)) = (number(), number(), number()) else {
                continue;
            };
            let trees: Vec<PathBuf> = line
                .match_indices(&marker)
                .filter_map(|(at, _)| {
                    let end = at + marker.len() + NUMBERED;
                    let before = line.get(..at)?;
                    let start = before
                        .rfind([' ', '(', '=', ':', ',', '\'', '"'])
                        .map_or(0, |s| s + 1);
                    let tree = line.get(start..end)?;
                    let after = line.get(end..)?.chars().next();
                    (tree.starts_with('/')
                        && numbered(line.get(at + marker.len()..end)?)
                        && after.is_none_or(|c| !c.is_ascii_alphanumeric()))
                    .then(|| PathBuf::from(tree))
                })
                .collect();
            if !trees.is_empty() {
                strays.push(Stray { pid, group, trees });
            }
        }
        strays
    }

    /// The process stopped with its group where it leads one, and waited
    /// for, as long as a second, to be gone.
    fn stop(&self) {
        use nix::sys::signal::{Signal, kill, killpg};
        let Ok(pid) = i32::try_from(self.pid) else {
            return;
        };
        let pid = nix::unistd::Pid::from_raw(pid);
        if self.group == self.pid {
            killpg(pid, Signal::SIGKILL).ok();
        } else {
            kill(pid, Signal::SIGKILL).ok();
        }
        for _ in 0..100 {
            if !crate::sysv::alive(self.pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// The process init is, which an orphan's parent becomes.
const INIT: u32 = 1;

/// The strays standing in trees made as `name` that `ours` takes, each
/// stopped, and their pids: none where `ps` does not answer.
fn stop_strays_in(name: &str, ours: impl Fn(&Path) -> bool) -> Vec<u32> {
    let Ok(ps) = std::process::Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,ppid=,pgid=,command="])
        .output()
    else {
        return Vec::new();
    };
    let strays = Stray::of(&String::from_utf8_lossy(&ps.stdout), name);
    let ours: Vec<Stray> = strays
        .into_iter()
        .filter(|stray| stray.trees.iter().any(|tree| ours(tree)))
        .collect();
    ours.iter().for_each(Stray::stop);
    ours.iter().map(|stray| stray.pid).collect()
}

/// The strays dead runs left in their trees made as `name` by
/// [`Tree::short`], stopped, and their pids: a tree whose lock comes, or
/// that is gone with its lock, is a dead run's ([`reap`]); one whose lock
/// will not come is a run's still going, whose strays are its own to stop
/// ([`Tree::stop_strays`]); and one standing with no lock was kept
/// ([`KEEP`]) and is never a candidate. Every session's runs lay their
/// trees in that one directory (#2626), so the lock is the only guard: a
/// dead run's strays are stopped by the next run of any session, and a
/// live run's by none but itself. A cut-off System V run's programs are
/// stopped by its own claims first (`sysv::stop_cut_off`). What a run's
/// start does before it makes its tree, so the files a killed run's
/// programs hold open in a tree removed under them are let go (#2663).
pub(crate) fn stop_forsaken(name: &str) -> Vec<u32> {
    let Ok(base) = std::fs::canonicalize(SHORT) else {
        return Vec::new();
    };
    stop_strays_in(name, |tree| {
        if tree.parent() != Some(base.as_path()) {
            return false;
        }
        match File::options()
            .read(true)
            .write(true)
            .open(beside(tree, LOCK))
        {
            Ok(lock) => lock.try_lock().is_ok(),
            Err(err) => err.kind() == std::io::ErrorKind::NotFound && !tree.exists(),
        }
    })
}

impl Tree {
    /// The strays standing in this tree, stopped, and their pids: what a run
    /// does once every program it started in the tree has ended, for what
    /// one of them started that left its group (#2663).
    pub(crate) fn stop_strays(&self) -> Vec<u32> {
        let named = self.path.file_name().and_then(|n| n.to_str());
        let name = named
            .and_then(|n| n.strip_prefix(PREFIX))
            .unwrap_or_default();
        let name = name
            .get(..name.len().saturating_sub(NUMBERED))
            .unwrap_or_default();
        stop_strays_in(name, |tree| tree == self.path)
    }
}

impl std::ops::Deref for Tree {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Tree {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<std::ffi::OsStr> for Tree {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.path.as_os_str()
    }
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.path, f)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // Holding a lock is what makes the tree ours to remove; under
        // `KEEP` there is no lock and the tree stays. The lock goes after
        // the tree, so a sweep arriving in between finds a lock whose tree
        // is already gone and removes the lock, which is the same ending.
        if let Some((lock, _)) = &self.held {
            std::fs::remove_dir_all(&self.path).ok();
            std::fs::remove_file(lock).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use std::os::unix::fs::MetadataExt;

    use super::{Full, LOCK, Stray, Tree, beside, hold, made, reap, stop_forsaken};
    use crate::reading::{Reading, Status};

    /// A sweep never meets a lock before it is held (#1252): one thread
    /// takes lock after lock in a directory of its own while another sweeps
    /// that directory without pause, and every lock is taken, and each is
    /// still its taker's when the taker lets it go.
    #[test]
    fn a_sweep_beside_a_run_taking_its_locks_never_takes_one() {
        let base = Tree::new("sweep-race").unwrap();
        let taken = AtomicBool::new(false);
        let failed = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !taken.load(Ordering::Relaxed) {
                    reap(&base);
                }
            });
            let failed = (0..2000).find_map(|n| {
                let tree = base.join(format!("xtask-race-{n}"));
                let (lock, file) = match hold(&tree) {
                    Ok(held) => held,
                    Err(err) => return Some(format!("lock {n}: {err:#}")),
                };
                if !lock.is_file() {
                    return Some(format!("lock {n} was swept while held"));
                }
                drop(file);
                std::fs::remove_file(&lock).ok();
                None
            });
            taken.store(true, Ordering::Relaxed);
            failed
        });
        assert_eq!(failed, None);
    }

    /// A program's tree is one length however many trees came before it,
    /// under `/tmp` whatever the session's `TMPDIR` (#2523): a dozen made
    /// in a row, so the counter crosses a digit wherever it stood, each
    /// named `xtask-<name>-<7 digits>-<6 digits>` under one directory.
    #[test]
    fn a_programs_tree_is_one_length_whatever_the_process_and_the_session() {
        let trees: Vec<Tree> = (0..12).map(|_| Tree::short("length").unwrap()).collect();
        let tmp = std::fs::canonicalize("/tmp").unwrap();
        for tree in &trees {
            assert_eq!(tree.parent(), Some(tmp.as_path()), "{tree:?}");
            let name = tree.file_name().unwrap().to_str().unwrap();
            let numbers = name.strip_prefix("xtask-length-").unwrap();
            let (pid, next) = numbers.split_once('-').unwrap();
            assert_eq!((pid.len(), next.len()), (7, 6), "{name}");
            assert!(numbers.bytes().all(|b| b == b'-' || b.is_ascii_digit()));
            assert_eq!(pid.parse::<u32>().unwrap(), std::process::id());
        }
    }

    /// A scratch tree the volume has no room for is [`Full`], naming the
    /// volume `df` says it stands on, under whatever context is added
    /// above it; any other trouble making it is not (#644). And the reading
    /// of the run it ended is the skip that says so, never a fail.
    #[test]
    fn a_scratch_the_volume_has_no_room_for_is_full_and_its_run_a_skip_naming_the_volume() {
        let tree = Tree::new("full").unwrap();
        let path = tree.join("lock-taking");
        let enospc = std::io::Error::from_raw_os_error(nix::errno::Errno::ENOSPC as i32);
        let err = made(enospc, &path, "opening the scratch lock")
            .context("running ext/fixture/tests/one.phpt");
        let full = Full::of(&err).expect("ENOSPC is the volume's");
        assert_eq!(full.path, path);
        // The volume `df` names, not the directory the fallback would: the
        // tree's own device, at a path that is not the tree's.
        let dev = |path: &std::path::Path| std::fs::metadata(path).unwrap().dev();
        assert_eq!(
            dev(&full.volume),
            dev(tree.path()),
            "{}",
            full.volume.display()
        );
        assert_ne!(full.volume, tree.path());

        let mut reading = Reading::new("phpt", "ext/fixture");
        reading.status = Status::Fail;
        let reading = Full::skipped(reading, "ext/fixture", &err).unwrap();
        assert_eq!(reading.status, Status::Skip);
        assert!(
            reading
                .message
                .starts_with("ext/fixture: not measured for want of space: ")
                && reading.message.contains(&format!(
                    "no space left on {} for {}",
                    full.volume.display(),
                    path.display()
                )),
            "{}",
            reading.message
        );
        assert_eq!(
            reading.numbers.get(Full::NUMBER),
            Some(&crate::reading::Number::Int(1))
        );

        let denied = std::io::Error::from_raw_os_error(nix::errno::Errno::EACCES as i32);
        assert!(Full::of(&made(denied, &path, "opening")).is_none());
    }

    /// What a stray is read from `ps`'s lines (#2663): an orphan naming a
    /// path in a tree made as the name, its trees each once per mention;
    /// never a process whose parent lives, nor a path whose numbers are
    /// not a tree's, nor a tree made as another name.
    #[test]
    fn a_stray_is_an_orphan_whose_command_names_a_tree_made_as_the_name() {
        let tree = "/tmp/xtask-phpt-tree-0012345-000001";
        let listed = format!(
            "  501 1 501 php-fpm: master process ({tree}/sapi/fpm/tests/a.ini)\n\
             502 1 501 php -d x={tree}/a {tree}.lock\n\
             503 77 503 zsh -c ls {tree}/a\n\
             504 1 504 vi /tmp/xtask-phpt-tree-marks-0012345-000001/a /tmp/xtask-phpt-tree-0012345-00000\n\
             505 1 505 php /tmp/xtask-phpt-tree-0012345-0000012/a"
        );
        let strays = Stray::of(&listed, "phpt-tree");
        let trees = |n| vec![std::path::PathBuf::from(tree); n];
        assert_eq!(
            strays,
            [
                Stray {
                    pid: 501,
                    group: 501,
                    trees: trees(1)
                },
                Stray {
                    pid: 502,
                    group: 501,
                    trees: trees(2)
                },
            ]
        );
    }

    /// A dead run's stray is stopped, and nothing else is (#2663): an
    /// orphan naming a tree whose run is gone is, with its group; an
    /// orphan naming a tree a run still holds is another run's program and
    /// is left to it until that run stops its own, as is one in a tree
    /// standing with no lock, a kept one; and a process whose parent lives
    /// is left however dead the tree it names.
    #[test]
    fn a_dead_runs_stray_is_stopped_and_a_live_runs_or_a_childs_never() {
        let marks = Tree::new("strays-marks").unwrap();
        let held = Tree::short("strays").unwrap();
        let dead = Tree::short("strays").unwrap();
        let dead_path = dead.path().to_path_buf();
        drop(dead);
        let kept = Tree::short("strays").unwrap();
        std::fs::remove_file(beside(&kept, LOCK)).unwrap();
        // perl forks, and the child takes a session of its own and writes
        // its pid before its parent exits and leaves it to init.
        let orphan = |tree: &std::path::Path, mark: &str| {
            let mark = marks.join(mark);
            let perl = "my $m = $ARGV[1]; if (fork) { select(undef, undef, undef, 0.05) until -s $m; exit } \
                        POSIX::setsid(); open my $f, '>', $m; print $f $$; close $f; sleep 60";
            let status = std::process::Command::new("perl")
                .args(["-MPOSIX", "-e", perl])
                .arg(tree.join("a"))
                .arg(&mark)
                .status()
                .unwrap();
            assert!(status.success());
            std::fs::read_to_string(&mark)
                .unwrap()
                .parse::<u32>()
                .unwrap()
        };
        let gone = |pid: u32| {
            (0..200).any(|_| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                !crate::sysv::alive(pid)
            })
        };
        let (live, forsaken) = (orphan(held.path(), "live"), orphan(&dead_path, "dead"));
        let kept_one = orphan(kept.path(), "kept");
        let mut child = std::process::Command::new("perl")
            .args(["-e", "sleep 60"])
            .arg(dead_path.join("a"))
            .spawn()
            .unwrap();
        let stopped = stop_forsaken("strays");
        let child_ran = child.try_wait().unwrap().is_none();
        child.kill().ok();
        child.wait().ok();
        let live_ran = crate::sysv::alive(live);
        let held_stopped = held.stop_strays();
        let kept_ran = crate::sysv::alive(kept_one);
        let kept_stopped = kept.stop_strays();
        std::fs::remove_dir_all(kept.path()).ok();
        assert_eq!(stopped, [forsaken]);
        assert!(gone(forsaken), "the dead run's stray {forsaken} runs on");
        assert!(child_ran, "a child whose parent lives was stopped");
        assert!(live_ran, "a live run's program {live} was stopped");
        assert_eq!(held_stopped, [live]);
        assert!(gone(live), "the run's own stray {live} runs on");
        assert!(kept_ran, "a kept tree's {kept_one} was stopped");
        assert_eq!(kept_stopped, [kept_one]);
    }
}
