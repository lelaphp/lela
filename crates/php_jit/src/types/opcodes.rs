//! What the trace SSA knows of each opcode, in one place: which slots an
//! opline reads and writes, what it writes, the operands php's guard
//! switch names (`Z/ext/opcache/jit/zend_jit_trace.c:1835-2245`), and the
//! parameters an op array's `RECV` oplines declare.

use php_engine::tier::Sample;
use php_heap::ZvalType;
use php_unit::decl::{Parameter, Type};
use php_unit::{ConstIdx, OpArray, Opcode, OperandKind, Opline, OplineIdx};

use super::lattice::{self, Types};
use super::{Constant, Frame, Imm, Layout, Line, Operand, Slot};

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
    /// Whether operands of `a` and `b` are the ones served: a number of
    /// either kind tested where it is read, as `zend_jit_math_helper`
    /// tests an operand php's inference types `MAY_BE_LONG|MAY_BE_DOUBLE`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:5332-5520`).
    pub(super) fn serves(self, a: Types, b: Types) -> bool {
        let one = |t: Types, of: Types| t == of;
        let number =
            |t: Types| one(t, Types::LONG) || one(t, Types::DOUBLE) || one(t, Types::NUMBER);
        match self {
            Arith::Numbers => number(a) && number(b),
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

/// What php holds of an operand at an opline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hold {
    /// The type the recording saw of it, in place or where the indirect
    /// `VAR` points: the trace SSA's guard ([`traced`]).
    Recorded,
    /// The reference and the type it refers to, by one exit, php's
    /// `zend_jit_fetch_reference` (`Z/ext/opcache/jit/zend_jit_ir.c:17021-17085`).
    Fetched,
    /// That it is a reference still, by an exit of its own, php's
    /// `zend_jit_guard_reference` (`:16990-17019`) with `add_ref_guard`.
    Reference,
    /// Read through the reference an earlier guard of the trace held,
    /// taking no exit: `zend_jit_guard_reference` without `add_ref_guard`.
    Held,
}

/// The operands whose type php holds at an opline, and what it holds of
/// each, which the code generator holds in [`checked_order`]: those its
/// trace SSA adds a guard for ([`traced`]), and the reference its code
/// generator fetches ([`fetched_reference`]). An operand the recording
/// saw through a reference is php's `IS_UNKNOWN` to its trace SSA, whose
/// `ADD_OP_GUARD` adds nothing for that type
/// (`Z/ext/opcache/jit/zend_jit_trace.c:403-407`, `:1819-1833`), so it
/// is held only where the code generator fetches it. `held` says whether
/// a guard earlier in the trace held `op1`'s value a reference, php's
/// `guarded_reference` on its SSA variable, which elides the guard of an
/// `ASSIGN` (`:5155-5167`).
pub(super) fn guard_sites(
    op: &Opline,
    data: Option<&Opline>,
    samples: [Option<Sample>; 3],
    (types, held): ([Types; 3], bool),
    code: &OpArray,
) -> Vec<(Site, Hold)> {
    let through = |site: Site| samples[site as usize].is_some_and(|s| s.reference && !s.indirect);
    let fetched = fetched_reference(op, data, samples, (types, held), code);
    let mut sites: Vec<(Site, Hold)> = traced(op, data, samples, types[0], code)
        .into_iter()
        .filter(|site| !through(*site))
        .map(|site| (site, Hold::Recorded))
        .collect();
    sites.extend(fetched.filter(|(site, _)| sites.iter().all(|(other, _)| other != site)));
    sites
}

/// The operand whose reference php's code generator holds at `op`,
/// `zend_jit_fetch_reference` (`Z/ext/opcache/jit/zend_jit_ir.c:17021-17085`):
/// the reference and its inner type, by one exit, where it compiles the
/// opline itself over an operand the recording saw through a reference,
/// over `types`, what php's inference knows of the three operands, its
/// `info & (MAY_BE_ANY|MAY_BE_UNDEF)` the kinds beside the reference. Its
/// callers in `zend_jit_trace`'s switch (`Z/ext/opcache/jit/zend_jit_trace.c`):
/// `ADD`, `SUB` and `MUL` over two operands that may be numbers, or two
/// arrays an `ADD` joins, the one variable of the two that is a reference
/// (`:4553-4612`); `ASSIGN_DIM_OP` whose `out` slot is unused, of an
/// operator `zend_jit_supported_binary_op` serves, and not of the array
/// into itself (`:4736-4775`); an object's increment and decrement of a
/// named property (`:4794-4846`); `ASSIGN_OBJ_OP` as `ASSIGN_DIM_OP`
/// (`:4878-4940`); `ASSIGN_OBJ` of a named property (`:4974-5023`);
/// `ASSIGN_DIM` not of the array into itself (`:5064-5095`);
/// `ISSET_ISEMPTY_CV` and `ISSET_ISEMPTY_DIM_OBJ` but for `empty()`
/// (`:5767-5785`, `:5954-5972`); `FETCH_DIM_R`, `FETCH_DIM_IS` and
/// `FETCH_LIST_R` (`:5845-5864`); `FETCH_DIM_W`, `FETCH_DIM_RW` and
/// `FETCH_LIST_W` of a variable (`:5902-5935`); `FETCH_OBJ_R`,
/// `FETCH_OBJ_IS` and `FETCH_OBJ_W` of a named property (`:6047-6097`);
/// `STRLEN` of a string and `COUNT` of an array the recording saw as no packed one,
/// as php's recorder marks `IS_TRACE_PACKED` behind a reference too
/// (`:6224-6259`, `Z/ext/opcache/jit/zend_jit_vm_helpers.c:835-845`); and
/// `INIT_METHOD_CALL` by a constant name (`:6358-6391`). Each object
/// opline but `INIT_METHOD_CALL` is over an operand php's inference says
/// may be an object, and each fetches `op1` but the arithmetic, which
/// fetches the one that is a reference; a temporary is fetched as a
/// variable is, the `VAR` a `DO_FCALL` of a `function &f()` writes among
/// them, at each site but the arithmetic's and those of a variable alone.
/// An `ASSIGN` to a variable holds the reference alone, php's
/// `zend_jit_guard_reference` (`:5155-5167`), and by no exit where
/// `held`, an earlier guard having held the variable's value one
/// ([`Hold::Held`]); a fetch takes its exit whether or not, its type
/// guard asking for it (`add_type_guard`). Every other case reads a reference with
/// `CHECK_OP_TRACE_TYPE`, which holds nothing of an `IS_UNKNOWN` type
/// (`:411-427`): `QM_ASSIGN` (`:5240-5248`), the comparisons
/// (`:5431-5444`, `:5481-5492`), `RETURN` (`:5580-5582`), `BOOL` and
/// `BOOL_NOT` (`:5658-5661`), the jumps (`:5670-5675`), `ECHO`
/// (`:6214-6223`), `FE_RESET_R` and `FE_FETCH_R` (`:6310-6322`), the
/// increments and decrements, `ASSIGN_OP`, the sends and the rest, and
/// the cases above for an operand they do not fetch.
///
/// The program #3688 found, a reference `ECHO` reads in a side trace
/// that php's trace holds by no guard, so the exit after it is php's
/// third, not a fourth:
///
/// ```php
/// function h() { return 1; }
/// function &p($n) { static $z = 0; if ($n == 0) { return $z; } $x = &p($n - 1); h(); return $x; }
/// $r = &p(6); echo $r, "\n";
/// for ($i = 0; $i < 5; $i++) { $r = &p(8); echo $r, "\n"; }
/// echo "done\n";
/// ```
/// ```expect regime=jit-inline process
/// 0
/// 0
/// 0
/// 0
/// 0
/// 0
/// done
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:4
/// ---- TRACE 1 abort (deep recursion)
///
/// ---- TRACE 1 start (enter) p() %s:3
/// ---- TRACE 1 stop (recursive call)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 1 p() %s:3
/// ---- TRACE 2 start (side trace 1/1) p() %s:3
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ---- TRACE 3 start (return) p() %s:3
/// ---- TRACE 3 stop (recursive return)
/// ---- TRACE 3 compiled
///
///      TRACE 3 exit 2 $main() %s:4
/// ---- TRACE 4 start (side trace 3/2) $main() %s:4
/// ---- TRACE 4 stop (link to 1)
/// ---- TRACE 4 compiled
///
///      TRACE 4 exit 0 $main() %s:5
/// ---- TRACE 5 start (side trace 4/0) $main() %s:5
/// ---- TRACE 5 stop (link to 1)
/// ---- TRACE 5 compiled
///
///      TRACE 5 exit 3 $main() %s:6
/// ---- TRACE 6 start (side trace 5/3) $main() %s:6
/// ---- TRACE 6 stop (return)
/// ---- TRACE 6 compiled
/// ```
///
/// A `COUNT` of a reference to a packed array is its handler's, taking
/// no exit, so the loop's direction is php's second exit, not a third:
///
/// ```php
/// function f(array &$a) { $s = 0; for ($i = 0; $i < count($a); $i++) { $s += $a[$i]; } return $s; }
/// $b = [1,2,3,4,5,6,7,8,9,10]; echo f($b), "\n";
/// for ($k = 0; $k < 3; $k++) { $b[] = $k; echo f($b), "\n"; }
/// ```
/// ```expect regime=jit-inline process
/// 55
/// 55
/// 56
/// 58
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) f() %s:2
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 2 f() %s:2
/// ---- TRACE 2 start (side trace 1/2) f() %s:2
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ---- TRACE 3 start (loop) $main() %s:4
/// ---- TRACE 3 stop (link to 1)
/// ---- TRACE 3 compiled
///
/// ---- TRACE 4 start (return) $main() %s:4
/// ---- TRACE 4 stop (link to 3)
/// ---- TRACE 4 compiled
///
///      TRACE 3 exit 1 $main() %s:4
/// ---- TRACE 5 start (side trace 3/1) $main() %s:4
/// ---- TRACE 5 stop (return)
/// ---- TRACE 5 compiled
/// ```
///
/// An `ASSIGN` to a variable recorded through a reference takes an exit
/// that holds the reference, and the `ASSIGN` after it in the same turn
/// none, its guard held already, so the loop's direction is php's exit
/// 3, not a second or a fourth:
///
/// ```php
/// function f() { $x = 0; $r = &$x; $c = 0; for ($i = 0; $i < 100; $i++) { $x = $i; $x = $c; if ($i > 50) { $c++; } } return $c + $x; }
/// echo f(), "\n";
/// ```
/// ```expect regime=jit-inline process
/// 97
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) f() %s:2
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 3 f() %s:2
/// ---- TRACE 2 start (side trace 1/3) f() %s:2
/// ---- TRACE 2 stop (link to 1)
/// ---- TRACE 2 compiled
///
///      TRACE 1 exit 1 f() %s:2
/// ---- TRACE 3 start (side trace 1/1) f() %s:2
/// ---- TRACE 3 stop (return)
/// ---- TRACE 3 compiled
/// ```
#[allow(
    clippy::match_same_arms,
    reason = "php's callers, case for case and in its order"
)]
fn fetched_reference(
    op: &Opline,
    data: Option<&Opline>,
    samples: [Option<Sample>; 3],
    (types, held): ([Types; 3], bool),
    code: &OpArray,
) -> Option<(Site, Hold)> {
    let through = |at: usize| samples[at].is_some_and(|s| s.reference && !s.indirect);
    let cv = |kind: OperandKind| kind == OperandKind::Cv;
    let [t1, t2, td] = types.map(Types::kinds);
    let unused = op.out_type == OperandKind::Unused;
    let object = op.op1_type != OperandKind::Unused && t1.meets(Types::OBJECT);
    let into_itself = cv(op.op1_type) && data.is_some_and(|d| cv(d.op1_type) && d.op1 == op.op1);
    let served =
        || lattice::binary_of(op.extended_value).is_some_and(|binary| supported(binary, td));
    let fetched = match op.opcode {
        Opcode::Add | Opcode::Sub | Opcode::Mul => {
            let joined = op.opcode == Opcode::Add
                && t1.kinds() == Types::ARRAY
                && t2.kinds() == Types::ARRAY;
            let numbers = t1.meets(Types::NUMBER) && t2.meets(Types::NUMBER);
            if t1.meets(Types::UNDEF) || t2.meets(Types::UNDEF) || !(joined || numbers) {
                return None;
            }
            return match (through(0), through(1)) {
                (true, false) if cv(op.op1_type) => Some((Site::Op1, Hold::Fetched)),
                (false, true) if cv(op.op2_type) => Some((Site::Op2, Hold::Fetched)),
                _ => None,
            };
        }
        Opcode::Assign if cv(op.op1_type) && through(0) => {
            return Some((Site::Op1, if held { Hold::Held } else { Hold::Reference }));
        }
        Opcode::AssignDimOp => unused && served() && !into_itself,
        Opcode::PreIncObj | Opcode::PreDecObj | Opcode::PostIncObj | Opcode::PostDecObj => {
            named(op, code) && object
        }
        Opcode::AssignObjOp => unused && named(op, code) && !into_itself && served() && object,
        Opcode::AssignObj => named(op, code) && object,
        Opcode::AssignDim => !into_itself,
        Opcode::IssetIsemptyCv | Opcode::IssetIsemptyDimObj => op.extended_value & ISEMPTY == 0,
        Opcode::FetchDimR | Opcode::FetchDimIs | Opcode::FetchListR => true,
        Opcode::FetchDimW | Opcode::FetchDimRw | Opcode::FetchListW => cv(op.op1_type),
        Opcode::FetchObjR | Opcode::FetchObjIs | Opcode::FetchObjW => named(op, code) && object,
        Opcode::Strlen => samples[0].is_some_and(|s| s.kind == ZvalType::String),
        Opcode::Count => samples[0].is_some_and(|s| s.kind == ZvalType::Array && !s.packed),
        Opcode::InitMethodCall => {
            op.op1_type != OperandKind::Unused
                && op.op2_type == OperandKind::Const
                && (code.literals.get(op.op2 as usize)).is_some_and(|name| name.as_str().is_some())
        }
        _ => false,
    };
    (fetched && through(0)).then_some((Site::Op1, Hold::Fetched))
}

/// What php's code generator leaves of `guarded_reference` on the value an
/// opline writes back to a variable it reads by value or binds, past the
/// reads [`fetched_reference`] names, which mark what they read and write
/// back themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Carried {
    /// The operand's new value is marked as its old one was.
    Kept(Site),
    /// The operand's new value is marked: a reference the opline bound.
    Marked(Site),
}

/// [`Carried`] at `op`, where php's code generator compiles it and so
/// reaches the mark (`Z/ext/opcache/jit/zend_jit_trace.c`): an `ASSIGN`'s
/// variable value (`:5228-5233`), a `QM_ASSIGN`'s operand and a `CAST`'s
/// to the type the recording saw (`:5235-5286`), a send's by value
/// (`:5338-5383`), an `ASSIGN_OBJ`'s and an `ASSIGN_DIM`'s `OP_DATA`
/// (`:5058-5062`, `:5112-5116`); a `SEND_REF`'s variable and a
/// `BIND_GLOBAL`'s are marked (`:5323-5336`, `:6157-6168`).
pub(super) fn carried(
    op: &Opline,
    data: Option<&Opline>,
    samples: [Option<Sample>; 3],
    t1: Types,
    code: &OpArray,
) -> Option<Carried> {
    let cv = |kind: OperandKind| kind == OperandKind::Cv;
    let named_args = op.op2_type == OperandKind::Const;
    let carried = match op.opcode {
        Opcode::Assign if cv(op.op1_type) && cv(op.op2_type) => Carried::Kept(Site::Op2),
        Opcode::Cast
            if kind_of(samples[0]).is_some_and(|k| op.extended_value == u32::from(k as u8)) =>
        {
            Carried::Kept(Site::Op1)
        }
        Opcode::QmAssign => Carried::Kept(Site::Op1),
        Opcode::SendVarEx | Opcode::SendVarNoRefEx if op.op2 > MAX_ARG_FLAG_NUM => return None,
        Opcode::SendVar
        | Opcode::SendVarEx
        | Opcode::SendVarNoRef
        | Opcode::SendVarNoRefEx
        | Opcode::SendFuncArg
            if !named_args =>
        {
            Carried::Kept(Site::Op1)
        }
        Opcode::SendRef if !named_args => Carried::Marked(Site::Op1),
        Opcode::BindGlobal => Carried::Marked(Site::Op1),
        Opcode::AssignDim
            if !(cv(op.op1_type) && data.is_some_and(|d| cv(d.op1_type) && d.op1 == op.op1)) =>
        {
            Carried::Kept(Site::Data)
        }
        Opcode::AssignObj
            if named(op, code)
                && (op.op1_type == OperandKind::Unused || t1.kinds().meets(Types::OBJECT)) =>
        {
            Carried::Kept(Site::Data)
        }
        _ => return None,
    };
    Some(carried)
}

/// php's `zend_jit_supported_binary_op` of a compound write's operator
/// over anything and the value `td` it writes (`Z/ext/opcache/jit/zend_jit.c:1395-1425`).
fn supported(binary: Opcode, td: Types) -> bool {
    if td.meets(Types::UNDEF) {
        return false;
    }
    match binary {
        Opcode::Add | Opcode::Sub | Opcode::Mul => td.meets(Types::NUMBER),
        Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor | Opcode::Sl | Opcode::Sr | Opcode::Mod => {
            td.meets(Types::LONG)
        }
        Opcode::Concat => td.meets(Types::STRING),
        _ => false,
    }
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
fn traced(
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
/// checks its overflow and leaves for the interpreter to serve it again,
/// but an `ADD` or a `SUB` of the constant `1` held long leaves past
/// itself with the double it overflowed to in its `out`,
/// `(double)ZEND_LONG_MAX + 1.0` where it went `up` and
/// `(double)ZEND_LONG_MIN - 1.0` where it went down, as php's
/// `zend_jit_math_long_long` takes that exit at `opline + 1` with the
/// constant in its stack map (`Z/ext/opcache/jit/zend_jit_ir.c:5086-5117`);
/// a fetch checks the element's type, leaving past itself with the element
/// copied, and, where `missed` says, an element it does not find leaves
/// past it `null` first, `FETCH_DIM_IS`'s `not_found_exit_addr` where the
/// type held is not `null` (`zend_jit_fetch_dim_read`,
/// `Z/ext/opcache/jit/zend_jit_ir.c:12576-12610`).
///
/// So the `ADD` that reads a reference past php's fetch of it takes two
/// exits, the fetch's at its opline and the overflow's past it, and the
/// loop's direction is php's exit 3 (#3710):
///
/// ```php
/// function f() { $x = 0; $r = &$x; $c = 0; for ($i = 0; $i < 100; $i++) { $t = $x + 1; $x = $t; if ($i > 50) { $c++; } } return $c + $x; }
/// echo f(), "\n";
/// ```
/// ```expect regime=jit-inline process
/// 149
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) f() %s:2
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 3 f() %s:2
/// ---- TRACE 2 start (side trace 1/3) f() %s:2
/// ---- TRACE 2 stop (link to 1)
/// ---- TRACE 2 compiled
///
///      TRACE 1 exit 0 f() %s:2
/// ---- TRACE 3 start (side trace 1/0) f() %s:2
/// ---- TRACE 3 stop (return)
/// ---- TRACE 3 compiled
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Within {
    Rerun,
    Overflowed { up: bool },
    Fetched { missed: bool },
}

/// [`Within`] of an inline op over `opcode` whose `out` a guard holds to
/// `tag`, `op2` its second operand.
pub(super) fn within(opcode: Opcode, tag: ZvalType, op2: Operand) -> Option<Within> {
    match opcode {
        Opcode::Add | Opcode::Sub
            if tag == ZvalType::Long && op2 == Operand::Constant(Constant::Long(1)) =>
        {
            Some(Within::Overflowed {
                up: opcode == Opcode::Add,
            })
        }
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
