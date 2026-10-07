//! A proc-macro crate with a test that passes.

use proc_macro::TokenStream;

/// The item, as it was written.
#[proc_macro_attribute]
pub fn as_written(_: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_test_of_a_proc_macro_crate_runs() {
        assert_eq!(1 + 1, 2);
    }
}
