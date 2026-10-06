//! A scratch tree that owns itself (#194): one directory under the
//! system's temporary directory, made by the constructor and taken away
//! again when the value goes out of scope, however it goes out of scope.
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
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::git::git;

/// Set to anything non-empty and a scratch tree is made, named on stderr
/// and left standing: what a session debugging a failure sets, so the tree
/// the failure ran in is still there to read afterwards.
pub const KEEP: &str = "XTASK_KEEP_SCRATCH";

/// What every scratch path begins with, the tree and its lock alike: what
/// the sweep reads the temporary directory for, and what tells a tree of
/// this harness's from everything else living there.
const PREFIX: &str = "xtask-";

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
/// make it this one's alone: the process, and a counter of the trees this
/// process has made. Two tests naming themselves the same thing at the
/// same time get two trees.
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
        static NEXT: AtomicU64 = AtomicU64::new(0);
        static SWEPT: Once = Once::new();

        // Before this process's first tree, and so before there is one of
        // ours to mistake for a dead run's.
        SWEPT.call_once(|| reap(&std::env::temp_dir()));

        let named = std::env::temp_dir().join(format!(
            "{PREFIX}{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let keep = std::env::var_os(KEEP).is_some_and(|v| !v.is_empty());

        // The lock is taken before the directory exists, so a sweep in
        // another process never meets a tree whose lock is not yet held
        // and reads it as a dead run's.
        let held = if keep { None } else { Some(hold(&named)?) };

        std::fs::remove_dir_all(&named).ok();
        std::fs::create_dir_all(&named)
            .with_context(|| format!("creating the scratch tree {}", named.display()))?;
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
/// not yet but not ignored, as the one commit on `main`, with the hooks
/// path the clone's own is set to and that commit made unsigned and
/// unhooked: a person's `commit.gpgsign` is not the scratch repository's,
/// and the real pre-commit measuring the scratch's files is not what a
/// test holds. The harness a build there compiles is this working tree's,
/// uncommitted changes and all, so a test that builds `xtask` in a
/// worktree of it builds the harness under test.
pub fn repository(workspace: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let listed = git(
        workspace,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        None,
    )?;
    for file in listed.split('\0').filter(|f| !f.is_empty()) {
        let target = to.join(file);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::copy(workspace.join(file), &target)
            .with_context(|| format!("copying {file} into {}", to.display()))?;
    }
    git(to, &["init", "-q", "-b", "main"], None)?;
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
    let beside = |suffix: &str| {
        let mut named = tree.to_path_buf().into_os_string();
        named.push(suffix);
        PathBuf::from(named)
    };
    let (taking, lock) = (beside(TAKING), beside(LOCK));
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&taking)
        .with_context(|| format!("opening the scratch lock {}", taking.display()))?;
    if file.try_lock().is_err() {
        anyhow::bail!(
            "the scratch lock {} is held: another run is working there",
            taking.display()
        );
    }
    std::fs::rename(&taking, &lock)
        .with_context(|| format!("naming the scratch lock {}", lock.display()))?;
    Ok((lock, file))
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

    use super::{Tree, hold, reap};

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
}
