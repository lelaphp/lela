//! Frame writes and evaluation in a paused frame for extensions
//! (design §4.3 seam 9): what a debugger written as an extension does
//! where it has stopped, as xdebug's `property_set` and `eval` do in
//! php. A variable of the frame is written ([`Frames::set_var`],
//! php's `zend_set_local_var`), and code is evaluated in the frame's
//! scope ([`Frames::eval_in_frame`], php's `zend_eval_stringl` with
//! `EG(current_execute_data)` at the frame, as xdebug's `xdebug_do_eval`
//! serves it).
//!
//! The frame is named by its [`FrameId`], kept from a [`FrameView`],
//! and checked where it stands each time
//! ([`Request::in_frame`]). A frame that stands on the running stack is
//! paused: the innermost, whose call or native the handler stands in,
//! and every one below it. A frame below a fiber, on the stack that
//! resumed it, is served with the request running on that stack, and a
//! suspended generator's with its frame stood where a resumption stands
//! it. A frame popped is refused, and so is a suspended fiber's, which
//! no stack the request runs on holds.
//!
//! Both re-enter the request, so both take it whole, `&mut Request`,
//! and are called from a handler through the [`Observed`] it is handed,
//! or from a native.
//!
//! [`FrameView`]: super::FrameView
//! [`Observed`]: crate::api::abi::Observed

use crate::api::raising::Thrown;
use crate::api::values::{Owner, Value, adopted};

use super::Frames;
use super::inspect::Evaluation;
use super::{FrameId, Request};

impl Request {
    /// What [`eval_in_frame`](Self::eval_in_frame) makes, its one owner
    /// not yet named.
    fn eval_in_frame_made(
        &mut self,
        frame: FrameId,
        code: &[u8],
        name: &[u8],
    ) -> Option<Result<Value, Thrown>> {
        self.in_frame(frame, |req, at| {
            if req.is_pending() {
                return if req.diagnostics().bailing_out() {
                    Err(Thrown::of(None))
                } else {
                    Ok(Value::null())
                };
            }
            let evaluated = req.eval_returning(Some(at), code, name);
            let returned = std::mem::take(&mut req.returned);
            let served = match req.fatal_of(evaluated) {
                Ok(Evaluation::Served) if !req.is_pending() => return Ok(returned),
                Ok(_) => Err(Thrown::of(req.take_pending_view())),
                Err(thrown) => Err(thrown),
            };
            req.release(returned);
            served
        })
    }
}

impl Frames<&mut Request> {
    /// `zend_set_local_var(name, value, 1)`
    /// (`Z/Zend/zend_execute_API.c:1917`) with `EG(current_execute_data)`
    /// at the frame `frame` names: the variable `name` of that frame
    /// written with `value`, a sink whose owner it takes, in its symbol
    /// table where it has one, else in the compiled variable of that
    /// name, else in the table `zend_rebuild_symbol_table` builds for it;
    /// what stood there released. A variable that holds a reference is
    /// bound to `value` in its place, as php's `zend_hash_update_ind`
    /// binds it. `false`, and the value given up, where the frame is not
    /// one this reaches: popped, or a suspended fiber's.
    pub fn set_var(self, frame: FrameId, name: &[u8], value: Owner<Value>) -> bool {
        let mut value = Some(value);
        let set = self.0.in_frame(frame, |req, at| {
            value
                .take()
                .is_some_and(|value| req.set_local_var_in(at, name, value, true))
        });
        if let Some(value) = value {
            value.give_up(self.0);
        }
        set.unwrap_or(false)
    }

    /// `zend_eval_stringl(code, &retval, name)`
    /// (`Z/Zend/zend_execute_API.c:1333-1390`) with
    /// `EG(current_execute_data)` at the frame `frame` names: `return
    /// code;` compiled as an `eval` under the file name `name` and served
    /// in the frame's scope, reading and writing its variables, its
    /// `$this` and its class; what it returned, one owner of it the
    /// caller's. The `Err` is what it threw, the compiler's `ParseError`
    /// among them, taken from the pending slot as a native's call hands
    /// one back, or the `exit` or fatal it ended in. With something
    /// pending already nothing is served and the value is `null`, as
    /// `zend_execute` serves nothing while `EG(exception)` stands; a
    /// debugger that evaluates there sets what is pending aside first
    /// ([`Diagnostics::take_pending`]), as xdebug's `xdebug_do_eval` does. A
    /// backtrace taken in the compile or the evaluated code starts at the
    /// frame evaluated in and passes the frames above it, as php's
    /// `prev_execute_data` chain from the evaluated code does. `None`
    /// where the frame is not one this reaches: popped, or a suspended
    /// fiber's.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn eval_in_frame(
        self,
        frame: FrameId,
        code: &[u8],
        name: &[u8],
    ) -> Option<Result<Owner<Value>, Thrown>> {
        let made = self.0.eval_in_frame_made(frame, code, name)?;
        Some(adopted(made))
    }
}
