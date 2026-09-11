//! The engine tests' adapter for seam 8 (design §4.3), beside `Nop`, so
//! the seam has two adapters before `php_jit` exists. It records every
//! site the loop asks it at, in order, as its own request state under
//! seam 3, and the tests assert the sequence.

use php_heap::FrameRef;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::request::Request;
use crate::tier::{Flow, Site, Tier};

/// The sites asked, in the order they were asked.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Asked(pub(crate) Vec<(Site, FrameRef)>);

/// The tier that serves nothing and remembers everything.
pub(crate) struct Probe;

impl Tier for Probe {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        req.state::<Asked>().0.push((site, frame));
        Flow::Continue
    }

    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .request_state::<Asked>()
                .build("tier"),
        )
    }
}
