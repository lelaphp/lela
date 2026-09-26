//! The `CARGO_CRATE_NAME` path switch, pinned (ADR-0007, #947 story 58):
//! this test crate is named `php_engine`, so the expansion names
//! `crate::api` and not `::php_ext`. Here `crate::api` is the facade with
//! one piece changed, a declaration line the facade never writes, so a
//! switch that stops firing, or a rename of `php_engine`, fails this test
//! by name.

mod api {
    pub use php_ext::{abi, decl, raising, request, values};

    pub mod __private {
        pub use php_ext::__private::*;

        /// The facade's, marked.
        pub fn native_function(name: &[u8]) -> php_ext::decl::FunctionDecl {
            let mut declaration = php_ext::__private::native_function(name);
            declaration.line = 7;
            declaration
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
    let (declaration, _) = marked::declared();
    assert_eq!(declaration.line, 7);
    assert_eq!(&*declaration.name.0, b"marked");
}
