//! The table a frameless call's `extended_value` indexes,
//! `zend_flf_functions` and `zend_flf_handlers` as `zend_register_functions`
//! fills them (`Z/Zend/zend_API.c:3115-3133`): one row per arity each
//! native declares, in registration order, the natives in the order the
//! bundle registers them and a native's arities in the order its
//! declaration lists them. The compiler numbers a `FRAMELESS_ICALL_<n>`
//! by this table (`find_frameless_function_offset`) and the engine
//! reads the number back through it (`ZEND_FLF_FUNC`, `ZEND_FLF_HANDLER`),
//! so it is computed here, below both, from the one list both hold.

use crate::decl::FunctionDecl;

/// One row of `zend_flf_functions`: the native at `function` in the
/// list the table was computed from, and the arity of the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramelessEntry {
    pub function: u32,
    pub arity: u32,
}

/// The table over `functions` in their order, each declaration's
/// `frameless` arities appended in theirs.
#[must_use]
pub fn frameless_table<'a>(
    functions: impl IntoIterator<Item = &'a FunctionDecl>,
) -> Vec<FramelessEntry> {
    functions
        .into_iter()
        .enumerate()
        .flat_map(|(at, decl)| {
            let function = u32::try_from(at).unwrap_or(u32::MAX);
            decl.frameless
                .iter()
                .map(move |&arity| FramelessEntry { function, arity })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decl::native_function;

    fn declared(name: &str, frameless: &[u32]) -> FunctionDecl {
        FunctionDecl {
            frameless: frameless.to_vec(),
            ..native_function(name.as_bytes())
        }
    }

    /// `trim`'s two rows come before `str_contains`'s one when `trim` is
    /// registered first, and a native with no entry takes no row.
    #[test]
    fn the_table_is_in_registration_order_with_arities_in_declaration_order() {
        let functions = [
            declared("strlen", &[]),
            declared("trim", &[1, 2]),
            declared("str_contains", &[2]),
        ];
        let table = frameless_table(&functions);
        assert_eq!(
            table,
            vec![
                FramelessEntry {
                    function: 1,
                    arity: 1
                },
                FramelessEntry {
                    function: 1,
                    arity: 2
                },
                FramelessEntry {
                    function: 2,
                    arity: 2
                },
            ]
        );
    }
}
