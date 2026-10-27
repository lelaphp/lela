//! `zend_may_throw_ex` (`Z/Zend/Optimizer/zend_inference.c:4925-5346`):
//! whether an opline may leave a throwable pending, read from what php's
//! inference knows of its operands, `t1` and `t2`. The tracing tier asks
//! it where it serves an opline through its handler, whether the
//! handler's return is checked for a throwable.

use php_heap::ZvalType;
use php_unit::{Opcode, OperandKind};

use crate::LinkedTable;
use crate::infer::Infer;
use crate::types::{Range, Types};

/// `MAY_BE_ARRAY_OF_OBJECT`.
const OF_OBJECT: Types = Types(Types::OBJECT.0 << 10);
/// `MAY_BE_ARRAY_OF_OBJECT`, `MAY_BE_ARRAY_OF_RESOURCE` and
/// `MAY_BE_ARRAY_OF_ARRAY`: an element whose release may call a
/// destructor.
const OF_OWNERS: Types = Types(Types::OF_ARRAY.0 | OF_OBJECT.0 | (Types::RESOURCE.0 << 10));
/// What releasing may call a destructor for.
const OWNERS: Types = Types(Types::OBJECT.0 | Types::RESOURCE.0 | OF_OWNERS.0);
/// What a binary arithmetic refuses or converts with a notice.
const NOT_NUMERIC: Types =
    Types(Types::STRING.0 | Types::ARRAY.0 | Types::OBJECT.0 | Types::RESOURCE.0);
/// What a shift or a bitwise operator refuses or converts with a notice.
const NOT_INTEGER: Types = Types(NOT_NUMERIC.0 | Types::DOUBLE.0);

/// The `IS_*` a `CAST` names its target by.
const IS_LONG: u32 = ZvalType::Long as u32;
const IS_DOUBLE: u32 = ZvalType::Double as u32;
const IS_STRING: u32 = ZvalType::String as u32;
const IS_ARRAY: u32 = ZvalType::Array as u32;
const IS_OBJECT: u32 = ZvalType::Object as u32;

impl<T: LinkedTable> Infer<'_, T> {
    /// `zend_may_throw_ex` at SSA op `at`, its operands of `t1` and `t2`.
    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::match_same_arms,
        reason = "php's switch, case for case"
    )]
    pub(crate) fn may_throw_ex(&self, at: usize, t1: Types, t2: Types) -> bool {
        let Some(&op) = self.opline(at) else {
            return true;
        };
        let ssa_op = self.op(at);
        let opcode = op.opcode;
        if op.op1_type == OperandKind::Cv {
            if t1.has(Types::UNDEF)
                && !matches!(
                    opcode,
                    Opcode::IssetIsemptyDimObj
                        | Opcode::IssetIsemptyPropObj
                        | Opcode::Assign
                        | Opcode::AssignDim
                        | Opcode::AssignRef
                        | Opcode::BindGlobal
                        | Opcode::BindStatic
                        | Opcode::BindInitStaticOrJmp
                        | Opcode::FetchDimIs
                        | Opcode::FetchObjIs
                        | Opcode::SendRef
                        | Opcode::UnsetCv
                        | Opcode::IssetIsemptyCv
                        | Opcode::MakeRef
                        | Opcode::FetchDimW
                )
            {
                // An undefined variable's warning.
                return true;
            }
        } else if matches!(op.op1_type, OperandKind::Tmp | OperandKind::Var)
            && t1.has(Types::RC1)
            && t1.has(OWNERS)
            && !matches!(
                opcode,
                Opcode::Case
                    | Opcode::CaseStrict
                    | Opcode::FeFetchR
                    | Opcode::FeFetchRw
                    | Opcode::FetchListR
                    | Opcode::QmAssign
                    | Opcode::SendVal
                    | Opcode::SendValEx
                    | Opcode::SendVar
                    | Opcode::SendVarEx
                    | Opcode::SendFuncArg
                    | Opcode::SendVarNoRef
                    | Opcode::SendVarNoRefEx
                    | Opcode::SendRef
                    | Opcode::Separate
                    | Opcode::EndSilence
                    | Opcode::MakeRef
            )
        {
            // A destructor may be called.
            return true;
        }
        if op.op2_type == OperandKind::Cv {
            if t2.has(Types::UNDEF)
                && !matches!(
                    opcode,
                    Opcode::AssignRef | Opcode::FeFetchR | Opcode::FeFetchRw
                )
            {
                return true;
            }
        } else if matches!(op.op2_type, OperandKind::Tmp | OperandKind::Var)
            && t2.has(Types::RC1)
            && t2.has(OWNERS)
            && !matches!(
                opcode,
                Opcode::Assign | Opcode::FeFetchR | Opcode::FeFetchRw
            )
        {
            return true;
        }
        let op2_range = || self.operand_range(op.op2_type, op.op2, ssa_op.op2_use);
        let may_be_zero = |range: Option<Range>| range.is_none_or(|r| r.min <= 0 && r.max >= 0);
        let negative = |range: Option<Range>| range.is_none_or(|r| r.min < 0);
        let only = |t: Types, kind: Types| (t & Types::ANY) == kind;
        let either = |kinds: Types| t1.has(kinds) || t2.has(kinds);
        match opcode {
            Opcode::Nop
            | Opcode::QmAssign
            | Opcode::Jmp
            | Opcode::CheckVar
            | Opcode::MakeRef
            | Opcode::BeginSilence
            | Opcode::EndSilence
            | Opcode::Free
            | Opcode::FeFree
            | Opcode::Separate
            | Opcode::TypeCheck
            | Opcode::Defined
            | Opcode::IssetIsemptyThis
            | Opcode::Coalesce
            | Opcode::SwitchLong
            | Opcode::SwitchString
            | Opcode::Match
            | Opcode::IssetIsemptyVar
            | Opcode::IssetIsemptyCv
            | Opcode::FuncNumArgs
            | Opcode::FuncGetArgs
            | Opcode::CopyTmp
            | Opcode::JmpNull
            | Opcode::JmpFrameless
            | Opcode::InitFcall => false,
            // An array compared to an array may recurse.
            Opcode::IsIdentical | Opcode::IsNotIdentical | Opcode::CaseStrict => {
                (t1 & t2).has(Types::OF_ARRAY)
            }
            // A named argument may be refused.
            Opcode::SendVar
            | Opcode::SendVal
            | Opcode::SendRef
            | Opcode::SendVarEx
            | Opcode::SendFuncArg
            | Opcode::CheckFuncArg => op.op2_type == OperandKind::Const,
            Opcode::BindGlobal => {
                let next = at + 1;
                self.opline(next)
                    .is_some_and(|o| o.opcode == Opcode::BindGlobal)
                    && self.may_throw(next)
            }
            Opcode::Add if only(t1, Types::ARRAY) && only(t2, Types::ARRAY) => false,
            Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Pow => either(NOT_NUMERIC),
            Opcode::Div => may_be_zero(op2_range()) || either(NOT_NUMERIC),
            Opcode::Mod => may_be_zero(op2_range()) || either(NOT_INTEGER) || negative(op2_range()),
            Opcode::Sl | Opcode::Sr => either(NOT_INTEGER) || negative(op2_range()),
            Opcode::Concat | Opcode::FastConcat => either(Types::ARRAY | Types::OBJECT),
            Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor
                if only(t1, Types::STRING) && only(t2, Types::STRING) =>
            {
                false
            }
            Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor => either(NOT_INTEGER),
            Opcode::BwNot => t1.has(
                Types::NULL
                    | Types::BOOL
                    | Types::DOUBLE
                    | Types::ARRAY
                    | Types::OBJECT
                    | Types::RESOURCE,
            ),
            Opcode::PreInc | Opcode::PostInc => t1.has(Types::BOOL | NOT_NUMERIC),
            // A null warns under `--`, which `++` makes one.
            Opcode::PreDec | Opcode::PostDec => t1.has(Types::NULL | Types::BOOL | NOT_NUMERIC),
            Opcode::Jmpz | Opcode::Jmpnz | Opcode::JmpzEx | Opcode::JmpnzEx | Opcode::JmpSet => {
                t1.has(Types::OBJECT)
            }
            // A NAN cast to a boolean warns, which a range rules out.
            Opcode::Bool | Opcode::BoolNot => {
                t1.has(Types::OBJECT)
                    || (t1.has(Types::DOUBLE)
                        && self
                            .operand_range(op.op1_type, op.op1, ssa_op.op1_use)
                            .is_none())
            }
            Opcode::BoolXor => either(Types::OBJECT),
            Opcode::IsEqual
            | Opcode::IsNotEqual
            | Opcode::IsSmaller
            | Opcode::IsSmallerOrEqual
            | Opcode::Case
            | Opcode::Spaceship => {
                if only(t1, Types::NULL) || only(t2, Types::NULL) {
                    false
                } else {
                    either(Types::OBJECT | Types::OF_ARRAY | OF_OBJECT)
                }
            }
            Opcode::AssignOp => {
                let binary = u8::try_from(op.extended_value)
                    .ok()
                    .and_then(Opcode::from_u8);
                match binary {
                    Some(Opcode::Add) if only(t1, Types::ARRAY) && only(t2, Types::ARRAY) => false,
                    Some(Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Pow) => {
                        either(NOT_NUMERIC)
                    }
                    Some(Opcode::Div | Opcode::Mod) => {
                        may_be_zero(op2_range()) || either(NOT_NUMERIC)
                    }
                    Some(Opcode::Sl | Opcode::Sr) => either(NOT_NUMERIC) || negative(op2_range()),
                    Some(Opcode::Concat) => either(Types::ARRAY | Types::OBJECT),
                    Some(Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor)
                        if only(t1, Types::STRING) && only(t2, Types::STRING) =>
                    {
                        false
                    }
                    Some(Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor) => either(NOT_NUMERIC),
                    _ => true,
                }
            }
            Opcode::Assign if t1.has(Types::REF) => true,
            Opcode::Assign | Opcode::UnsetVar => t1.has(OWNERS),
            // A destructor may throw.
            Opcode::BindStatic | Opcode::BindInitStaticOrJmp => t1.has(OWNERS),
            Opcode::AssignDim => {
                self.data_undefined(at)
                    || t1.has(OF_OWNERS | Types::OF_REF)
                    || t1.has(
                        Types::OBJECT
                            | Types::RESOURCE
                            | Types::BOOL
                            | Types::STRING
                            | Types::LONG
                            | Types::DOUBLE,
                    )
                    || op.op2_type == OperandKind::Unused
                    || t2.has(Types::UNDEF | Types::ARRAY | Types::OBJECT | Types::RESOURCE)
            }
            // php reads the class's handlers and its property table here,
            // which a linked table does not show: may throw, as php's
            // answer is for every class it cannot prove plain.
            Opcode::AssignObj => true,
            Opcode::RopeInit | Opcode::RopeAdd | Opcode::RopeEnd => {
                t2.has(Types::ARRAY | Types::OBJECT)
            }
            Opcode::InitArray => {
                op.op2_type != OperandKind::Unused
                    && t2.has(Types::ARRAY | Types::OBJECT | Types::RESOURCE)
            }
            Opcode::AddArrayElement => {
                op.op2_type == OperandKind::Unused
                    || t2.has(Types::NULL | Types::ARRAY | Types::OBJECT | Types::RESOURCE)
            }
            Opcode::Strlen => !only(t1, Types::STRING),
            Opcode::Count => !only(t1, Types::ARRAY),
            Opcode::RecvInit => self.recv_init_may_throw(op.op1, op.op2),
            Opcode::FetchIs => t2.has(Types::ARRAY | Types::OBJECT),
            Opcode::IssetIsemptyDimObj => {
                t1.has(Types::OBJECT) || t2.has(Types::DOUBLE | Types::ARRAY | Types::OBJECT)
            }
            Opcode::FetchDimIs => {
                t1.has(Types::OBJECT)
                    || t2.has(Types::DOUBLE | Types::ARRAY | Types::OBJECT | Types::RESOURCE)
            }
            Opcode::Cast => match op.extended_value {
                IS_LONG => t1.has(Types::DOUBLE | Types::STRING | Types::OBJECT),
                IS_DOUBLE | IS_ARRAY => t1.has(Types::OBJECT),
                IS_STRING => t1.has(Types::ARRAY | Types::OBJECT),
                IS_OBJECT => false,
                _ => false,
            },
            Opcode::ArrayKeyExists => {
                !only(t2, Types::ARRAY) || t1.has(Types::ARRAY | Types::OBJECT | Types::RESOURCE)
            }
            Opcode::FeResetR | Opcode::FeResetRw => {
                (t1 & (Types::ANY | Types::REF)) != Types::ARRAY
            }
            Opcode::FeFetchR => {
                (t1 & (Types::ANY | Types::REF)) != Types::ARRAY
                    || (op.op2_type == OperandKind::Cv && t2.has(Types::RC1) && t2.has(OWNERS))
            }
            Opcode::FetchDimW | Opcode::FetchListW => {
                t1.has(
                    Types::BOOL
                        | Types::LONG
                        | Types::DOUBLE
                        | Types::STRING
                        | Types::OBJECT
                        | Types::RESOURCE
                        | Types::REF,
                ) || t2.has(Types::RESOURCE | Types::ARRAY | Types::OBJECT)
                    || op.op2_type == OperandKind::Unused
            }
            _ => true,
        }
    }

    /// `zend_may_throw`: [`Infer::may_throw_ex`] over what inference knows
    /// of the opline's operands.
    pub(crate) fn may_throw(&self, at: usize) -> bool {
        let t1 = self.op1_types(at);
        let t2 = self.opline(at).map_or(Types::NONE, |op| {
            self.operand_types(op.op2_type, op.op2, self.op(at).op2_use)
        });
        self.may_throw_ex(at, t1, t2)
    }

    /// Whether an `ASSIGN_DIM`'s `OP_DATA` reads a variable that may be
    /// undefined, `OP1_DATA_INFO() & MAY_BE_UNDEF` of a CV.
    fn data_undefined(&self, at: usize) -> bool {
        let Some(data) = self.opline(at + 1) else {
            return false;
        };
        data.op1_type == OperandKind::Cv
            && self
                .operand_types(data.op1_type, data.op1, self.op(at + 1).op1_use)
                .has(Types::UNDEF)
    }

    /// `ZEND_RECV_INIT`'s case: a default that is a constant expression
    /// may throw as it is evaluated, and a parameter that declares a type
    /// may refuse what it is given.
    fn recv_init_may_throw(&self, arg: u32, default: u32) -> bool {
        if self
            .literal(default)
            .is_some_and(|value| value.kind() == ZvalType::ConstExpr)
        {
            return true;
        }
        let Some(decl) = self.function.decl else {
            return false;
        };
        let num_args = self.function.num_args();
        let index = if arg <= num_args {
            arg.checked_sub(1)
        } else if decl.parameters.last().is_some_and(|p| p.variadic) {
            Some(num_args)
        } else {
            return false;
        };
        index
            .and_then(|i| decl.parameters.get(i as usize))
            .is_some_and(|p| p.ty.is_some())
    }
}
