//! What the trace SSA knows of each opcode, in one place: which slots an
//! opline reads and writes, what it writes, the operands php's guard
//! switch names (`Z/ext/opcache/jit/zend_jit_trace.c:1835-2245`), and the
//! parameters an op array's `RECV` oplines declare.

use php_engine::tier::Sample;
use php_heap::ZvalType;
use php_unit::decl::{Parameter, Type};
use php_unit::{ConstIdx, OpArray, Opcode, OperandKind, Opline, OplineIdx};

use super::lattice::{self, Types};
use super::{Constant, Frame, Imm, Layout, Line, Slot};

/// php's `MAX_ARG_FLAG_NUM`: past it, how a send passes is read late.
pub(super) const MAX_ARG_FLAG_NUM: u32 = 12;
/// php's `ZEND_FETCH_LOCAL`: a fetch by name from the frame's variables.
const FETCH_LOCAL: u32 = 1 << 2;
/// php's `ZEND_ISEMPTY`.
pub(super) const ISEMPTY: u32 = 1 << 0;
/// php's `ZEND_BIND_REF` and `ZEND_ARRAY_ELEMENT_REF`.
const BY_REF: u32 = 1 << 0;
/// The functions that read their caller's variables by name, php's
/// `zend_optimizer_classify_function`.
const BY_NAME: [&[u8]; 4] = [b"extract", b"compact", b"get_defined_vars", b"db2_execute"];

/// The slot an operand of an opline of `frame` names, where it names
/// one: the field itself, a temporary's folded past the CVs by the
/// compiler as `pass_two` folds it.
pub(super) fn slot_of(frame: Frame, kind: OperandKind, field: u32) -> Option<Slot> {
    let var = match kind {
        OperandKind::Cv | OperandKind::Tmp | OperandKind::Var => field,
        OperandKind::Const | OperandKind::Unused => return None,
    };
    Some(Slot { frame, var })
}

/// Whether an op array reads its own variables by name, php's
/// `ZEND_FUNC_INDIRECT_VAR_ACCESS` (`Z/Zend/Optimizer/zend_cfg.c:316-420`):
/// an `include` or `eval`, a fetch by name from the frame, or a call to a
/// function that reads its caller's variables.
pub(super) fn reads_by_name(code: &OpArray) -> bool {
    code.oplines.iter().any(|op| match op.opcode {
        Opcode::IncludeOrEval => true,
        Opcode::FetchR
        | Opcode::FetchW
        | Opcode::FetchRw
        | Opcode::FetchFuncArg
        | Opcode::FetchIs
        | Opcode::FetchUnset
        | Opcode::UnsetVar
        | Opcode::IssetIsemptyVar => op.extended_value & FETCH_LOCAL != 0,
        Opcode::InitFcall | Opcode::InitNsFcallByName => {
            // `INIT_NS_FCALL_BY_NAME`'s third constant is the lowered
            // unqualified name.
            let at = if op.opcode == Opcode::InitNsFcallByName {
                op.op2.saturating_add(2)
            } else {
                op.op2
            };
            code.literals
                .get(at as usize)
                .and_then(|name| name.as_str())
                .is_some_and(|name| BY_NAME.contains(&name.bytes()))
        }
        _ => false,
    })
}

/// What an opline writes beside its `out` slot: a variable `op1`, an
/// `op2`, the `OP_DATA`'s `op1` (php's `_zend_ssa_rename_op`, less the
/// writes reference-count inference alone adds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Writes {
    pub(super) op1: bool,
    pub(super) op2: bool,
    pub(super) data: bool,
}

pub(super) fn writes(op: &Opline, data: Option<&Opline>) -> Writes {
    let cv1 = op.op1_type == OperandKind::Cv;
    let op1 = match op.opcode {
        Opcode::VerifyReturnType => matches!(
            op.op1_type,
            OperandKind::Cv | OperandKind::Tmp | OperandKind::Var
        ),
        Opcode::InitArray | Opcode::AddArrayElement => cv1 && op.extended_value & BY_REF != 0,
        Opcode::Assign
        | Opcode::AssignRef
        | Opcode::AssignDim
        | Opcode::AssignObj
        | Opcode::AssignDimOp
        | Opcode::AssignObjOp
        | Opcode::AssignOp
        | Opcode::PreInc
        | Opcode::PreDec
        | Opcode::PostInc
        | Opcode::PostDec
        | Opcode::BindGlobal
        | Opcode::BindStatic
        | Opcode::BindInitStaticOrJmp
        | Opcode::SendVarNoRef
        | Opcode::SendVarNoRefEx
        | Opcode::SendVarEx
        | Opcode::SendFuncArg
        | Opcode::SendRef
        | Opcode::SendUnpack
        | Opcode::FeResetRw
        | Opcode::MakeRef
        | Opcode::PreIncObj
        | Opcode::PreDecObj
        | Opcode::PostIncObj
        | Opcode::PostDecObj
        | Opcode::UnsetDim
        | Opcode::UnsetObj
        | Opcode::FetchDimW
        | Opcode::FetchDimRw
        | Opcode::FetchDimFuncArg
        | Opcode::FetchDimUnset
        | Opcode::FetchListW
        | Opcode::UnsetCv
        | Opcode::Yield => cv1,
        _ => false,
    };
    let op2 = match op.opcode {
        Opcode::AssignRef => op.op2_type == OperandKind::Cv,
        Opcode::FeFetchR | Opcode::FeFetchRw => {
            matches!(
                op.op2_type,
                OperandKind::Cv | OperandKind::Var | OperandKind::Tmp
            )
        }
        Opcode::BindLexical => op.op2_type == OperandKind::Cv && op.extended_value & BY_REF != 0,
        _ => false,
    };
    let data = matches!(
        op.opcode,
        Opcode::AssignObjRef | Opcode::AssignStaticPropRef
    ) && data.is_some_and(|data| data.op1_type == OperandKind::Cv);
    Writes { op1, op2, data }
}

/// Whether an operand kind names a slot.
fn slotted(kind: OperandKind) -> bool {
    matches!(kind, OperandKind::Cv | OperandKind::Tmp | OperandKind::Var)
}

/// The slots an opline reads: its operands, and its `OP_DATA`'s.
pub(super) fn reads(op: &Opline, after: Option<&Opline>) -> Vec<u32> {
    let mut vars = Vec::with_capacity(3);
    for (kind, field) in [(op.op1_type, op.op1), (op.op2_type, op.op2)] {
        if let Some(slot) = slot_of(Frame::ROOT, kind, field) {
            vars.push(slot.var);
        }
    }
    if let Some(data) = after.filter(|data| data.opcode == Opcode::OpData)
        && let Some(slot) = slot_of(Frame::ROOT, data.op1_type, data.op1)
    {
        vars.push(slot.var);
    }
    vars
}

/// The slots an opline writes.
pub(super) fn written(op: &Opline, after: Option<&Opline>) -> Vec<u32> {
    let data = after.filter(|data| data.opcode == Opcode::OpData);
    let writes = writes(op, data);
    let mut vars = Vec::with_capacity(3);
    let mut add = |kind: OperandKind, field: u32| {
        if let Some(slot) = slot_of(Frame::ROOT, kind, field) {
            vars.push(slot.var);
        }
    };
    if slotted(op.out_type) {
        add(op.out_type, op.out);
    }
    if writes.op1 {
        add(op.op1_type, op.op1);
    }
    if writes.op2 {
        add(op.op2_type, op.op2);
    }
    if let (true, Some(data)) = (writes.data, data) {
        add(data.op1_type, data.op1);
    }
    vars
}

/// What a value of `t` becomes once assigned: what a reference holds,
/// and null for an unset variable, as php's read of one gives.
pub(super) fn assigned(t: Types) -> Types {
    let t = lattice::deref(t);
    if t.meets(Types::UNDEF) {
        t.without(Types::UNDEF).or(Types::NULL)
    } else {
        t
    }
}

/// One parameter: the variable its `RECV` writes, its place among the
/// arguments, from one, and what it holds once the `RECV` ran.
#[derive(Debug, Clone, Copy)]
pub(super) struct Param {
    pub(super) cv: u32,
    pub(super) number: u32,
    pub(super) declared: Declared,
    /// `RECV`, `RECV_INIT` or `RECV_VARIADIC`.
    pub(super) opcode: Opcode,
    /// Whether its declaration names a type, php's
    /// `ZEND_TYPE_IS_SET(arg_info->type)`: where the view does not list
    /// it, a `RECV`'s mask other than `MAY_BE_ANY`, which reads `mixed`
    /// and a `RECV_INIT`'s or a `RECV_VARIADIC`'s type, which no mask
    /// names, as none.
    pub(super) typed: bool,
}

/// What a parameter's declaration says it holds, php's `arg_info` as
/// `zend_fetch_arg_info_type` reads it.
#[derive(Debug, Clone, Copy)]
pub(super) enum Declared {
    /// No type: whatever was sent.
    Untyped,
    /// A type, which an argument of another is coerced to or refused by.
    Typed(Types),
    /// Passed by reference: a reference to anything of the type.
    Referenced(Types),
}

/// The parameters an op array's leading `RECV` oplines take, each as its
/// declaration in `declared` says, where the view lists it.
pub(super) fn params(code: &OpArray, declared: Option<&[Parameter]>) -> Vec<Param> {
    code.oplines
        .iter()
        .map_while(|op| {
            if !matches!(
                op.opcode,
                Opcode::Recv | Opcode::RecvInit | Opcode::RecvVariadic
            ) {
                return None;
            }
            let parameter = declared.and_then(|list| list.get(op.op1.saturating_sub(1) as usize));
            let declared = match parameter {
                Some(p) if p.by_ref => Declared::Referenced(of_type(p.ty.as_ref(), p.variadic)),
                Some(p) if p.ty.is_some() || p.variadic => {
                    Declared::Typed(of_type(p.ty.as_ref(), p.variadic))
                }
                Some(_) => Declared::Untyped,
                // A declaration the view does not list: what the `RECV`'s
                // mask allows, with an object a class name may add.
                None => match op.opcode {
                    Opcode::Recv => Declared::Typed(Types::of_mask(op.op2).without(Types::REF)),
                    Opcode::RecvVariadic => Declared::Typed(Types::of_tag(ZvalType::Array)),
                    _ => Declared::Untyped,
                },
            };
            let typed = match parameter {
                Some(p) => p.ty.is_some(),
                None => op.opcode == Opcode::Recv && op.op2 != Type::MAY_BE_ANY,
            };
            Some(Param {
                cv: op.out,
                number: op.op1,
                declared,
                opcode: op.opcode,
                typed,
            })
        })
        .collect()
}

/// The `RECV` oplines of `code`'s prologue at which php's `zend_jit_recv`
/// takes a `ZEND_JIT_EXIT_TO_VM` exit for a root frame whose argument
/// count the trace does not know (`Z/ext/opcache/jit/zend_jit_ir.c:10825-10872`),
/// in order: none unless a parameter has a type, php's
/// `ZEND_ACC_HAS_TYPE_HINTS`, without which php's entry trace starts past
/// the prologue (`zend_jit_setup_hot_trace_counters`,
/// `Z/ext/opcache/jit/zend_jit_trace.c:8983-9001`); and then one per
/// `RECV` whose parameter has a type ([`Param::typed`]) or that no `RECV`
/// follows. A `RECV_INIT` and a `RECV_VARIADIC` take none.
pub(super) fn recv_exits(code: &OpArray, declared: Option<&[Parameter]>) -> Vec<OplineIdx> {
    let params = params(code, declared);
    if !params.iter().any(|param| param.typed) {
        return Vec::new();
    }
    let recv = |param: Option<&Param>| param.is_some_and(|param| param.opcode == Opcode::Recv);
    // `params` are the prologue's oplines from the first, one each.
    (params.iter().enumerate())
        .filter(|&(at, param)| recv(Some(param)) && (param.typed || !recv(params.get(at + 1))))
        .filter_map(|(at, _)| u32::try_from(at).ok().map(OplineIdx))
        .collect()
}

/// The kinds a declared type allows, `zend_fetch_arg_info_type`'s mask:
/// a class name, `object`, `static`, `self`, `parent` and an intersection
/// an object, `iterable` an array or an object, `callable` a string, an
/// array or an object; a variadic parameter is an array of them.
pub(super) fn of_type(ty: Option<&Type>, variadic: bool) -> Types {
    if variadic {
        return Types::of_tag(ZvalType::Array);
    }
    let Some(ty) = ty else {
        return Types::ANY;
    };
    match ty {
        Type::Mixed => Types::ANY,
        Type::Void | Type::Null => Types::NULL,
        Type::Never => Types::NONE,
        Type::Bool => Types::BOOL,
        Type::False => Types::FALSE,
        Type::True => Types::TRUE,
        Type::Int => Types::LONG,
        Type::Float => Types::DOUBLE,
        Type::String => Types::STRING,
        Type::Array => Types::of_tag(ZvalType::Array),
        Type::Iterable => Types::of_tag(ZvalType::Array).or(Types::OBJECT),
        Type::Callable => Types::STRING
            .or(Types::of_tag(ZvalType::Array))
            .or(Types::OBJECT),
        Type::Object
        | Type::CalledClass
        | Type::SelfClass
        | Type::ParentClass
        | Type::Named(_)
        | Type::Intersection(_) => Types::OBJECT,
        Type::Nullable(inner) => of_type(Some(inner), false).or(Types::NULL),
        Type::Union(members) => members.iter().fold(Types::NONE, |all, member| {
            all.or(of_type(Some(member), false))
        }),
    }
}

/// The kinds a parameter's declared type `ty` holds as they are, which
/// its `RECV` neither refuses nor coerces whatever the regime: the type
/// byte's test php's `zend_jit_verify_arg_type` makes first, and skips
/// where its stack map knows the argument of one of them
/// (`Z/ext/opcache/jit/zend_jit_ir.c:10766-10823`, the skip at
/// `:10773-10781`). None for a type whose
/// check reads more than the type byte, a class, `callable` or
/// `iterable`, or `never`.
pub(super) fn held_as_is(ty: &Type) -> Option<Types> {
    Some(match ty {
        Type::Mixed => Types::ANY.kinds(),
        Type::Null => Types::NULL,
        Type::Bool => Types::BOOL,
        Type::False => Types::FALSE,
        Type::True => Types::TRUE,
        Type::Int => Types::LONG,
        Type::Float => Types::DOUBLE,
        Type::String => Types::STRING,
        Type::Array => Types::of_tag(ZvalType::Array).kinds(),
        Type::Nullable(inner) => held_as_is(inner)?.or(Types::NULL),
        Type::Union(members) => members
            .iter()
            .try_fold(Types::NONE, |all, member| Some(all.or(held_as_is(member)?)))?,
        _ => return None,
    })
}

/// What a parameter holds once its `RECV` ran: what was sent where the
/// declaration keeps it unchanged, what is declared otherwise, as php's
/// trace SSA propagates an argument's type into an untyped parameter
/// (`zend_jit_trace.c:2130-2170`).
pub(super) fn param_types(declared: Declared, sent: Option<Types>) -> Types {
    let sent = sent.filter(|t| t.kinds() != Types::NONE && !t.meets(Types::REF));
    match declared {
        Declared::Untyped => sent.unwrap_or(Types::ANY),
        Declared::Typed(types) => match sent {
            Some(t) if t.within(types) => t,
            _ => types,
        },
        Declared::Referenced(types) => types.or(Types::REF),
    }
}

/// Whether an opline may make variable `var` a reference: an assignment
/// or a binding by reference, a send that may pass it by reference, a
/// `foreach` by reference, an array element or a closure's `use` by
/// reference, a return by reference.
pub(super) fn referenced(code: &OpArray, var: u32) -> bool {
    code.oplines.iter().enumerate().any(|(at, op)| {
        let cv1 = op.op1_type == OperandKind::Cv && op.op1 == var;
        let cv2 = op.op2_type == OperandKind::Cv && op.op2 == var;
        match op.opcode {
            Opcode::AssignRef => cv1 || cv2,
            Opcode::BindGlobal
            | Opcode::BindStatic
            | Opcode::BindInitStaticOrJmp
            | Opcode::SendRef
            | Opcode::SendVarEx
            | Opcode::SendVarNoRef
            | Opcode::SendVarNoRefEx
            | Opcode::SendFuncArg
            | Opcode::SendUser
            | Opcode::FeResetRw
            | Opcode::MakeRef
            | Opcode::ReturnByRef
            | Opcode::Yield
            | Opcode::YieldFrom => cv1,
            Opcode::InitArray | Opcode::AddArrayElement => cv1 && op.extended_value & BY_REF != 0,
            Opcode::FeFetchRw => cv2,
            Opcode::BindLexical => cv2 && op.extended_value & BY_REF != 0,
            Opcode::AssignObjRef | Opcode::AssignStaticPropRef => code
                .oplines
                .get(at + 1)
                .is_some_and(|data| data.op1_type == OperandKind::Cv && data.op1 == var),
            _ => false,
        }
    })
}

/// A constant operand: a scalar by its value.
pub(super) fn constant(code: &OpArray, at: u32) -> Constant {
    let Some(value) = code.literals.get(at as usize) else {
        return Constant::Other(ConstIdx(at));
    };
    match value.kind() {
        ZvalType::Null => Constant::Null,
        ZvalType::False => Constant::False,
        ZvalType::True => Constant::True,
        ZvalType::Long => value
            .as_long()
            .map_or(Constant::Other(ConstIdx(at)), Constant::Long),
        ZvalType::Double => value
            .as_double()
            .map_or(Constant::Other(ConstIdx(at)), Constant::Double),
        _ => Constant::Other(ConstIdx(at)),
    }
}

/// What a constant operand is.
pub(super) fn constant_types(constant: Constant, code: &OpArray) -> Types {
    match constant {
        Constant::Null => Types::NULL,
        Constant::False => Types::FALSE,
        Constant::True => Types::TRUE,
        Constant::Long(_) => Types::LONG,
        Constant::Double(_) => Types::DOUBLE,
        Constant::Other(at) => code
            .literals
            .get(at.0 as usize)
            .map_or(Types::ANY, Types::of_constant),
    }
}

/// php's `zend_jit_trace_may_exit` past the jumps whose direction the
/// record keeps (`Z/ext/opcache/jit/zend_jit_trace.c:261-320`): the
/// oplines whose handler may go on elsewhere than the opline the record
/// went to. A table's jump (`SWITCH_LONG`, `SWITCH_STRING`, `MATCH`),
/// whose target the record does not say; a `NEW` with no arguments
/// before a `DO_FCALL`, which skips the call where the class has no
/// constructor; and a `DO_FCALL`, php's potentially polymorphic call,
/// whose callee may be another function than the one recorded.
pub(super) fn may_exit(op: &Opline, after: Option<&Opline>) -> bool {
    match op.opcode {
        Opcode::SwitchLong | Opcode::SwitchString | Opcode::Match | Opcode::DoFcall => true,
        Opcode::New => {
            op.extended_value == 0 && after.is_some_and(|after| after.opcode == Opcode::DoFcall)
        }
        _ => false,
    }
}

/// php's unconditional branch (`Z/ext/opcache/jit/zend_jit_trace.c:5655-5691`):
/// a `JMPZ`, `JMPNZ`, `JMPZ_EX` or `JMPNZ_EX` whose condition, once its
/// guards held, is one kind no greater than `true` (`has_concrete_type`,
/// `concrete_type(op1_info) <= IS_TRUE`) goes the way the record went
/// and takes no exit for the way it did not. php's test ignores
/// `MAY_BE_REF`; a condition that may be a reference has no one tag here,
/// so it keeps its exit, one more than php's at worst and never the other
/// way taken.
pub(super) fn goes_one_way(opcode: Opcode, held: Types) -> bool {
    matches!(
        opcode,
        Opcode::Jmpz | Opcode::Jmpnz | Opcode::JmpzEx | Opcode::JmpnzEx
    ) && matches!(
        held.tag(),
        Some(ZvalType::Undef | ZvalType::Null | ZvalType::False | ZvalType::True)
    )
}

/// The oplines whose handler leaves a temporary it reads alive: a
/// `switch`'s or a `match`'s subject, a `foreach`'s iterator, a `list()`'s
/// array, a copied temporary, a coalescing jump's value.
pub(super) fn keeps_temporaries(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Case
            | Opcode::CaseStrict
            | Opcode::SwitchLong
            | Opcode::SwitchString
            | Opcode::Match
            | Opcode::FeFetchR
            | Opcode::FeFetchRw
            | Opcode::FetchListR
            | Opcode::FetchListW
            | Opcode::CopyTmp
            | Opcode::JmpSet
            | Opcode::Coalesce
            | Opcode::JmpNull
    )
}

/// The operands an inline arithmetic serves: two numbers, long or
/// double each, or two longs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Arith {
    Numbers,
    Longs,
}

impl Arith {
    /// Whether operands of `a` and `b` are the ones served.
    pub(super) fn serves(self, a: Types, b: Types) -> bool {
        let one = |t: Types, of: Types| t == of;
        match self {
            Arith::Numbers => {
                (one(a, Types::LONG) || one(a, Types::DOUBLE))
                    && (one(b, Types::LONG) || one(b, Types::DOUBLE))
            }
            Arith::Longs => one(a, Types::LONG) && one(b, Types::LONG),
        }
    }
}

/// The arithmetic the scalar core serves inline, and over what.
pub(super) fn arith(opcode: Opcode) -> Option<Arith> {
    match opcode {
        Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div => Some(Arith::Numbers),
        Opcode::Mod | Opcode::Sl | Opcode::Sr | Opcode::BwAnd | Opcode::BwOr | Opcode::BwXor => {
            Some(Arith::Longs)
        }
        _ => None,
    }
}

/// The oplines that make a frame of a call.
pub(super) fn is_call(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::DoFcall | Opcode::DoIcall | Opcode::DoUcall | Opcode::DoFcallByName
    )
}

/// The inline oplines that may not finish, and leave the trace for the
/// interpreter to serve them again: a division or a modulo by zero or a
/// shift by a negative number throws, where no constant divisor or shift
/// rules it out as php's code generator rules it out
/// ([`cannot_raise`](super::cannot_raise)), an element past a packed array is a hole, a
/// property's slot nothing holds is `__get`'s or the warning's, a
/// call's frame may not fit, a value sent where the callee takes a
/// reference is refused. A `DO_*CALL` the trace follows is not among
/// them: php's code takes no exit there, and what the call's kernel
/// throws, a parameter's check among it, unwinds (#3203's measurement).
pub(super) fn inline_may_leave(line: &Line) -> bool {
    let arithmetic = match (line.opcode, line.imm) {
        (Opcode::AssignOp, Imm::Binary(binary)) => Some(binary),
        (Opcode::Div | Opcode::Mod | Opcode::Sl | Opcode::Sr, _) => Some(line.opcode),
        _ => None,
    };
    if let Some(op) = arithmetic {
        return !super::cannot_raise(op, line.op2);
    }
    matches!(
        line.opcode,
        Opcode::AssignOp
            | Opcode::FetchDimR
            | Opcode::FetchObjR
            | Opcode::FetchObjW
            | Opcode::InitFcall
            | Opcode::InitFcallByName
            | Opcode::InitNsFcallByName
            | Opcode::SendValEx
    )
}

/// One operand of an opline: its `op1`, its `op2`, its `OP_DATA`'s `op1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Site {
    Op1 = 0,
    Op2 = 1,
    Data = 2,
}

/// The type of a sample php's guard compares, `op_type` with a reference
/// or an indirect one `IS_UNKNOWN`.
fn kind_of(sample: Option<Sample>) -> Option<ZvalType> {
    sample
        .filter(|s| !s.reference && !s.indirect)
        .map(|s| s.kind)
}

/// Whether a constant `op2` is a non-empty string, a property's name.
fn named(op: &Opline, code: &OpArray) -> bool {
    op.op2_type == OperandKind::Const
        && code
            .literals
            .get(op.op2 as usize)
            .and_then(|name| name.as_str())
            .is_some_and(|name| !name.bytes().is_empty())
}

/// The operands php's switch in `zend_jit_trace_build_tssa` adds a guard
/// for (`ADD_OP1_TRACE_GUARD`, `ADD_OP2_TRACE_GUARD`,
/// `ADD_OP1_DATA_TRACE_GUARD`), in its order, which marks them; the code
/// generator holds them in [`checked_order`]. A division is left out, as
/// php's switch leaves it (`// case ZEND_DIV: // TODO: check for division
/// by zero ???`): its operands are typed by php's inference over the op
/// array, so a loop's variable `$i` read by `3.0 / $i` is a long there
/// with no guard.
#[allow(
    clippy::too_many_lines,
    clippy::match_same_arms,
    reason = "php's switch, case for case and in its order"
)]
pub(super) fn guard_sites(
    op: &Opline,
    data: Option<&Opline>,
    samples: [Option<Sample>; 3],
    t1: Types,
    code: &OpArray,
) -> Vec<Site> {
    use Site::{Data, Op1, Op2};
    let [k1, k2, _] = samples.map(kind_of);
    let cv1 = op.op1_type == OperandKind::Cv;
    let self_assigned =
        || cv1 && data.is_some_and(|d| d.op1_type == OperandKind::Cv && d.op1 == op.op1);
    match op.opcode {
        Opcode::AssignOp => {
            let unguarded = matches!(
                lattice::binary_of(op.extended_value),
                Some(Opcode::Pow | Opcode::Div)
            );
            if unguarded || !cv1 || op.out_type != OperandKind::Unused {
                vec![]
            } else {
                vec![Op1, Op2]
            }
        }
        Opcode::AssignDimOp if op.out_type != OperandKind::Unused => vec![],
        Opcode::AssignDim if self_assigned() => vec![],
        Opcode::AssignDimOp => vec![Data, Op2, Op1],
        Opcode::AssignDim if cv1 => vec![Data, Op2, Op1],
        Opcode::AssignDim => vec![Op2, Op1],
        Opcode::AssignObjOp
            if matches!(
                lattice::binary_of(op.extended_value),
                Some(Opcode::Pow | Opcode::Div)
            ) || op.out_type != OperandKind::Unused =>
        {
            vec![]
        }
        Opcode::AssignObj | Opcode::AssignObjOp if !named(op, code) || self_assigned() => vec![],
        Opcode::AssignObj | Opcode::AssignObjOp => vec![Data, Op1],
        Opcode::PreIncObj | Opcode::PreDecObj | Opcode::PostIncObj | Opcode::PostDecObj => {
            if named(op, code) { vec![Op1] } else { vec![] }
        }
        Opcode::Concat | Opcode::FastConcat => {
            let strung = |kind: OperandKind, k: Option<ZvalType>| {
                kind == OperandKind::Const || k == Some(ZvalType::String)
            };
            if strung(op.op1_type, k1) && strung(op.op2_type, k2) {
                vec![Op2, Op1]
            } else {
                vec![]
            }
        }
        Opcode::Add | Opcode::Sub | Opcode::Mul
            if k1 == Some(ZvalType::Undef) || k2 == Some(ZvalType::Undef) =>
        {
            vec![]
        }
        Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::IsEqual
        | Opcode::IsNotEqual
        | Opcode::IsSmaller
        | Opcode::IsSmallerOrEqual
        | Opcode::Case
        | Opcode::IsIdentical
        | Opcode::IsNotIdentical
        | Opcode::CaseStrict
        | Opcode::BwOr
        | Opcode::BwAnd
        | Opcode::BwXor
        | Opcode::Sl
        | Opcode::Sr
        | Opcode::Mod => vec![Op2, Op1],
        Opcode::Echo
        | Opcode::Strlen
        | Opcode::Count
        | Opcode::QmAssign
        | Opcode::FeResetR
        | Opcode::FeFetchR
        | Opcode::Jmpz
        | Opcode::Jmpnz
        | Opcode::JmpzEx
        | Opcode::JmpnzEx
        | Opcode::Bool
        | Opcode::BoolNot
        | Opcode::Return => vec![Op1],
        Opcode::VerifyReturnType => {
            if matches!(op.op1_type, OperandKind::Unused | OperandKind::Const) {
                vec![]
            } else {
                vec![Op1]
            }
        }
        Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec if cv1 => vec![Op1],
        Opcode::Assign if cv1 => {
            if k1.is_some() && t1.meets(Types::REF) {
                vec![Op2, Op1]
            } else {
                vec![Op2]
            }
        }
        Opcode::Cast if k1.is_some_and(|k| op.extended_value == u32::from(k as u8)) => vec![Op1],
        Opcode::IssetIsemptyCv if op.extended_value & ISEMPTY == 0 => vec![Op1],
        Opcode::InArray if !matches!(op.op1_type, OperandKind::Var | OperandKind::Tmp) => {
            vec![Op1]
        }
        Opcode::IssetIsemptyDimObj if op.extended_value & ISEMPTY != 0 => vec![],
        Opcode::IssetIsemptyDimObj
        | Opcode::FetchDimR
        | Opcode::FetchDimIs
        | Opcode::FetchListR => {
            vec![Op1, Op2]
        }
        Opcode::FetchDimW | Opcode::FetchDimRw | Opcode::FetchListW
            if cv1 || samples[0].is_some_and(|s| s.indirect) =>
        {
            vec![Op1, Op2]
        }
        Opcode::SendValEx | Opcode::SendVarEx | Opcode::SendVarNoRefEx
            if op.op2_type == OperandKind::Const || op.op2 > MAX_ARG_FLAG_NUM =>
        {
            vec![]
        }
        Opcode::SendValEx
        | Opcode::SendVarEx
        | Opcode::SendVarNoRefEx
        | Opcode::SendVal
        | Opcode::SendVar
        | Opcode::SendVarNoRef
        | Opcode::SendFuncArg
            if op.op2_type != OperandKind::Const =>
        {
            vec![Op1]
        }
        Opcode::FetchObjR | Opcode::FetchObjIs | Opcode::FetchObjW
            if named(op, code)
                && op.op1_type != OperandKind::Unused
                && k1 == Some(ZvalType::Object) =>
        {
            vec![Op1]
        }
        Opcode::InitMethodCall if named(op, code) => vec![Op1],
        Opcode::TypeCheck => {
            // php's `MAY_BE_RESOURCE` is no exact check; guards only for
            // exact ones, "to avoid code duplication".
            let resource = 1 << 9;
            match k1 {
                Some(k) if op.extended_value != resource => {
                    let bit = 1u32 << (k as u8);
                    if op.extended_value == bit || op.extended_value == 0x3fe - bit {
                        vec![Op1]
                    } else {
                        vec![]
                    }
                }
                _ => vec![],
            }
        }
        Opcode::RopeInit | Opcode::RopeAdd | Opcode::RopeEnd if k2 == Some(ZvalType::String) => {
            vec![Op2]
        }
        _ => vec![],
    }
}

/// The order php's code generator holds an opline's operands' types in,
/// as each case of `zend_jit_trace`'s switch calls `CHECK_OP1_TRACE_TYPE`,
/// `CHECK_OP2_TRACE_TYPE` and `CHECK_OP1_DATA_TRACE_TYPE`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:4380-6500`): an `ASSIGN`'s value
/// before its variable (`:5130-5150`), and every other opline's `op1`,
/// then its `op2`, then its `OP_DATA`'s. Their exits are taken in this
/// order, each knowing the types the guards before it held, so it is the
/// order they are numbered in, where [`guard_sites`] says which are held.
pub(super) fn checked_order(opcode: Opcode) -> [Site; 3] {
    match opcode {
        Opcode::Assign => [Site::Op2, Site::Op1, Site::Data],
        _ => [Site::Op1, Site::Op2, Site::Data],
    }
}

/// The operands php's code generator holds the recorded type of where its
/// inference adds no guard (`CHECK_OP_TRACE_TYPE` in
/// `zend_jit_trace.c`'s cases): a concatenation's two, a rope part's, and
/// an arithmetic's where the recording saw a number, in the order it holds
/// them; it holds one only where the operand's inference is marked
/// `MAY_BE_GUARD` ([`guard_sites`] elsewhere).
pub(super) fn check_sites(op: &Opline, samples: [Option<Sample>; 3]) -> Vec<Site> {
    use Site::{Op1, Op2};
    let number = |at: usize| {
        matches!(
            kind_of(samples[at]),
            Some(ZvalType::Long | ZvalType::Double)
        )
    };
    match op.opcode {
        Opcode::Concat | Opcode::FastConcat => vec![Op1, Op2],
        Opcode::RopeInit | Opcode::RopeAdd | Opcode::RopeEnd => vec![Op2],
        Opcode::Add | Opcode::Sub | Opcode::Mul => [(Op1, 0), (Op2, 1)]
            .into_iter()
            .filter_map(|(site, at)| number(at).then_some(site))
            .collect(),
        _ => vec![],
    }
}

/// Whether the key `op2` of a dimension opline, seen as `t2`, is one the
/// lookups take inline: a long, or a string a constant or a variable
/// holds, which the opline gives up nothing of.
pub(super) fn keyed(op: &Opline, t2: Types) -> bool {
    t2 == Types::LONG
        || (t2 == Types::STRING && matches!(op.op2_type, OperandKind::Const | OperandKind::Cv))
}

/// Whether an `ASSIGN_DIM_OP` is compiled inline, as php compiles a
/// compound write to an element over a variable's array, through the
/// reference it may be or the `VAR` a fetch for writing left indirect
/// (`t1` what it sees of it), by a key the lookups
/// take ([`keyed`]), calling its helpers where the array is shared, a
/// packed array short of the key, a key a hash has not got or an element
/// of no kind the operator serves (`zend_jit_assign_dim_op`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:13528-13750`): its `out` slot unused,
/// and its operator one `zend_jit_math_helper` serves over two numbers,
/// `+`, `-`, `*` and `/`, one `zend_jit_long_math_helper` serves over two
/// longs, `%`, the shifts and the bitwise operators, or `.` over two
/// strings, `zend_jit_concat_helper`; php leaves `**` out of all three
/// (`zend_jit_supported_binary_op`).
pub(super) fn compound_in_place(op: &Opline, t1: Types, t2: Types, td: Types) -> bool {
    let served = |binary| match binary {
        Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div => {
            td == Types::LONG || td == Types::DOUBLE
        }
        Opcode::Mod | Opcode::Sl | Opcode::Sr | Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor => {
            td == Types::LONG
        }
        Opcode::Concat => td == Types::STRING,
        _ => false,
    };
    op.out_type == OperandKind::Unused
        && t1.tag() == Some(ZvalType::Array)
        && keyed(op, t2)
        && lattice::binary_of(op.extended_value).is_some_and(served)
}

/// The oplines php follows an indirect `VAR` for, to the element they
/// write, guarding the type of what it points at
/// (`zend_jit_fetch_indirect_var`'s callers, `Z/ext/opcache/jit/zend_jit_trace.c:4749-4762`,
/// `:5068-5083`, `:5906-5922`): a write, a compound write and a fetch for
/// writing of an element.
pub(super) fn written_through(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::AssignDim | Opcode::AssignDimOp | Opcode::FetchDimW | Opcode::FetchDimRw
    )
}

/// php's packed guard over an array `op1` read or written by a long key
/// (`MAY_BE_PACKED_GUARD`): the layout it holds, where php adds one. An
/// array recorded through a reference is held to its layout only where it
/// is read, as php's code generator adds the guard to what
/// `zend_jit_fetch_reference` left of it for a read alone, its trace SSA
/// knowing no type of a reference (`Z/ext/opcache/jit/zend_jit_trace.c:5830-5880`).
pub(super) fn packed_guard(
    op: &Opline,
    samples: [Option<Sample>; 3],
    code: &OpArray,
) -> Option<Layout> {
    let first = samples[0].filter(|s| !s.indirect && s.kind == ZvalType::Array)?;
    let reads = matches!(
        op.opcode,
        Opcode::FetchDimR | Opcode::FetchDimIs | Opcode::FetchListR
    );
    if first.reference && !reads {
        return None;
    }
    let recorded = if first.packed {
        Layout::Packed
    } else {
        Layout::Hash
    };
    let long_key = if op.op2_type == OperandKind::Const {
        code.literals
            .get(op.op2 as usize)
            .is_some_and(|key| key.kind() == ZvalType::Long)
    } else {
        kind_of(samples[1]) == Some(ZvalType::Long)
    };
    let read = op.op1_type != OperandKind::Const && long_key;
    match op.opcode {
        Opcode::FetchDimR | Opcode::FetchDimIs | Opcode::FetchListR => read.then_some(recorded),
        Opcode::IssetIsemptyDimObj if op.extended_value & ISEMPTY == 0 => read.then_some(recorded),
        Opcode::AssignDim | Opcode::FetchDimW | Opcode::FetchDimRw | Opcode::FetchListW => {
            (long_key && recorded == Layout::Hash).then_some(Layout::Hash)
        }
        Opcode::AssignDimOp if long_key && recorded == Layout::Hash => Some(Layout::Hash),
        Opcode::AssignDimOp if long_key => samples[2]
            .filter(|s| s.kind != ZvalType::Undef)
            .map(|_| Layout::Packed),
        Opcode::FeFetchR => Some(recorded),
        _ => None,
    }
}

/// How an inline op holds a guard on what it computes: an arithmetic
/// checks its overflow and leaves for the interpreter to serve it again; a
/// fetch checks the element's type, leaving past itself with the element
/// copied, and, where `missed` says, an element it does not find leaves
/// past it `null` first, `FETCH_DIM_IS`'s `not_found_exit_addr` where the
/// type held is not `null` (`zend_jit_fetch_dim_read`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:12576-12610`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Within {
    Rerun,
    Fetched { missed: bool },
}

pub(super) fn within(opcode: Opcode, tag: ZvalType) -> Option<Within> {
    match opcode {
        Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::PreInc
        | Opcode::PreDec
        | Opcode::PostInc
        | Opcode::PostDec
        | Opcode::AssignOp
            if matches!(tag, ZvalType::Long | ZvalType::Double) =>
        {
            Some(Within::Rerun)
        }
        Opcode::FetchDimR | Opcode::FetchObjR => Some(Within::Fetched { missed: false }),
        Opcode::FetchDimIs => Some(Within::Fetched {
            missed: tag != ZvalType::Null,
        }),
        _ => None,
    }
}

/// Whether php's tracing code generator has a case that compiles `opcode`
/// inline where nothing about its operands breaks it to the handler
/// (`Z/ext/opcache/jit/zend_jit_trace.c:4436-6477`): the calls, the sends
/// and the returns, the comparisons and jumps, the fetches and writes of
/// elements and properties, and the rest its switch lists; `NOP` and
/// `JMP` compile to nothing (`:6480`).
pub(super) fn compiled_inline(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Nop
            | Opcode::Jmp
            | Opcode::AssignDimOp
            | Opcode::PreIncObj
            | Opcode::PreDecObj
            | Opcode::PostIncObj
            | Opcode::PostDecObj
            | Opcode::AssignObjOp
            | Opcode::AssignObj
            | Opcode::AssignDim
            | Opcode::QmAssign
            | Opcode::InitFcall
            | Opcode::InitFcallByName
            | Opcode::InitNsFcallByName
            | Opcode::SendVal
            | Opcode::SendValEx
            | Opcode::SendRef
            | Opcode::SendVar
            | Opcode::SendVarEx
            | Opcode::SendVarNoRef
            | Opcode::SendVarNoRefEx
            | Opcode::SendFuncArg
            | Opcode::CheckFuncArg
            | Opcode::CheckUndefArgs
            | Opcode::DoUcall
            | Opcode::DoIcall
            | Opcode::DoFcallByName
            | Opcode::DoFcall
            | Opcode::IsEqual
            | Opcode::IsNotEqual
            | Opcode::IsSmaller
            | Opcode::IsSmallerOrEqual
            | Opcode::Case
            | Opcode::IsIdentical
            | Opcode::IsNotIdentical
            | Opcode::CaseStrict
            | Opcode::Defined
            | Opcode::Return
            | Opcode::Bool
            | Opcode::BoolNot
            | Opcode::Jmpz
            | Opcode::Jmpnz
            | Opcode::JmpzEx
            | Opcode::JmpnzEx
            | Opcode::JmpFrameless
            | Opcode::FetchDimFuncArg
            | Opcode::FetchDimR
            | Opcode::FetchDimIs
            | Opcode::FetchListR
            | Opcode::FetchDimW
            | Opcode::FetchDimRw
            | Opcode::FetchListW
            | Opcode::IssetIsemptyDimObj
            | Opcode::FetchObjFuncArg
            | Opcode::FetchObjR
            | Opcode::FetchObjIs
            | Opcode::FetchObjW
            | Opcode::FetchStaticPropFuncArg
            | Opcode::FetchStaticPropR
            | Opcode::FetchStaticPropIs
            | Opcode::FetchStaticPropW
            | Opcode::FetchStaticPropRw
            | Opcode::FetchStaticPropUnset
            | Opcode::BindGlobal
            | Opcode::Recv
            | Opcode::RecvInit
            | Opcode::Free
            | Opcode::FeFree
            | Opcode::Echo
            | Opcode::Strlen
            | Opcode::Count
            | Opcode::FetchThis
            | Opcode::SwitchLong
            | Opcode::SwitchString
            | Opcode::Match
            | Opcode::VerifyReturnType
            | Opcode::FeResetR
            | Opcode::FeFetchR
            | Opcode::FetchConstant
            | Opcode::InitMethodCall
            | Opcode::InitStaticMethodCall
            | Opcode::InitDynamicCall
            | Opcode::FramelessIcall0
            | Opcode::FramelessIcall1
            | Opcode::FramelessIcall2
            | Opcode::FramelessIcall3
    )
}
