//! What an extension supplies the layer, served as that extension
//! (#2113): a stream wrapper, a filter factory and the filters it makes,
//! and the kind of a stream it opens with [`Request::new_stream`]. Each
//! is called from wherever the program reaches it, so a debug build
//! takes who registered or made it ([`Code::supplier`]) and serves
//! every call into it as that extension ([`Code::serve_supplied`]),
//! and a leak it leaves names it, as a leak of the extension's natives
//! does (#1993). A release build keeps nothing and serves each call as
//! it stands.

use std::ops::Deref;

#[cfg(debug_assertions)]
use crate::api::raising::Thrown;
#[cfg(debug_assertions)]
use crate::api::request::{Brigade, FilterFlags, FilterStatus, Stream, Whence};
use crate::api::request::{Request, StreamFilter, StreamOps, Supplier};
use crate::state::Layer as _;

/// One row an extension registered, with who registered it.
pub(crate) struct Supplied<T> {
    pub(crate) row: T,
    pub(crate) by: Supplier,
}

impl<T> Deref for Supplied<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.row
    }
}

/// `serve` served as the extension that registered the wrapper at `at`.
pub(crate) fn wrapper<T>(req: &mut Request, at: usize, serve: impl FnOnce(&mut Request) -> T) -> T {
    let by = req.streams().wrappers[at].by;
    req.code_mut().serve_supplied(by, serve)
}

/// The kind of a stream `req`'s code is making, each operation served
/// as that code in a debug build.
pub(crate) fn kind(req: &Request, ops: Box<dyn StreamOps>) -> Box<dyn StreamOps> {
    #[cfg(debug_assertions)]
    return Box::new(SuppliedOps {
        ops,
        by: req.code().supplier(),
    });
    #[cfg(not(debug_assertions))]
    {
        let _ = req;
        ops
    }
}

/// A filter `by`'s factory made, each call served as `by` in a debug
/// build.
pub(crate) fn filter(by: Supplier, filter: Box<dyn StreamFilter>) -> Box<dyn StreamFilter> {
    #[cfg(debug_assertions)]
    return Box::new(SuppliedFilter { filter, by });
    #[cfg(not(debug_assertions))]
    {
        let _ = by;
        filter
    }
}

/// A kind of stream an extension made, served as it.
#[cfg(debug_assertions)]
struct SuppliedOps {
    ops: Box<dyn StreamOps>,
    by: Supplier,
}

// Every operation is forwarded, so one added to the trait and missed
// here is a lint and never the default standing in for the kind's.
#[cfg(debug_assertions)]
#[deny(clippy::missing_trait_methods)]
impl StreamOps for SuppliedOps {
    fn label(&self) -> &'static str {
        self.ops.label()
    }

    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let ops = &mut self.ops;
        req.code_mut()
            .serve_supplied(self.by, |req| ops.write(req, buf))
    }

    fn read(&mut self, req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let ops = &mut self.ops;
        req.code_mut()
            .serve_supplied(self.by, |req| ops.read(req, buf, eof))
    }

    fn close(&mut self, req: &mut Request) {
        let ops = &mut self.ops;
        req.code_mut().serve_supplied(self.by, |req| ops.close(req));
    }

    fn seek(
        &mut self,
        req: &mut Request,
        offset: i64,
        whence: Option<Whence>,
        position: &mut i64,
    ) -> Option<bool> {
        let ops = &mut self.ops;
        req.code_mut()
            .serve_supplied(self.by, |req| ops.seek(req, offset, whence, position))
    }

    fn seekable(&self) -> bool {
        self.ops.seekable()
    }

    fn size(&self) -> Option<i64> {
        self.ops.size()
    }

    fn stat(&mut self, req: &mut Request) -> Option<[i64; 13]> {
        let ops = &mut self.ops;
        req.code_mut().serve_supplied(self.by, |req| ops.stat(req))
    }

    fn flush(&mut self, req: &mut Request) -> bool {
        let ops = &mut self.ops;
        req.code_mut().serve_supplied(self.by, |req| ops.flush(req))
    }

    fn flock(&mut self, req: &mut Request, operation: i64) -> Option<bool> {
        let ops = &mut self.ops;
        req.code_mut()
            .serve_supplied(self.by, |req| ops.flock(req, operation))
    }

    fn meta_data_from(&self) -> Option<Stream> {
        self.ops.meta_data_from()
    }

    fn buffered(&self) -> bool {
        self.ops.buffered()
    }
}

/// A filter an extension's factory made, served as it.
#[cfg(debug_assertions)]
struct SuppliedFilter {
    filter: Box<dyn StreamFilter>,
    by: Supplier,
}

#[cfg(debug_assertions)]
#[deny(clippy::missing_trait_methods)]
impl StreamFilter for SuppliedFilter {
    fn filter(
        &mut self,
        req: &mut Request,
        stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let filter = &mut self.filter;
        req.code_mut().serve_supplied(self.by, |req| {
            filter.filter(req, stream, input, output, consumed, flags)
        })
    }

    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let filter = &mut self.filter;
        req.code_mut()
            .serve_supplied(self.by, |req| filter.close(req))
    }

    fn share(&self, req: &mut Request) -> Option<Box<dyn StreamFilter>> {
        let shared = self.filter.share(req)?;
        Some(filter(self.by, shared))
    }
}
