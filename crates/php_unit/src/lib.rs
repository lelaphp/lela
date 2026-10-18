//! The unit format (design §4.7, ADR-0001): the declaration model, op
//! arrays with their tables and safepoints, the constant-expression tree,
//! and the opcode enum generated from `ops.toml` with its effects and the
//! `safepoint` column. Types and their invariants only; the compiler and
//! the native macros produce these values and the linker (`php_link`,
//! ADR-0012) consumes them. `unit.lock` is this crate's public surface.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

pub mod decl;
mod dump;
mod enum_methods;
mod frameless;
mod op;
mod types;
mod unit;

mod opcode {
    //! The generated enums and their tables (`build.rs`).
    use crate::op::{Effects, Fused, OperandKind, Opline, data_kind};
    include!(concat!(env!("OUT_DIR"), "/opcode.rs"));
}

pub use enum_methods::enum_methods;
pub use frameless::{FramelessEntry, frameless_table};
pub use op::{
    BoundVar, CacheSlot, ConstIdx, Effects, LiveKind, LiveRange, MapPtrOffset, OpArray,
    OperandKind, Opline, OplineIdx, Safepoints, TryCatch,
};
pub use opcode::{Handler, Opcode};
pub use types::{type_to_string, type_to_string_called};
pub use unit::{AutoGlobal, Diagnostic, HookBody, Level, MethodBody, TopLevel, Unit, UnitCell};
