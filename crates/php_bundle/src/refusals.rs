//! What the native attributes refuse at build time (design §7.1),
//! each a `compile_fail` block, the type-level ones pinned to their
//! error code. Here and not in `php_macros` because the expansion names
//! the facade, which `php_macros` does not depend on (ADR-0007). Compiled
//! under `cfg(doctest)` alone. A tentative bit on a function:
//!
//! ```compile_fail
//! # use php_ext::attr::php_function;
//! #[php_function(tentative)]
//! fn f() -> i64 { 0 }
//! # fn main() {}
//! ```
//!
//! On a `final` method:
//!
//! ```compile_fail
//! # use php_ext::attr::php_class;
//! # use php_ext::abi::This;
//! #[php_class]
//! pub struct C;
//! #[php_class]
//! impl C {
//!     #[php(final, tentative)]
//!     fn f(this: This<Self>) -> i64 { 0 }
//! }
//! # fn main() {}
//! ```
//!
//! On a method whose return declares nothing:
//!
//! ```compile_fail,E0080
//! # use php_ext::attr::php_class;
//! # use php_ext::abi::{This, Untyped};
//! # use php_ext::values::Value;
//! #[php_class]
//! pub struct C;
//! #[php_class]
//! impl C {
//!     #[php(tentative)]
//!     fn f(this: This<Self>) -> Untyped<Value> { Untyped(Value::null()) }
//! }
//! # fn main() {}
//! ```
//!
//! A body-less method in a class that is neither an interface nor
//! abstract:
//!
//! ```compile_fail,E0080
//! # use php_ext::attr::php_class;
//! # use php_ext::abi::This;
//! #[php_class]
//! pub struct C;
//! #[php_class]
//! impl C {
//!     fn f(this: This<Self>) -> i64;
//! }
//! # fn main() {}
//! ```
//!
//! A class registered without its member impl:
//!
//! ```compile_fail,E0277
//! # use php_ext::attr::php_class;
//! # use php_ext::abi::ExtensionBuilder;
//! #[php_class]
//! pub struct C;
//! fn main() { let _ = ExtensionBuilder::default().class::<C>(); }
//! ```
//!
//! An unknown key, and a key where it does not belong:
//!
//! ```compile_fail
//! # use php_ext::attr::php_function;
//! #[php_function]
//! fn f(#[php(defualt = 1)] x: i64) -> i64 { x }
//! # fn main() {}
//! ```
//!
//! ```compile_fail
//! # use php_ext::attr::php_function;
//! #[php_function]
//! fn f(#[php(protected)] x: i64) -> i64 { x }
//! # fn main() {}
//! ```
//!
//! A type php refuses, `?mixed`, `bool|false` and a member twice:
//!
//! ```compile_fail,E0080
//! # use php_ext::attr::php_function;
//! # use php_ext::values::Value;
//! #[php_function]
//! fn f(x: Option<Value>) -> i64 { 0 }
//! # fn main() {}
//! ```
//!
//! ```compile_fail,E0080
//! # use php_ext::attr::php_function;
//! # use php_ext::abi::OrFalse;
//! #[php_function]
//! fn f() -> OrFalse<bool> { OrFalse::False }
//! # fn main() {}
//! ```
//!
//! ```compile_fail,E0080
//! # use php_ext::attr::php_function;
//! # use php_ext::abi::Union;
//! #[php_function]
//! fn f(x: Union<i64, i64>) -> i64 { 0 }
//! # fn main() {}
//! ```
//!
//! `default = null` on a type that admits no `null`:
//!
//! ```compile_fail,E0277
//! # use php_ext::attr::php_function;
//! #[php_function]
//! fn f(#[php(default = null)] x: i64) -> i64 { x }
//! # fn main() {}
//! ```
//!
//! A pointer into a request's heap held in engine state (ADR-0005, ruled
//! 2026-11-09), the `engine-state` rule's compile-fail fixtures under
//! `xtask/fixtures/api-check/engine-state/`: a type derived over a field
//! holding a request's value, refused at the field,
//!
//! ```compile_fail,E0277
//! # fn main() {}
//! include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../xtask/fixtures/api-check/engine-state/handle.rs"));
//! ```
//!
//! and one with no derive handed to the engine start, refused by the
//! entry's bound:
//!
//! ```compile_fail,E0277
//! # fn main() {}
//! include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../xtask/fixtures/api-check/engine-state/entry.rs"));
//! ```
//!
//! A class identity held in engine state is the `EngineClassId` an
//! extension's start resolves, and a type derived over a field
//! holding a `ClassId`, which may be a class of one request's own, is
//! refused at the field:
//!
//! ```compile_fail,E0277
//! # fn main() {}
//! include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../xtask/fixtures/api-check/engine-state/class.rs"));
//! ```
