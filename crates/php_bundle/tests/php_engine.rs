//! The `CARGO_CRATE_NAME` path switch, pinned (ADR-0007):
//! this test crate is named `php_engine`, so the expansion names
//! `crate::api` and not `::php_ext`. Here `crate::api` is the facade with
//! one piece changed, its static entry a type of this file's own, so a
//! switch that stops firing, or a rename of `php_engine`, fails this test
//! to build.

mod api {
    pub use php_ext::{abi, decl, raising, request, values};

    pub mod __private {
        pub use php_ext::__private::*;

        /// The facade's static entry, as a type of this file's own, which
        /// the expansion lays down only where it names `crate::api`.
        #[allow(dead_code)] // the fields the expansion writes, which no test reads
        pub struct NativeFunction {
            pub name: &'static [u8],
            pub parameters: &'static [NativeParameter],
            pub return_type: fn() -> Option<super::decl::Type>,
            pub frameless: &'static [u32],
            pub attributes: &'static [fn() -> super::decl::Attribute],
            pub entry: Entry,
        }
    }
}

use php_ext::attr::php_function;

#[php_function]
fn marked(#[php(default = 2)] n: i64) -> i64 {
    n
}

#[test]
fn the_expansion_names_crate_api_inside_php_engine() {
    let entry: &api::__private::NativeFunction = &marked::DECLARED;
    assert_eq!(entry.name, b"marked");
}
