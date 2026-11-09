//! The workload programs (measurement protocol §11): the directory the
//! harness's readers reach them through, named once. `bench`,
//! `dispatch-census`, `trace-dump` and makala's bench run them, and
//! `budget` holds them under `[budget] workloads` apart from `docs` where
//! that row stands (#4341).

use std::path::{Path, PathBuf};

/// The directory of the workload programs, relative to the workspace root.
pub(crate) const DIR: &str = "docs/perf/workloads";

/// [`DIR`] under `root`.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

/// The program `stem` under `root`: `<DIR>/<stem>.php`.
pub(crate) fn program(root: &Path, stem: &str) -> PathBuf {
    dir(root).join(format!("{stem}.php"))
}
