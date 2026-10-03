//! The kernels: one with a witness, and one without, which the rule fails
//! on.

/// A kernel with its own witness.
///
/// ```php
/// var_dump(1 + 2);
/// ```
/// ```expect
/// int(3)
/// ```
pub(crate) fn add() {}

/// A kernel with none: the subject the rule fails on.
pub(crate) fn sub() {}

/// A private function beside them is no kernel.
fn private_helper() {}
