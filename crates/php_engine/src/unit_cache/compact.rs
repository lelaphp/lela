//! The runtime cache size `zend_optimizer_compact_literals` counts for
//! an op array opcache persists (`Z/Zend/Optimizer/compact_literals.c:
//! 452-760`), opcache's optimizer's pass 11: php's compiler gives every
//! site a slot of its own (`zend_alloc_cache_slot`), and the pass gives
//! the slots out again from the op array extensions' end, every call of
//! one function sharing one, every fetch, `new`, `catch` and
//! `instanceof` of one class one, and so for constants, the properties
//! and methods of `$this`, the variables `global` binds and frameless
//! jumps.
//! `cache_size` is what it counts, and the runtime cache a request lays
//! down for such an op array is that many bytes of its heap.
//!
//! The engine runs no pass over what executes (design §6.1), so the
//! oplines keep the compiler's slots, which the engine's cache lays down
//! ([`php_unit::OpArray::cache_size`]), and the count is the room the
//! heap is charged ([`php_unit::OpArray::cache_room`]). The count is the
//! pass's over the compiler's oplines; the passes before it that fold or
//! drop a site (a call `SCCP` evaluates, such as `abs(-1)`, a block the
//! CFG pass finds unreachable, a `INIT_FCALL_BY_NAME` pass 4 turns into
//! an `INIT_FCALL`) move php's count where they apply, and are not
//! counted here.

use std::collections::HashSet;

use php_unit::{OpArray, Opcode, OperandKind, Opline};

use crate::exec::SLOT_BYTES as SLOT;

/// `sizeof(zval)`: what a `RECV_INIT` over an `IS_CONSTANT_AST` keeps its
/// evaluated default in, past every other slot.
const ZVAL: u32 = 16;

/// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE` (`Z/Zend/zend_compile.h:1141`)
/// in a `FETCH_CONSTANT`'s `op1`: its name carries a third constant, the
/// global fallback.
const UNQUALIFIED_IN_NAMESPACE: u32 = 0x800;

/// The pass's tables of shared slots, one per kind of name: `const_slot`,
/// `class_slot`, `func_slot`, `bind_var_slot`, `property_slot`,
/// `method_slot` and `jmp_slot`, each by merged constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Table {
    Constant,
    Class,
    Function,
    BoundVar,
    Property,
    Method,
    Frameless,
}

/// `add_static_slot`'s kinds, the bias on its member string's hash
/// (`Z/Zend/Optimizer/compact_literals.c:34-36`, `:46-77`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Member {
    ClassConstant,
    StaticMethod,
    StaticProperty,
}

/// What a shared slot is found by: a merged constant of the op array in
/// one of the tables, its bytes with how many related constants the pass
/// marked it with (`LITERAL_INFO`), which keeps a name of one kind of
/// site apart from the same bytes of another; or a class's member,
/// `zend_create_member_string`'s pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key<'a> {
    Literal(Table, u8, &'a [u8]),
    Member(Member, &'a [u8], &'a [u8]),
}

/// The pass's count over `code`, from `first`, the op array extensions'
/// slots (`zend_op_array_extension_handles * sizeof(void*)`): a site
/// whose name a site before it took a slot for shares that slot, and
/// every other takes as many as php's compiler gives it. An op array
/// with no constant is left as the compiler sized it, as the pass is.
pub(super) fn cache_size(code: &OpArray, first: u32) -> u32 {
    if code.literals.is_empty() {
        return code.cache_size;
    }
    let mut counted = Counted {
        code,
        size: first,
        taken: HashSet::new(),
    };
    for op in &code.oplines {
        counted.site(op);
    }
    // A default naming a constant keeps its value past every slot, as
    // the pass's last loop over the `RECV*` run lays it; every slot is
    // eight bytes, so the alignment it asks for is already there.
    let defaults = code
        .oplines
        .iter()
        .take_while(|op| matches!(op.opcode, Opcode::Recv | Opcode::RecvInit))
        .filter(|op| op.opcode == Opcode::RecvInit && counted.const_expr(op.op2))
        .count();
    let defaults = u32::try_from(defaults).unwrap_or(u32::MAX);
    counted.size.saturating_add(defaults.saturating_mul(ZVAL))
}

/// The count as the pass walks the oplines.
struct Counted<'a> {
    code: &'a OpArray,
    size: u32,
    taken: HashSet<Key<'a>>,
}

impl<'a> Counted<'a> {
    /// The slots `op` takes, none where it shares one.
    fn site(&mut self, op: &Opline) {
        use Opcode as O;
        let (op1, op2) = (
            op.op1_type == OperandKind::Const,
            op.op2_type == OperandKind::Const,
        );
        let this = op.op1_type == OperandKind::Unused;
        match op.opcode {
            O::AssignObjOp
            | O::AssignObj
            | O::AssignObjRef
            | O::FetchObjR
            | O::FetchObjW
            | O::FetchObjRw
            | O::FetchObjIs
            | O::FetchObjUnset
            | O::FetchObjFuncArg
            | O::UnsetObj
            | O::PreIncObj
            | O::PreDecObj
            | O::PostIncObj
            | O::PostDecObj
            | O::IssetIsemptyPropObj
                if op2 =>
            {
                self.shared_if(this, Table::Property, 1, op.op2, 3);
            }
            O::InitFcall => self.shared(Table::Function, 1, op.op2, 1),
            O::InitFcallByName => self.shared(Table::Function, 2, op.op2, 1),
            O::InitNsFcallByName => self.shared(Table::Function, 3, op.op2, 1),
            O::InitMethodCall if op2 => self.shared_if(this, Table::Method, 2, op.op2, 2),
            O::InitStaticMethodCall if op2 && op1 => {
                self.member(Member::StaticMethod, op.op1, op.op2, 2);
            }
            O::InitStaticMethodCall if op2 => self.fresh(2),
            O::InitStaticMethodCall | O::New | O::Catch if op1 => {
                self.shared(Table::Class, 2, op.op1, 1);
            }
            O::Defined => self.shared(Table::Constant, 1, op.op1, 1),
            O::FetchConstant => {
                let related = if op.op1 & UNQUALIFIED_IN_NAMESPACE != 0 {
                    3
                } else {
                    2
                };
                self.shared(Table::Constant, related, op.op2, 1);
            }
            O::FetchClassConstant if op1 && op2 && self.string(op.op2).is_some() => {
                self.member(Member::ClassConstant, op.op1, op.op2, 2);
            }
            O::FetchClassConstant => self.fresh(2),
            O::AssignStaticPropOp
            | O::AssignStaticProp
            | O::AssignStaticPropRef
            | O::FetchStaticPropR
            | O::FetchStaticPropW
            | O::FetchStaticPropRw
            | O::FetchStaticPropIs
            | O::FetchStaticPropUnset
            | O::FetchStaticPropFuncArg
            | O::UnsetStaticProp
            | O::IssetIsemptyStaticProp
            | O::PreIncStaticProp
            | O::PreDecStaticProp
            | O::PostIncStaticProp
            | O::PostDecStaticProp => match (op1, op2) {
                (true, true) => self.member(Member::StaticProperty, op.op2, op.op1, 3),
                (true, false) => self.fresh(3),
                (false, true) => self.shared(Table::Class, 2, op.op2, 1),
                (false, false) => {}
            },
            O::FetchClass | O::Instanceof if op2 => self.shared(Table::Class, 2, op.op2, 1),
            O::BindGlobal => self.shared(Table::BoundVar, 1, op.op2, 1),
            O::DeclareAnonClass | O::DeclareClassDelayed => self.fresh(1),
            O::JmpFrameless => self.shared(Table::Frameless, 1, op.op1, 1),
            O::SendVal
            | O::SendValEx
            | O::SendVar
            | O::SendVarEx
            | O::SendVarNoRef
            | O::SendVarNoRefEx
            | O::SendRef
            | O::SendFuncArg
            | O::CheckFuncArg
                if op2 =>
            {
                self.fresh(2);
            }
            _ => {}
        }
    }

    /// `slots` of the shared slot `table` keeps for the constant at `at`,
    /// marked with `related` constants: taken where none is kept yet.
    fn shared(&mut self, table: Table, related: u8, at: u32, slots: u32) {
        self.shared_if(true, table, related, at, slots);
    }

    /// [`Self::shared`] where `shares`, as a property's or a method's
    /// slot is shared only for `$this` (`op1` unused); a fresh one where
    /// not, which no later site shares.
    fn shared_if(&mut self, shares: bool, table: Table, related: u8, at: u32, slots: u32) {
        match self.string(at) {
            Some(name) if shares => self.taken_once(Key::Literal(table, related, name), slots),
            _ => self.fresh(slots),
        }
    }

    /// `add_static_slot`: `slots` for the member `member` of `class`,
    /// shared by every site naming the pair.
    fn member(&mut self, kind: Member, class: u32, member: u32, slots: u32) {
        match (self.string(class), self.string(member)) {
            (Some(class), Some(member)) => {
                self.taken_once(Key::Member(kind, class, member), slots);
            }
            _ => self.fresh(slots),
        }
    }

    /// `slots` taken for `key` where nothing has taken them yet.
    fn taken_once(&mut self, key: Key<'a>, slots: u32) {
        if self.taken.insert(key) {
            self.fresh(slots);
        }
    }

    /// `cache_size += slots * sizeof(void *)`.
    fn fresh(&mut self, slots: u32) {
        self.size = self.size.saturating_add(slots.saturating_mul(SLOT));
    }

    /// The bytes of the string constant at `at`.
    fn string(&self, at: u32) -> Option<&'a [u8]> {
        self.code.literals.get(at as usize)?.str_bytes()
    }

    /// Whether the constant at `at` is an `IS_CONSTANT_AST`.
    fn const_expr(&self, at: u32) -> bool {
        self.code
            .literals
            .get(at as usize)
            .is_some_and(|literal| literal.as_const_expr().is_some())
    }
}
