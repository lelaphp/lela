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
//! To keep a tree and look at it, set [`KEEP`] to anything non-empty; the
//! constructor names each path it makes and nothing removes it afterwards.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

/// Set to anything non-empty and a scratch tree is made, named on stderr
/// and left standing: what a session debugging a failure sets, so the tree
/// the failure ran in is still there to read afterwards.
pub const KEEP: &str = "XTASK_KEEP_SCRATCH";

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
    keep: bool,
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
        let named = std::env::temp_dir().join(format!(
            "xtask-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let keep = std::env::var_os(KEEP).is_some_and(|v| !v.is_empty());
        let _ = std::fs::remove_dir_all(&named);
        std::fs::create_dir_all(&named)
            .with_context(|| format!("creating the scratch tree {}", named.display()))?;
        let path = std::fs::canonicalize(&named)
            .with_context(|| format!("resolving the scratch tree {}", named.display()))?;
        if keep {
            eprintln!("{KEEP}: keeping {}", path.display());
        }
        Ok(Tree { path, keep })
    }

    /// The directory itself, where a caller needs the `&Path` by name.
    pub fn path(&self) -> &Path {
        &self.path
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
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
