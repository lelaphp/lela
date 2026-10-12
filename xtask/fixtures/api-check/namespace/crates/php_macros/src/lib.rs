//! A module and a function of one name, which rustc keeps in two
//! namespaces (#2791): `php_ext`'s `use` of the name brings in the
//! function, the module being private.
mod marker;

pub fn marker() {}
