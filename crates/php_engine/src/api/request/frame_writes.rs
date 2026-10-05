//! Frame writes and evaluation in a paused frame for extensions (#1828,
//! design §4.3 seam 9): what a debugger written as an extension does
//! where it has stopped, as xdebug's `property_set` and `eval` do in
//! php. A variable of the frame is written ([`Request::set_frame_var`],
//! php's `zend_set_local_var`), and code is evaluated in the frame's
//! scope ([`Request::eval_in_frame`], php's `zend_eval_stringl` with
//! `EG(current_execute_data)` at the frame, as xdebug's `xdebug_do_eval`
//! serves it).
//!
//! The frame is named by its [`FrameId`], kept from a [`FrameView`]
//! (#1827), and checked where it stands each time. A frame that stands
//! on the running stack is paused: the innermost, whose call or native
//! the handler stands in, and every one below it. A frame on no stack,
//! popped or a suspended generator's or fiber's, is refused, as
//! [`Request::frame_of`] refuses it; so is a frame below a fiber, on the
//! stack that resumed it, which [`Request::frame_of`] reads but whose
//! slots and table this request cannot reach while the fiber's stack
//! runs (#2030).
//!
//! Both re-enter the request, so both take it whole, `&mut Request`,
//! and are called from a handler through the [`Observed`] it is handed,
//! or from a native.
//!
//! [`FrameView`]: super::FrameView
//! [`Observed`]: crate::api::abi::Observed

use crate::api::raising::Thrown;
use crate::api::values::Value;

use super::inspect::Evaluation;
use super::{FrameId, Request};

impl Request {
    /// `zend_set_local_var(name, value, 1)`
    /// (`Z/Zend/zend_execute_API.c:1917`) with `EG(current_execute_data)`
    /// at the frame `frame` names: the variable `name` of that frame
    /// written with `value`, one owner of which it takes, in its symbol
    /// table where it has one, else in the compiled variable of that
    /// name, else in the table `zend_rebuild_symbol_table` builds for it;
    /// what stood there released. A variable that holds a reference is
    /// bound to `value` in its place, as php's `zend_hash_update_ind`
    /// binds it. `false`, and the value released, where the frame is not
    /// one this reaches: popped, suspended, or below a fiber.
    pub fn set_frame_var(&mut self, frame: FrameId, name: &[u8], value: Value) -> bool {
        let Some(at) = self.running_frame_of(frame) else {
            self.release(value);
            return false;
        };
        self.set_local_var_in(at, name, value, true)
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
    /// `zend_execute` serves nothing while `EG(exception)` stands. `None`
    /// where the frame is not one this reaches: popped, suspended, or
    /// below a fiber.
    pub fn eval_in_frame(
        &mut self,
        frame: FrameId,
        code: &[u8],
        name: &[u8],
    ) -> Option<Result<Value, Thrown>> {
        let at = self.running_frame_of(frame)?;
        if self.is_pending() {
            return Some(if self.bailing_out() {
                Err(Thrown::of(None))
            } else {
                Ok(Value::null())
            });
        }
        let evaluated = self.eval_returning(Some(at), code, name);
        let returned = std::mem::replace(&mut self.returned, Value::null());
        let served = match self.fatal_of(evaluated) {
            Ok(Evaluation::Served) if !self.is_pending() => return Some(Ok(returned)),
            Ok(_) => Err(Thrown::of(self.take_pending())),
            Err(thrown) => Err(thrown),
        };
        self.release(returned);
        Some(served)
    }
}
