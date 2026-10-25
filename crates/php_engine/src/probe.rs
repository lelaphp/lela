//! The engine tests' adapter for seam 8 (design §4.3), beside `Nop`, so
//! the seam has two adapters before `php_jit` exists. It records every
//! site the loop asks it at, in order, as its own request state under
//! seam 3, and the tests assert the sequence. What it answers is that
//! state too: a test that wants the `Resume` contract (`jit-design.md`
//! §2.2) queues the frame to resume in, and one that wants the
//! interrupt a compiled loop's exit would have left set asks for it.

use php_heap::FrameRef;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::request::Request;
use crate::tier::{Flow, Site, Tier};

/// The sites asked, in the order they were asked, and what to answer.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    pub(crate) sites: Vec<(Site, FrameRef)>,
    /// The frame the next site answers `Flow::Resume` with, taken when
    /// it is answered; `None` answers `Flow::Continue`.
    pub(crate) resume: Option<FrameRef>,
    /// The interrupt a tier that served code would have left set: the
    /// seconds an execution timer that fired while it ran was armed
    /// with, taken at the first site it is asked at.
    pub(crate) elapsed: Option<i64>,
}

/// The tier that serves nothing and remembers everything.
pub(crate) struct Probe;

impl Tier for Probe {
    type State = ();

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, _: crate::tier::Held<'_>) -> Flow {
        let asked = req.state::<Asked>();
        asked.sites.push((site, frame));
        let resume = asked.resume.take();
        if let Some(seconds) = asked.elapsed.take() {
            req.limits().interrupt().elapsed(seconds);
        }
        match resume {
            Some(at) => Flow::Resume(at),
            None => Flow::Continue,
        }
    }

    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .request_state::<Asked>()
                .build("tier"),
        )
    }
}
