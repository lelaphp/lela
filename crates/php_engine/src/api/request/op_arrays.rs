//! Op-array reads for extensions (#1829, design §4.3 seam 9): a
//! function's op array as php's `zend_op_array` hands it to an observer,
//! a profiler or a coverage tool, read through an [`OpArrayView`]: how
//! many oplines it holds, each one's opcode, operands, line and jump
//! targets, the constants a `CONST` operand names, the name of each
//! compiled variable, and its file and lines.
//!
//! An op array is the published unit's (design §5.14): immutable once
//! the unit is published and shared by every request that serves it, so
//! a read needs no copy and no check, and nothing on this face writes
//! one (design §7.1, revision 83: no extension rewrites a published
//! unit). The view borrows the request, which holds the unit, so it
//! lives no longer than the request's own share of it; what is kept
//! past the borrow is copied out of it, as php's `xdebug` and `pcov`
//! copy lines out of `op_array->opcodes` into tables of their own.
//!
//! The view is reached from a frame, [`FrameView::op_array`], which
//! serves a function's op array or an included, evaluated or main
//! file's, and from a function, [`Request::op_array`], which answers
//! none for a native, as php's `ZEND_USER_CODE(func->type)` does.
//!
//! The debugging host's reads of the same op arrays are the tier's
//! ([`crate::tier::Code`]), copies taken by name; whether that host
//! reads this face instead is #1911's question, and this face does not
//! answer it.
//!
//! [`FrameView::op_array`]: super::FrameView::op_array

use php_unit::{OpArray, OperandKind, Opline};

pub use php_unit::{ConstIdx, Opcode, OplineIdx};

use crate::api::call::Function;
use crate::api::values::Value;
use crate::exec;

use super::Request;

/// One op array, read: php's `zend_op_array` as a user function, a
/// method, a closure or an included, evaluated or main file holds it.
#[derive(Clone, Copy)]
pub struct OpArrayView<'a> {
    code: &'a OpArray,
}

impl std::fmt::Debug for OpArrayView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpArrayView")
            .field("filename", &String::from_utf8_lossy(self.filename()))
            .field("oplines", &self.len())
            .finish_non_exhaustive()
    }
}

impl<'a> OpArrayView<'a> {
    pub(crate) fn of(code: &'a OpArray) -> OpArrayView<'a> {
        OpArrayView { code }
    }

    /// `op_array->last`: how many oplines it holds, the compiler's own
    /// `RETURN` at the end among them.
    #[must_use]
    pub fn len(&self) -> usize {
        self.code.oplines.len()
    }

    /// Whether it holds no opline, which no op array the compiler makes
    /// is.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.code.oplines.is_empty()
    }

    /// `op_array->opcodes[at]`; none past the last.
    #[must_use]
    pub fn opline(&self, at: OplineIdx) -> Option<OplineView<'a>> {
        let op = self.code.oplines.get(at.0 as usize)?;
        Some(OplineView {
            code: self.code,
            op,
            at,
        })
    }

    /// Every opline in order, from `op_array->opcodes` to
    /// `op_array->last`.
    pub fn oplines(&self) -> impl ExactSizeIterator<Item = OplineView<'a>> + use<'a> {
        let code = self.code;
        code.oplines
            .iter()
            .enumerate()
            .map(move |(at, op)| OplineView {
                code,
                op,
                // An op array's oplines are counted in `u32`, as php's
                // `op_array->last` is.
                at: OplineIdx(u32::try_from(at).unwrap_or(u32::MAX)),
            })
    }

    /// `RT_CONSTANT(opline, node)` over `op_array->literals`: the
    /// constant a `CONST` operand names ([`Operand::Const`]), a view of
    /// the unit's own value, which a copy keeps past the view; none for
    /// an index past the table.
    #[must_use]
    pub fn constant(&self, at: ConstIdx) -> Option<Value> {
        self.code
            .literals
            .get(at.0 as usize)
            .map(|zval| Value::of_zval(*zval))
    }

    /// `op_array->vars[cv]`: the name, without its `$`, of the compiled
    /// variable a `CV` operand names ([`Operand::Cv`]); none past
    /// `op_array->last_var`.
    #[must_use]
    pub fn variable(&self, cv: u32) -> Option<&'a [u8]> {
        self.code.vars.get(cv as usize).map(|name| &*name.0)
    }

    /// `op_array->filename`: the file it was compiled from, `… : eval()'d
    /// code` for an evaluated string's.
    #[must_use]
    pub fn filename(&self) -> &'a [u8] {
        &self.code.filename.0
    }

    /// `op_array->line_start` and `op_array->line_end`: the lines its
    /// declaration spans, a file's first and last.
    #[must_use]
    pub fn lines(&self) -> (u32, u32) {
        (self.code.line_start, self.code.line_end)
    }
}

/// One opline of an [`OpArrayView`], php's `zend_op`.
#[derive(Clone, Copy)]
pub struct OplineView<'a> {
    code: &'a OpArray,
    op: &'a Opline,
    at: OplineIdx,
}

impl std::fmt::Debug for OplineView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OplineView")
            .field("at", &self.at.0)
            .field("opcode", &self.op.opcode.name())
            .field("line", &self.op.lineno)
            .finish_non_exhaustive()
    }
}

/// What one operand field of an opline names, by php's `IS_*` kind
/// (`Z/Zend/zend_compile.h:766-770`). A field that names no value is
/// [`Operand::Unused`], what it holds read through the opline's other
/// reads: a jump's target through [`OplineView::jump_targets`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operand {
    /// `IS_UNUSED`.
    Unused,
    /// `IS_CONST`: the constant at this index of the op array's table,
    /// read by [`OpArrayView::constant`].
    Const(ConstIdx),
    /// `IS_TMP_VAR`, numbered as php's dumps number it,
    /// `EX_VAR_TO_NUM`: past the compiled variables.
    Tmp(u32),
    /// `IS_VAR`, numbered as a [`Operand::Tmp`] is.
    Var(u32),
    /// `IS_CV`: the compiled variable at this index, named by
    /// [`OpArrayView::variable`].
    Cv(u32),
}

impl<'a> OplineView<'a> {
    /// Where it stands in its op array.
    #[must_use]
    pub fn at(&self) -> OplineIdx {
        self.at
    }

    /// `opline->opcode`.
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        self.op.opcode
    }

    /// `opline->lineno`: the line it was compiled from.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.op.lineno
    }

    /// `opline->op1`.
    #[must_use]
    pub fn op1(&self) -> Operand {
        self.operand(self.op.op1_type, self.op.op1)
    }

    /// `opline->op2`: for a call's `INIT_FCALL`, `INIT_FCALL_BY_NAME`,
    /// `INIT_NS_FCALL_BY_NAME`, `INIT_METHOD_CALL` or
    /// `INIT_STATIC_METHOD_CALL`, the constant that names the function
    /// or the method where the name was written.
    #[must_use]
    pub fn op2(&self) -> Operand {
        self.operand(self.op.op2_type, self.op.op2)
    }

    /// `opline->result`, unused where nothing reads the value, as
    /// `zend_do_free` leaves it.
    #[must_use]
    pub fn out(&self) -> Operand {
        self.operand(self.op.out_type, self.op.out)
    }

    fn operand(&self, kind: OperandKind, field: u32) -> Operand {
        match kind {
            OperandKind::Unused => Operand::Unused,
            OperandKind::Const => Operand::Const(ConstIdx(field)),
            OperandKind::Tmp => Operand::Tmp(exec::temporary_slot(self.code, field)),
            OperandKind::Var => Operand::Var(exec::temporary_slot(self.code, field)),
            OperandKind::Cv => Operand::Cv(field),
        }
    }

    /// The oplines it may jump to, past falling through to the next one,
    /// as `zend_dump_op` prints them: each target of a `SWITCH_LONG`,
    /// `SWITCH_STRING` or `MATCH`'s table in its order, then the one
    /// field `zend_vm_opcodes_flags` shapes `JMP_ADDR`, `op1` for a
    /// `JMP` or a `FAST_CALL`, `op2` for a conditional jump, a
    /// `FE_RESET_*` or a `CATCH` that is not its try's last, and
    /// `extended_value` for a `FE_FETCH_*` or a table's default. None
    /// for an opline that jumps nowhere.
    pub fn jump_targets(&self) -> impl Iterator<Item = OplineIdx> + use<'a> {
        let table = matches!(
            self.op.opcode,
            Opcode::SwitchLong | Opcode::SwitchString | Opcode::Match
        )
        .then_some(self.op)
        .filter(|op| op.op2_type == OperandKind::Const)
        .and_then(|op| self.code.literals.get(op.op2 as usize))
        .and_then(|zval| zval.as_arr());
        let cases = table.into_iter().flat_map(|table| {
            table.iter().filter_map(|(_, target)| {
                target
                    .as_long()
                    .and_then(|at| u32::try_from(at).ok())
                    .map(OplineIdx)
            })
        });
        cases.chain(self.op.jump_target())
    }
}

impl Request {
    /// The op array a user function, a method or a closure serves,
    /// `func->op_array`; none for a native, which has none.
    #[must_use]
    pub fn op_array(&self, function: Function) -> Option<OpArrayView<'_>> {
        let user = self.user_function(function)?;
        exec::body_of(&user.unit, user.body).map(OpArrayView::of)
    }
}
