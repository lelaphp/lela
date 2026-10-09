//! The bindings deny nothing: the module and the file under it are named.

mod inner;

pub(crate) fn one() -> u8 {
    inner::one()
}
