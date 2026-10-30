//! The negative control for `vocab.sapi`: a host whose public names and
//! rustdoc prose carry `_Avoid_` words of every section of the glossary.
//! It is scanned by `xtask vocab`, never compiled.

/// Run the script and hand the registry back.
pub fn run() {}

/// What the host prints once a request is served.
pub enum Report {
    Served,
}

/// php's own word, in a code span, passes: phpdbg's `run` command.
pub fn serve() {}
