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
//! file's, and from a function, [`Frames::op_array`], which answers
//! none for a native, as php's `ZEND_USER_CODE(func->type)` does.
//!
//! The debugging host reads the same op arrays here, as an extension
//! does (#1911): its `print` writes one as [`Frames::dump_op_array`]
//! dumps it, then each function its body declares
//! ([`OpArrayView::dynamic_functions`]), as phpdbg's
//! `phpdbg_print_function_helper` does.
//!
//! [`FrameView::op_array`]: super::FrameView::op_array

use php_unit::decl::FunctionDecl;
use php_unit::{OpArray, OperandKind, Opline, Unit};

pub use php_unit::{ConstIdx, OplineIdx};

use crate::api::call::Function;
use crate::api::values::Value;
use crate::exec;

use super::Frames;
use super::Request;

/// One op array, read: php's `zend_op_array` as a user function, a
/// method, a closure or an included, evaluated or main file holds it.
#[derive(Clone, Copy)]
pub struct OpArrayView<'a> {
    /// The unit the op array was compiled into, which holds the
    /// functions its body declares.
    unit: &'a Unit,
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
    pub(crate) fn of(unit: &'a Unit, code: &'a OpArray) -> OpArrayView<'a> {
        OpArrayView { unit, code }
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
            .map(|zval| Value::of_slot(*zval))
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

    /// `op_array->dynamic_func_defs`: each function or closure its body
    /// declares, in the order the compiler met them, with its
    /// declaration and its own op array; empty for an op array that
    /// declares none.
    pub fn dynamic_functions(
        &self,
    ) -> impl Iterator<Item = (&'a FunctionDecl, OpArrayView<'a>)> + use<'a> {
        let unit = self.unit;
        self.code.dynamic_func_defs.iter().filter_map(move |at| {
            let (decl, code) = unit.dynamic.get(*at as usize)?;
            Some((decl, OpArrayView { unit, code }))
        })
    }
}

/// One opline's opcode as an observer reads it (#2357): php's
/// `zend_op.opcode` byte and `zend_get_opcode_name`'s name for it, and
/// nothing of what the compiled tier reads off the unit format's opcode.
/// An extension tells one opcode from another by its name, `ZEND_RETURN`,
/// as php's dumps and phpdbg's opcode breakpoints do.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Opcode(php_unit::Opcode);

impl Opcode {
    /// `zend_get_opcode_name`: php's name, `ZEND_ADD`.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// php's number, the byte `zend_op.opcode` holds.
    #[must_use]
    pub fn num(self) -> u8 {
        self.0.num()
    }
}

impl std::fmt::Debug for Opcode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The opcode php numbers `n`; the byte back where no opcode has it.
impl TryFrom<u8> for Opcode {
    type Error = u8;

    fn try_from(n: u8) -> Result<Opcode, u8> {
        php_unit::Opcode::from_u8(n).map(Opcode).ok_or(n)
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

/// An operand as its opline holds it: a temporary's field is its slot,
/// which the compiler folded past the CVs.
fn operand(kind: OperandKind, field: u32) -> Operand {
    match kind {
        OperandKind::Unused => Operand::Unused,
        OperandKind::Const => Operand::Const(ConstIdx(field)),
        OperandKind::Tmp => Operand::Tmp(field),
        OperandKind::Var => Operand::Var(field),
        OperandKind::Cv => Operand::Cv(field),
    }
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
        Opcode(self.op.opcode)
    }

    /// `opline->lineno`: the line it was compiled from.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.op.lineno
    }

    /// `opline->op1`.
    #[must_use]
    pub fn op1(&self) -> Operand {
        operand(self.op.op1_type, self.op.op1)
    }

    /// `opline->op2`: for a call's `INIT_FCALL`, `INIT_FCALL_BY_NAME`,
    /// `INIT_NS_FCALL_BY_NAME`, `INIT_METHOD_CALL` or
    /// `INIT_STATIC_METHOD_CALL`, the constant that names the function
    /// or the method where the name was written.
    #[must_use]
    pub fn op2(&self) -> Operand {
        operand(self.op.op2_type, self.op.op2)
    }

    /// `opline->result`, unused where nothing reads the value, as
    /// `zend_do_free` leaves it.
    #[must_use]
    pub fn out(&self) -> Operand {
        operand(self.op.out_type, self.op.out)
    }

    /// The oplines it may jump to, past falling through to the next one,
    /// as `zend_dump_op` prints them ([`crate::tier::jump_targets`]).
    pub fn jump_targets(&self) -> impl Iterator<Item = OplineIdx> + use<'a> {
        crate::tier::jump_targets(self.code, self.at)
    }
}

impl<'r> Frames<&'r Request> {
    /// The op array a user function, a method or a closure serves,
    /// `func->op_array`; none for a native, which has none.
    #[must_use]
    pub fn op_array(self, function: Function) -> Option<OpArrayView<'r>> {
        let user = self.0.user_function(function)?;
        exec::body_of(user.unit(), user.body()).map(|code| OpArrayView::of(user.unit(), code))
    }

    /// `zend_dump_op_array(op_array, ZEND_DUMP_LINE_NUMBERS, NULL, NULL)`
    /// (`Z/Zend/Optimizer/zend_dump.c`), what phpdbg's `print` writes: a
    /// blank line, `name`, the counts with `parameters` the declared
    /// parameters less a variadic one, the file with the line range,
    /// then each opline opened by `L` and its line, and the
    /// `try`/`catch` table. A `FRAMELESS_ICALL_*` prints the native its
    /// `extended_value` indexes in `zend_flf_functions`, which is the
    /// engine's, so the dump is the request's to write.
    #[must_use]
    pub fn dump_op_array(self, code: OpArrayView<'_>, name: &[u8], parameters: usize) -> Vec<u8> {
        let frameless = self.0.shared.frameless_names();
        code.code.dump_lines(name, parameters, &frameless)
    }
}
