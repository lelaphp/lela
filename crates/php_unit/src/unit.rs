//! A unit (design §4.7): one compiled file, immutable once published, its
//! declarations beside their op arrays.

use crate::decl::{ClassDecl, ConstDecl, FunctionDecl};
use crate::op::OpArray;

/// A compiled PHP program: the main op array, the functions with their
/// bodies at the same index, the classes and the constants declared.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Unit {
    pub main: OpArray,
    pub functions: Vec<(FunctionDecl, OpArray)>,
    pub classes: Vec<ClassDecl>,
    pub constants: Vec<ConstDecl>,
}

impl Unit {
    /// The op arrays in php's dump format, what `zend_dump_op_array`
    /// prints under `opcache.opt_debug_level=0x10000` and what
    /// `compile-identity` compares with the oracle's (design §6.1):
    /// one block per op array, the header's four lines, one line per
    /// opline with the operand `result` names opening it, `T1 = `, when its
    /// kind is `TMP`, `VAR` or `CV`, its constants, variables and jump
    /// targets printed as php prints them, and the `try`/`catch` table.
    #[must_use]
    pub fn dump(&self) -> String {
        crate::dump::unit(self)
    }
}
