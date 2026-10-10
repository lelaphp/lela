//! The enabled set, stood in for here: the items the template names and
//! nothing behind them (#2524).
#![forbid(unsafe_code)]

/// The enabled set, empty.
pub fn enabled() -> &'static [fn()] {
    &[]
}

/// The `configure` switches, none.
pub const CONFIGURE: &[&str] = &[];

/// The compiled tier, none.
pub type Tier = ();
