//! The facade: a page of re-exports (ADR-0004).
pub mod abi {
    pub use php_engine::api::abi::*;
}
pub use php_engine::api::raising;
pub use php_engine::api::raising::Level as Severity;
