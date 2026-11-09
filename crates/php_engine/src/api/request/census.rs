//! The counting build of the loop (design §5.8, #4328): under the
//! `dispatch-census` feature, off by default and in no host's default
//! build, the loop counts each opline it dispatches by the
//! `php_unit::Handler` it is served by, and the request writes the counts
//! to the host's error stream where it ends, after everything it served.
//! The block opens with a newline, so it starts a line whatever the
//! program left on the stream, and holds one line per variant met,
//! [`LINE`], the variant's name and its count, in the variants' order.
//! `xtask dispatch-census` builds the engine with the feature, runs each
//! program `IN_THE_LOOP`'s doc names and reads these lines back.

use std::fmt::Write as _;

use php_unit::Handler;

use super::Request;

/// What each line of the block starts with, before the variant's name
/// and its count, a space apart.
pub(crate) const LINE: &str = "dispatch-census ";

/// The variants the loop dispatched, each at its discriminant with how
/// many oplines it served; `None` at one no opline was served by.
#[derive(Debug, Default)]
pub(crate) struct Census {
    met: Vec<Option<(Handler, u64)>>,
}

impl Census {
    /// One opline served by `handler`.
    fn count(&mut self, handler: Handler) {
        let at = usize::from(handler as u16);
        if self.met.len() <= at {
            self.met.resize(at + 1, None);
        }
        if let Some(slot) = self.met.get_mut(at) {
            let (_, served) = slot.get_or_insert((handler, 0));
            *served += 1;
        }
    }

    /// The block the request writes: a newline, then a line per variant
    /// met.
    fn block(&self) -> String {
        let mut block = String::from("\n");
        for (handler, served) in self.met.iter().flatten() {
            writeln!(block, "{LINE}{handler:?} {served}").ok();
        }
        block
    }
}

impl Request {
    /// One opline the loop dispatched, served by `handler`.
    pub(crate) fn count_dispatched(&mut self, handler: Handler) {
        self.census.count(handler);
    }

    /// The counts written to the host's error stream and set back to
    /// none, where the request ends.
    pub(crate) fn write_census(&mut self) {
        let census = std::mem::take(&mut self.census);
        self.response_mut().write_stderr(census.block().as_bytes());
    }
}
