//! A known callee's push, the call that makes it its frame and the
//! callee's leave, written in place over the engine's frame layout
//! (#3222's ruling, `jit-design.md` §5.3): php's `zend_jit_push_call_frame`,
//! `zend_jit_do_fcall` with `i_init_func_execute_data` inline and
//! `zend_jit_leave_func` (`Z/ext/opcache/jit/zend_jit_ir.c:8466-8650`,
//! `:10016-10600`, `:11035-11263`). Every offset is
//! [`php_engine::tier::FRAME`]'s, read here as Rust constants.
//!
//! Each writes what the engine's own push, call and leave write, as the
//! parity test in `enter`'s tests holds it, and the [`super::Stood`] the
//! kernel it stands in for would have written, so the code after it reads
//! where the interpreter stands as it reads it after a kernel. Each falls
//! back to that kernel where the engine's own path would refuse the plain
//! case: the page's end or a full frame table at the push; the interrupt
//! raised, or a callee's cache not yet laid down, at the call; and at the
//! leave a frame that holds a receiver, a closure, an annex or arguments
//! past its parameters, one a native's call made, one off the top page,
//! or a caller's slot that holds a counted value. None takes an exit of
//! its own: php's code takes none there.

use std::num::NonZeroU32;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I32, I64};
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, MemFlagsData, Value};
use cranelift_frontend::Variable;
use php_engine::tier::{self, FRAME, FrameLayout, FuncRef, PLACE, PlaceLayout, PlainCall};
use php_heap::{Obj, Ref, Zval, ZvalType};
use php_unit::{Opcode, OperandKind};

use super::{
    COUNTED, Dtor, EX_OPLINE, Emit, KIND, Kernel, Refuse, STOOD_CALL, STOOD_FRAME, STOOD_FUNC,
    STOOD_OPLINE, STOOD_ROOM, Scalar, TYPE_AT, imm, leave_of, mismatch, tag, trusted,
};
use crate::lower::{IMAGES_AT, INTERRUPT_AT, OperandKinds, READY_AT, RETURNED, THREW};
use crate::types::{
    Abort, ArgChecks, Check, Constant, Defaults, End, ExitId, Frame, Home, Imm, Line, Object, OpId,
    Operand, Owners, PairCallee, PairClass, Returned, Types, Vars,
};

/// The receiver a method's call `line` pushes in place.
fn receiver_of(line: &Line) -> Result<Object, Abort> {
    match line.op1 {
        Operand::Unused => Ok(Object::This(line.frame)),
        Operand::Value(value) => Ok(Object::Value(value)),
        Operand::Constant(_) => Err(mismatch("a method called on a constant")),
    }
}

/// How a call pushed in place takes its receiver, as the handler's
/// inline path takes it (`inline_init_method_call`): `$this`, lent; a
/// variable's object, one owner more; a temporary's, its owner moved out
/// of the slot.
#[derive(Clone, Copy)]
enum Taken {
    Lent,
    Shared,
    Moved,
}

/// The engine's runtime-cache slot layout, as the guard on a call's pair
/// reads it.
const P: PlaceLayout = PLACE;

/// What a push in place under its pair's guard writes past the image it
/// copies ([`Emit::pair_guard`]): the called scope where the image names
/// none, and the receiver lent from the calling frame.
#[derive(Default)]
struct Over {
    scope: Option<Scope>,
    this: Option<Value>,
}

/// A called scope compiled code writes: a class's number, which the
/// header keeps inverted, or the calling frame's word as it stands.
enum Scope {
    Number(Value),
    Word(Value),
}

/// What a leave in place writes into the caller's slot: a scalar, or a
/// zval's two words, copied from a constant or moved from a temporary.
#[derive(Clone, Copy)]
enum Out {
    Scalar(Scalar),
    Copied(Value, Value),
    Moved(Value, Value),
}

/// The engine's frame layout, as compiled.
const L: FrameLayout = FRAME;

/// A layout offset as the immediate a load or a store takes.
const fn at(word: u32) -> i32 {
    imm(word as usize)
}

/// Where a counted block keeps its count.
const COUNT_AT: i32 = imm(Zval::COUNT_AT);
/// Where an object counts the frames it was lent to ([`Obj::PINS_AT`]).
const PINS_AT: i32 = imm(Obj::PINS_AT);

/// Where variable `var` of a frame stands past its room.
fn var_at(var: u32) -> Result<i32, Abort> {
    let slot = u64::from(L.room()) + u64::from(SLOT) * u64::from(var);
    i32::try_from(slot).map_err(|_| mismatch("a variable past an immediate"))
}

/// A slot, `sizeof(zval)`, the layout's.
const SLOT: u32 = L.slot();
const _: () = assert!(L.header_bytes().is_multiple_of(8));

/// Where the stack keeps its words.
pub(super) const TOP: i32 = at(L.top());
const END: i32 = at(L.end());
const FRAMES: i32 = at(L.frames());
pub(super) const FRAMES_LEN: i32 = at(L.frames_len());
const FRAMES_CAP: i32 = at(L.frames_cap());
const INNERMOST: i32 = at(L.innermost());

/// What a leave in place reads of the frame table before it frees its
/// variables, held in variables a kernel call among the frees reads again:
/// the table, the caller's slots and the slot taking the value.
struct Reread {
    /// The stack, the caller's place and the slot the call returns into.
    from: (Value, Value, Value),
    /// The frame table, the caller's slots and the slot taking the value.
    vars: [Variable; 3],
}

impl Reread {
    /// The reads `values` held in variables of `emit`'s function.
    fn new(emit: &mut Emit<'_>, from: (Value, Value, Value), values: [Value; 3]) -> Reread {
        let vars = values.map(|value| {
            let var = emit.b.declare_var(I64);
            emit.b.def_var(var, value);
            var
        });
        Reread { from, vars }
    }

    /// The frame table, the caller's record and the slot read again, after
    /// a kernel that may have served PHP code.
    fn again(&self, emit: &mut Emit<'_>) {
        let (stack, place, returns) = self.from;
        let frames = emit.b.ins().load(I64, trusted(), stack, FRAMES);
        let callers = emit.record_word(frames, place, L.record_at());
        let target = emit.taken_at(callers, returns);
        for (var, value) in self.vars.iter().zip([frames, callers, target]) {
            emit.b.def_var(*var, value);
        }
    }
}

impl Emit<'_> {
    /// The request's VM stack, read at entry.
    pub(super) fn stack(&self) -> Result<Value, Abort> {
        self.stack
            .ok_or_else(|| mismatch("a call in place with no stack read"))
    }

    /// `where_` and `then` blocks for a path in place and its kernel's,
    /// the second cold.
    fn split(&mut self) -> (Block, Block) {
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        (slow, self.b.create_block())
    }

    /// The `Stood` a kernel would write, written by code in place.
    fn stand(&mut self, [frame, call, func, opline, room]: [Value; 5]) {
        for (value, at) in [
            (frame, STOOD_FRAME),
            (call, STOOD_CALL),
            (func, STOOD_FUNC),
            (opline, STOOD_OPLINE),
            (room, STOOD_ROOM),
        ] {
            self.b.ins().stack_store(I64, value, self.stood, at);
        }
    }

    /// The bytes the top page has left past `top`, `EG(vm_stack_end) - top`.
    pub(super) fn left_past(&mut self, stack: Value, top: Value) -> Value {
        let end = self.b.ins().load(I64, trusted(), stack, END);
        self.b.ins().isub(end, top)
    }

    /// Where the request's header for `plain` is read from: its place
    /// among the trace's calls in place, in bytes past [`IMAGES_AT`]'s
    /// address.
    fn header_of(&self, plain: &PlainCall) -> Result<i32, Abort> {
        let at = (self.held.iter())
            .position(|held| held == plain)
            .ok_or_else(|| mismatch("a call in place the trace does not hold"))?;
        // One address a call, `IMAGES_AT`'s table.
        i32::try_from(at * size_of::<usize>()).map_err(|_| mismatch("too many calls in place"))
    }

    /// php's `zend_jit_func_guard` after a stepped `INIT_*`
    /// ([`crate::types::Check::Callee`]): leave through `exit` unless the
    /// call its handler pushed, the one the kernel stood, serves `func`,
    /// its `EX(func)` the record word of the request's image of a held
    /// call to it; and where no call was pushed, or the request is not
    /// ready for the trace's calls in place. `static::f()` serves `B::f`
    /// where the trace recorded `A::f`, and leaves at the guard; a
    /// constructor made in place throws out of its call:
    ///
    /// ```php
    /// class A { static function f($x) { return $x; } static function g($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += static::f($i); } return $t; } }
    /// class B extends A { static function f($x) { return 2 * $x; } }
    /// class C { public $v; function __construct($v) { if ($v == 7) { throw new Exception("seven"); } $this->v = $v; } }
    /// function n($k) { $t = 0; for ($i = 0; $i < $k; $i++) { try { $c = new C($i); $t += $c->v; } catch (Exception $e) { echo $e->getMessage(), " "; } } return $t; }
    /// echo A::g(30), " ", B::g(30), " ", n(12), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 435 870 seven 59
    /// ```
    ///
    /// ```php
    /// class A { static function f($x) { return $x; } static function g($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += static::f($i); } return $t; } }
    /// class B extends A { static function f($x) { return 2 * $x; } }
    /// class C { public $v; function __construct($v) { if ($v == 7) { throw new Exception("seven"); } $this->v = $v; } }
    /// function n($k) { $t = 0; for ($i = 0; $i < $k; $i++) { try { $c = new C($i); $t += $c->v; } catch (Exception $e) { echo $e->getMessage(), " "; } } return $t; }
    /// echo A::g(30), " ", B::g(30), " ", n(12), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 435 870 seven 59
    /// ```
    pub(super) fn callee_guard(&mut self, func: FuncRef, exit: ExitId) -> Result<(), Abort> {
        let held = (self.held.iter())
            .find(|held| held.callee() == Some(func))
            .ok_or_else(|| mismatch("a callee guard on a call the trace does not hold"))?;
        let header = self.header_of(held)?;
        let call = self.stood_word(I64, STOOD_CALL);
        let none = self.b.ins().icmp_imm_s(IntCC::Equal, call, 0);
        let unready = self.unready()?;
        let refused = self.b.ins().bor(none, unready);
        self.leave_if(refused, exit)?;
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a callee guard before the entry"))?;
        let images = self.b.ins().load(I64, trusted(), cx, IMAGES_AT);
        let image = self.b.ins().load(I64, trusted(), images, header);
        let expected = self
            .b
            .ins()
            .load(I64, trusted(), image, at(L.callee() - L.header()));
        let pushed = self.b.ins().load(I64, trusted(), call, at(L.callee()));
        let other = self.b.ins().icmp(IntCC::NotEqual, pushed, expected);
        self.leave_if(other, exit)
    }

    /// Whether the request is not ready for the trace's calls in place,
    /// an `i8` ([`READY_AT`]).
    fn unready(&mut self) -> Result<Value, Abort> {
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a call in place before the entry"))?;
        let ready = self.b.ins().load(I32, trusted(), cx, READY_AT);
        Ok(self.b.ins().icmp_imm_s(IntCC::Equal, ready, 0))
    }

    /// An inline `INIT_*` whose callee needs no guard pushed in place, as
    /// `zend_jit_push_call_frame` pushes it and `push_call` lays it down:
    /// the room taken off the top of the stack's top page, its link the
    /// innermost room and the word moved to it, the header the push wrote
    /// copied whole, every slot `UNDEF`, and its record on the frame table.
    /// Where the page has less room than the frame takes or the table is
    /// full, the opline is stepped, which pushes the same call; php's own
    /// stack check, a guard before the push, leaves first where the page
    /// is short of what the trace pushes ([`crate::types::Check::Room`]).
    ///
    /// A method's call held by the pair its runtime-cache slot holds
    /// ([`Imm::Pair`]) is pushed where the slot holds the pair the trace
    /// recorded ([`Emit::pair_guard`]), its called scope written where its
    /// class is the request's own or the calling frame forwards it, and
    /// the calling frame's `$this` lent where a static call's syntax
    /// calls a method that takes it, and its handler pushes it otherwise,
    /// which looks the class and the method up and may push another
    /// callee, so the call it pushed is held to the one recorded after it
    /// ([`Emit::callee_guard`]), as php's code calls
    /// `zend_jit_find_method_helper` or `zend_jit_find_static_method_helper`
    /// cold and holds the function after (`Z/ext/opcache/jit/zend_jit_ir.c:9047-9145`,
    /// `:9243-9300`).
    pub(super) fn push_in_place(
        &mut self,
        id: OpId,
        line: &Line,
        plain: &PlainCall,
    ) -> Result<(), Abort> {
        let (slow, join) = self.split();
        let over = match line.imm {
            Imm::Pair { function, class } => {
                self.pair_guard(line, plain, (function, class), slow)?
            }
            _ => Over::default(),
        };
        let stack = self.stack()?;
        let header = self.header_of(plain)?;
        let unready = self.unready()?;
        let used = i64::from(L.room() + SLOT * plain.slots());
        let top = self.b.ins().load(I64, trusted(), stack, TOP);
        let left = self.left_past(stack, top);
        let short = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, left, used);
        let len = self.b.ins().load(I64, trusted(), stack, FRAMES_LEN);
        let cap = self.b.ins().load(I64, trusted(), stack, FRAMES_CAP);
        let full = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, len, cap);
        let refused = self.b.ins().bor(short, full);
        let refused = self.b.ins().bor(refused, unready);
        self.divert_if(refused, slow);
        // The link, the room linked before this one, and the header.
        let below = self.b.ins().load(I64, trusted(), stack, INNERMOST);
        self.b.ins().store(trusted(), below, top, at(L.link()));
        let cx = self.cx.ok_or_else(|| mismatch("a push before the entry"))?;
        let images = self.b.ins().load(I64, trusted(), cx, IMAGES_AT);
        let header = self.b.ins().load(I64, trusted(), images, header);
        for word in (0..L.header_bytes()).step_by(8) {
            let value = self.b.ins().load(I64, trusted(), header, at(word));
            self.b
                .ins()
                .store(trusted(), value, top, at(L.header() + word));
        }
        let zero = self.b.ins().iconst(I64, 0);
        for slot in 0..plain.slots() {
            let cell = L.room() + SLOT * slot;
            self.b.ins().store(trusted(), zero, top, at(cell));
            self.b.ins().store(trusted(), zero, top, at(cell + 8));
        }
        match over.scope {
            Some(Scope::Number(class)) => {
                // `ScopeAt`'s half-word: the class's number inverted.
                let word = self.b.ins().bnot(class);
                (self.b.ins()).store(trusted(), word, top, at(L.called_scope()));
            }
            Some(Scope::Word(word)) => {
                (self.b.ins()).store(trusted(), word, top, at(L.called_scope()));
            }
            None => {}
        }
        if line.opcode == Opcode::InitMethodCall {
            self.receive_of(line, top)?;
        } else if let Some(this) = over.this {
            self.receive(this, Taken::Lent, top);
        }
        self.b.ins().store(trusted(), top, stack, INNERMOST);
        // The record, then the table's length and the top.
        let frames = self.b.ins().load(I64, trusted(), stack, FRAMES);
        let offset = self.b.ins().imul_imm_s(len, i64::from(L.record()));
        let record = self.b.ins().iadd(frames, offset);
        let first = self.b.ins().iadd_imm_s(top, i64::from(L.room()));
        self.b
            .ins()
            .store(trusted(), first, record, at(L.record_at()));
        let slots = self.b.ins().iconst(I64, i64::from(plain.slots()));
        self.b
            .ins()
            .store(trusted(), slots, record, at(L.record_len()));
        let tag = self.b.ins().iconst(I8, i64::from(L.on_page()));
        self.b
            .ins()
            .store(trusted(), tag, record, at(L.record_placed()));
        let grown = self.b.ins().iadd_imm_s(len, 1);
        self.b.ins().store(trusted(), grown, stack, FRAMES_LEN);
        let past = self.b.ins().iadd_imm_s(top, used);
        self.b.ins().store(trusted(), past, stack, TOP);
        // Where the interpreter stands after the `INIT_*`: on past it in
        // the frame, the call pushed under way.
        let room = self.room(line.frame)?;
        let func = self.func_of(line.frame)?;
        let next = self.b.ins().iconst(I32, i64::from(line.opline.0 + 1));
        let left = self.left_past(stack, past);
        self.stand([room, top, func, next, left]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let [room, opline] = self.served(line)?;
        let func = self.func_of(line.frame)?;
        let stood = self.b.ins().stack_addr(I64, self.stood, 0);
        self.call(Kernel::Step, &[room, opline, func, stood], leave_of(line))?;
        for guard in self.within(id) {
            match guard.check {
                Check::Callee { func } => self.callee_guard(func, guard.exit)?,
                _ => return Err(mismatch("a push in place holds a guard of another kind")),
            }
        }
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        Ok(())
    }

    /// php's guard on the pair a method's call reads from its own
    /// runtime-cache slot before it pushes the call in place
    /// (`zend_jit_init_method_call`, `zend_jit_init_static_method_call`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:9025-9056`, `:9255-9265`): on to
    /// `slow` unless the slot holds a pair whose callee is `function`, by
    /// its number, or by its record where it is a request-scoped class's
    /// own method, compared with the record of the header the request laid
    /// out for the call ([`PairCallee::Record`]), and whose class is the
    /// receiver's, or the published class the call names; through `self` or `parent`, whose class the op array's scope
    /// fixes, the class is not compared, as php reads `fbc` alone. What
    /// the push writes past the image ([`Over`]): the class's number where
    /// the call's called scope is a request-scoped class, the calling
    /// frame's own called scope where `self` or `parent` forwards it, as
    /// php takes `EX(This)`'s (`:9340-9358`), and the calling frame's
    /// `$this` where the callee takes it, held to the class the trace
    /// recorded, as php's `zend_jit_push_this_method_call_frame` holds it
    /// to be an instance of the class named; nothing where the image
    /// names it all. A frame with no called scope or no `$this`, and
    /// `$this` of another class, are the handler's, which raises php's
    /// `Error` where it is no instance. The scope the slot was primed
    /// from is not compared: [`tier::UnitView::hold_callee`] takes only a
    /// slot of an op array of one scope that its own frame reads, so
    /// every frame primes it alike.
    ///
    /// A request-scoped class's own method, held by its record: a lazy
    /// copy's, on an object of the copy, of a subclass whose slot's pair
    /// names another class, and of an evaluated class whose method of the
    /// same name is another, its static method through its name, and an
    /// enum's on two cases, under force and where every guard fails once:
    ///
    /// ```php
    /// class Getter { public $b = 1; }
    /// eval('class Base extends Getter { }');
    /// class Child extends Base { function get($v) { return $this->b + $v; } static function twice($v) { return $v + $v; } }
    /// class Grand extends Child { }
    /// eval('class Other { public $b = 100; function get($v) { return -$v; } }');
    /// enum Suit { case Hearts; case Spades; function weight($v) { return $this === Suit::Hearts ? $v * 2 : $v; } }
    /// function f($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->get($i); } return $t; }
    /// function s($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += Child::twice($i); } return $t; }
    /// function c($e, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $e->weight($i); } return $t; }
    /// echo f(new Child, 30), " ", f(new Grand, 30), " ", f(new Other, 30), " ", f(new Child, 5), " ", s(30), " ", c(Suit::Hearts, 30), " ", c(Suit::Spades, 30), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 465 465 -435 15 870 870 435
    /// ```
    ///
    /// ```php
    /// class Getter { public $b = 1; }
    /// eval('class Base extends Getter { }');
    /// class Child extends Base { function get($v) { return $this->b + $v; } static function twice($v) { return $v + $v; } }
    /// class Grand extends Child { }
    /// eval('class Other { public $b = 100; function get($v) { return -$v; } }');
    /// enum Suit { case Hearts; case Spades; function weight($v) { return $this === Suit::Hearts ? $v * 2 : $v; } }
    /// function f($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->get($i); } return $t; }
    /// function s($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += Child::twice($i); } return $t; }
    /// function c($e, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $e->weight($i); } return $t; }
    /// echo f(new Child, 30), " ", f(new Grand, 30), " ", f(new Other, 30), " ", f(new Child, 5), " ", s(30), " ", c(Suit::Hearts, 30), " ", c(Suit::Spades, 30), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 465 465 -435 15 870 870 435
    /// ```
    ///
    /// Through `self` a static method forwarding `static`, and a method
    /// taking `$this` through its class's name and through `parent`, from
    /// an instance of the class the trace recorded, of a subclass, and of
    /// no subclass, under force and where every guard fails once:
    ///
    /// ```php
    /// class A { public $v = 1; static function f($x) { return static::class . $x; } function m($x) { return $this->v + $x; }
    ///   static function selfs($n) { $s = ""; for ($i = 0; $i < $n; $i++) { $s = self::f($i); } return $s; }
    ///   function named($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += A::m($i); } return $t; } }
    /// class B extends A { public $v = 10; function p($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += parent::m($i); } return $t; } }
    /// class X { function call($n) { $t = 0; for ($i = 0; $i < $n; $i++) { try { $t += A::m($i); } catch (Error $e) { return $e->getMessage(); } } return $t; } }
    /// echo A::selfs(30), " ", B::selfs(30), " ", (new A)->named(30), " ", (new B)->named(30), " ", (new B)->p(30), "\n";
    /// echo (new X)->call(3), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// A29 B29 465 735 735
    /// Non-static method A::m() cannot be called statically
    /// ```
    ///
    /// ```php
    /// class A { public $v = 1; static function f($x) { return static::class . $x; } function m($x) { return $this->v + $x; }
    ///   static function selfs($n) { $s = ""; for ($i = 0; $i < $n; $i++) { $s = self::f($i); } return $s; }
    ///   function named($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += A::m($i); } return $t; } }
    /// class B extends A { public $v = 10; function p($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += parent::m($i); } return $t; } }
    /// class X { function call($n) { $t = 0; for ($i = 0; $i < $n; $i++) { try { $t += A::m($i); } catch (Error $e) { return $e->getMessage(); } } return $t; } }
    /// echo A::selfs(30), " ", B::selfs(30), " ", (new A)->named(30), " ", (new B)->named(30), " ", (new B)->p(30), "\n";
    /// echo (new X)->call(3), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// A29 B29 465 735 735
    /// Non-static method A::m() cannot be called statically
    /// ```
    ///
    /// `zend_lazy_class_load`'s copy of a class linked over one the request
    /// evaluated, whose method and static method are pushed in place, then
    /// an evaluated class whose method of the same name is another and a
    /// published class's, a static call on a published class, and a static
    /// call whose class a second request names as another, under force
    /// and where every guard fails once:
    ///
    /// ```php
    /// eval('class Base { public $b = 1; function get($v) { return $this->b + $v; } static function twice($v) { return 2 * $v; } }');
    /// class Child extends Base { static function who() { return static::class; } }
    /// eval('class Other { public $b = 100; function get($v) { return -$v; } }');
    /// class Pub { public $b = 7; function get($v) { return $this->b * $v; } static function three($v) { return 3 * $v; } }
    /// function f($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->get($i); } return $t; }
    /// function s($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += Child::twice($i) + Pub::three($i); $w = Child::who(); } return "$t $w"; }
    /// echo f(new Child, 30), " ", f(new Other, 30), " ", f(new Pub, 30), " ", f(new Child, 5), "\n";
    /// echo s(30), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 465 -435 3045 15
    /// 2175 Child
    /// ```
    ///
    /// ```php
    /// eval('class Base { public $b = 1; function get($v) { return $this->b + $v; } static function twice($v) { return 2 * $v; } }');
    /// class Child extends Base { static function who() { return static::class; } }
    /// eval('class Other { public $b = 100; function get($v) { return -$v; } }');
    /// class Pub { public $b = 7; function get($v) { return $this->b * $v; } static function three($v) { return 3 * $v; } }
    /// function f($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->get($i); } return $t; }
    /// function s($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += Child::twice($i) + Pub::three($i); $w = Child::who(); } return "$t $w"; }
    /// echo f(new Child, 30), " ", f(new Other, 30), " ", f(new Pub, 30), " ", f(new Child, 5), "\n";
    /// echo s(30), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 465 -435 3045 15
    /// 2175 Child
    /// ```
    fn pair_guard(
        &mut self,
        line: &Line,
        plain: &PlainCall,
        (function, class): (PairCallee, PairClass),
        slow: Block,
    ) -> Result<Over, Abort> {
        let refuse = Refuse::To(slow);
        let place = match class {
            PairClass::Receiver => {
                let object = receiver_of(line)?;
                let obj = self.object(object)?;
                self.cache_guard(line, (P.pair(), P.pair_class()), obj, refuse)?
            }
            PairClass::Published(_)
            | PairClass::Named
            | PairClass::Forwarded
            | PairClass::This { .. } => self.cached_slot(line, P.pair(), refuse)?,
        };
        match function {
            PairCallee::Number(function) => {
                let callee = (self.b.ins()).load(I32, trusted(), place, at(P.pair_function()));
                let other = self
                    .b
                    .ins()
                    .icmp_imm_u(IntCC::NotEqual, callee, i64::from(function));
                self.refuse_if(other, refuse)?;
            }
            PairCallee::Record => {
                // The record of the header the request laid out for the
                // call, read once the request is ready, so laid out.
                let unready = self.unready()?;
                self.refuse_if(unready, refuse)?;
                let header = self.header_of(plain)?;
                let cx = self
                    .cx
                    .ok_or_else(|| mismatch("a pair's record read before the entry"))?;
                let images = self.b.ins().load(I64, trusted(), cx, IMAGES_AT);
                let image = self.b.ins().load(I64, trusted(), images, header);
                let laid = (self.b.ins()).load(I64, trusted(), image, at(L.callee() - L.header()));
                let record = (self.b.ins()).load(I64, trusted(), place, at(P.pair_record()));
                let other = self.b.ins().icmp(IntCC::NotEqual, record, laid);
                self.refuse_if(other, refuse)?;
            }
        }
        let named = match class {
            PairClass::Published(class)
            | PairClass::This {
                named: Some(class), ..
            } => Some(class),
            _ => None,
        };
        if let Some(class) = named {
            let held = (self.b.ins()).load(I32, trusted(), place, at(P.pair_class()));
            let number = i64::from(tier::class_number(class));
            let other = self.b.ins().icmp_imm_u(IntCC::NotEqual, held, number);
            self.refuse_if(other, refuse)?;
        }
        match class {
            PairClass::Published(_) => Ok(Over::default()),
            PairClass::Receiver | PairClass::Named => {
                let held = (self.b.ins()).load(I32, trusted(), place, at(P.pair_class()));
                Ok(Over {
                    scope: Some(Scope::Number(held)),
                    this: None,
                })
            }
            PairClass::Forwarded => {
                // `static` of the calling frame, its `ScopeAt` as it
                // stands; a frame with none is the handler's to refuse.
                let room = self.room(line.frame)?;
                let word = (self.b.ins()).load(I32, trusted(), room, at(L.called_scope()));
                let none = self.b.ins().icmp_imm_u(IntCC::Equal, word, 0);
                self.refuse_if(none, refuse)?;
                Ok(Over {
                    scope: Some(Scope::Word(word)),
                    this: None,
                })
            }
            PairClass::This { receiver, .. } => {
                // `Z_TYPE(EX(This)) == IS_OBJECT` and of the class the
                // trace recorded, which is an instance of the one named;
                // any other `$this`, or none, is the handler's, which
                // raises php's `Error` where it is no instance.
                let room = self.room(line.frame)?;
                let this = self.b.ins().load(I64, trusted(), room, at(L.this()));
                let none = self.b.ins().icmp_imm_u(IntCC::Equal, this, 0);
                self.refuse_if(none, refuse)?;
                let of = self.b.ins().load(I32, trusted(), this, imm(Obj::CLASS_AT));
                let number = i64::from(tier::class_number(receiver));
                let other = self.b.ins().icmp_imm_u(IntCC::NotEqual, of, number);
                self.refuse_if(other, refuse)?;
                Ok(Over {
                    scope: None,
                    this: Some(this),
                })
            }
        }
    }

    /// The receiver of method's call `line` pushed in place at `top`,
    /// taken as [`Taken`] says: a temporary's moved out of its slot.
    fn receive_of(&mut self, line: &Line, top: Value) -> Result<(), Abort> {
        let object = receiver_of(line)?;
        let this = self.object(object)?;
        let taken = match object {
            Object::This(_) => Taken::Lent,
            Object::Value(value) => {
                let slot = self.value(value)?.slot;
                if self.is_temporary(slot)? {
                    let (room, slot_at) = self.place(slot)?;
                    let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
                    (self.b.ins()).store(trusted(), undef, room, slot_at + TYPE_AT);
                    Taken::Moved
                } else {
                    Taken::Shared
                }
            }
        };
        self.receive(this, taken, top);
        Ok(())
    }

    /// The receiver of a method's call pushed in place at `top`, as
    /// `zend_jit_push_call_frame` writes it (`Z/ext/opcache/jit/zend_jit_ir.c:8657-8678`):
    /// `Z_PTR(call->This)`, and over a variable one owner more of it with
    /// `ZEND_CALL_RELEASE_THIS`; over a temporary, `(new D)->m()`, its
    /// owner moved out of the slot, which is left undefined, with
    /// `ZEND_CALL_RELEASE_THIS`; over `$this` the frame's receiver lent,
    /// pinned as [`php_heap::Obj::pin`] pins it, which the leave lets go
    /// ([`Emit::free_this`]). An object of another class leaves at the
    /// class guard before the push; a receiver whose method gives up the
    /// last other owner is destroyed as its frame leaves:
    ///
    /// ```php
    /// class A { public $v = 1; function m($x) { return $this->v + $x + $this->n(); } function n() { return 10; } }
    /// class B extends A { function m($x) { return 100; } }
    /// function f($os, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $o = $os[$i % 3]; $t += $o->m($i); } return $t; }
    /// class C { public $o; function m() { $this->o = null; return 1; } function __destruct() { echo "c "; } }
    /// function g($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $c = new C; $c->o = $c; $t += $c->m(); unset($c); } return $t; }
    /// class D { function m($x) { return $x + 1; } function __destruct() { echo "d "; } }
    /// function h($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += (new D)->m($i); } return $t; }
    /// echo f([new A, new A, new B], 30), " ", g(3), " ", h(3), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1500 c c c 3 d d d 6
    /// ```
    ///
    /// ```php
    /// class A { public $v = 1; function m($x) { return $this->v + $x + $this->n(); } function n() { return 10; } }
    /// class B extends A { function m($x) { return 100; } }
    /// function f($os, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $o = $os[$i % 3]; $t += $o->m($i); } return $t; }
    /// class C { public $o; function m() { $this->o = null; return 1; } function __destruct() { echo "c "; } }
    /// function g($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $c = new C; $c->o = $c; $t += $c->m(); unset($c); } return $t; }
    /// class D { function m($x) { return $x + 1; } function __destruct() { echo "d "; } }
    /// function h($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += (new D)->m($i); } return $t; }
    /// echo f([new A, new A, new B], 30), " ", g(3), " ", h(3), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 1500 c c c 3 d d d 6
    /// ```
    fn receive(&mut self, this: Value, taken: Taken, top: Value) {
        self.b.ins().store(trusted(), this, top, at(L.this()));
        if let Taken::Moved = taken {
            let info = self.b.ins().load(I32, trusted(), top, at(L.info()));
            let info = self.b.ins().bor_imm_u(info, i64::from(L.release_this()));
            self.b.ins().store(trusted(), info, top, at(L.info()));
        } else if let Taken::Shared = taken {
            // `GC_ADDREF`, the count kept below the bit above it.
            let count = self.b.ins().load(I32, trusted(), this, COUNT_AT);
            let bits = self.b.ins().band_imm_u(count, i64::from(Zval::COUNT_BITS));
            let room =
                (self.b.ins()).icmp_imm_u(IntCC::NotEqual, bits, i64::from(Zval::COUNT_BITS));
            let room = self.b.ins().uextend(I32, room);
            let more = self.b.ins().iadd(count, room);
            self.b.ins().store(trusted(), more, this, COUNT_AT);
            let info = self.b.ins().load(I32, trusted(), top, at(L.info()));
            let info = self.b.ins().bor_imm_u(info, i64::from(L.release_this()));
            self.b.ins().store(trusted(), info, top, at(L.info()));
        } else {
            let word = self.b.ins().load(I32, trusted(), this, PINS_AT);
            let pins = self.b.ins().band_imm_u(word, i64::from(Obj::PINS));
            let room = self
                .b
                .ins()
                .icmp_imm_u(IntCC::NotEqual, pins, i64::from(Obj::PINS));
            let room = self.b.ins().uextend(I32, room);
            let word = self.b.ins().iadd(word, room);
            self.b.ins().store(trusted(), word, this, PINS_AT);
        }
    }

    /// The checks of a call's arguments made in place, after its frame is
    /// the callee's and before it stands past its `RECV`s, as php's trace
    /// compiles `zend_jit_recv` and `zend_jit_verify_arg_type`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:10750-10873`): an argument whose
    /// type its parameter's mask holds goes on; one it does not is handed,
    /// cold, to its own `RECV`'s handler through [`Kernel::Handle`], php's
    /// `zend_jit_verify_arg_slow`, which coerces it where it stands or
    /// throws from the callee's frame, stood at that `RECV`, the caller at
    /// its call; the call goes on in place either way it did not throw.
    /// What the coercion runs, an error handler or `__toString`, writes
    /// nothing the trace holds in a register, as no kernel's user code
    /// does (`Builder::clobber`). A call whose argument its `RECV` coerces,
    /// one whose `RECV_INIT`'s is checked, and one whose error handler
    /// throws from the coercion and whose `TypeError` a caller catches,
    /// with the caller's registers kept across them:
    ///
    /// ```php
    /// function twice(int $n) { return $n * 2; }
    /// function opt(int $n, int $m = 3) { return $n + $m; }
    /// function f($k) { $t = 0; $x = 1.5; for ($i = 0; $i < $k; $i++) { $t += twice($i % 2 ? "4" : $i) + opt("1", $i) + $x; } return $t; }
    /// function g($k) { $t = 0; $x = 2; for ($i = 0; $i < $k; $i++) { try { $t += twice($i == 3 ? [] : $i) + $x; } catch (TypeError $e) { $t += 100; } } return $t; }
    /// set_error_handler(function ($no, $msg) { throw new Exception($msg); });
    /// function h($k) { $t = 0; for ($i = 0; $i < $k; $i++) { try { $t += twice($i == 4 ? 1.5 : $i); } catch (Exception $e) { echo $e->getMessage(), "\n"; } } return $t; }
    /// echo f(8), " ", g(6), "\n", h(6), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 104 134
    /// Implicit conversion from float 1.5 to int loses precision
    /// 22
    /// ```
    ///
    /// ```php
    /// function twice(int $n) { return $n * 2; }
    /// function opt(int $n, int $m = 3) { return $n + $m; }
    /// function f($k) { $t = 0; $x = 1.5; for ($i = 0; $i < $k; $i++) { $t += twice($i % 2 ? "4" : $i) + opt("1", $i) + $x; } return $t; }
    /// function g($k) { $t = 0; $x = 2; for ($i = 0; $i < $k; $i++) { try { $t += twice($i == 3 ? [] : $i) + $x; } catch (TypeError $e) { $t += 100; } } return $t; }
    /// set_error_handler(function ($no, $msg) { throw new Exception($msg); });
    /// function h($k) { $t = 0; for ($i = 0; $i < $k; $i++) { try { $t += twice($i == 4 ? 1.5 : $i); } catch (Exception $e) { echo $e->getMessage(), "\n"; } } return $t; }
    /// echo f(8), " ", g(6), "\n", h(6), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 104 134
    /// Implicit conversion from float 1.5 to int loses precision
    /// 22
    /// ```
    fn verify_args(
        &mut self,
        line: &Line,
        (room, func): (Value, Value),
        checks: ArgChecks,
    ) -> Result<(), Abort> {
        for (arg, mask) in checks.each() {
            let taken = self.takes(room, arg, mask)?;
            let (other, on) = (self.b.create_block(), self.b.create_block());
            self.b.set_cold_block(other);
            self.b.ins().brif(taken, on, &[], other, &[]);
            self.b.switch_to_block(other);
            self.b.seal_block(other);
            let recv = self.b.ins().iconst(I32, i64::from(arg));
            self.call(Kernel::Handle, &[room, recv, func], leave_of(line))?;
            self.b.ins().jump(on, &[]);
            self.b.switch_to_block(on);
            self.b.seal_block(on);
        }
        Ok(())
    }

    /// A `DO_*CALL` of a call pushed in place made its callee's frame in
    /// place, as `zend_jit_do_fcall` makes it with
    /// `i_init_func_execute_data` inline and `Request::enter_call` writes
    /// it: the flags, the opline past the prologue, the callee's own cache,
    /// the slot of the caller's that takes the return and the caller
    /// written over the pushed header, the caller stood at the call,
    /// php's `SET_EX_OPLINE`, and the arguments checked before the opline
    /// is written ([`Emit::verify_args`]). Where the interrupt is raised, which the
    /// callee's activation tests, or the callee's cache is not laid down,
    /// [`Kernel::Call`] makes the call; where the callee serves a prologue
    /// it does on every pass.
    pub(super) fn make_in_place(
        &mut self,
        id: OpId,
        line: &Line,
        (returns, checks, defaults): (Option<u32>, ArgChecks, Defaults),
    ) -> Result<(), Abort> {
        let (call, plain) = *self
            .calls
            .last()
            .ok_or_else(|| mismatch("a call made with no call under way"))?;
        let callee = self
            .entered_by(id, line)
            .ok_or_else(|| mismatch("a call made in place the trace does not follow"))?;
        let (Some(activation), super::CallRoom::Held(var)) =
            (plain.and_then(PlainCall::activation), call)
        else {
            return self.stepped(id, line);
        };
        let stack = self.stack()?;
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a call made before the entry"))?;
        let interrupt = self.b.ins().load(I64, trusted(), cx, INTERRUPT_AT);
        let raised = self.b.ins().load(I8, trusted(), interrupt, 0);
        let raised = self.b.ins().icmp_imm_s(IntCC::NotEqual, raised, 0);
        let unready = self.unready()?;
        let refused = self.b.ins().bor(raised, unready);
        let room = self.b.use_var(var);
        let (slow, join) = self.split();
        self.divert_if(refused, slow);
        self.copy_defaults(room, defaults)?;
        let flags = self.b.ins().load(I32, trusted(), room, at(L.info()));
        let kept = self.b.ins().band_imm_u(flags, i64::from(activation.keep()));
        let set = i64::from(activation.info(returns.is_some()));
        let flags = self.b.ins().bor_imm_u(kept, set);
        self.b.ins().store(trusted(), flags, room, at(L.info()));
        let words = [
            (L.cache(), activation.cache()),
            (
                L.returns_to(),
                returns.map_or(0, |slot| slot + L.numbered_from()),
            ),
        ];
        for (field, word) in words {
            let word = self.b.ins().iconst(I32, i64::from(word));
            self.b.ins().store(trusted(), word, room, at(field));
        }
        let place = self.frame_ref(line.frame)?;
        let place = self.b.ins().ireduce(I32, place);
        let named = self.b.ins().iadd_imm_s(place, i64::from(L.numbered_from()));
        self.b.ins().store(trusted(), named, room, at(L.caller()));
        let from = self.room(line.frame)?;
        let opline = self.b.ins().iconst(I32, i64::from(line.opline.0));
        self.b.ins().store(trusted(), opline, from, at(L.opline()));
        let func = self.func_of(callee)?;
        self.verify_args(line, (room, func), checks)?;
        let entry = self.b.ins().iconst(I32, i64::from(activation.opline()));
        self.b.ins().store(trusted(), entry, room, at(L.opline()));
        // Where the interpreter stands: in the callee, at its entry.
        let none = self.b.ins().iconst(I64, 0);
        let top = self.b.ins().load(I64, trusted(), stack, TOP);
        let left = self.left_past(stack, top);
        self.stand([room, none, func, entry, left]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        self.step_kernel(id, line)?;
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        self.after_step(id, line)
    }

    /// Copies each default `defaults` names into its parameter of the
    /// call whose room is `room`, its two words as they stand among the
    /// op array's constants, which no reference counts
    /// (`jit_ZVAL_COPY_CONST`, `Z/ext/opcache/jit/zend_jit_ir.c:10886-10950`).
    fn copy_defaults(&mut self, room: Value, defaults: Defaults) -> Result<(), Abort> {
        for (arg, address) in defaults.each() {
            let to = var_at(arg)?;
            let address = i64::try_from(address.get())
                .map_err(|_| mismatch("a constant past a 64-bit address"))?;
            let zval = self.b.ins().iconst(I64, address);
            let word = self.b.ins().load(I64, trusted(), zval, 0);
            let info = self.b.ins().load(I32, trusted(), zval, super::TYPE_AT);
            self.b.ins().store(trusted(), word, room, to);
            self.b
                .ins()
                .store(trusted(), info, room, to + super::TYPE_AT);
        }
        Ok(())
    }

    /// Whether argument `arg` of the call whose room is `room`, from zero,
    /// is of a type `mask` takes, `mask >> Z_TYPE_P(arg) & 1`, read
    /// through the reference it is where it is one, as php's `RECV`
    /// reads it (`ZVAL_DEREF`) and its trace's `zend_jit_recv` checks it
    /// inline (`Z/ext/opcache/jit/zend_jit_ir.c:10825-10873`): one where
    /// the `RECV` would take it untouched, zero where it would coerce or
    /// refuse it.
    fn takes(&mut self, room: Value, arg: u32, mask: NonZeroU32) -> Result<Value, Abort> {
        let at = var_at(arg)?;
        let byte = self.b.ins().load(I8, trusted(), room, at + super::TYPE_AT);
        let reference =
            self.b
                .ins()
                .icmp_imm_u(IntCC::Equal, byte, i64::from(ZvalType::Reference as u8));
        let word = self.b.ins().load(I64, trusted(), room, at);
        let inner = self.b.ins().iadd_imm_s(word, i64::from(imm(Ref::VALUE_AT)));
        let outer = self.b.ins().iadd_imm_s(room, i64::from(at));
        let zval = self.b.ins().select(reference, inner, outer);
        let byte = self.b.ins().load(I8, trusted(), zval, super::TYPE_AT);
        let byte = self.b.ins().uextend(I32, byte);
        let mask = self.b.ins().iconst(I32, i64::from(mask.get()));
        let shifted = self.b.ins().ushr(mask, byte);
        Ok(self.b.ins().band_imm_u(shifted, 1))
    }

    /// Whether a variable of `frame`, whose room is `room`, owns
    /// something: its type word counted, every variable's folded into one
    /// test, as `Request::leave_plainly` folds them. What a leave over
    /// more variables than it frees one by one ([`Owners::Kernel`]) asks.
    fn owners(&mut self, room: Value, frame: Frame) -> Result<Value, Abort> {
        let cvs = (self.ir.frames.get(usize::from(frame.0)))
            .ok_or_else(|| mismatch("a leave in a frame the trace does not hold"))?
            .cvs;
        let mut words = self.b.ins().iconst(I32, 0);
        for var in 0..cvs {
            let word = self
                .b
                .ins()
                .load(I32, trusted(), room, var_at(var)? + super::TYPE_AT);
            words = self.b.ins().bor(words, word);
        }
        let counted = self.b.ins().band_imm_u(words, COUNTED);
        Ok(self.b.ins().icmp_imm_s(IntCC::NotEqual, counted, 0))
    }

    /// The place of the frame `line`'s `RETURN` leaves to, read off its
    /// header's caller before it leaves, for the `Op::Back` after it.
    pub(super) fn returns_to(&mut self, line: &Line) -> Result<Option<Value>, Abort> {
        if self.stack.is_none() {
            return Ok(None);
        }
        let room = self.room(line.frame)?;
        let caller = self.b.ins().load(I32, trusted(), room, at(L.caller()));
        let caller = self.b.ins().uextend(I64, caller);
        let place = self
            .b
            .ins()
            .iadd_imm_s(caller, -i64::from(L.numbered_from()));
        let var = self.b.declare_var(I64);
        self.b.def_var(var, place);
        self.returned_to = Some(var);
        Ok(Some(caller))
    }

    /// A function's `RETURN` that leaves its frame in place, as
    /// `zend_jit_leave_func` leaves it and `Request::leave_plainly` leaves
    /// a frame that owns nothing: the value into the caller's slot
    /// where the call reads it, the frame's record off the table, the word
    /// moved back to its link and the top back to its room, and the caller
    /// on past its call. `types` holds its value a scalar, a constant no
    /// reference counts or a temporary ([`Imm::Leave`], [`Returned`]);
    /// the frame's header and the caller's slot are read here, and where
    /// the leave kernel would serve the frame, the opline is stepped.
    /// Once nothing refuses it, the variables `types` does not hold
    /// uncounted are given up in place ([`Owners`], `Emit::free_cvs`)
    /// before the value is written and the frame popped, as php's trace
    /// frees them before `zend_jit_leave_func`; the order is the
    /// interpreter's owning leave's, the variables before the receiver
    /// and the closure, of which a frame left in place holds neither.
    ///
    /// A variable that owns something is given up as its frame leaves, its
    /// destructor called at the return, before the caller goes on:
    ///
    /// ```php
    /// class D { function __destruct() { echo "freed\n"; } }
    /// function f($i) { $d = $i == 30 ? new D : "s$i"; return $i; }
    /// $s = 0;
    /// for ($i = 0; $i < 40; $i++) { $s += f($i); }
    /// echo $s, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// freed
    /// 780
    /// ```
    ///
    /// and what that destructor throws unwinds from the return:
    ///
    /// ```php
    /// class T { function __destruct() { throw new Exception("at return"); } }
    /// function g($i) { $t = $i == 30 ? new T : null; return $i; }
    /// try {
    ///     for ($i = 0; $i < 40; $i++) { g($i); }
    /// } catch (Exception $e) { echo $e->getMessage(), " ", $i, "\n"; }
    /// ```
    /// ```expect regime=jit-force process
    /// at return 30
    /// ```
    ///
    /// A trace that stops at a `RETURN` leaves its frame in place and goes
    /// on in the caller ([`End::Return`]); the `RETURN` of a frame a native
    /// called ends the serving through the leave kernel ([`Emit::leave_top`]):
    ///
    /// ```php
    /// function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }
    /// function sq($x) { return $x * $x; }
    /// echo fib(20), " ", array_sum(array_map('sq', range(1, 100))), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 6765 338350
    /// ```
    pub(super) fn leave_in_place(&mut self, id: OpId, line: &Line) -> Result<(), Abort> {
        let Imm::Leave {
            owners, receives, ..
        } = line.imm
        else {
            return Err(mismatch("a leave in place with no value named"));
        };
        let returned = self.returned(id, line)?;
        let stack = self.stack()?;
        let caller = self
            .returns_to(line)?
            .ok_or_else(|| mismatch("a leave in place with no caller read"))?;
        let room = self.room(line.frame)?;
        let (slow, join) = self.split();
        let refused = self.leave_refused(room, caller, line)?;
        self.divert_if(refused, slow);
        // The frame is the table's last record, on the top page.
        let len = self.b.ins().load(I64, trusted(), stack, FRAMES_LEN);
        let frames = self.b.ins().load(I64, trusted(), stack, FRAMES);
        let last = self.b.ins().iadd_imm_s(len, -1);
        let refused = self.not_on_top(frames, last, room);
        self.divert_if(refused, slow);
        // The caller's place, and the slot of its that takes the return,
        // which holds nothing counted.
        let place = self
            .b
            .ins()
            .iadd_imm_s(caller, -i64::from(L.numbered_from()));
        let callers = self.record_word(frames, place, L.record_at());
        let named = self.b.ins().load(I32, trusted(), room, at(L.returns_to()));
        let returns = self.b.ins().uextend(I64, named);
        let target = self.taken_at(callers, returns);
        let reads = self.b.ins().icmp_imm_s(IntCC::NotEqual, returns, 0);
        let kind = self.b.ins().load(I32, trusted(), target, super::TYPE_AT);
        let counted = self.b.ins().band_imm_u(kind, COUNTED);
        let counted = self.b.ins().icmp_imm_s(IntCC::NotEqual, counted, 0);
        let counted = self.b.ins().band(reads, counted);
        self.divert_if(counted, slow);
        if let Out::Moved(_, info) = returned {
            // What the caller does not read is given up, which the leave
            // kernel does.
            let owned = self.b.ins().band_imm_u(info, COUNTED);
            let owned = self.b.ins().icmp_imm_s(IntCC::NotEqual, owned, 0);
            let unread = self.b.ins().icmp_imm_s(IntCC::Equal, returns, 0);
            let dropped = self.b.ins().band(owned, unread);
            self.divert_if(dropped, slow);
        }
        // Nothing refuses the leave from here: what the variables own is
        // given up, and a destructor the kernel served may have grown the
        // frame table, whose records are read again after each kernel
        // call alone ([`Reread`]), so a leave that calls none reads them
        // once.
        let read = Reread::new(self, (stack, place, returns), [frames, callers, target]);
        let freed = match owners {
            Owners::Each(vars) => Some(self.free_cvs(room, line, vars, &read)?),
            Owners::Uncounted | Owners::Kernel => None,
        };
        let let_go = if receives {
            Some(self.free_this(room, line, &read)?)
        } else {
            None
        };
        let threw = match (freed, let_go) {
            (Some(freed), Some(let_go)) => Some(self.b.ins().bor(freed, let_go)),
            (threw, None) | (None, threw) => threw,
        };
        let [frames, callers, target] = read.vars.map(|var| self.b.use_var(var));
        self.when(reads, |emit| {
            match returned {
                Out::Scalar(scalar) => emit.write(target, 0, scalar),
                Out::Copied(word, info) | Out::Moved(word, info) => {
                    emit.b.ins().store(trusted(), word, target, 0);
                    emit.b.ins().store(trusted(), info, target, super::TYPE_AT);
                }
            }
            Ok(())
        })?;
        // The record off the table, the word and the top back.
        self.b.ins().store(trusted(), last, stack, FRAMES_LEN);
        let below = self.b.ins().load(I64, trusted(), room, at(L.link()));
        self.b.ins().store(trusted(), below, stack, INNERMOST);
        self.b.ins().store(trusted(), room, stack, TOP);
        // The caller on past its call, `LOAD_NEXT_OPLINE()`; or, where a
        // destructor threw, at its call, where the throwable is raised,
        // `zend_rethrow_exception` with the return written, which
        // `HANDLE_EXCEPTION` frees as the call's result.
        let back = self.b.ins().iadd_imm_s(callers, -i64::from(L.room()));
        let at_call = self.b.ins().load(I32, trusted(), back, at(L.opline()));
        let mut past = self.b.ins().iadd_imm_s(at_call, 1);
        if let Some(threw) = threw {
            past = self.b.ins().select(threw, at_call, past);
        }
        self.b.ins().store(trusted(), past, back, at(L.opline()));
        // Where the interpreter stands: in the caller, the call under way
        // in it the record now on top where that is not the caller's own.
        let under = self.b.ins().iadd_imm_s(len, -2);
        let first = self.record_word(frames, under, L.record_at());
        let built = self.b.ins().iadd_imm_s(first, -i64::from(L.room()));
        let own = self.b.ins().icmp(IntCC::Equal, under, place);
        let nothing = self.b.ins().iconst(I64, 0);
        let call = self.b.ins().select(own, nothing, built);
        let func = self.b.ins().load(I32, trusted(), back, at(L.cache()));
        let left = self.left_past(stack, room);
        self.stand([back, call, func, past, left]);
        if let Some(threw) = threw {
            // php's `jit_stub_leave_throw` after `zend_jit_leave_func`: the
            // caller unwinds from its call.
            let unwind = line
                .unwind
                .ok_or_else(|| mismatch("a leave that frees with no unwind"))?;
            let to = self.unwind_block(unwind)?;
            self.b.ins().brif(threw, to, &[], join, &[]);
        } else {
            self.b.ins().jump(join, &[]);
        }
        self.left_or_not(id, line, (caller, returned), (slow, join))
    }

    /// The slot of the caller's slots at `callers` that a call returning
    /// into `returns`, numbered past [`FrameLayout::numbered_from`], takes
    /// the value in.
    fn taken_at(&mut self, callers: Value, returns: Value) -> Value {
        let taken = self
            .b
            .ins()
            .iadd_imm_s(returns, -i64::from(L.numbered_from()));
        let offset = self.b.ins().imul_imm_s(taken, i64::from(SLOT));
        self.b.ins().iadd(callers, offset)
    }

    /// php's `zend_jit_free_cv` over each of `vars`, the variables of the
    /// frame in `room` that `line`'s `RETURN` gives up, in slot order as
    /// `i_free_compiled_variables` gives them up, each owner given up as
    /// `jit_ZVAL_PTR_DTOR` gives it up
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:1819-1903`): a type test, then,
    /// where another owner stands and the collector holds the value already
    /// or never takes it (`GC_MAY_LEAK`'s bits, [`Zval::LEAK_BITS`]), a
    /// `GC_DELREF` inline; the last owner, a value the collector may take
    /// and a reference, whose value the collector looks through to, are
    /// [`Kernel::FreeCv`]'s, which takes the frame off the chain first, as
    /// `zend_jit_leave_frame` does before the first free. Each slot is left
    /// `UNDEF` before its owner goes, so the frame owns only what is still
    /// to be freed whatever a destructor does. Whether a destructor threw,
    /// an `i32`: the frees go on past a throw, as php's do, and a fatal
    /// or a panic unwinds.
    ///
    /// A destructor served at the leave sees the caller as the frame that
    /// runs, the leaving frame off the chain:
    ///
    /// ```php
    /// class D {
    ///     function __construct(public $n) {}
    ///     function __destruct() {
    ///         if ($this->n % 10 == 7) {
    ///             echo __LINE__, " ", $this->n;
    ///             foreach (debug_backtrace() as $f) { echo " ", $f['function'], "@", $f['line'] ?? '-'; }
    ///             echo "\n";
    ///         }
    ///     }
    /// }
    /// function f($i) { $d = new D($i); $s = "s$i"; return $i; }
    /// $t = 0;
    /// for ($i = 0; $i < 30; $i++) { $t += f($i); }
    /// echo $t, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 6 7 __destruct@14
    /// 6 17 __destruct@14
    /// 6 27 __destruct@14
    /// 435
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D {
    ///     function __construct(public $n) {}
    ///     function __destruct() {
    ///         if ($this->n % 10 == 7) {
    ///             echo __LINE__, " ", $this->n;
    ///             foreach (debug_backtrace() as $f) { echo " ", $f['function'], "@", $f['line'] ?? '-'; }
    ///             echo "\n";
    ///         }
    ///     }
    /// }
    /// function f($i) { $d = new D($i); $s = "s$i"; return $i; }
    /// $t = 0;
    /// for ($i = 0; $i < 30; $i++) { $t += f($i); }
    /// echo $t, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 6 7 __destruct@14
    /// 6 17 __destruct@14
    /// 6 27 __destruct@14
    /// 435
    /// ```
    ///
    /// A destructor that throws: the variable after it is freed all the
    /// same, the array returned is freed once, and the caller unwinds from
    /// its call:
    ///
    /// ```php
    /// class T { function __construct(public $n) {} function __destruct() { if ($this->n == 25) { throw new Exception("at leave {$this->n}"); } } }
    /// class U { function __destruct() { echo "u"; } }
    /// function g($i) { $t = new T($i); $u = new U; return [$i]; }
    /// $s = 0;
    /// for ($i = 0; $i < 30; $i++) {
    ///     try { $r = g($i); $s += $r[0]; } catch (Exception $e) { echo $e->getMessage(), " ", $i, "\n"; }
    /// }
    /// echo "\n", $s, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// uuuuuuuuuuuuuuuuuuuuuuuuuuat leave 25 25
    /// uuuu
    /// 410
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class T { function __construct(public $n) {} function __destruct() { if ($this->n == 25) { throw new Exception("at leave {$this->n}"); } } }
    /// class U { function __destruct() { echo "u"; } }
    /// function g($i) { $t = new T($i); $u = new U; return [$i]; }
    /// $s = 0;
    /// for ($i = 0; $i < 30; $i++) {
    ///     try { $r = g($i); $s += $r[0]; } catch (Exception $e) { echo $e->getMessage(), " ", $i, "\n"; }
    /// }
    /// echo "\n", $s, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// uuuuuuuuuuuuuuuuuuuuuuuuuuat leave 25 25
    /// uuuu
    /// 410
    /// ```
    ///
    /// Cycles left as possible roots, an array through a reference to
    /// itself and an object that holds itself, each counted by the
    /// collection after as php counts them:
    ///
    /// ```php
    /// function f($i) { $a = [$i]; $a[] = &$a; $b = [$i]; $o = new stdClass; $o->self = $o; return $i; }
    /// function g($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += f($i); } return $s; }
    /// gc_collect_cycles();
    /// echo g(50), " ";
    /// $s = gc_status();
    /// echo $s['roots'], " ", gc_collect_cycles(), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1225 100 100
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($i) { $a = [$i]; $a[] = &$a; $b = [$i]; $o = new stdClass; $o->self = $o; return $i; }
    /// function g($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += f($i); } return $s; }
    /// gc_collect_cycles();
    /// echo g(50), " ";
    /// $s = gc_status();
    /// echo $s['roots'], " ", gc_collect_cycles(), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 1225 100 100
    /// ```
    fn free_cvs(
        &mut self,
        room: Value,
        line: &Line,
        vars: Vars,
        read: &Reread,
    ) -> Result<Value, Abort> {
        let unwind = line
            .unwind
            .ok_or_else(|| mismatch("a leave that frees with no unwind"))?;
        let threw = self.b.declare_var(I32);
        let none = self.b.ins().iconst(I32, 0);
        self.b.def_var(threw, none);
        let [_, opline] = self.served(line)?;
        for var in vars.iter() {
            let slot = var_at(var)?;
            let (owned, kernel, next) = (
                self.b.create_block(),
                self.b.create_block(),
                self.b.create_block(),
            );
            self.b.set_cold_block(kernel);
            let info = self
                .b
                .ins()
                .load(I32, trusted(), room, slot + super::TYPE_AT);
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            self.b.ins().brif(counted, owned, &[], next, &[]);
            self.b.switch_to_block(owned);
            self.b.seal_block(owned);
            let undef = self.b.ins().iconst(I32, super::tag(ZvalType::Undef));
            self.b
                .ins()
                .store(trusted(), undef, room, slot + super::TYPE_AT);
            let word = self.b.ins().load(I64, trusted(), room, slot);
            let count = self.b.ins().load(I32, trusted(), word, COUNT_AT);
            let called = self.kernel_releases(word, info, count, Dtor::Gc);
            let inline = self.b.create_block();
            self.b.ins().brif(called, kernel, &[], inline, &[]);
            self.b.switch_to_block(inline);
            self.b.seal_block(inline);
            let fewer = self.b.ins().iadd_imm_s(count, -1);
            self.b.ins().store(trusted(), fewer, word, COUNT_AT);
            self.b.ins().jump(next, &[]);
            self.b.switch_to_block(kernel);
            self.b.seal_block(kernel);
            let status = self.call_status(Kernel::FreeCv, &[room, opline, word, info])?;
            let unwinds =
                self.b
                    .ins()
                    .icmp_imm_u(IntCC::UnsignedGreaterThan, status, i64::from(THREW));
            let to = self.unwind_block(unwind)?;
            let on = self.b.create_block();
            self.b.ins().brif(unwinds, to, &[], on, &[]);
            self.b.switch_to_block(on);
            self.b.seal_block(on);
            let thrown = self
                .b
                .ins()
                .icmp_imm_u(IntCC::Equal, status, i64::from(THREW));
            let thrown = self.b.ins().uextend(I32, thrown);
            let before = self.b.use_var(threw);
            let now = self.b.ins().bor(before, thrown);
            self.b.def_var(threw, now);
            read.again(self);
            self.b.ins().jump(next, &[]);
            self.b.switch_to_block(next);
            self.b.seal_block(next);
        }
        Ok(self.b.use_var(threw))
    }

    /// The receiver of the frame in `room` given up after its variables,
    /// as `zend_jit_leave_func` gives it up
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11135-11160`): where the frame
    /// owns it, `ZEND_CALL_RELEASE_THIS`, one owner fewer in place, and
    /// [`Kernel::FreeThis`] where that is the last or the collector may
    /// take the object as a possible root, `OBJ_RELEASE`; where it was
    /// lent the receiver, the pin let go in place, and the kernel's where
    /// that pin is the last over an object whose last owner went under
    /// it, whose bytes the heap then frees. Whether a destructor the
    /// kernel served threw, an `i32`. A temporary's object is the last
    /// owner its method's frame gives up, its destructor served there,
    /// one that throws thrown from the call; `$this->one()` lets its pin
    /// go:
    ///
    /// ```php
    /// class D { public $n; function __construct($n) { $this->n = $n; } function __destruct() { echo "d", $this->n, " "; } function get() { return $this->n + $this->one(); } function one() { return 1; } }
    /// class E { function __destruct() { throw new Exception("e"); } function m() { return 3; } }
    /// function f($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += (new D($i))->get(); } return $t; }
    /// function g($n) { $t = 0; for ($i = 0; $i < $n; $i++) { try { $t += (new E)->m(); } catch (Exception $e) { $t += 100; } } return $t; }
    /// echo f(4), "| ", g(5), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// d0 d1 d2 d3 10| 500
    /// ```
    ///
    /// ```php
    /// class D { public $n; function __construct($n) { $this->n = $n; } function __destruct() { echo "d", $this->n, " "; } function get() { return $this->n + $this->one(); } function one() { return 1; } }
    /// class E { function __destruct() { throw new Exception("e"); } function m() { return 3; } }
    /// function f($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += (new D($i))->get(); } return $t; }
    /// function g($n) { $t = 0; for ($i = 0; $i < $n; $i++) { try { $t += (new E)->m(); } catch (Exception $e) { $t += 100; } } return $t; }
    /// echo f(4), "| ", g(5), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// d0 d1 d2 d3 10| 500
    /// ```
    fn free_this(&mut self, room: Value, line: &Line, read: &Reread) -> Result<Value, Abort> {
        let unwind = line
            .unwind
            .ok_or_else(|| mismatch("a leave that frees with no unwind"))?;
        let threw = self.b.declare_var(I32);
        let none = self.b.ins().iconst(I32, 0);
        self.b.def_var(threw, none);
        let [_, opline] = self.served(line)?;
        // The inline arms leave `EX(This)` and `RELEASE_THIS` as they
        // stand, where the kernel clears them: the leave pops the frame
        // before anything past it, an unwind among them, reads its header.
        let [held, owned, lent, fewer, kernel, next] = [(); 6].map(|()| self.b.create_block());
        self.b.set_cold_block(kernel);
        let this = self.b.ins().load(I64, trusted(), room, at(L.this()));
        self.b.ins().brif(this, held, &[], next, &[]);
        self.b.switch_to_block(held);
        self.b.seal_block(held);
        let info = self.b.ins().load(I32, trusted(), room, at(L.info()));
        let release = self.b.ins().band_imm_u(info, i64::from(L.release_this()));
        self.b.ins().brif(release, owned, &[], lent, &[]);
        self.b.switch_to_block(owned);
        self.b.seal_block(owned);
        let count = self.b.ins().load(I32, trusted(), this, COUNT_AT);
        let object = self.b.ins().iconst(I32, super::tag(ZvalType::Object));
        let called = self.kernel_releases(this, object, count, Dtor::Gc);
        let one_fewer = self.b.ins().iadd_imm_s(count, -1);
        self.b.ins().brif(
            called,
            kernel,
            &[],
            fewer,
            &[BlockArg::Value(one_fewer), BlockArg::Value(this)],
        );
        self.b.switch_to_block(lent);
        self.b.seal_block(lent);
        let word = self.b.ins().load(I32, trusted(), this, PINS_AT);
        let pins = self.b.ins().band_imm_u(word, i64::from(Obj::PINS));
        let count = self.b.ins().load(I32, trusted(), this, COUNT_AT);
        let owners = self.b.ins().band_imm_u(count, i64::from(Zval::COUNT_BITS));
        let gone = self.b.ins().icmp_imm_u(IntCC::Equal, owners, 0);
        let last = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, pins, 1);
        let last = self.b.ins().band(gone, last);
        let unpinned = self.b.ins().icmp_imm_u(IntCC::Equal, pins, 0);
        let called = self.b.ins().bor(last, unpinned);
        let one_fewer = self.b.ins().iadd_imm_s(word, -1);
        let pinned = self.b.ins().iadd_imm_s(this, i64::from(PINS_AT - COUNT_AT));
        self.b.ins().brif(
            called,
            kernel,
            &[],
            fewer,
            &[BlockArg::Value(one_fewer), BlockArg::Value(pinned)],
        );
        // One owner or one pin fewer, the word at its own place.
        self.b.append_block_param(fewer, I32);
        self.b.append_block_param(fewer, I64);
        self.b.switch_to_block(fewer);
        self.b.seal_block(fewer);
        let [remaining, place] = [0, 1].map(|n| self.b.block_params(fewer)[n]);
        self.b.ins().store(trusted(), remaining, place, COUNT_AT);
        self.b.ins().jump(next, &[]);
        self.b.switch_to_block(kernel);
        self.b.seal_block(kernel);
        let status = self.call_status(Kernel::FreeThis, &[room, opline])?;
        let unwinds = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedGreaterThan, status, i64::from(THREW));
        let to = self.unwind_block(unwind)?;
        let on = self.b.create_block();
        self.b.ins().brif(unwinds, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        let thrown = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, status, i64::from(THREW));
        let thrown = self.b.ins().uextend(I32, thrown);
        self.b.def_var(threw, thrown);
        read.again(self);
        self.b.ins().jump(next, &[]);
        self.b.switch_to_block(next);
        self.b.seal_block(next);
        Ok(self.b.use_var(threw))
    }

    /// What a `RETURN` left in place hands its caller ([`Returned`]): a
    /// scalar, `null` for none; a constant's two words, read from the op
    /// array the trace's epoch holds; or a temporary's, moved. A tree's
    /// leaves returned as a constant and its nodes as the arrays built,
    /// read back, and returns no caller reads, given up:
    ///
    /// ```php
    /// function leaf($d) { if ($d === 0) { return [null, null]; } $e = $d - 1; return [leaf($e), leaf($e)]; }
    /// function tally($t) { return $t[0] === null ? 1 : tally($t[0]) + tally($t[1]); }
    /// function drop($n) { for ($i = 0; $i < $n; $i++) { leaf(2); } return $i; }
    /// echo tally(leaf(10)), " ", drop(50), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1024 50
    /// ```
    fn returned(&mut self, id: OpId, line: &Line) -> Result<Out, Abort> {
        let Imm::Leave { value, .. } = line.imm else {
            return Err(mismatch("a leave in place with no value named"));
        };
        let (zval, at, moved) = match (value, line.op1) {
            (Returned::Scalar, Operand::Unused) => return Ok(Out::Scalar(Scalar::Null)),
            (Returned::Scalar, operand) => return Ok(Out::Scalar(self.scalar(id, operand)?)),
            (Returned::Constant(address), _) => {
                let address = i64::try_from(address)
                    .map_err(|_| mismatch("a constant past a 64-bit address"))?;
                (self.b.ins().iconst(I64, address), 0, false)
            }
            (Returned::Moved, Operand::Value(value)) => {
                let (room, at) = self.place(self.value(value)?.slot)?;
                (room, at, true)
            }
            (Returned::Moved, _) => return Err(mismatch("a temporary moved from no slot")),
        };
        let word = self.b.ins().load(I64, trusted(), zval, at);
        let info = self.b.ins().load(I32, trusted(), zval, at + super::TYPE_AT);
        Ok(if moved {
            Out::Moved(word, info)
        } else {
            Out::Copied(word, info)
        })
    }

    /// What the header of the frame in `room`, which leaves to `caller`,
    /// says the leave kernel serves: a receiver of a frame the trace takes
    /// to have none ([`Imm::Leave`]'s `receives`), a closure, an annex, a
    /// call not made a frame or arguments past the parameters, a caller
    /// that is none or the loop's, and, where the frame has more variables
    /// than the leave frees one by one ([`Owners::Kernel`]), a variable
    /// that owns something, every variable's type word tested at once.
    /// The interpreter's owning leave gives up the receiver or the
    /// closure after the variables; a frame that holds a closure is the
    /// leave kernel's, and a receiver is the last of what a leave in
    /// place gives up ([`Emit::free_this`]).
    fn leave_refused(&mut self, room: Value, caller: Value, line: &Line) -> Result<Value, Abort> {
        let receives = matches!(line.imm, Imm::Leave { receives: true, .. });
        let this = (!receives).then(|| self.b.ins().load(I64, trusted(), room, at(L.this())));
        let closure = self.b.ins().load(I64, trusted(), room, at(L.closure()));
        let held = match this {
            Some(this) => self.b.ins().bor(this, closure),
            None => closure,
        };
        let mut refused = self.b.ins().icmp_imm_s(IntCC::NotEqual, held, 0);
        let annex = self.b.ins().load(I32, trusted(), room, at(L.annex()));
        let info = self.b.ins().load(I32, trusted(), room, at(L.info()));
        let flagged = self
            .b
            .ins()
            .band_imm_u(info, i64::from(L.leaves_by_kernel()));
        let owns = self.b.ins().bor(annex, flagged);
        let owns = self.b.ins().icmp_imm_s(IntCC::NotEqual, owns, 0);
        refused = self.b.ins().bor(refused, owns);
        let outside = self.outside(caller);
        refused = self.b.ins().bor(refused, outside);
        if let Imm::Leave {
            owners: Owners::Kernel,
            ..
        } = line.imm
        {
            let owners = self.owners(room, line.frame)?;
            refused = self.b.ins().bor(refused, owners);
        }
        Ok(refused)
    }

    /// Whether the frame whose header names `caller` was served from outside
    /// the loop: none, or the loop's (`exec::Caller::Outside`, `Loop`).
    fn outside(&mut self, caller: Value) -> Value {
        let none = self.b.ins().icmp_imm_s(IntCC::Equal, caller, 0);
        let looped = self.b.ins().band_imm_u(caller, i64::from(L.caller_loop()));
        let looped = self.b.ins().icmp_imm_s(IntCC::NotEqual, looped, 0);
        self.b.ins().bor(none, looped)
    }

    /// Whether the record `last` of the frame table at `frames` is not the
    /// frame in `room`, or not on the top page.
    fn not_on_top(&mut self, frames: Value, last: Value, room: Value) -> Value {
        let offset = self.b.ins().imul_imm_s(last, i64::from(L.record()));
        let record = self.b.ins().iadd(frames, offset);
        let first = self.b.ins().load(I64, trusted(), record, at(L.record_at()));
        let own = self.b.ins().iadd_imm_s(room, i64::from(L.room()));
        let elsewhere = self.b.ins().icmp(IntCC::NotEqual, first, own);
        let tag = self
            .b
            .ins()
            .load(I8, trusted(), record, at(L.record_placed()));
        let off_page = self
            .b
            .ins()
            .icmp_imm_s(IntCC::NotEqual, tag, i64::from(L.on_page()));
        self.b.ins().bor(elsewhere, off_page)
    }

    /// The room of the call under way in `frame` as the frame table has
    /// it, as the interpreter's `EX(call)` reads after a call it made is
    /// popped: the record on top where it is not `frame`'s own, null where
    /// it is.
    pub(super) fn call_left(&mut self, frame: Frame) -> Result<Value, Abort> {
        let stack = self.stack()?;
        let len = self.b.ins().load(I64, trusted(), stack, FRAMES_LEN);
        let frames = self.b.ins().load(I64, trusted(), stack, FRAMES);
        let last = self.b.ins().iadd_imm_s(len, -1);
        let first = self.record_word(frames, last, L.record_at());
        let top = self.b.ins().iadd_imm_s(first, -i64::from(L.room()));
        let room = self.room(frame)?;
        let own = self.b.ins().icmp(IntCC::Equal, top, room);
        let nothing = self.b.ins().iconst(I64, 0);
        Ok(self.b.ins().select(own, nothing, top))
    }

    /// The word at `field` of record `place` of the frame table at
    /// `frames`.
    fn record_word(&mut self, frames: Value, place: Value, field: u32) -> Value {
        let offset = self.b.ins().imul_imm_s(place, i64::from(L.record()));
        let record = self.b.ins().iadd(frames, offset);
        self.b.ins().load(I64, trusted(), record, at(field))
    }

    /// A leave's way out where it was not made in place, from `slow`, and
    /// on from `join`: the leave kernel, or, for the `RETURN` a trace stops
    /// at, its end exit, through which the interpreter serves it; and past
    /// that `RETURN` left in place, the interpreter goes on in the caller,
    /// php's `zend_jit_trace_return`, with no exit taken.
    fn left_or_not(
        &mut self,
        id: OpId,
        line: &Line,
        (caller, returned): (Value, Out),
        (slow, join): (Block, Block),
    ) -> Result<(), Abort> {
        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let at_end = self.returns_at_end(id);
        if let Some(exit) = at_end {
            self.leave_top(line, (caller, returned), exit)?;
        } else {
            self.step_kernel(id, line)?;
            self.b.ins().jump(join, &[]);
        }
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        if at_end.is_some() {
            let escape = self.escape_block();
            let place = self
                .returned_to
                .ok_or_else(|| mismatch("a leave in place with no caller read"))?;
            let place = self.b.use_var(place);
            self.b.ins().jump(escape, &[BlockArg::Value(place)]);
            Ok(())
        } else {
            self.after_step(id, line)
        }
    }

    /// The `RETURN` a trace ends at, of a frame served from outside the
    /// loop, `zend_call_function`'s (`caller` none, or the loop's: a
    /// native's call, or a magic method a handler reached), served
    /// through [`Kernel::Leave`] where the frame is
    /// the serving's entry, and [`RETURNED`] answered, as php's
    /// `zend_jit_leave_func` over a frame that may be the top returns
    /// `ZEND_VM_RETURN` to `execute_ex` for a `ZEND_CALL_TOP` frame
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11160`), so the native that called
    /// it, `__get`'s reader or `array_map`, goes on with no exit taken;
    /// `exit`, the trace's end, for any other such frame. A `__get` that
    /// writes the object it is called on, which the loop reads after, as
    /// §4.2's clobber rule has it, and throws, one that reads another
    /// property through `__get` again, one a native calls, and one that
    /// frees the object it is called on:
    ///
    /// ```php
    /// class G { private $v = 3; public $r = 0; function __get($n) { if ($n === 'boom' && $this->r++ > 5) { throw new Exception("at $this->r"); } return $n === 'deep' ? $this->again + 1 : $this->v; } }
    /// class F { function __get($n) { $GLOBALS['o'] = null; return 7; } }
    /// function k($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->x + $o->deep; try { $t += $o->boom; } catch (Exception $e) { $t += 100; } $t += $o->r; } return $t; }
    /// $o = new F; $f = 0; for ($i = 0; $i < 20; $i++) { $f += $o->x; $o = new F; }
    /// echo k(new G, 20), " ", $f, " ", array_sum(array_map(fn ($o) => $o->x, [new G, new G])), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1768 140 6
    /// ```
    ///
    /// ```php
    /// class G { private $v = 3; public $r = 0; function __get($n) { if ($n === 'boom' && $this->r++ > 5) { throw new Exception("at $this->r"); } return $n === 'deep' ? $this->again + 1 : $this->v; } }
    /// class F { function __get($n) { $GLOBALS['o'] = null; return 7; } }
    /// function k($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->x + $o->deep; try { $t += $o->boom; } catch (Exception $e) { $t += 100; } $t += $o->r; } return $t; }
    /// $o = new F; $f = 0; for ($i = 0; $i < 20; $i++) { $f += $o->x; $o = new F; }
    /// echo k(new G, 20), " ", $f, " ", array_sum(array_map(fn ($o) => $o->x, [new G, new G])), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 1768 140 6
    /// ```
    fn leave_top(
        &mut self,
        line: &Line,
        (caller, returned): (Value, Out),
        exit: ExitId,
    ) -> Result<(), Abort> {
        // The serving's entry is the path taken: a `__get`'s or a native's
        // callback's trace returns there every time it is entered.
        let top = self.b.create_block();
        let block = self.exit_block(exit)?;
        let outside = self.outside(caller);
        self.b.ins().brif(outside, top, &[], block, &[]);
        self.b.switch_to_block(top);
        self.b.seal_block(top);
        let slot = self.answer();
        let (word, info) = match returned {
            Out::Scalar(scalar) => {
                // The scalar's two words, written where the answer is read.
                let at = self.b.ins().stack_addr(I64, slot, 0);
                self.write(at, 0, scalar);
                let word = self.b.ins().stack_load(I64, I64, slot, 0);
                let info = self.b.ins().stack_load(I64, I32, slot, super::TYPE_AT);
                (word, info)
            }
            Out::Copied(word, info) | Out::Moved(word, info) => (word, info),
        };
        let [room, opline] = self.served(line)?;
        let left = self.b.ins().stack_addr(I64, slot, 0);
        self.call(
            Kernel::Leave,
            &[room, opline, word, info, left],
            leave_of(line),
        )?;
        let left = self.b.ins().stack_load(I64, I64, slot, 0);
        // A frame served from outside that is not the serving's entry is
        // the rare one, and its block the cold one.
        let (other, returned_block) = self.split();
        let not = self.b.ins().icmp_imm_s(IntCC::Equal, left, 0);
        self.b.ins().brif(not, other, &[], returned_block, &[]);
        self.b.switch_to_block(returned_block);
        self.b.seal_block(returned_block);
        // The temporary handed its value over, as the handler takes it.
        if let (Out::Moved(..), Operand::Value(value)) = (returned, line.op1) {
            let (room, at) = self.place(self.value(value)?.slot)?;
            let undef = self.b.ins().iconst(I32, super::tag(ZvalType::Undef));
            self.b
                .ins()
                .store(trusted(), undef, room, at + super::TYPE_AT);
        }
        let answer = self.b.ins().iconst(I32, i64::from(RETURNED));
        self.b.ins().return_(&[answer]);
        self.b.switch_to_block(other);
        self.b.seal_block(other);
        self.b.ins().jump(block, &[]);
        Ok(())
    }

    /// The exit the trace takes where the `RETURN` at `id`, its last op,
    /// is not left in place ([`End::Return`]); none for any other.
    fn returns_at_end(&self, id: OpId) -> Option<ExitId> {
        match self.ir.end {
            End::Return(exit) if id.index() + 1 == self.ir.ops.len() => Some(exit),
            End::Return(_) | End::Leave(_) | End::Loop(_) => None,
        }
    }
}

/// What a native's call made from compiled code writes its return to:
/// the temporary the `DO_ICALL` or the frameless call names, as php's
/// code hands the native `EX_VAR(opline->result.var)`; or, where the
/// opline's `result` is `UNUSED`, the stack's own zval, php's
/// `alloca`'d one, which compiled code gives up after the call.
#[derive(Clone, Copy)]
enum Returns {
    Slot(Value),
    Unread(Value),
}

impl Returns {
    /// The address the native writes what it returns to.
    fn address(self) -> Value {
        let (Returns::Slot(out) | Returns::Unread(out)) = self;
        out
    }
}

impl Emit<'_> {
    /// Where `line`'s native writes what it returns ([`Returns`]).
    fn returns_of(&mut self, line: &Line) -> Result<Returns, Abort> {
        Ok(if let Some(out) = line.out {
            let (room, at) = self.place(self.value(out)?.slot)?;
            Returns::Slot(self.b.ins().iadd_imm_s(room, i64::from(at)))
        } else {
            let answer = self.answer();
            Returns::Unread(self.b.ins().stack_addr(I64, answer, 0))
        })
    }

    /// What a native returned that nothing reads given up, as php's
    /// `zend_jit_do_fcall` gives up its `alloca`'d zval once the call is
    /// popped, a destructor's throw leaving through the opline's unwind.
    fn release_unread(&mut self, line: &Line, returns: Returns) -> Result<(), Abort> {
        if let Returns::Unread(_) = returns {
            let answer = self.answer();
            let word = self.b.ins().stack_load(I64, I64, answer, 0);
            let info = self.b.ins().stack_load(I64, I32, answer, TYPE_AT);
            self.release(line, word, info, Dtor::Gc)?;
        }
        Ok(())
    }

    /// A native's `DO_ICALL`, made as php's `zend_jit_do_fcall` makes an
    /// native's call (`Z/ext/opcache/jit/zend_jit_ir.c:10522-10720`):
    /// the caller stood at the call, `SET_EX_OPLINE`; the call made the
    /// one being served, `EG(current_execute_data) = call`, its flags
    /// written in place ([`FrameLayout::building`],
    /// [`FrameLayout::return_unused`]); the native served through
    /// [`Kernel::CallNative`] with the call and the address of the slot
    /// that takes its return, its observer told there where one is
    /// declared; then, the call still on top, the interrupt tested, and
    /// checked through [`Kernel::Interrupt`] where it is raised, php's
    /// `zend_fcall_interrupt`; the call popped in place
    /// ([`Emit::pop_native`]); a return nothing reads given up; and last
    /// the throw tested, php's `EG(exception)` guard, which leaves through
    /// the opline's unwind. A native that throws is answered with its
    /// call popped, as the handler pops it, and leaves at once.
    ///
    /// The frame a native's backtrace and its warning read is the
    /// caller's at the call, an interrupt a native raised is checked
    /// before the next opline, with the call on the stack, and a
    /// destructor the return nobody reads runs at the call:
    ///
    /// ```php
    /// pcntl_async_signals(true);
    /// pcntl_signal(SIGUSR1, function () { echo "usr1 ", count(debug_backtrace()), "\n"; });
    /// class D { function __destruct() { echo "d\n"; } }
    /// function f($n) {
    ///     $t = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if ($i == 3) { posix_kill(posix_getpid(), SIGUSR1); echo "after kill\n"; }
    ///         $t += strlen(str_repeat("ab", $i)) + intdiv($i, 1);
    ///         array_reverse([new D]);
    ///     }
    ///     return $t;
    /// }
    /// echo f(5), "\n";
    /// echo @intdiv(1, 1) + array_sum(array_map('intdiv', [6, 8], [2, 2])), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// d
    /// d
    /// d
    /// usr1 3
    /// after kill
    /// d
    /// d
    /// 30
    /// 8
    /// ```
    ///
    /// ```php
    /// pcntl_async_signals(true);
    /// pcntl_signal(SIGUSR1, function () { echo "usr1 ", count(debug_backtrace()), "\n"; });
    /// class D { function __destruct() { echo "d\n"; } }
    /// function f($n) {
    ///     $t = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if ($i == 3) { posix_kill(posix_getpid(), SIGUSR1); echo "after kill\n"; }
    ///         $t += strlen(str_repeat("ab", $i)) + intdiv($i, 1);
    ///         array_reverse([new D]);
    ///     }
    ///     return $t;
    /// }
    /// echo f(5), "\n";
    /// echo @intdiv(1, 1) + array_sum(array_map('intdiv', [6, 8], [2, 2])), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// d
    /// d
    /// d
    /// usr1 3
    /// after kill
    /// d
    /// d
    /// 30
    /// 8
    /// ```
    pub(super) fn native_call(
        &mut self,
        line: &Line,
        [room, opline]: [Value; 2],
    ) -> Result<(), Abort> {
        let (_, plain) = *self
            .calls
            .last()
            .ok_or_else(|| mismatch("a native's call with no call under way"))?;
        let count = plain.map(PlainCall::count);
        let call = self.call_room()?;
        self.b.ins().store(trusted(), opline, room, EX_OPLINE);
        let flags = self.b.ins().load(I32, trusted(), call, at(L.info()));
        let made = self
            .b
            .ins()
            .band_imm_u(flags, i64::from(!(L.building() | L.return_unused())));
        let returns = self.returns_of(line)?;
        let made = match returns {
            Returns::Slot(_) => made,
            Returns::Unread(_) => self.b.ins().bor_imm_u(made, i64::from(L.return_unused())),
        };
        self.b.ins().store(trusted(), made, call, at(L.info()));
        let stack = self.stack()?;
        let len = self.b.ins().load(I64, trusted(), stack, FRAMES_LEN);
        let last = self.b.ins().iadd_imm_s(len, -1);
        let place = self.b.ins().ireduce(I32, last);
        let out = returns.address();
        self.call(
            Kernel::CallNative,
            &[room, opline, place, out],
            leave_of(line),
        )?;
        let threw = self.interrupt_after(line, [room, opline])?;
        self.pop_native(line, (call, count), [room, opline], threw)?;
        self.release_unread(line, returns)?;
        let threw = self.b.use_var(threw);
        let to = self.left_by(leave_of(line))?;
        let on = self.b.create_block();
        self.b.ins().brif(threw, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(())
    }

    /// The interrupt tested after a native returned and, where it is
    /// raised, checked through [`Kernel::Interrupt`], php's
    /// `ZEND_VM_FCALL_INTERRUPT_CHECK` as its code compiles it under a
    /// `zend_interrupt_function`, `zend_fcall_interrupt` on a cold path
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:10583-10590`, `:17842-17850`): a
    /// variable that holds whether a hook left a throwable pending, which
    /// the caller tests once it has done what php's code does before its
    /// throw guard; an exit leaves at once.
    fn interrupt_after(
        &mut self,
        line: &Line,
        [room, opline]: [Value; 2],
    ) -> Result<Variable, Abort> {
        let threw = self.b.declare_var(I8);
        let none = self.b.ins().iconst(I8, 0);
        self.b.def_var(threw, none);
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a native's call before the entry"))?;
        let interrupt = self.b.ins().load(I64, trusted(), cx, INTERRUPT_AT);
        let raised = self.b.ins().load(I8, trusted(), interrupt, 0);
        let (checked, on) = self.split();
        self.b.ins().brif(raised, checked, &[], on, &[]);
        self.b.switch_to_block(checked);
        self.b.seal_block(checked);
        let status = self.call_status(Kernel::Interrupt, &[room, opline])?;
        self.thrown_into(line, status, threw)?;
        self.b.ins().jump(on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(threw)
    }

    /// A kernel's `status` read where compiled code goes on past a throw
    /// to finish what php's code finishes before its throw guard: an
    /// exit or a panic leaves through `line`'s way out at once, and a
    /// throw is or'd into `threw`.
    fn thrown_into(&mut self, line: &Line, status: Value, threw: Variable) -> Result<(), Abort> {
        let unwinds = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedGreaterThan, status, i64::from(THREW));
        let to = self.left_by(leave_of(line))?;
        let on = self.b.create_block();
        self.b.ins().brif(unwinds, to, &[], on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        let thrown = self
            .b
            .ins()
            .icmp_imm_u(IntCC::Equal, status, i64::from(THREW));
        let before = self.b.use_var(threw);
        let now = self.b.ins().bor(before, thrown);
        self.b.def_var(threw, now);
        Ok(())
    }

    /// The native's call at `call` popped once it returned,
    /// `zend_vm_stack_free_args` and `zend_vm_stack_free_call_frame` as
    /// php's `zend_jit_do_fcall` writes them inline (`:10592-10640`):
    /// where the push was compiled code's, of `count` arguments, the
    /// call is the frame table's last record on the top page and carries
    /// no annex (an `INIT_FCALL`'s call carries no receiver and no
    /// closure), and no argument's release can be the last of its value,
    /// each counted argument's owner given up by a `GC_DELREF` inline,
    /// the record off the table and the top back; through
    /// [`Kernel::PopCall`] otherwise, a destructor's throw or'd into
    /// `threw`. An argument of `count` is the last owner only where its
    /// value has no more owners than there are arguments, which may each
    /// hold it, so no release in place frees a value.
    fn pop_native(
        &mut self,
        line: &Line,
        (call, count): (Value, Option<u32>),
        [room, opline]: [Value; 2],
        threw: Variable,
    ) -> Result<(), Abort> {
        let (by_kernel, join) = self.split();
        let Some(count) = count else {
            self.b.ins().jump(by_kernel, &[]);
            return self.popped_by_kernel(line, (by_kernel, join), [room, opline], threw);
        };
        let stack = self.stack()?;
        let len = self.b.ins().load(I64, trusted(), stack, FRAMES_LEN);
        let frames = self.b.ins().load(I64, trusted(), stack, FRAMES);
        let last = self.b.ins().iadd_imm_s(len, -1);
        let refused = self.not_on_top(frames, last, call);
        self.divert_if(refused, by_kernel);
        let annex = self.b.ins().load(I32, trusted(), call, at(L.annex()));
        self.divert_if(annex, by_kernel);
        for arg in 0..count {
            let slot = var_at(arg)?;
            let info = self.b.ins().load(I32, trusted(), call, slot + TYPE_AT);
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            self.when(counted, |emit| {
                let word = emit.b.ins().load(I64, trusted(), call, slot);
                let owners = emit.b.ins().load(I32, trusted(), word, COUNT_AT);
                let owners = emit.b.ins().band_imm_u(owners, i64::from(Zval::COUNT_BITS));
                let freed = emit.b.ins().icmp_imm_u(
                    IntCC::UnsignedLessThanOrEqual,
                    owners,
                    i64::from(count),
                );
                emit.divert_if(freed, by_kernel);
                Ok(())
            })?;
        }
        for arg in 0..count {
            let slot = var_at(arg)?;
            let info = self.b.ins().load(I32, trusted(), call, slot + TYPE_AT);
            let counted = self.b.ins().band_imm_u(info, COUNTED);
            self.when(counted, |emit| {
                let word = emit.b.ins().load(I64, trusted(), call, slot);
                let owners = emit.b.ins().load(I32, trusted(), word, COUNT_AT);
                let fewer = emit.b.ins().iadd_imm_s(owners, -1);
                emit.b.ins().store(trusted(), fewer, word, COUNT_AT);
                Ok(())
            })?;
        }
        self.b.ins().store(trusted(), last, stack, FRAMES_LEN);
        let below = self.b.ins().load(I64, trusted(), call, at(L.link()));
        self.b.ins().store(trusted(), below, stack, INNERMOST);
        self.b.ins().store(trusted(), call, stack, TOP);
        self.b.ins().jump(join, &[]);
        self.popped_by_kernel(line, (by_kernel, join), [room, opline], threw)
    }

    /// The pop's kernel path, `slow`, joined at `join`: [`Kernel::PopCall`],
    /// its throw or'd into `threw`.
    fn popped_by_kernel(
        &mut self,
        line: &Line,
        (slow, join): (Block, Block),
        [room, opline]: [Value; 2],
        threw: Variable,
    ) -> Result<(), Abort> {
        self.b.switch_to_block(slow);
        self.b.seal_block(slow);
        let status = self.call_status(Kernel::PopCall, &[room, opline])?;
        self.thrown_into(line, status, threw)?;
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        Ok(())
    }

    /// A `FRAMELESS_ICALL_*`, made as php's `jit_frameless_icall*` makes
    /// it (`Z/ext/opcache/jit/zend_jit_ir.c:17868-18110`): the frame
    /// stood at the call, `SET_EX_OPLINE`; the operand zvals read where
    /// they stand, a temporary's moved out of its slot, php's `FREE_OP*`
    /// after the call being the native's record's; the native at the
    /// opline's place in the frameless table served over them through
    /// [`Kernel::Frameless`], which reads no opline, its return written to
    /// the temporary the opline names; then the interrupt tested, php's
    /// `jit_frameless_interrupt_check`, and a throw leaving through the
    /// opline's unwind. A variable's reference is read through in place,
    /// php's `jit_ZVAL_DEREF_ref`; a variable that may be unset, or a
    /// temporary that may hold a reference, is tested first and, where it
    /// is, the call is its handler's, through [`Kernel::Handle`], which
    /// warns of it as php's `zend_jit_zval_check_undef` calls
    /// `zend_jit_undefined_op_helper` to.
    ///
    /// The three arities over constants, variables, a temporary, a
    /// variable that is unset once and one that holds a reference, a
    /// coercion's deprecation and a refusal:
    ///
    /// ```php
    /// function f($n, $a) {
    ///     $t = []; $r = "xyz"; $ref = &$r;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if ($i == 2) { unset($u); } else { $u = "u$i"; }
    ///         $t[] = trim(" $i ") . substr($a, $i % 3, 2) . str_contains($a, $u ?? "") . substr($ref, $i & 1) . max(1, min(9, $i & 7)) . in_array($i, [1, 2]) . substr($u, 0, 1);
    ///     }
    ///     return implode(",", $t);
    /// }
    /// echo f(5, "abcdef"), "\n";
    /// try { f(1, [1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-force process
    /// Warning: Undefined variable $u in %SCRATCH%/witness.php on line 6
    ///
    /// Deprecated: substr(): Passing null to parameter #1 ($string) of type string is deprecated in %SCRATCH%/witness.php on line 6
    /// 0abxyz1u,1bcyz11u,2cd1xyz21,3abyz3u,4bcxyz4u
    /// substr(): Argument #1 ($string) must be of type string, array given
    /// ```
    ///
    /// ```php
    /// function f($n, $a) {
    ///     $t = []; $r = "xyz"; $ref = &$r;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if ($i == 2) { unset($u); } else { $u = "u$i"; }
    ///         $t[] = trim(" $i ") . substr($a, $i % 3, 2) . str_contains($a, $u ?? "") . substr($ref, $i & 1) . max(1, min(9, $i & 7)) . in_array($i, [1, 2]) . substr($u, 0, 1);
    ///     }
    ///     return implode(",", $t);
    /// }
    /// echo f(5, "abcdef"), "\n";
    /// try { f(1, [1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// Warning: Undefined variable $u in %SCRATCH%/witness.php on line 6
    ///
    /// Deprecated: substr(): Passing null to parameter #1 ($string) of type string is deprecated in %SCRATCH%/witness.php on line 6
    /// 0abxyz1u,1bcyz11u,2cd1xyz21,3abyz3u,4bcxyz4u
    /// substr(): Argument #1 ($string) must be of type string, array given
    /// ```
    pub(super) fn frameless_call(
        &mut self,
        line: &Line,
        [room, opline, func]: [Value; 3],
    ) -> Result<(), Abort> {
        let Imm::Frameless(native) = line.imm else {
            return Err(mismatch("a frameless call with no native named"));
        };
        let arity = match line.opcode {
            Opcode::FramelessIcall0 => 0,
            Opcode::FramelessIcall1 => 1,
            Opcode::FramelessIcall2 => 2,
            Opcode::FramelessIcall3 => 3,
            _ => return Err(mismatch("a frameless call of another opcode")),
        };
        let operands = [line.op1, line.op2, line.data];
        let (handler, join) = self.split();
        for operand in operands.iter().take(arity) {
            let Operand::Value(value) = *operand else {
                continue;
            };
            let held = self.value(value)?;
            // A variable's reference is read through in place below; a
            // temporary's is the handler's, as is an unset variable,
            // which php's code warns of out of line.
            let through = self.kind_of(*operand)? == OperandKind::Cv;
            let refused = if through {
                Types::UNDEF
            } else {
                Types::UNDEF.or(Types::REF)
            };
            if !held.types.meets(refused) {
                continue;
            }
            let (base, slot) = self.place(held.slot)?;
            let info = self.b.ins().load(I32, trusted(), base, slot + TYPE_AT);
            let kind = self.b.ins().band_imm_u(info, KIND);
            let mut either = self
                .b
                .ins()
                .icmp_imm_u(IntCC::Equal, kind, tag(ZvalType::Undef));
            if !through && held.types.meets(Types::REF) {
                let reference =
                    self.b
                        .ins()
                        .icmp_imm_u(IntCC::Equal, kind, tag(ZvalType::Reference));
                either = self.b.ins().bor(either, reference);
            }
            self.divert_if(either, handler);
        }
        self.b.ins().store(trusted(), opline, room, EX_OPLINE);
        let mut kinds = [OperandKind::Unused; 3];
        let mut words = Vec::with_capacity(6);
        for (place, operand) in operands.into_iter().enumerate() {
            if place >= arity {
                words.push(self.b.ins().iconst(I64, 0));
                words.push(self.b.ins().iconst(I32, 0));
                continue;
            }
            let kind = self.kind_of(operand)?;
            let (word, info) = match operand {
                Operand::Value(value)
                    if kind == OperandKind::Cv && self.value(value)?.types.meets(Types::REF) =>
                {
                    // php's `ZVAL_DEREF` before the native reads it.
                    let (base, slot) = self.place(self.value(value)?.slot)?;
                    let zval = self.b.ins().iadd_imm_s(base, i64::from(slot));
                    let zval = self.deref(zval);
                    let word = self.b.ins().load(I64, trusted(), zval, 0);
                    let info = self.b.ins().load(I32, trusted(), zval, TYPE_AT);
                    (word, info)
                }
                _ => self.operand_words(line.frame, operand)?,
            };
            if let (OperandKind::Tmp, Operand::Value(value)) = (kind, operand) {
                // The temporary's owner is the native's record's now.
                let (base, slot) = self.place(self.value(value)?.slot)?;
                let undef = self.b.ins().iconst(I32, tag(ZvalType::Undef));
                self.b.ins().store(trusted(), undef, base, slot + TYPE_AT);
            }
            kinds[place] = kind;
            words.extend([word, info]);
        }
        let returns = self.returns_of(line)?;
        let out = returns.address();
        let place = self.frame_ref(line.frame)?;
        let place = self.b.ins().ireduce(I32, place);
        let native = self.b.ins().iconst(I32, i64::from(native.0));
        let kinds = self.b.ins().iconst(I32, OperandKinds::of(kinds).word());
        let mut args = vec![room, opline, place, native, kinds];
        args.extend(words);
        args.push(out);
        self.call(Kernel::Frameless, &args, leave_of(line))?;
        let threw = self.interrupt_after(line, [room, opline])?;
        self.release_unread(line, returns)?;
        let threw = self.b.use_var(threw);
        let to = self.left_by(leave_of(line))?;
        self.b.ins().brif(threw, to, &[], join, &[]);
        self.b.switch_to_block(handler);
        self.b.seal_block(handler);
        self.call(Kernel::Handle, &[room, opline, func], leave_of(line))?;
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
        self.b.seal_block(join);
        Ok(())
    }

    /// The two words of the zval `operand` of `frame` stands for: a
    /// scalar constant's, as php's op array holds it, a value's held in a
    /// register as php's code writes it to the zval it hands on, or the
    /// zval's as it stands in its slot or among the op array's constants.
    fn operand_words(&mut self, frame: Frame, operand: Operand) -> Result<(Value, Value), Abort> {
        if let Operand::Value(value) = operand
            && self.value(value)?.home == Home::Register
        {
            return Ok(match self.register(value)? {
                Scalar::Long(x) => (x, self.b.ins().iconst(I32, tag(ZvalType::Long))),
                Scalar::Double(x) => (
                    self.b.ins().bitcast(I64, MemFlagsData::new(), x),
                    self.b.ins().iconst(I32, tag(ZvalType::Double)),
                ),
                Scalar::Bool(b) => {
                    let b = self.b.ins().uextend(I32, b);
                    (
                        self.b.ins().iconst(I64, 0),
                        self.b.ins().iadd_imm_s(b, tag(ZvalType::False)),
                    )
                }
                Scalar::Null => (
                    self.b.ins().iconst(I64, 0),
                    self.b.ins().iconst(I32, tag(ZvalType::Null)),
                ),
            });
        }
        let (word, kind) = match operand {
            Operand::Constant(Constant::Null) => (0, ZvalType::Null),
            Operand::Constant(Constant::False) => (0, ZvalType::False),
            Operand::Constant(Constant::True) => (0, ZvalType::True),
            Operand::Constant(Constant::Long(n)) => (n, ZvalType::Long),
            Operand::Constant(Constant::Double(d)) => (
                i64::from_ne_bytes(d.to_bits().to_ne_bytes()),
                ZvalType::Double,
            ),
            _ => return self.zval_words(frame, operand),
        };
        let word = self.b.ins().iconst(I64, word);
        let info = self.b.ins().iconst(I32, tag(kind));
        Ok((word, info))
    }
}
