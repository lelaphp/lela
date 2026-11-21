//! The binary's exit status: 0 done, 1 a refusal or a reading that fails,
//! 2 the argument parser's usage error, and the two of their own named
//! here, each the exit of one typed error the binary reads off the error
//! it ends on. `xtask --help` renders them from these constants.

/// The exit of a worktree laid down but not ready to be worked in, the
/// error [`crate::worktree::add`] ends on: neither done nor undone, so a
/// caller can tell it from a refusal that changed nothing, which is 1.
pub const NOT_READY: u8 = 3;

/// The exit of an instrument that could not run to its end, its readings
/// not whole: the error `Unfinished`. A caller refuses such a run, where it
/// records one that came to its end and judged a test as failing, which
/// is 1. Every instrument whose readings can be not whole ends through
/// the one rule beside the tally's ladder, `tally::whole`: `phpt`,
/// `witness`, `compile-identity`, `crosscheck`, `fuzz` and `hostile`.
pub const UNFINISHED: u8 = 4;

/// An instrument that could not run to its end, and what stopped it.
#[derive(Debug)]
pub(crate) struct Unfinished {
    instrument: &'static str,
    why: anyhow::Error,
}

impl Unfinished {
    /// `why` as the error of `instrument` that could not run to its end.
    pub(crate) fn of(instrument: &'static str, why: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Unfinished { instrument, why })
    }
}

impl std::fmt::Display for Unfinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} could not run to its end", self.instrument)
    }
}

impl std::error::Error for Unfinished {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.why)
    }
}

/// The status the binary exits with on `err`: [`NOT_READY`] and
/// [`UNFINISHED`] for their errors, wherever in its chain, and 1 for any
/// other.
pub(crate) fn status(err: &anyhow::Error) -> u8 {
    if err.downcast_ref::<crate::worktree::NotReady>().is_some() {
        NOT_READY
    } else if err.downcast_ref::<Unfinished>().is_some() {
        UNFINISHED
    } else {
        1
    }
}
