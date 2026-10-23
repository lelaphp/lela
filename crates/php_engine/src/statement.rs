//! Statement events: a Zend extension's `statement_handler`
//! (`Z/Zend/zend_extensions.h:93`), told at each `EXT_STMT` a compile
//! with extended info leaves before a statement (`-e`, #1780, #1822),
//! as `ZEND_EXT_STMT` applies `zend_extension_statement_handler` to
//! every loaded Zend extension in the order they were loaded
//! (`Z/Zend/zend_vm_def.h:7923-7933`, `Z/Zend/zend_execute.c:2457-2462`).
//! It is what a debugger's line breakpoints and stepping stand on, as
//! xdebug's `xdebug_statement_call` does.
//!
//! An extension declares one on its Zend extension row
//! ([`ZendExtension::statement_handler`]), as php's is a field of the
//! `zend_extension` struct beside its name, so only a Zend extension has
//! one. The bundle's are listed in the order the Zend extensions start
//! ([`crate::supplied`]).
//!
//! Statement events are not the observer's (seam 9, [`crate::observer`]):
//! php's observer is told of calls, and of a statement nothing but a
//! Zend extension is. What they cost where no Zend extension declares a
//! handler is nothing a plain compile reaches: no `EXT_STMT` is emitted
//! without extended info, and under it the opline's handler tests the
//! empty list and goes on.
//!
//! [`ZendExtension::statement_handler`]: crate::api::abi::ZendExtension::statement_handler

use std::sync::Arc;

use php_heap::FrameRef;

use crate::api::raising::Thrown;
use crate::api::request::{FrameView, Request, TimeLimitSuspended};

/// `statement_handler_func_t` (`Z/Zend/zend_extensions.h:68`): told of
/// the statement the frame stands before, at its `EXT_STMT`. What it
/// throws is raised where the opline stands, as
/// `ZEND_VM_NEXT_OPCODE_CHECK_EXCEPTION` raises `EG(exception)` after
/// the handlers.
pub type StatementHandler = fn(&mut Statement<'_>) -> Result<(), Thrown>;

/// The frame a statement handler is handed, php's `execute_data` at an
/// `EXT_STMT`, borrowed for the handler's span alone: the request through
/// `Deref`, and the frame through [`Statement::frame`], whose
/// [`line`](FrameView::line) is the statement's, `EX(opline)->lineno`
/// after the handler's `SAVE_OPLINE`. A frame kept past the handler is
/// its [`FrameId`](crate::api::request::FrameId).
pub struct Statement<'a> {
    req: &'a mut Request,
    frame: FrameRef,
}

/// A handler's view that holds the request as its `req`: the request
/// read and written through it, and the request whose time limit a
/// [`TimeLimitSuspended`] over the view suspends.
macro_rules! over_request {
    ($view:ident) => {
        impl std::ops::Deref for $view<'_> {
            type Target = Request;
            fn deref(&self) -> &Request {
                self.req
            }
        }

        impl std::ops::DerefMut for $view<'_> {
            fn deref_mut(&mut self) -> &mut Request {
                self.req
            }
        }

        impl AsMut<Request> for $view<'_> {
            fn as_mut(&mut self) -> &mut Request {
                self.req
            }
        }
    };
}
pub(crate) use over_request;

over_request!(Statement);

impl Statement<'_> {
    /// The frame that stands before the statement, read for as long as the
    /// handler does not write through the request: the file, the line,
    /// the function, the frames below it, and its identity to keep.
    #[must_use]
    pub fn frame(&self) -> FrameView<'_> {
        self.req.frame_view(self.frame)
    }

    /// [`Limits::suspend_time_limit`] over the statement: the guard
    /// derefs to this, so the frame's reads stay the handler's while it
    /// blocks, as a debugger's does at a breakpoint waiting on its client.
    pub fn suspend_time_limit(&mut self) -> TimeLimitSuspended<'_, Self> {
        TimeLimitSuspended::over(self)
    }
}

/// `ZEND_EXT_STMT`'s walk (`Z/Zend/zend_vm_def.h:7927-7930`): unless
/// `EG(no_extensions)` is set ([`Request::serve_host_string`]), each
/// Zend extension's handler told of the statement `frame` stands at, in
/// the order the extensions start, and what was thrown raised after the
/// walk. As `zend_llist_apply_with_argument` tests nothing between two
/// handlers, a handler after one that threw is told with the throwable
/// standing pending, `EG(exception)` set, so PHP code it calls is not
/// called; what it throws then takes the slot with the first chained
/// as its `previous` (`zend_throw_exception_internal`,
/// `Z/Zend/zend_exceptions.c:193-206`). A fatal or an exit ends the
/// walk, as `zend_bailout` leaves it.
pub(crate) fn told(req: &mut Request, frame: FrameRef) -> Result<(), Thrown> {
    if req.shared.supplied.statement_handlers.is_empty() || req.no_extensions {
        return Ok(());
    }
    let shared = Arc::clone(&req.shared);
    let mut pending: Option<Thrown> = None;
    for (by, handler) in shared.supplied.statement_handlers.iter() {
        pending = match pending {
            None => req
                .serve_for(by, |req| handler(&mut Statement { req, frame }))
                .err(),
            Some(thrown) => {
                let ran_on = thrown.exception().copied();
                let told = req.while_pending(&thrown, |req| {
                    req.serve_for(by, |req| handler(&mut Statement { req, frame }))
                        .map_err(|raised| req.raised_over(raised, ran_on))
                });
                match told {
                    None => return Err(thrown),
                    Some(Ok(())) => Some(thrown),
                    Some(Err(raised)) => Some(raised),
                }
            }
        };
    }
    pending.map_or(Ok(()), Err)
}
