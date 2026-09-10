//! The negative control for a rule that reads one section of the glossary:
//! a harness whose prose and public names carry words the Harness section
//! avoids, and words the Engine section avoids, which are the harness's
//! own. It is scanned by `xtask vocab`, never compiled.

/// Serve the run and print the scoreboard: the registry of every reading.
pub fn run() {}

/// One line per instrument.
pub fn summary() {}

/// The type the glossary marks as code passes bare, in a name and in prose.
pub type Result<T> = core::result::Result<T, ()>;
