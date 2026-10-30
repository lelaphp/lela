//! The opcodes phpdbg tells apart, by php's names: the native face hands
//! an opline's opcode out as its `zend_get_opcode_name` name and its
//! number (#2357), as php's own phpdbg compares `opline->opcode` with
//! `ZEND_RETURN` and its kin. Each set is one list here, and a test holds
//! every name in it to an opcode the face answers for, so a misspelt name
//! fails a test rather than never matching.

use php_ext::request::Opcode;

/// The returns from an op array: what the compiler lays down last in
/// every one, which `phpdbg_oplog_fill_executable` leaves out.
pub(crate) const RETURNS: &[&str] = &["ZEND_RETURN", "ZEND_RETURN_BY_REF", "ZEND_GENERATOR_RETURN"];

/// The oplines that pop the frame they stand in, which on the frame a
/// serving entered are the loop's to step.
pub(crate) const LEAVES: &[&str] = &[
    "ZEND_RETURN",
    "ZEND_RETURN_BY_REF",
    "ZEND_GENERATOR_RETURN",
    "ZEND_YIELD",
    "ZEND_YIELD_FROM",
];

/// `ZEND_RETURN`, `ZEND_FAST_RET`, `ZEND_GENERATOR_RETURN`, `ZEND_YIELD`
/// and `ZEND_YIELD_FROM`, where a seek to a function's end stops.
pub(crate) const ENDS: &[&str] = &[
    "ZEND_RETURN",
    "ZEND_FAST_RET",
    "ZEND_GENERATOR_RETURN",
    "ZEND_YIELD",
    "ZEND_YIELD_FROM",
];

/// The `RECV*` oplines php's compiler opens a function with, one per
/// parameter, a variadic one's included.
pub(crate) const RECEIVES: &[&str] = &["ZEND_RECV", "ZEND_RECV_INIT", "ZEND_RECV_VARIADIC"];

/// The `RECV` oplines of the parameters before a variadic one.
pub(crate) const DECLARED: &[&str] = &["ZEND_RECV", "ZEND_RECV_INIT"];

/// `ZEND_NEW`, whose constructor call counts with it.
pub(crate) const NEW: &[&str] = &["ZEND_NEW"];

/// `ZEND_DO_FCALL`, the constructor call a `NEW` is followed by.
pub(crate) const DO_FCALL: &[&str] = &["ZEND_DO_FCALL"];

/// `phpdbg_is_ignored_opcode` (`Z/sapi/phpdbg/phpdbg.c:423-433`).
pub(crate) const IGNORED: &[&str] = &[
    "ZEND_NOP",
    "ZEND_OP_DATA",
    "ZEND_FE_FREE",
    "ZEND_FREE",
    "ZEND_ASSERT_CHECK",
    "ZEND_VERIFY_RETURN_TYPE",
    "ZEND_DECLARE_CONST",
    "ZEND_DECLARE_ATTRIBUTED_CONST",
    "ZEND_DECLARE_CLASS",
    "ZEND_DECLARE_FUNCTION",
    "ZEND_DECLARE_CLASS_DELAYED",
    "ZEND_DECLARE_ANON_CLASS",
    "ZEND_FAST_RET",
    "ZEND_TICKS",
    "ZEND_EXT_STMT",
    "ZEND_EXT_FCALL_BEGIN",
    "ZEND_EXT_FCALL_END",
    "ZEND_BIND_GLOBAL",
    "ZEND_BIND_INIT_STATIC_OR_JMP",
];

/// Whether `opcode` is one of `set`.
pub(crate) fn is(opcode: Opcode, set: &[&str]) -> bool {
    set.contains(&opcode.name())
}

#[cfg(test)]
mod tests {
    use super::{DECLARED, DO_FCALL, ENDS, IGNORED, LEAVES, NEW, RECEIVES, RETURNS};
    use php_ext::request::Opcode;

    #[test]
    fn every_name_is_an_opcodes() {
        let names: Vec<&str> = (0..=u8::MAX)
            .filter_map(|n| Opcode::try_from(n).ok())
            .map(Opcode::name)
            .collect();
        let sets = [
            RETURNS, LEAVES, ENDS, RECEIVES, DECLARED, NEW, DO_FCALL, IGNORED,
        ];
        for name in sets.into_iter().flatten() {
            assert!(names.contains(name), "{name} is no opcode's name");
        }
    }
}
