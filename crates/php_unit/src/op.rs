//! Oplines and op arrays (design §5.7): php's instruction set exactly, as
//! `zend_compile.c` and `pass_two` leave it, with the `safepoints` table
//! the compiler numbers beside the line table.

use php_heap::Zval;

use crate::decl::{ConstExpr, Name};
use crate::opcode;
use crate::{Handler, Opcode};

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

/// `ZEND_MAP_PTR(op_array->run_time_cache)` as an op array carries it
/// (design §5.14): which of the runtime caches of the request serving
/// it is the op array's. The engine gives each op array of a unit it
/// publishes an offset of its own map-pointer space
/// (`ZEND_MAP_PTR_NEW`), and each op array of a unit compiled for one
/// request alone a number of that request's, where php gives such an op
/// array a cache on the heap and no offset (`ZEND_ACC_HEAP_RT_CACHE`).
/// The cache itself is the request's and its type the engine's, which
/// converts this where it reads it. The compiler leaves the engine's
/// first offset, which the engine overwrites before the unit is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MapPtrOffset {
    Engine(u32),
    Request(u32),
}

impl Default for MapPtrOffset {
    fn default() -> MapPtrOffset {
        MapPtrOffset::Engine(0)
    }
}

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
    pub fn from_u8(n: u8) -> Option<Opcode> {
        opcode::BY_NUM.get(usize::from(n)).copied().flatten()
    }

    /// php's number, the byte `zend_op.opcode` holds.
    #[must_use]
    pub fn num(self) -> u8 {
        self as u8
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

    /// `ZEND_VM_COMMUTATIVE`: the opcode's handler line is specialised
    /// `COMMUTATIVE`, so `pass_two`'s `zend_vm_set_opcode_handler` swaps
    /// its operands when `op1_type < op2_type`, which is what puts the
    /// CV first in `2 * $a` and `2 == $a` (`Z/Zend/zend_vm_execute.h`,
    /// `zend_vm_set_opcode_handler`). `MUL`, the three bitwise operators,
    /// `BOOL_XOR` and the four equalities; `ADD` is not one, since two
    /// arrays add in order.
    #[must_use]
    pub fn is_commutative(self) -> bool {
        opcode::COMMUTATIVE
            .get(usize::from(self.num()))
            .copied()
            .unwrap_or(false)
    }

    /// `ZEND_OP_IS_FRAMELESS_ICALL`: one of the four opcodes
    /// `zend_compile_frameless_icall` emits, whose `extended_value` is an
    /// index into `zend_flf_functions`.
    #[must_use]
    pub fn is_frameless_icall(self) -> bool {
        matches!(
            self,
            Opcode::FramelessIcall0
                | Opcode::FramelessIcall1
                | Opcode::FramelessIcall2
                | Opcode::FramelessIcall3
        )
    }

    /// How many opcodes php has.
    pub const COUNT: usize = opcode::COUNT;
}

/// One instruction, php's `zend_op` field for field (design §5.7);
/// `out` is php's `result` and `out_type` its `result_type`, spelled so
/// because the glossary keeps that word for `Result`. An operand of
/// kind `TMP` or `VAR` holds its frame slot, past the CVs, which the
/// compiler folds as `pass_two`'s `EX_NUM_TO_VAR(last_var + n)` does,
/// a slot where php's is a byte offset; a `CV`'s is its slot as well.
/// The four bytes before the fields of four are what php's are: the
/// opcode and the kind of each operand, `result`'s last. A site
/// opcode's `out` is its safepoint number and its `out_type` stays
/// `Unused`, as php leaves it; on every other opcode `out_type` is
/// `RETURN_VALUE_USED`, what the compiler wrote where `zend_do_free`
/// found nothing reading the value.
/// `handler` is php's `zend_op.handler`, the handler the opline is
/// served by, which the compiler leaves `Unresolved` and publication
/// resolves from the fields before it ([`Handler`]); php's is a pointer
/// and this its number. Aligned to thirty-two bytes, the size of php's,
/// so an opline's place is its index shifted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, align(32))]
pub struct Opline {
    pub opcode: Opcode,
    pub op1_type: OperandKind,
    pub op2_type: OperandKind,
    pub out_type: OperandKind,
    pub op1: u32,
    pub op2: u32,
    pub out: u32,
    pub extended_value: u32,
    pub lineno: u32,
    pub handler: Handler,
}

impl Default for Opline {
    fn default() -> Opline {
        Opline {
            opcode: Opcode::Nop,
            op1_type: OperandKind::Unused,
            op2_type: OperandKind::Unused,
            out_type: OperandKind::Unused,
            op1: 0,
            op2: 0,
            out: 0,
            extended_value: 0,
            lineno: 0,
            handler: Handler::Unresolved,
        }
    }
}

/// Whether a comparison whose handler line says `SMART_BRANCH` is fused
/// to the opline after it, and to which jump ([`Opline::fuses`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fused {
    No,
    Jmpz,
    Jmpnz,
}

impl Fused {
    /// What `op` is fused to, `next` the opline after it.
    pub(crate) fn of(op: &Opline, next: Option<&Opline>) -> Fused {
        match next {
            Some(jump) if op.fuses(jump) && jump.opcode == Opcode::Jmpz => Fused::Jmpz,
            Some(jump) if op.fuses(jump) => Fused::Jmpnz,
            _ => Fused::No,
        }
    }
}

/// The kind of the value an assignment's `OP_DATA` carries, `next` the
/// opline after the assignment; none where `next` is no `OP_DATA`.
pub(crate) fn data_kind(next: Option<&Opline>) -> Option<OperandKind> {
    next.filter(|data| data.opcode == Opcode::OpData)
        .map(|data| data.op1_type)
}

impl Opline {
    /// Whether `jump`, the opline after this comparison, is the `JMPZ` or
    /// `JMPNZ` that branches on the comparison's temporary, which makes
    /// the comparison's handler one over both oplines: php's
    /// `ZEND_VM_SMART_BRANCH`, which `zend_emit_cond_jump` marks in the
    /// comparison's `result_type` where the jump's `op1` is its `result`
    /// (`Z/Zend/zend_compile.c:2399`), and from whose mark `pass_two`'s
    /// `zend_vm_set_opcode_handler` picks the handler (design §5.8).
    #[must_use]
    pub fn fuses(&self, jump: &Opline) -> bool {
        matches!(jump.opcode, Opcode::Jmpz | Opcode::Jmpnz)
            && self.out_type == OperandKind::Tmp
            && jump.op1_type == OperandKind::Tmp
            && jump.op1 == self.out
    }
    /// The opline the jump lands on, read from the one field whose shape
    /// is `JMP_ADDR` in `zend_vm_opcodes_flags`: `op1` for a `JMP`, `op2`
    /// for a conditional jump, a `FE_RESET_*` or a `CATCH`, and
    /// `extended_value` for a `FE_FETCH_*` or a `SWITCH_*`'s default.
    /// `None` for an opline that jumps nowhere. A `JMP_ADDR` operand
    /// that holds a zval, a `CATCH` with no next catch, is no target.
    #[must_use]
    pub fn jump_target(&self) -> Option<OplineIdx> {
        use crate::dump::{LAST_CATCH, ext, op};
        let flags = self.opcode.flags();
        if flags & op::MASK == op::JMP_ADDR && self.op1_type == OperandKind::Unused {
            return Some(OplineIdx(self.op1));
        }
        if (flags >> 8) & op::MASK == op::JMP_ADDR && self.op2_type == OperandKind::Unused {
            if self.opcode == Opcode::Catch && self.extended_value & LAST_CATCH != 0 {
                return None;
            }
            return Some(OplineIdx(self.op2));
        }
        if flags & ext::MASK == ext::JMP_ADDR {
            return Some(OplineIdx(self.extended_value));
        }
        None
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

/// What a live range holds open, php's `ZEND_LIVE_*` with php's
/// numbers (`Z/Zend/zend_compile.h:180-184`): what the unwinder frees
/// when a throw leaves the range, and what the dump prints after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum LiveKind {
    /// `ZEND_LIVE_TMPVAR`: a temporary whose value is released.
    #[default]
    TmpVar = 0,
    /// `ZEND_LIVE_LOOP`: a `foreach` iterator, freed as `FE_FREE` frees it.
    Loop = 1,
    /// `ZEND_LIVE_SILENCE`: the `error_reporting` a `@` saved, restored.
    Silence = 2,
    /// `ZEND_LIVE_ROPE`: the strings an interpolation has collected.
    Rope = 3,
    /// `ZEND_LIVE_NEW`: the object `NEW` made before its constructor ran.
    New = 4,
}

/// The oplines over which a temporary is live (`zend_live_range`), for
/// the unwinder to free it. `var` is the temporary's frame slot, past
/// the CVs, as an opline's operand holds it, and `kind` is what php
/// packs into the low bits of its `var` (`ZEND_LIVE_MASK`); php stores
/// `EX_NUM_TO_VAR(last_var + var_num) | kind` and reads both back, so
/// two fields hold what one field held, a slot where php's is a byte
/// offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LiveRange {
    pub var: u32,
    pub kind: LiveKind,
    pub start: OplineIdx,
    pub end: OplineIdx,
}

/// One entry of `op_array->static_variables` (`Z/Zend/zend_compile.h:534`):
/// the variable's name and the value the table starts with, which a
/// request copies before its first `BIND_STATIC` of the op array and
/// keeps the copy of, since a published unit is never written
/// (`ZEND_MAP_PTR(static_variables_ptr)`, design §5.14). A closure's
/// `use` variable is `lexical` and starts `null`, as
/// `zend_compile_static_var_common` puts it; a `static` starts at the
/// value its initializer folded to, or `null` where the initializer is
/// computed behind the `BIND_INIT_STATIC_OR_JMP` before it.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundVar {
    pub name: Name,
    pub value: Zval,
    pub lexical: bool,
}

/// One op array with its tables (design §5.7). `safepoints` is the entry,
/// the loop headers and the returns, the entry number zero; the site
/// opcodes carry their number in `out`, php's `result`. One rule for
/// every loop: the entry a backward jump's number names holds the
/// opline the jump lands on, its header, the `FE_FETCH_*` of a
/// `foreach` and the body's first opline of a `for` or a `while`, never
/// the jump's own opline; two backward jumps to one header carry one
/// number. A return's entry holds the `RETURN*` itself.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpArray {
    pub oplines: Vec<Opline>,
    pub literals: Vec<Zval>,
    /// The constant expressions a constant of kind `IS_CONSTANT_AST`
    /// indexes, php's `zend_ast_ref` behind such a zval: what
    /// `zend_compile_params` leaves in a `RECV_INIT`'s `op2` for a
    /// default naming a constant, evaluated by the opline that copies it
    /// and read as written by Reflection.
    pub const_exprs: Vec<ConstExpr>,
    pub vars: Vec<Name>,
    pub num_cvs: u32,
    pub num_tmps: u32,
    pub try_catch: Vec<TryCatch>,
    pub live_ranges: Vec<LiveRange>,
    pub safepoints: Safepoints,
    pub filename: Name,
    pub line_start: u32,
    pub line_end: u32,
    pub cache_size: u32,
    /// `ZEND_MAP_PTR(op_array->run_time_cache)`: which runtime cache of
    /// the request serving the op array is its own ([`MapPtrOffset`]),
    /// laid down on the op array's first activation.
    pub runtime_cache: MapPtrOffset,
    /// `op_array->dynamic_func_defs` (`Z/Zend/zend_compile.h:559`): the
    /// functions written inside this op array that no name binds while
    /// the file compiles, in the order `zend_add_dynamic_func_def` took
    /// them, which is the order a `DECLARE_FUNCTION`'s or a
    /// `DECLARE_LAMBDA_FUNCTION`'s `op2` indexes. php holds a pointer
    /// per entry; a unit holds every such op array once, in
    /// [`crate::Unit::dynamic`], and the entry is its index there, so
    /// no part of a unit points into another.
    pub dynamic_func_defs: Vec<u32>,
    /// `op_array->static_variables` (`Z/Zend/zend_compile.h:534`): the
    /// variables the op array binds through `BIND_STATIC`, in the order
    /// `zend_compile_static_var_common` added them to the table, which
    /// is what a `BIND_LEXICAL`'s, a `BIND_STATIC`'s and a
    /// `BIND_INIT_STATIC_OR_JMP`'s `extended_value` names: a closure's
    /// `use` variables, written or implied, first, then every `static`
    /// of the body. The values a request binds live in the request, per
    /// closure object or per function, never here.
    pub bound_vars: Vec<BoundVar>,
    /// `ZEND_ACC_STATIC` on the op array (`Z/Zend/zend_compile.h:283`):
    /// a closure written `static function` or `static fn`, which
    /// `ZEND_DECLARE_LAMBDA_FUNCTION` reads to bind it no receiver. A
    /// method's own is its [`crate::decl::MethodDecl::is_instance`],
    /// which the linker reads; nothing but a closure sets this.
    pub no_receiver: bool,
    /// `ZEND_ACC_STRICT_TYPES` on the op array
    /// (`Z/Zend/zend_compile.h:409`): the file the op array was compiled
    /// from was declared `declare(strict_types=1)`, which
    /// `zend_compile_declare` sets on the main op array and
    /// `zend_compile_func_decl` copies to every op array compiled under
    /// it (`Z/Zend/zend_compile.c:8465`). What php's
    /// `ZEND_CALL_USES_STRICT_TYPES` asks of a frame's function: the
    /// caller's decides an argument's coercion and the frame's own a
    /// return's and a typed property's.
    pub strict_types: bool,
    /// `ZEND_TYPE_PURE_MASK` of `arg_info[-1]`, the declared return
    /// type's [`crate::decl::Type::mask`] as `zend_compile_typename`
    /// computes it, and 0 where none is declared: the one test a
    /// `VERIFY_RETURN_TYPE` makes before it reads the declaration. A
    /// class name adds no bit and `static`'s matches no value, so the
    /// link changes nothing of it.
    pub return_mask: u32,
    /// `zend_handle_numeric_dim`'s pairs (`Z/Zend/zend_compile.c:2788`):
    /// for every constant dimension whose written string was a canonical
    /// decimal integer, the slot of `literals` holding the integer php
    /// folded it to, with the string as written in the slot right after
    /// it. php marks the first of the two `Z_EXTRA = ZEND_EXTRA_VALUE`
    /// and a dimension handler over an object reads the second (`dim++`,
    /// `Z/Zend/zend_vm_def.h:6822`), so `$o['10']` hands `offsetGet` the
    /// string `"10"` where `$a['10']` is the key `10` (bug #63217). The
    /// slots are ascending, as the compiler lays them down.
    pub numeric_dims: Vec<ConstIdx>,
}

/// An op array's safepoints (design §5.7), numbered by place: the
/// entry, then each loop header and return as the compiler meets it.
/// Nearly every op array has a handful, which stand inline, so the table
/// makes no allocation of its own until it holds more than
/// [`Safepoints::INLINE`]; it reads as the slice of them either way.
#[derive(Debug, Clone)]
pub struct Safepoints(Sites);

/// Where the safepoints stand: inline up to [`Safepoints::INLINE`], on
/// the heap past it.
#[derive(Debug, Clone)]
enum Sites {
    Inline {
        len: u8,
        sites: [OplineIdx; Safepoints::INLINE],
    },
    Spilled(Vec<OplineIdx>),
}

impl Safepoints {
    /// How many safepoints stand inline: the entry and five more, a
    /// function's returns and a loop or two.
    pub const INLINE: usize = 6;

    /// No safepoint.
    #[must_use]
    pub const fn new() -> Safepoints {
        Safepoints(Sites::Inline {
            len: 0,
            sites: [OplineIdx(0); Safepoints::INLINE],
        })
    }

    /// `at` numbered next.
    pub fn push(&mut self, at: OplineIdx) {
        match &mut self.0 {
            Sites::Inline { len, sites } => {
                if let Some(slot) = sites.get_mut(usize::from(*len)) {
                    *slot = at;
                    *len += 1;
                } else {
                    let mut spilled = Vec::with_capacity(2 * Safepoints::INLINE);
                    spilled.extend_from_slice(sites);
                    spilled.push(at);
                    self.0 = Sites::Spilled(spilled);
                }
            }
            Sites::Spilled(spilled) => spilled.push(at),
        }
    }

    /// The heap's room shrunk to what is held, inline again where that
    /// fits.
    fn shrink_to_fit(&mut self) {
        if let Sites::Spilled(spilled) = &mut self.0 {
            if spilled.len() <= Safepoints::INLINE {
                *self = Safepoints::from(std::mem::take(spilled));
            } else {
                spilled.shrink_to_fit();
            }
        }
    }
}

impl Default for Safepoints {
    fn default() -> Safepoints {
        Safepoints::new()
    }
}

impl From<Vec<OplineIdx>> for Safepoints {
    fn from(sites: Vec<OplineIdx>) -> Safepoints {
        if sites.len() > Safepoints::INLINE {
            return Safepoints(Sites::Spilled(sites));
        }
        let mut inline = Safepoints::new();
        for at in sites {
            inline.push(at);
        }
        inline
    }
}

impl std::ops::Deref for Safepoints {
    type Target = [OplineIdx];

    fn deref(&self) -> &[OplineIdx] {
        match &self.0 {
            Sites::Inline { len, sites } => sites.get(..usize::from(*len)).unwrap_or_default(),
            Sites::Spilled(spilled) => spilled,
        }
    }
}

impl std::ops::DerefMut for Safepoints {
    fn deref_mut(&mut self) -> &mut [OplineIdx] {
        match &mut self.0 {
            Sites::Inline { len, sites } => sites.get_mut(..usize::from(*len)).unwrap_or_default(),
            Sites::Spilled(spilled) => spilled,
        }
    }
}

impl PartialEq for Safepoints {
    fn eq(&self, other: &Safepoints) -> bool {
        **self == **other
    }
}

impl Eq for Safepoints {}

impl OpArray {
    /// Every opline's handler resolved from its opcode and the kinds of
    /// its operands, as php's `pass_two` calls `zend_vm_set_opcode_handler`
    /// on every opline it finishes ([`crate::Unit::published`]).
    pub(crate) fn resolve(&mut self) {
        for at in 0..self.oplines.len() {
            let handler = match self.oplines.get(at) {
                Some(op) => Handler::of(op, self.oplines.get(at + 1)),
                None => continue,
            };
            if let Some(op) = self.oplines.get_mut(at) {
                op.handler = handler;
            }
        }
    }

    /// The tables shrunk to what they hold ([`crate::Unit::compact`]).
    pub(crate) fn compact(&mut self) {
        self.oplines.shrink_to_fit();
        self.literals.shrink_to_fit();
        self.const_exprs.shrink_to_fit();
        self.vars.shrink_to_fit();
        self.try_catch.shrink_to_fit();
        self.live_ranges.shrink_to_fit();
        self.safepoints.shrink_to_fit();
        self.dynamic_func_defs.shrink_to_fit();
        self.bound_vars.shrink_to_fit();
        self.numeric_dims.shrink_to_fit();
    }

    /// `Z_EXTRA_P(dim) == ZEND_EXTRA_VALUE ? dim + 1 : dim`: the
    /// dimension a handler over an object reads for the `CONST` operand
    /// at `at`, which is the string as written where
    /// `zend_handle_numeric_dim` folded one, and the operand's own slot
    /// of `literals` everywhere else.
    #[must_use]
    pub fn dimension_written(&self, at: ConstIdx) -> Option<Zval> {
        let paired = self
            .numeric_dims
            .binary_search_by_key(&at.0, |c| c.0)
            .is_ok();
        let at = if paired { at.0.saturating_add(1) } else { at.0 };
        self.literals.get(at as usize).copied()
    }
}

#[cfg(test)]
mod tests {
    /// A unit held for more than one request keeps no table's slack: the
    /// emitter's vectors grow as they are written and stay so for an
    /// `eval`'s unit, and [`crate::Unit::compact`] shrinks every op
    /// array's, a method's among them, to what it holds.
    #[test]
    fn a_compacted_unit_holds_no_more_than_its_tables_hold() {
        let grown = || {
            let mut oplines = Vec::with_capacity(16);
            oplines.push(Opline::default());
            let mut literals = Vec::with_capacity(16);
            literals.push(Zval::long(1));
            OpArray {
                oplines,
                literals,
                safepoints: (0..9).map(OplineIdx).collect::<Vec<_>>().into(),
                ..OpArray::default()
            }
        };
        let mut unit = crate::Unit {
            main: grown(),
            methods: vec![crate::unit::MethodBody {
                class: 0,
                method: 0,
                body: grown(),
            }],
            ..crate::Unit::default()
        };
        unit.compact();
        for body in [&unit.main, &unit.methods[0].body] {
            assert_eq!(body.oplines.capacity(), 1);
            assert_eq!(body.literals.capacity(), 1);
            assert!(matches!(body.safepoints.0, Sites::Spilled(ref s) if s.capacity() == 9));
        }
    }

    use super::*;

    /// The table stands inline to [`Safepoints::INLINE`], spills past it
    /// in order, and comes back inline where a shrink finds room.
    #[test]
    fn safepoints_stand_inline_until_they_spill() {
        let mut sites = Safepoints::new();
        for at in 0..7 {
            assert!(matches!(sites.0, Sites::Inline { .. }), "{at} inline");
            sites.push(OplineIdx(at));
        }
        assert!(matches!(sites.0, Sites::Spilled(_)));
        assert_eq!(&*sites, &(0..7).map(OplineIdx).collect::<Vec<_>>()[..]);
        if let Some(first) = sites.first_mut() {
            first.0 = 3;
        }
        assert_eq!(sites.first(), Some(&OplineIdx(3)));
        sites.shrink_to_fit();
        assert!(matches!(sites.0, Sites::Spilled(ref s) if s.capacity() == 7));
        let back = Safepoints::from(vec![OplineIdx(1), OplineIdx(2)]);
        assert!(matches!(back.0, Sites::Inline { len: 2, .. }));
        assert_eq!(back, Safepoints::from(vec![OplineIdx(1), OplineIdx(2)]));
    }

    #[test]
    fn an_opline_is_thirty_two_bytes_and_the_enum_keeps_php_numbers() {
        assert_eq!(
            std::mem::size_of::<Opline>(),
            32,
            "php's zend_op: twenty-four bytes of fields, the handler and its alignment"
        );
        assert_eq!(std::mem::size_of::<Handler>(), 2);
        assert_eq!(
            std::mem::size_of::<Opcode>(),
            1,
            "php's `zend_op.opcode` is one byte"
        );
        assert_eq!(std::mem::size_of::<OperandKind>(), 1);
        assert_eq!(Opcode::Nop.num(), 0);
        assert_eq!(Opcode::Add.num(), 1);
        assert_eq!(Opcode::Add.name(), "ZEND_ADD");
        assert_eq!(Opcode::from_u8(1), Some(Opcode::Add));
        assert_eq!(
            Opcode::from_u8(45),
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
        assert!(Opcode::Mul.is_commutative());
        assert!(Opcode::IsEqual.is_commutative());
        assert!(Opcode::IsNotIdentical.is_commutative());
        assert!(!Opcode::Add.is_commutative(), "two arrays add in order");
        assert!(!Opcode::IsSmaller.is_commutative());
    }

    /// An opline of `opcode` over operands of these kinds into the
    /// temporary 3.
    fn over(opcode: Opcode, op1_type: OperandKind, op2_type: OperandKind) -> Opline {
        Opline {
            opcode,
            op1_type,
            op2_type,
            out_type: OperandKind::Tmp,
            out: 3,
            ..Opline::default()
        }
    }

    /// The handlers of `oplines` as a unit of them resolves them where it
    /// is published, none resolved before.
    fn resolved(oplines: Vec<Opline>) -> Vec<Handler> {
        let unit = crate::Unit {
            main: OpArray {
                oplines,
                ..OpArray::default()
            },
            ..crate::Unit::default()
        };
        assert!(
            unit.main
                .oplines
                .iter()
                .all(|op| op.handler == Handler::Unresolved),
            "the compiler leaves every opline unresolved"
        );
        let unit = unit.published(None);
        unit.main.oplines.iter().map(|op| op.handler).collect()
    }

    /// The handlers publication resolves: a comparison whose line says
    /// `SMART_BRANCH` takes the variant of the jump after it where the
    /// jump branches on its temporary and the plain one otherwise, a
    /// specialised opcode the variant of its operands' kinds, one over a
    /// `JMP_ADDR` operand none for it, and the kinds a row does not list
    /// a variant of their own.
    #[test]
    fn publication_resolves_each_oplines_handler_from_it_and_the_jump_after_it() {
        let jump = |opcode, op1| Opline {
            opcode,
            op1_type: OperandKind::Tmp,
            op1,
            ..Opline::default()
        };
        let (cv, konst, tmp) = (OperandKind::Cv, OperandKind::Const, OperandKind::Tmp);
        assert_eq!(
            resolved(vec![
                over(Opcode::IsSmaller, cv, konst),
                jump(Opcode::Jmpz, 3)
            ]),
            [Handler::IsSmallerCvConstJmpz, Handler::JmpzTmp]
        );
        assert_eq!(
            resolved(vec![over(Opcode::IsEqual, cv, cv), jump(Opcode::Jmpnz, 3)]),
            [Handler::IsEqualCvCvJmpnz, Handler::JmpnzTmp]
        );
        assert_eq!(
            resolved(vec![
                over(Opcode::IsSmallerOrEqual, cv, tmp),
                jump(Opcode::Jmpz, 3)
            ]),
            [Handler::IsSmallerOrEqualJmpz, Handler::JmpzTmp]
        );
        assert_eq!(
            resolved(vec![over(Opcode::IsSmaller, cv, cv), jump(Opcode::Jmpz, 4)]),
            [Handler::IsSmallerCvCv, Handler::JmpzTmp],
            "a jump over another temporary is no fused jump"
        );
        assert_eq!(
            resolved(vec![
                over(Opcode::IsNotEqual, cv, cv),
                over(Opcode::Add, tmp, konst)
            ]),
            [Handler::IsNotEqual, Handler::AddTmpConst]
        );
        assert_eq!(
            resolved(vec![over(Opcode::IsSmaller, cv, cv)]),
            [Handler::IsSmallerCvCv],
            "the last opline has no jump after it"
        );
        assert_eq!(
            resolved(vec![over(Opcode::Add, OperandKind::Unused, cv)]),
            [Handler::AddUnlisted]
        );
    }

    /// An assignment whose line says `SPEC(OP_DATA=...)` is resolved over
    /// the kind of the `OP_DATA` after it.
    #[test]
    fn publication_resolves_an_assignment_over_its_op_datas_kind() {
        let (cv, konst) = (OperandKind::Cv, OperandKind::Const);
        let data = |op1_type| Opline {
            opcode: Opcode::OpData,
            op1_type,
            ..Opline::default()
        };
        for (kind, handler) in [
            (konst, Handler::AssignDimCvConstOpDataConst),
            (OperandKind::Tmp, Handler::AssignDimCvConstOpDataTmp),
            (OperandKind::Var, Handler::AssignDimCvConstOpDataVar),
            (cv, Handler::AssignDimCvConstOpDataCv),
        ] {
            assert_eq!(
                resolved(vec![over(Opcode::AssignDim, cv, konst), data(kind)]),
                [handler, Handler::OpData]
            );
        }
        assert_eq!(
            resolved(vec![over(Opcode::AssignObj, OperandKind::Unused, konst)]),
            [Handler::AssignObjUnlisted],
            "an assignment with no OP_DATA after it is no unit the compiler writes"
        );
    }
}
