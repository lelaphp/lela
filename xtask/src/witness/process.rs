//! What serves a fence marked `process` (harness-design.md §8, revision
//! 61; #1062): the `php` binary in a process of its own, never the engine
//! linked into this one. Such a snippet forks, sends or takes a signal, or
//! sets the process's ids, session or limits, and the harness runs its
//! lane on threads: a `fork()` there copies one thread of a process whose
//! other threads hold what they hold, a signal lands on whichever thread
//! the kernel picks, and a `setrlimit` is the harness's own. The binary
//! serves one request on the one thread it has, as design revision 35
//! requires of pcntl.
//!
//! The binary is `php-witness`, `crates/php`'s `main.rs` built with
//! `php_engine`'s `witness-trace`, which this module builds once per run
//! with cargo where the run meets its first such fence. Its build shares
//! the engine's compiled crates with this harness's, since both turn the
//! same features on, so the build is the bundle, the host and a link. It
//! runs under the recipe exactly as the oracle does
//! ([`Oracle::run_under`]), and every request it serves appends the
//! natives it entered to the file `PHP_WITNESS_TRACE` names, a forked
//! child's request its own beside its parent's: so coverage is observed
//! for a fence served here as for one served by the linked engine (harness
//! §4.5 condition 2, revision 58), what a fence entered and nothing it
//! only spells.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};

use super::anchor::Entry;
use crate::oracle::{Invocation, Observation, Oracle, Program};
use crate::scratch::Tree;

/// The binary's name, `crates/php`'s second `[[bin]]`.
const BINARY: &str = "php-witness";

/// The variable the binary reads the trace file's path from
/// (`crates/php/src/main.rs`).
const TRACE: &str = "PHP_WITNESS_TRACE";

/// The binary for one run: built at the first fence that asks for it, and
/// the build's failure the trouble each such fence borks on.
#[derive(Debug)]
pub(crate) struct Binary {
    root: PathBuf,
    built: OnceLock<Result<PathBuf, String>>,
}

impl Binary {
    /// The binary of the workspace at `root`, not yet built.
    pub(super) fn of(root: &Path) -> Binary {
        Binary {
            root: root.to_path_buf(),
            built: OnceLock::new(),
        }
    }

    /// Where the binary is, built from the tree now if this run has not
    /// built it yet.
    fn path(&self) -> Result<&Path> {
        self.built
            .get_or_init(|| build(&self.root).map_err(|e| format!("{e:#}")))
            .as_deref()
            .map_err(|why| anyhow!("{why}"))
    }

    /// `snippet` served by the binary under the recipe, and `regime`'s ini
    /// after it where the fence names one, as the oracle serves it; with
    /// the natives every request it served entered, in the order they
    /// were appended.
    pub(super) fn serve(
        &self,
        oracle: &Oracle<'_>,
        snippet: &str,
        regime: Option<&str>,
    ) -> Result<(Observation, Vec<Entry>)> {
        let binary = self.path()?;
        let traces = Tree::new("witness-trace")?;
        let trace = traces.join("entered");
        let env = [(TRACE.to_owned(), trace.display().to_string())];
        let observed = oracle.run_under(&Invocation {
            regime,
            env: &env,
            ..Invocation::new(binary, Program::Snippet(snippet))
        })?;
        // A request that entered no native appends nothing, and a binary
        // that never reached its end leaves no file: both are no entry.
        let text = fs::read_to_string(&trace).unwrap_or_default();
        Ok((observed, text.lines().filter_map(Entry::parse).collect()))
    }
}

/// `cargo build -p php --bin php-witness --features witness-trace` in
/// `root`, and where cargo put the binary: the target directory cargo
/// was told of, or `root`'s own.
fn build(root: &Path) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "--quiet",
            "--package",
            "php",
            "--bin",
            BINARY,
            "--features",
            "witness-trace",
        ])
        .current_dir(root)
        .status()
        .with_context(|| format!("running cargo to build {BINARY}"))?;
    if !status.success() {
        bail!(
            "cargo could not build {BINARY} ({status}); its errors are on the harness's second stream"
        );
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root.join("target"), |dir| root.join(dir));
    let binary = target.join("debug").join(BINARY);
    if !binary.is_file() {
        bail!("cargo built {BINARY} and it is not at {}", binary.display());
    }
    Ok(binary)
}
