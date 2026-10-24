//! What compiled code calls for the strings it builds, where php's code
//! calls a function of its own (`jit-design.md` §4.3): a rope's pieces and
//! its join (`zend_jit_rope`, `zend_jit_rope_end`,
//! `Z/ext/opcache/jit/zend_jit_ir.c:11753-11799`,
//! `Z/ext/opcache/jit/zend_jit_helpers.c:3442-3463`), and `.=` of a
//! string onto a variable's string (`zend_jit_fast_assign_concat_helper`,
//! `:1788-1822`), and a concatenation of two strings
//! (`zend_jit_fast_concat_helper`, `_tmp_helper`, `:1824-1880`). Each
//! serves through the one place the interpreter's handler serves through,
//! the request's ropes ([`Request::rope_push`], [`Request::rope_end`]),
//! [`crate::ops::concat_assign`] and [`crate::ops::concat_strings`]; a rope's
//! piece is handed with its owner, as the handler's string arm takes one:
//! a temporary's moved, a variable's or a constant's with one owner more.
//! None runs user code; a rope's reads no frame, and compiled code stood
//! the frame at the opline first where what it serves may raise.

use php_heap::{Fatal, Zval, ZvalRef};
use php_unit::OperandKind;

use crate::api::request::Request;
use crate::api::request::ropes::RopeAt;
use crate::api::values::{PhpString, Value};

/// The string `piece` holds, which compiled code handed with its owner.
fn piece_of(piece: Zval) -> Result<PhpString, Fatal> {
    piece.as_str().map(PhpString::of).ok_or_else(|| {
        Fatal::error("InternalError: compiled code handed a rope a piece that is no string")
    })
}

/// The rope a rope's temporary holds, the position `ROPE_INIT` wrote.
fn rope_at(rope: i64) -> Result<RopeAt, Fatal> {
    RopeAt::of(rope)
        .ok_or_else(|| Fatal::error("InternalError: compiled code handed a rope no rope holds"))
}

/// `ROPE_INIT` over a string, `piece` with its owner: a rope opened with
/// it as its first piece, and its position, which the rope's temporary
/// holds, as the handler writes it.
///
/// # Errors
///
/// A piece that is no string, which no compiled code hands.
pub fn rope_open(req: &mut Request, piece: Zval) -> Result<i64, Fatal> {
    let piece = piece_of(piece)?;
    let rope = req.rope_open();
    req.rope_push(rope, piece);
    rope.value()
        .as_int()
        .ok_or_else(|| Fatal::error("InternalError: a rope's position is no integer"))
}

/// `ROPE_ADD` over a string, `piece` with its owner, onto the rope at
/// `rope`, as the handler pushes one.
///
/// # Errors
///
/// A piece that is no string or a position no rope holds, which no
/// compiled code hands.
pub fn rope_push(req: &mut Request, rope: i64, piece: Zval) -> Result<(), Fatal> {
    let piece = piece_of(piece)?;
    req.rope_push(rope_at(rope)?, piece);
    Ok(())
}

/// `ROPE_END` over a string, `piece` with its owner: the last piece onto
/// the rope at `rope` and the rope joined ([`Request::rope_end`]), the
/// joined string with its owner; none where the join refused, the
/// memory limit or php's `String size overflow`, which is left pending
/// as the handler leaves it, the rope given back.
///
/// # Errors
///
/// A piece that is no string or a position no rope holds, which no
/// compiled code hands.
pub fn rope_end(req: &mut Request, rope: i64, piece: Zval) -> Result<Option<Zval>, Fatal> {
    let piece = piece_of(piece)?;
    let rope = rope_at(rope)?;
    req.rope_push(rope, piece);
    Ok(match req.rope_end(rope) {
        Ok(joined) => Some(Zval::string(joined.heap())),
        Err(thrown) => {
            req.leave_pending(&thrown);
            None
        }
    })
}

/// `.=` over `place`, a variable or an array's element that holds a
/// string and no reference, and `right`, a string compiled code hands
/// with no owner and gives up after, as the handler's `.=` serves it
/// ([`crate::ops::concat_assign`]): the string grown in place where
/// `place` is its one owner, as `zend_string_extend` grows it, and a new
/// string otherwise; what the join refuses, the memory limit or php's
/// `String size overflow`, left pending with the string put back where it
/// stood. Nothing but the join runs between the writes to `place`, so the
/// table an element stands in is not written under it.
pub fn assign_concat(req: &mut Request, place: ZvalRef, right: Zval) {
    let left = Value::of_slot(place.read());
    let right = Value::of_slot(right);
    // `i_zval_ptr_dtor(result); ZVAL_NULL(result)` and its undoing, over
    // a place that holds no reference.
    let put = |req: &mut Request, value: Value| {
        let held = Value::of_slot(place.read());
        place.write(value.to_zval());
        req.release(held);
    };
    let computed = crate::ops::concat_assign(
        req,
        &left,
        &right,
        |req| put(req, Value::null()),
        |req, left| put(req, left),
    );
    match computed {
        Ok(joined) => put(req, joined),
        Err(thrown) => req.leave_pending(&thrown),
    }
}

/// `CONCAT` or `FAST_CONCAT` of two strings, `left` of `left_kind`, with
/// its owner where it is a temporary compiled code moved out of its slot
/// and a view otherwise, and `right` a view, whose temporary compiled
/// code gives up after: [`crate::ops::concat_strings`], as the handler
/// joins them, the joined string with its owner; none where the join
/// refused, the memory limit or php's `String size overflow`, which is
/// left pending as the handler leaves it.
///
/// # Errors
///
/// A side that is no string, which no compiled code hands.
pub fn concat(
    req: &mut Request,
    (left, left_kind): (Zval, OperandKind),
    (right, right_kind): (Zval, OperandKind),
) -> Result<Option<Zval>, Fatal> {
    let strings = left.as_str().zip(right.as_str()).ok_or_else(|| {
        Fatal::error("InternalError: compiled code joined a side that is no string")
    })?;
    let (a, b) = (PhpString::of(strings.0), PhpString::of(strings.1));
    Ok(
        match crate::ops::concat_strings(req, (a, left_kind), (b, right_kind)) {
            Ok(joined) => Some(Zval::string(joined.heap())),
            Err(thrown) => {
                req.leave_pending(&thrown);
                None
            }
        },
    )
}
