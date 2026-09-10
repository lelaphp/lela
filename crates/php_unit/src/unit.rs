//! A unit (design §4.7): one compiled file, immutable once published, its
//! declarations beside their op arrays.

use std::fmt::Write as _;

use crate::decl::{ClassDecl, ConstDecl, FunctionDecl};
use crate::op::{OpArray, OperandKind};

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
    /// The op arrays in php's dump format (`opcache.opt_debug_level=0x10000`),
    /// what `compile-identity` compares with the oracle's: one line per
    /// opline, operands printed by the opcode's kinds and an unused field
    /// not printed.
    #[must_use]
    pub fn dump(&self) -> String {
        let mut out = String::new();
        dump_op_array(&mut out, "$_main", &self.main);
        for (decl, body) in &self.functions {
            let _ = writeln!(out);
            dump_op_array(&mut out, &String::from_utf8_lossy(&decl.name.0), body);
        }
        out
    }
}

fn dump_op_array(out: &mut String, name: &str, body: &OpArray) {
    let _ = writeln!(
        out,
        "{name}:\n     ; (lines={}, args=0, vars={}, tmps={})",
        body.oplines.len(),
        body.num_cvs,
        body.num_tmps
    );
    for (i, op) in body.oplines.iter().enumerate() {
        let _ = write!(
            out,
            "{i:04} {}",
            op.opcode.name().trim_start_matches("ZEND_")
        );
        for (kind, value) in [(op.op1_type, op.op1), (op.op2_type, op.op2)] {
            match kind {
                OperandKind::Unused => {}
                OperandKind::Const => {
                    let _ = write!(out, " CONST[{value}]");
                }
                OperandKind::Tmp => {
                    let _ = write!(out, " T{value}");
                }
                OperandKind::Var => {
                    let _ = write!(out, " V{value}");
                }
                OperandKind::Cv => {
                    let _ = write!(out, " CV{value}");
                }
            }
        }
        let _ = writeln!(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Opcode, Opline};

    #[test]
    fn an_empty_unit_dumps_its_main_op_array() {
        let mut unit = Unit::default();
        unit.main.oplines = vec![Opline {
            opcode: Opcode::Return,
            op1_type: OperandKind::Const,
            op1: 0,
            ..Opline::default()
        }]
        .into_boxed_slice();
        assert_eq!(
            unit.dump(),
            "$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n0000 RETURN CONST[0]\n"
        );
    }
}
