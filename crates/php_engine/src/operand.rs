//! The operand kinds as types (design §5.8, "bodies written once,
//! monomorphised over operand kinds"). A handler of a hot opcode, one
//! `ops.toml` marks `specialise`, is generic over the kind of each
//! operand it reads, and `build.rs` emits a dispatch arm per kind the
//! spec lists for each operand, as `zend_vm_gen.php` emits
//! `ZEND_ADD_SPEC_TMPVAR_CV_HANDLER`: the handler's inline path finds
//! its operand at a place fixed when it was compiled, and tests no kind
//! while the request is served. A handler the spec does not specialise
//! reads through [`ByKind`], the kind the opline's byte says, with the
//! same inline path behind it. The kernel behind an inline path reads
//! the operand by the opline's byte either way, which says the same
//! kind.

use php_heap::{Zval, ZvalType};
use php_unit::{OpArray, OperandKind, Opline};

/// One way of addressing an operand: where it stands, read without an
/// owner taken.
pub(crate) trait Operand: Copy {
    /// The kind, as the opline's `op1_type` or `op2_type` byte says it.
    fn kind(self) -> OperandKind;

    /// The index among the frame's slots of the operand `field` names,
    /// the field itself for a CV and for a temporary, which the compiler
    /// folded past the last CV as `pass_two` folds it, and none for a
    /// constant, which stands in the op array's table, or an unused
    /// operand.
    fn slot(self, field: u32) -> Option<usize>;

    /// `GET_OPn_ZVAL_PTR_UNDEF(BP_VAR_R)` for an inline path: the zval
    /// `field` names, a copy with no owner taken, and `UNDEF` for a
    /// field past the frame or the table. An inline path takes none of
    /// what a kernel would warn of, so an undefined CV is the kernel's,
    /// and its warning with it.
    #[inline(always)] // one load at a fixed offset, which is the point
    fn read(self, slots: &[Zval], code: &OpArray, field: u32) -> Zval {
        let found = if self.kind() == OperandKind::Const {
            code.literals.get(field as usize)
        } else {
            self.slot(field).and_then(|at| slots.get(at))
        };
        // The zval copied out of the reference itself, never through an
        // `Option<Zval>`: inlined into every arm, such a temporary is a
        // stack slot of its own in the debug build's `serve_frame`.
        let Some(z) = found else {
            std::hint::cold_path();
            return Zval::UNDEF;
        };
        *z
    }
}

/// `IS_CONST`: the op array's constant at the field's index.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Const;

/// `IS_TMP_VAR`: a temporary, past the last CV (`EX_NUM_TO_VAR`), at
/// the slot the compiler folded into the field.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Tmp;

/// `IS_VAR`: a temporary as `IS_TMP_VAR` is, which may hold a reference
/// or an `IS_INDIRECT`; an inline path takes neither as a value, and a
/// property opcode's follows both to its receiver
/// (`handlers::inline_receiver`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Var;

/// `IS_CV`: a compiled variable, the frame's slot at the field's index.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Cv;

/// `IS_UNUSED`: no operand at all, which reads as `UNDEF`; where the
/// opcode reads one as `&EX(This)` (`FETCH_OBJ_R`, `ASSIGN_OBJ`), the
/// handler reads the frame's receiver itself.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Unused;

/// An operand of the kind the opline's byte names, read with one test
/// of the kind: what a handler the spec does not specialise addresses
/// its inline path through.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ByKind(pub(crate) OperandKind);

impl Operand for Const {
    fn kind(self) -> OperandKind {
        OperandKind::Const
    }

    fn slot(self, _: u32) -> Option<usize> {
        None
    }
}

impl Operand for Tmp {
    fn kind(self) -> OperandKind {
        OperandKind::Tmp
    }

    #[inline(always)] // as `read`
    fn slot(self, field: u32) -> Option<usize> {
        Some(field as usize)
    }
}

impl Operand for Var {
    fn kind(self) -> OperandKind {
        OperandKind::Var
    }

    #[inline(always)] // as `read`
    fn slot(self, field: u32) -> Option<usize> {
        Some(field as usize)
    }
}

impl Operand for Cv {
    fn kind(self) -> OperandKind {
        OperandKind::Cv
    }

    #[inline(always)] // as `read`
    fn slot(self, field: u32) -> Option<usize> {
        Some(field as usize)
    }
}

impl Operand for Unused {
    fn kind(self) -> OperandKind {
        OperandKind::Unused
    }

    fn slot(self, _: u32) -> Option<usize> {
        None
    }
}

impl Operand for ByKind {
    #[inline(always)] // as `read`
    fn kind(self) -> OperandKind {
        self.0
    }

    #[inline(always)] // as `read`
    fn slot(self, field: u32) -> Option<usize> {
        match self.0 {
            OperandKind::Cv | OperandKind::Tmp | OperandKind::Var => Some(field as usize),
            OperandKind::Const | OperandKind::Unused => None,
        }
    }
}

/// Whether an opline's `out` is read, `RETURN_VALUE_USED(opline)`: a
/// constant of the handler where the opcode's line says `SPEC(RETVAL)`
/// and the hot list specialises it, as php resolves `_RETVAL_UNUSED`
/// and `_RETVAL_USED` where it publishes the opline
/// (`php_unit::Handler`), and read off the opline's `out_type` byte
/// everywhere else ([`RetvalByte`]).
pub(crate) trait Retval {
    /// Whether `op`'s `out` is read.
    fn used(op: &Opline) -> bool;
}

/// php's `_RETVAL_UNUSED` handler: the compiler wrote `UNUSED` into the
/// `out_type`, as `zend_do_free` does where nothing reads the value.
pub(crate) struct RetvalUnused;

/// php's `_RETVAL_USED` handler.
pub(crate) struct RetvalUsed;

/// An `out` whose use the opline's `out_type` byte says, read while
/// the request is served: a handler the spec does not resolve over it.
pub(crate) struct RetvalByte;

impl Retval for RetvalUnused {
    #[inline(always)] // a constant, so the write of `out` folds away
    fn used(_: &Opline) -> bool {
        false
    }
}

impl Retval for RetvalUsed {
    #[inline(always)] // a constant, as `RetvalUnused`'s
    fn used(_: &Opline) -> bool {
        true
    }
}

impl Retval for RetvalByte {
    #[inline(always)] // a test of the opline's byte, on an inline path
    fn used(op: &Opline) -> bool {
        op.out_type != OperandKind::Unused
    }
}

/// Whether a zval may be copied bitwise with no owner taken and none
/// given up (`Z_OPT_REFCOUNTED` clear): `null`, a bool, a number, an
/// interned string or an immutable array. An `UNDEF` and a constant
/// expression, which no variable may hold, and anything refcounted are
/// not.
#[inline(always)] // a test of two bytes, on an inline path
pub(crate) fn owns_nothing(z: Zval) -> bool {
    !z.is_refcounted()
        && matches!(
            z.kind(),
            ZvalType::Null
                | ZvalType::False
                | ZvalType::True
                | ZvalType::Long
                | ZvalType::Double
                | ZvalType::String
                | ZvalType::Array
        )
}

/// `ZVAL_DEREF`: the value a reference holds, and any other zval as it
/// stands.
#[inline(always)] // a test of the type byte, on an inline path
pub(crate) fn deref(z: Zval) -> Zval {
    match z.as_reference() {
        Some(cell) => cell.val(),
        None => z,
    }
}

/// Whether a zval is a value an assignment or a send may take as it
/// stands, moved or copied with one more owner: anything a variable
/// may hold but a reference, whose value is what is taken
/// (`ZVAL_COPY_DEREF`), and nothing that is no value at all, `UNDEF`,
/// an `IS_INDIRECT` a write fetch left, a constant expression or an
/// `IS_PTR`.
#[inline(always)] // a test of the type byte, on an inline path
pub(crate) fn movable(z: Zval) -> bool {
    !matches!(
        z.kind(),
        ZvalType::Undef
            | ZvalType::Reference
            | ZvalType::Indirect
            | ZvalType::ConstExpr
            | ZvalType::Ptr
    )
}
