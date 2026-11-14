//! The modules of `state.engine`'s `allow` (#4633, #4647), in a crate no
//! seam names, so the fixture is read for the seal alone: a hand impl in
//! `frozen` passes under an `allow` naming `php_syntax::frozen`, as
//! php_unit's will under the row #4648 lands, and the one in `thawed`, a
//! module no entry names, fails as it does with no `allow` at all, beside
//! the line in `frozen` the entry does not admit.

mod frozen;
mod thawed;
