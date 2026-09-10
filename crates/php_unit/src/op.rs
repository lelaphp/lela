//! Oplines and op arrays (design §5.7): php's instruction set exactly, as
//! `zend_compile.c` and `pass_two` leave it, with the `safepoints` table
//! the compiler numbers beside the line table.

use php_heap::Zval;

use crate::Opcode;
use crate::decl::Name;
use crate::opcode;

/// An index into an op array's oplines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct OplineIdx(pub u32);

/// An index into an op array's `literals`, what a `CONST` operand holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ConstIdx(pub u32);

/// An index into the op array's runtime cache, what an opline's `u2`
/// holds for a fetch or a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CacheSlot(pub u32);

/// What an operand field holds, numbered as `IS_*` in `zend_compile.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum OperandKind {
    #[default]
    Unused = 0,
    Const = 1,
    Tmp = 2,
    Var = 4,
    Cv = 8,
}

/// The effects `ops.toml` states for an opcode: what the dispatch loop
/// must do after it (a pending-throwable test), and what the tier may not
/// move across.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Effects(pub(crate) u8);

impl Effects {
    pub const NONE: Effects = Effects(0);
    /// `zend_may_throw` says so.
    pub const THROW: Effects = Effects(1);
    /// The transitive body allocates.
    pub const ALLOC: Effects = Effects(2);
    /// Control enters a callee frame or user code.
    pub const CALL: Effects = Effects(4);
    /// The global symbol table is written.
    pub const GLOBAL_WRITE: Effects = Effects(8);

    /// Whether every effect of `other` is one of these.
    #[must_use]
    pub const fn contains(self, other: Effects) -> bool {
        self.0 & other.0 == other.0
    }
}

impl Opcode {
    /// php's name, `ZEND_ADD`.
    #[must_use]
    pub fn name(self) -> &'static str {
        opcode::NAMES
            .get(usize::from(self.num()))
            .copied()
            .unwrap_or("")
    }

    /// The opcode with php's number, or `None` for a number no opcode has.
    #[must_use]
    pub fn from_u16(n: u16) -> Option<Opcode> {
        opcode::BY_NUM.get(usize::from(n)).copied().flatten()
    }

    /// php's number.
    #[must_use]
    pub fn num(self) -> u16 {
        self as u16
    }

    /// The effects `ops.toml` states.
    #[must_use]
    pub fn effects(self) -> Effects {
        Effects(
            opcode::EFFECTS
                .get(usize::from(self.num()))
                .copied()
                .unwrap_or(0),
        )
    }

    /// Whether some handler body reads the opline's `result` operand.
    #[must_use]
    pub fn out_used(self) -> bool {
        opcode::RESULT_USED
            .get(usize::from(self.num()))
            .copied()
            .unwrap_or(false)
    }

    /// Whether the opcode is a safepoint site whose `result` field, `out`
    /// here, carries the safepoint's number: the backward jumps and the returns.
    #[must_use]
    pub fn is_safepoint_site(self) -> bool {
        opcode::SAFEPOINT
            .get(usize::from(self.num()))
            .copied()
            .unwrap_or(false)
    }

    /// How many opcodes php has.
    pub const COUNT: usize = opcode::COUNT;
}

/// One instruction, php's 24-byte `zend_op` field for field; `out` is
/// php's `result`, spelled so because the glossary keeps that word for
/// `Result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Opline {
    pub opcode: Opcode,
    pub op1_type: OperandKind,
    pub op2_type: OperandKind,
    pub op1: u32,
    pub op2: u32,
    pub out: u32,
    pub extended_value: u32,
    pub lineno: u32,
}

impl Default for Opline {
    fn default() -> Opline {
        Opline {
            opcode: Opcode::Nop,
            op1_type: OperandKind::Unused,
            op2_type: OperandKind::Unused,
            op1: 0,
            op2: 0,
            out: 0,
            extended_value: 0,
            lineno: 0,
        }
    }
}

/// One `try`/`catch`/`finally` range of an op array (`zend_try_catch_element`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TryCatch {
    pub try_op: OplineIdx,
    pub catch_op: OplineIdx,
    pub finally_op: OplineIdx,
    pub finally_end: OplineIdx,
}

/// The oplines over which a temporary is live (`zend_live_range`), for
/// the unwinder to free it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LiveRange {
    pub var: u32,
    pub start: OplineIdx,
    pub end: OplineIdx,
}

/// One op array with its tables (design §5.7). `safepoints` is the entry,
/// the loop headers and the returns, the entry number zero; the site
/// opcodes carry their number in `out`, php's `result`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpArray {
    pub oplines: Box<[Opline]>,
    pub literals: Box<[Zval]>,
    pub vars: Box<[Name]>,
    pub num_cvs: u32,
    pub num_tmps: u32,
    pub try_catch: Box<[TryCatch]>,
    pub live_ranges: Box<[LiveRange]>,
    pub safepoints: Box<[OplineIdx]>,
    pub filename: Name,
    pub line_start: u32,
    pub line_end: u32,
    pub cache_size: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_opline_is_twenty_four_bytes_and_the_enum_keeps_php_numbers() {
        assert_eq!(std::mem::size_of::<Opline>(), 24);
        assert_eq!(Opcode::Nop.num(), 0);
        assert_eq!(Opcode::Add.num(), 1);
        assert_eq!(Opcode::Add.name(), "ZEND_ADD");
        assert_eq!(Opcode::from_u16(1), Some(Opcode::Add));
        assert_eq!(
            Opcode::from_u16(45),
            None,
            "45 is one of php's unused numbers"
        );
        assert_eq!(Opcode::COUNT, 209);
        assert!(Opcode::Add.effects().contains(Effects::THROW));
        assert!(Opcode::Add.effects().contains(Effects::ALLOC));
        assert!(!Opcode::Nop.effects().contains(Effects::THROW));
        assert!(Opcode::Add.out_used());
        assert!(Opcode::Return.is_safepoint_site());
        assert!(Opcode::Jmpnz.is_safepoint_site());
        assert!(!Opcode::Add.is_safepoint_site());
    }
}
