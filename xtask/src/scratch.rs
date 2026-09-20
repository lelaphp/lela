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
        SWEPT.call_once(reap);

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

        let _ = std::fs::remove_dir_all(&named);
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

/// The lock beside `tree`, opened and taken.
///
/// A lock that will not come says another live process is working at this
/// exact path, which the process id and the counter in the name are
/// supposed to make impossible; it is a failure rather than a thing to
/// work around, because the alternative is two runs in one directory.
fn hold(tree: &Path) -> Result<(PathBuf, File)> {
    let mut named = tree.to_path_buf().into_os_string();
    named.push(LOCK);
    let lock = PathBuf::from(named);
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .with_context(|| format!("opening the scratch lock {}", lock.display()))?;
    if file.try_lock().is_err() {
        anyhow::bail!(
            "the scratch lock {} is held: another run is working there",
            lock.display()
        );
    }
    Ok((lock, file))
}

/// Every scratch tree a dead process left, taken away.
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
fn reap() {
    let base = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&base) else {
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
        if file.try_lock().is_err() {
            continue;
        }
        let _ = std::fs::remove_dir_all(base.join(tree));
        let _ = std::fs::remove_file(&lock);
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
            let _ = std::fs::remove_dir_all(&self.path);
            let _ = std::fs::remove_file(lock);
        }
    }
}
