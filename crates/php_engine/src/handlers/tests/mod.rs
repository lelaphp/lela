//! handlers.rs's tests, one file per concern, and what they share:
//! the `served!` edge of a handler called outside the loop, a constant
//! interned into a unit's table and the message of a pending throwable.

/// The edge a handler called outside the loop served, as
/// [`crate::tier::step`] hands it out: the position the loop would
/// have held written back to the frame, which the handler was
/// entered at as the dispatch enters it.
macro_rules! served {
    ($req:ident, $frame:expr, $at:ident => $call:expr) => {{
        let $at = $req.vm().opline($frame);
        let edge = $call;
        edge.map(|next| crate::tier::settled(&mut $req, $frame, $at, next))
    }};
}
use std::sync::Arc;

use php_heap::Intern as _;
use php_unit::{Opcode, OperandKind, PublishedUnit, Unit};

use super::*;
use crate::api::request::Leaving;
use crate::engine::{Engine, RequestDescription};
use crate::tier::Nop;

mod class_slots;
mod frameless;
mod function_slots;
mod object_walk;
mod property_slots;
mod safepoints;
mod writes;

/// A `RETURN`, which a fixture's op array ends on so that no opline goes
/// on past the last, as publication holds it to.
fn back() -> Opline {
    Opline {
        opcode: Opcode::Return,
        ..Opline::default()
    }
}

/// Interned `bytes` as a constant.
fn interned(strings: &mut php_heap::Strings, bytes: &[u8]) -> php_heap::Zval {
    php_heap::Zval::string(strings.intern(bytes).expect("intern a constant"))
}

/// The message of the throwable pending, empty where none is.
fn pending_message(req: &Request) -> String {
    req.pending()
        .and_then(|throwable| throwable.prop(req, b"message"))
        .and_then(|message| message.as_str_bytes().map(<[u8]>::to_vec))
        .map(|message| String::from_utf8_lossy(&message).into_owned())
        .unwrap_or_default()
}
