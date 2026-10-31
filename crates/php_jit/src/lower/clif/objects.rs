//! The property oplines compiled in place on a declared slot, as php's
//! code generator compiles one whose `prop_info` it knows on an immutable
//! class: the class guard (`zend_jit_class_guard`,
//! `Z/ext/opcache/jit/zend_jit_ir.c:14191-14204`), then the slot read
//! (`zend_jit_fetch_obj`, `:14206-14700`), written (`zend_jit_assign_obj`,
//! `:14737-15097`), compound-written (`zend_jit_assign_obj_op`,
//! `:15099-15530`) or stepped (`zend_jit_incdec_obj`, `:15533-15990`) at
//! [`Obj::SLOTS_AT`] past the object. Where php's code calls out, a
//! slot nothing holds, a reference in it, a value no number, the handler
//! serves the opline through the `handle` kernel, so a write takes no exit;
//! a read leaves for the interpreter, which serves `__get` and the warning.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I32, I64};
use cranelift_codegen::ir::{InstBuilder, Value};
use php_engine::tier::{self, ClassId, FRAME, PLACE, PlaceLayout};
use php_heap::{Obj, ZvalType};
use php_unit::Opcode;

use super::{Emit, KIND, Kind, Refuse, SLOT, Scalar, TYPE_AT, imm, mismatch, tag, trusted};
use crate::lower::IMAGES_AT;
use crate::types::{Abort, ExitId, Home, Imm, Line, Object, OpId, Operand, Slotted, ValueId};

/// The engine's runtime-cache slot layout, as the guard on a request's own
/// slot reads it.
const P: PlaceLayout = PLACE;

impl Emit<'_> {
    /// The object `object` names, a `zend_object *`: `$this` of its frame,
    /// or what the value's slot holds, an object as its guard held it.
    pub(super) fn object(&mut self, object: Object) -> Result<Value, Abort> {
        let (base, at) = match object {
            Object::This(frame) => (self.room(frame)?, imm(FRAME.this() as usize)),
            Object::Value(value) => self.place(self.value(value)?.slot)?,
        };
        Ok(self.b.ins().load(I64, trusted(), base, at))
    }

    /// php's class guard: leave through `exit` unless `object` is of
    /// `class`, a published class whose id is the same in every request of
    /// the trace's epoch.
    ///
    /// A variable written over between two reads holds an object the
    /// guard has not held, whatever the recording saw it of, so the second
    /// read holds its class again; an object of another class whose slot
    /// stands elsewhere leaves there:
    ///
    /// ```php
    /// class A { public $x = 1; }
    /// class B { public $q = 0; public $x = 2; }
    /// function f($os, $n) { $t = 0; $o = $os[0]; for ($i = 0; $i < $n; $i++) { $t += $o->x; $o = $os[$i]; $t += $o->x * 10; } return $t; }
    /// $a = new A; $os = []; for ($j = 0; $j < 30; $j++) { $os[] = $a; } $os[20] = new B;
    /// echo f($os, 30), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 341
    /// ```
    ///
    /// ```php
    /// class A { public $x = 1; }
    /// class B { public $q = 0; public $x = 2; }
    /// function f($os, $n) { $t = 0; $o = $os[0]; for ($i = 0; $i < $n; $i++) { $t += $o->x; $o = $os[$i]; $t += $o->x * 10; } return $t; }
    /// $a = new A; $os = []; for ($j = 0; $j < 30; $j++) { $os[] = $a; } $os[20] = new B;
    /// echo f($os, 30), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 341
    /// ```
    pub(super) fn class_guard(
        &mut self,
        object: Object,
        class: ClassId,
        exit: ExitId,
    ) -> Result<(), Abort> {
        let obj = self.object(object)?;
        let held = self.b.ins().load(I32, trusted(), obj, imm(Obj::CLASS_AT));
        let number = i64::from(tier::class_number(class));
        let other = self.b.ins().icmp_imm_u(IntCC::NotEqual, held, number);
        self.leave_if(other, exit)
    }

    /// The address of the declared slot `line` serves in place, on the
    /// object its `op1` names, `$this` where it names none: by its number
    /// on a published class, or read from the request's cache slot where
    /// it holds a declared slot of the object's class whose type has a bit
    /// of `kinds` (each of them, none tested where `kinds` is none), as
    /// php's code reads it for a class that is not
    /// immutable, and refused to `refuse` otherwise
    /// (`zend_jit_fetch_obj`, `Z/ext/opcache/jit/zend_jit_ir.c:14341-14370`;
    /// `zend_jit_assign_obj`, `:14874-14895`).
    ///
    /// `zend_lazy_class_load`'s copy of a class linked over one the
    /// request evaluated, whose trace then serves an evaluated class whose
    /// slots stand elsewhere and whose `$n` takes a float, a published
    /// class, and a fresh object of the first again, and an enum's
    /// `value` and `name` read for two cases, under force and where every
    /// guard fails once:
    ///
    /// ```php
    /// eval('class Base { public $x = 1; public $y = 0; public int $n = 0; }');
    /// class Child extends Base { }
    /// eval('class E { public $w = 0; public $x = 10; public $y = 0; public float $n = 0.5; }');
    /// class P { public $y = 0; public $x = 3; public int $n = 0; }
    /// enum Suit: string { case H = 'h'; case S = 's'; }
    /// function f($o, $m) { $t = 0; for ($i = 0; $i < $m; $i++) { $t += $o->x; $o->y = $i; $o->x += 1; $o->y++; $o->n = $i; } return "$t {$o->y} " . var_export($o->n, true); }
    /// function g($e, $m) { $s = ''; for ($i = 0; $i < $m; $i++) { $s .= $e->value . $e->name; } return $s; }
    /// echo f(new Child, 10), "\n", f(new E, 10), "\n", f(new P, 10), "\n", f(new Child, 4), "\n";
    /// echo g(Suit::H, 3), " ", g(Suit::S, 2), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 55 10 9
    /// 145 10 9.0
    /// 75 10 9
    /// 10 4 3
    /// hHhHhH sSsS
    /// ```
    ///
    /// ```php
    /// eval('class Base { public $x = 1; public $y = 0; public int $n = 0; }');
    /// class Child extends Base { }
    /// eval('class E { public $w = 0; public $x = 10; public $y = 0; public float $n = 0.5; }');
    /// class P { public $y = 0; public $x = 3; public int $n = 0; }
    /// enum Suit: string { case H = 'h'; case S = 's'; }
    /// function f($o, $m) { $t = 0; for ($i = 0; $i < $m; $i++) { $t += $o->x; $o->y = $i; $o->x += 1; $o->y++; $o->n = $i; } return "$t {$o->y} " . var_export($o->n, true); }
    /// function g($e, $m) { $s = ''; for ($i = 0; $i < $m; $i++) { $s .= $e->value . $e->name; } return $s; }
    /// echo f(new Child, 10), "\n", f(new E, 10), "\n", f(new P, 10), "\n", f(new Child, 4), "\n";
    /// echo g(Suit::H, 3), " ", g(Suit::S, 2), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 55 10 9
    /// 145 10 9.0
    /// 75 10 9
    /// 10 4 3
    /// hHhHhH sSsS
    /// ```
    fn property(
        &mut self,
        line: &Line,
        kinds: Option<u32>,
        refuse: Refuse,
    ) -> Result<Value, Abort> {
        let Imm::Property { slot, .. } = line.imm else {
            return Err(mismatch("a property opline with no slot"));
        };
        let object = match line.op1 {
            Operand::Unused => Object::This(line.frame),
            Operand::Value(value) => Object::Value(value),
            Operand::Constant(_) => return Err(mismatch("a property of a constant")),
        };
        let obj = self.object(object)?;
        let slots = i64::from(imm(Obj::SLOTS_AT));
        match slot {
            Slotted::Declared(slot) => {
                Ok(self.b.ins().iadd_imm_s(obj, slots + SLOT * i64::from(slot)))
            }
            Slotted::Cached => {
                let place = self.cache_guard(line, (P.property(), P.class()), obj, refuse)?;
                self.declared_in(place, kinds, refuse)?;
                let number = (self.b.ins()).uload32(trusted(), place, imm(P.slot() as usize));
                let offset = self.b.ins().imul_imm_u(number, SLOT);
                let slot = self.b.ins().iadd(obj, offset);
                Ok(self.b.ins().iadd_imm_s(slot, slots))
            }
        }
    }

    /// Refused to `refuse` unless the property's place at `place` is a
    /// declared slot whose type holds every bit of `kinds`, where the
    /// opline writes: `zend_get_property_offset`'s offset past
    /// `ZEND_FIRST_PROPERTY_OFFSET` and the `prop_info` php's code reads at
    /// `cache_slot + 2` before it writes in place
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14370`, `:14417`, `:14947`,
    /// `:14963`).
    fn declared_in(
        &mut self,
        place: Value,
        kinds: Option<u32>,
        refuse: Refuse,
    ) -> Result<(), Abort> {
        let offset = (self.b.ins()).load(I8, trusted(), place, imm(P.offset_tag() as usize));
        let declared = i64::from(P.declared());
        let mut refused = self.b.ins().icmp_imm_u(IntCC::NotEqual, offset, declared);
        if let Some(kinds) = kinds {
            let ty = (self.b.ins()).uload16(I32, trusted(), place, imm(P.ty() as usize));
            let held = self.b.ins().band_imm_u(ty, i64::from(kinds));
            let short = self
                .b
                .ins()
                .icmp_imm_u(IntCC::NotEqual, held, i64::from(kinds));
            refused = self.b.ins().bor(refused, short);
        }
        self.refuse_if(refused, refuse)
    }

    /// The one guard on a slot of the request's own runtime cache, the
    /// one `line` reads ([`IMAGES_AT`]): refused to `refuse` unless the
    /// slot's tag is `tag` and the class it names at `class_at` is the
    /// object's, as php's code compares `CACHED_PTR(cache_slot)` with
    /// `obj->ce` before it reads past it for a class that is not immutable
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14341-14370`, and a method's
    /// call, `zend_jit_init_method_call`, `:9025-9056`). The tag is tested
    /// before anything past it is read; the slot's address is answered.
    pub(super) fn cache_guard(
        &mut self,
        line: &Line,
        (tag, class_at): (u32, u32),
        obj: Value,
        refuse: Refuse,
    ) -> Result<Value, Abort> {
        let place = self.cached_slot(line, tag, refuse)?;
        let class = self
            .b
            .ins()
            .load(I32, trusted(), place, imm(class_at as usize));
        let of = self.b.ins().load(I32, trusted(), obj, imm(Obj::CLASS_AT));
        let other = self.b.ins().icmp(IntCC::NotEqual, class, of);
        self.refuse_if(other, refuse)?;
        Ok(place)
    }

    /// The address of the slot of the request's own runtime cache `line`
    /// reads ([`IMAGES_AT`]), refused to `refuse` unless its tag is `tag`,
    /// which is tested before anything past it is read: a slot the touch
    /// has not answered reads a tag that names nothing.
    pub(super) fn cached_slot(
        &mut self,
        line: &Line,
        tag: u32,
        refuse: Refuse,
    ) -> Result<Value, Abort> {
        let map = line
            .map_ptr
            .ok_or_else(|| mismatch("a cached place with no cache slot"))?;
        let cx = self
            .cx
            .ok_or_else(|| mismatch("a cache slot read before the entry"))?;
        // The slots stand past the trace's calls' headers ([`IMAGES_AT`]).
        let addresses = self.b.ins().load(I64, trusted(), cx, IMAGES_AT);
        let at = (self.held.len().checked_add(map))
            .and_then(|at| at.checked_mul(size_of::<usize>()))
            .and_then(|at| i32::try_from(at).ok())
            .ok_or_else(|| mismatch("a cache slot past an immediate"))?;
        let place = self.b.ins().load(I64, trusted(), addresses, at);
        let held = self
            .b
            .ins()
            .load(I32, trusted(), place, imm(P.tag() as usize));
        let other = self
            .b
            .ins()
            .icmp_imm_u(IntCC::NotEqual, held, i64::from(tag));
        self.refuse_if(other, refuse)?;
        Ok(place)
    }

    /// `FETCH_OBJ_R` of a declared slot: a slot nothing holds leaves for
    /// the interpreter, as php's `ZEND_JIT_EXIT_TO_VM` exit does, to serve
    /// `__get`, the lazy object's initialiser or the warning; what it holds
    /// is copied to the temporary as `ZVAL_COPY_DEREF` copies it, or held
    /// to the type the reader's guards ask, read in place and through the
    /// reference it is, as an element a fetch reads is.
    ///
    /// Objects of the class the trace recorded, of a subclass, a lazy
    /// ghost and a lazy proxy, whose slots stand empty until the
    /// interpreter initialises them, and an object whose slot was unset:
    ///
    /// ```php
    /// class A { public $x = 1; public $y = 2; }
    /// class B extends A { public $z = 3; }
    /// function f($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->x; $o->y = $i; $o->x += 1; $o->y++; } return $t; }
    /// $g = (new ReflectionClass(A::class))->newLazyGhost(function ($o) { echo "ghost\n"; $o->x = 100; });
    /// $p = (new ReflectionClass(A::class))->newLazyProxy(function ($o) { echo "proxy\n"; return new A; });
    /// $u = new A; unset($u->x);
    /// echo f(new A, 10), " ", f(new B, 10), " ", f($g, 10), " ", f($p, 10), " ", $g->x, " ", $p->x, "\n";
    /// echo f($u, 2), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 55 55 ghost
    /// 1045 proxy
    /// 55 110 11
    ///
    /// Warning: Undefined property: A::$x in %s on line 4
    ///
    /// Warning: Undefined property: A::$x in %s on line 4
    /// 1
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class A { public $x = 1; public $y = 2; }
    /// class B extends A { public $z = 3; }
    /// function f($o, $n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += $o->x; $o->y = $i; $o->x += 1; $o->y++; } return $t; }
    /// $g = (new ReflectionClass(A::class))->newLazyGhost(function ($o) { echo "ghost\n"; $o->x = 100; });
    /// $p = (new ReflectionClass(A::class))->newLazyProxy(function ($o) { echo "proxy\n"; return new A; });
    /// $u = new A; unset($u->x);
    /// echo f(new A, 10), " ", f(new B, 10), " ", f($g, 10), " ", f($p, 10), " ", $g->x, " ", $p->x, "\n";
    /// echo f($u, 2), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 55 55 ghost
    /// 1045 proxy
    /// 55 110 11
    ///
    /// Warning: Undefined property: A::$x in %s on line 4
    ///
    /// Warning: Undefined property: A::$x in %s on line 4
    /// 1
    /// ```
    ///
    /// A typed property read through `$this` in a method the loop's trace
    /// calls (#3665): php's analysis types the read by the declaration
    /// (`zend_fetch_prop_info` on the method's scope), so the read holds
    /// no guard and its value is kept in a register, which the read
    /// defines from the slot as `zend_jit_zval_copy_deref_reg` does
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14611-14616`):
    ///
    /// ```php
    /// class P { public int $m = 0; function f() { return $this->m; } }
    /// $p = new P;
    /// for ($i = 1; $i < 4; $i++) { $p->m = $i; echo $p->f(), "\n"; }
    /// ```
    /// ```expect regime=jit-inline process
    /// 1
    /// 2
    /// 3
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) $main() %s:4
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit %d $main() %s:4
    /// ---- TRACE 2 start (side trace 1/%d) $main() %s:4
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    /// ```
    ///
    /// The same over every scalar a declaration types alone, an `int`
    /// through a reference among them, read on the compiler's thread
    /// once its trace is entered:
    ///
    /// ```php
    /// class P {
    ///     public int $m = 0; public float $d = 0.0; public bool $b = false;
    ///     function m() { return $this->m; } function d() { return $this->d; } function b() { return $this->b; }
    /// }
    /// $p = new P; $r = new P; $q = &$r->m; $bad = 0;
    /// for ($i = 1; $i < 200000; $i++) {
    ///     $p->m = $i; $p->d = $i * 0.5; $p->b = ($i & 1) === 1; $q = $i;
    ///     if ($p->m() !== $i || $p->d() !== $i * 0.5 || $p->b() !== (($i & 1) === 1) || $r->m() !== $i) { $bad++; }
    /// }
    /// echo $bad, "\n";
    /// ```
    /// ```expect regime=jit-tracing process
    /// 0
    /// ```
    pub(super) fn fetch_obj(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let exit = line
            .exit
            .ok_or_else(|| mismatch("a property fetch that cannot leave"))?;
        let out = line
            .out
            .ok_or_else(|| mismatch("a property fetch that writes nothing"))?;
        let zval = self.property(line, None, Refuse::Exit(exit))?;
        let byte = self.b.ins().load(I8, trusted(), zval, TYPE_AT);
        let undef = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Undef));
        self.leave_if(undef, exit)?;
        match self.within(at).as_slice() {
            [] => self.copy_deref(zval, out),
            [in_place, through] => self.held_element(*in_place, Some(*through), zval, out),
            _ => Err(mismatch(
                "a property fetch holds other guards than its value's two",
            )),
        }
    }

    /// The property's zval at `zval`, through the reference it may be,
    /// written to `out`, the value of a fetch no guard holds because php's
    /// analysis typed it by the declaration: read as the scalar of its
    /// kind where `out` is kept in a register, as
    /// `zend_jit_zval_copy_deref_reg` reads it, and copied to its slot
    /// otherwise, as `zend_jit_zval_copy_deref` copies it
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:14611-14623`).
    fn copy_deref(&mut self, zval: Value, out: ValueId) -> Result<(), Abort> {
        let zval = self.deref(zval);
        let held = self.value(out)?;
        if held.home == Home::Register {
            let kind =
                Kind::of(held.types).ok_or_else(|| mismatch("a register of no scalar kind"))?;
            let read = self.read(zval, 0, kind);
            return self.define(out, read);
        }
        let info = self.b.ins().load(I32, trusted(), zval, TYPE_AT);
        self.copy(zval, info, held.slot)
    }

    /// `FETCH_OBJ_W` of a declared slot with no type: the slot itself,
    /// `IS_INDIRECT`, written to the `VAR` the write after it reads, as
    /// `zend_jit_fetch_obj` writes it; a slot nothing holds leaves for the
    /// interpreter, `__get`'s, a lazy object's or the warning's.
    ///
    /// An array appended to through the fetch, a nested element
    /// compound-written, a typed array's append the handler serves, an
    /// object's property written through it, a reference taken of it, and
    /// a slot unset, which the handler lays down:
    ///
    /// ```php
    /// class L { public $a = []; public $m = [[0]]; public ?array $t = []; public $o; }
    /// function f($l, $n) { for ($i = 0; $i < $n; $i++) { $l->a[] = $i; $l->m[0][0] += $i; $l->t[] = $i; $l->o->x = $i; $r = &$l->a; $r[] = -1; unset($r); } return $l; }
    /// $l = new L; $l->o = new stdClass; echo json_encode(f($l, 4)), "\n";
    /// $u = new L; unset($u->a); $u->o = new stdClass; echo json_encode(f($u, 2)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// {"a":[0,-1,1,-1,2,-1,3,-1],"m":[[6]],"t":[0,1,2,3],"o":{"x":3}}
    /// {"a":[0,-1,1,-1],"m":[[1]],"t":[0,1],"o":{"x":1}}
    /// ```
    ///
    /// ```php
    /// class L { public $a = []; public $m = [[0]]; public ?array $t = []; public $o; }
    /// function f($l, $n) { for ($i = 0; $i < $n; $i++) { $l->a[] = $i; $l->m[0][0] += $i; $l->t[] = $i; $l->o->x = $i; $r = &$l->a; $r[] = -1; unset($r); } return $l; }
    /// $l = new L; $l->o = new stdClass; echo json_encode(f($l, 4)), "\n";
    /// $u = new L; unset($u->a); $u->o = new stdClass; echo json_encode(f($u, 2)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// {"a":[0,-1,1,-1,2,-1,3,-1],"m":[[6]],"t":[0,1,2,3],"o":{"x":3}}
    /// {"a":[0,-1,1,-1],"m":[[1]],"t":[0,1],"o":{"x":1}}
    /// ```
    pub(super) fn fetch_obj_w(&mut self, line: &Line) -> Result<(), Abort> {
        let exit = line
            .exit
            .ok_or_else(|| mismatch("a property fetch that cannot leave"))?;
        let out = line
            .out
            .ok_or_else(|| mismatch("a property fetch that writes nothing"))?;
        let zval = self.property(line, Some(P.untyped()), Refuse::Exit(exit))?;
        let byte = self.b.ins().load(I8, trusted(), zval, TYPE_AT);
        let undef = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Undef));
        self.leave_if(undef, exit)?;
        let (room, at) = self.place(self.value(out)?.slot)?;
        let indirect = self.b.ins().iconst(I32, tag(ZvalType::Indirect));
        self.b.ins().store(trusted(), zval, room, at);
        self.b.ins().store(trusted(), indirect, room, at + TYPE_AT);
        Ok(())
    }

    /// `ASSIGN_OBJ` to a declared slot with no type, as
    /// `zend_jit_assign_obj` writes it through `zend_jit_assign_to_variable`:
    /// what the slot held written over where it counts nothing, given up
    /// after where it counts, through the release kernel where it is the
    /// last owner; the handler's where the slot holds nothing, `__set`'s or
    /// the lazy object's, or a reference, which it writes through.
    ///
    /// An array written over its predecessor, an object whose destructor
    /// runs as it is written over, a slot that holds a reference, written
    /// through by the handler, and a string:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d"; } }
    /// class W { public $a = [1]; public $o; public $r; public $s = "s"; }
    /// function g($w, $m) { for ($i = 0; $i < $m; $i++) { $w->a = [$i]; $w->o = $i == 2 ? new D : null; $w->r = $i; $w->s = str_repeat("x", $i); } return $w; }
    /// $y = 0; $w = new W; $w->r = &$y; echo json_encode(g($w, 5)), " $y\n";
    /// ```
    /// ```expect regime=jit-force process
    /// d{"a":[4],"o":null,"r":4,"s":"xxxx"} 4
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d"; } }
    /// class W { public $a = [1]; public $o; public $r; public $s = "s"; }
    /// function g($w, $m) { for ($i = 0; $i < $m; $i++) { $w->a = [$i]; $w->o = $i == 2 ? new D : null; $w->r = $i; $w->s = str_repeat("x", $i); } return $w; }
    /// $y = 0; $w = new W; $w->r = &$y; echo json_encode(g($w, 5)), " $y\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// d{"a":[4],"o":null,"r":4,"s":"xxxx"} 4
    /// ```
    ///
    /// Typed slots: a long and a `null` their types hold written in place,
    /// a long a `float` slot converts and a `readonly` slot's refusal the
    /// handler's:
    ///
    /// ```php
    /// class T { public int $n = 0; public ?int $m = 1; public float $f = 0.0; public readonly int $r; function __construct() { $this->r = 1; } }
    /// function f($o, $k) { $c = 0; for ($i = 0; $i < $k; $i++) { $o->n = $i; $o->m = $i % 2 ? null : $i; $o->f = $i; try { $o->r = $i; } catch (Error $e) { $c++; } } return [$o->n, $o->m, $o->f, $o->r, $c]; }
    /// echo json_encode(f(new T, 6)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [5,null,5,1,6]
    /// ```
    ///
    /// ```php
    /// class T { public int $n = 0; public ?int $m = 1; public float $f = 0.0; public readonly int $r; function __construct() { $this->r = 1; } }
    /// function f($o, $k) { $c = 0; for ($i = 0; $i < $k; $i++) { $o->n = $i; $o->m = $i % 2 ? null : $i; $o->f = $i; try { $o->r = $i; } catch (Error $e) { $c++; } } return [$o->n, $o->m, $o->f, $o->r, $c]; }
    /// echo json_encode(f(new T, 6)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [5,null,5,1,6]
    /// ```
    pub(super) fn assign_obj(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let kind = self.scalar_kind(line.data)?;
        let value = match kind {
            Some(_) => Some(self.scalar(at, line.data)?),
            None => None,
        };
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let zval = self.property(line, Some(kinds_of(kind)), Refuse::To(handled))?;
        let old = self.b.ins().load(I32, trusted(), zval, TYPE_AT);
        let kind = self.b.ins().band_imm_u(old, KIND);
        let undef = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Undef));
        self.divert_if(undef, handled);
        // Where every guard fails once, the handler serves the write.
        let never = self.b.ins().iconst(I8, 0);
        let fuzzed = self.fuzzed(never)?;
        self.divert_if(fuzzed, handled);
        self.written_over(line, (zval, old), value, (handled, done))?;
        self.handled_then(at, line, (handled, done))?;
        match (line.out, value) {
            (Some(out), Some(value)) => self.define(out, value),
            (Some(_), None) => Err(mismatch(
                "a property write of a value no scalar, its result used",
            )),
            (None, _) => Ok(()),
        }
    }

    /// `ASSIGN_OBJ_OP` of a number to a declared slot with no type, and
    /// `PRE_INC_OBJ`, `PRE_DEC_OBJ`, `POST_INC_OBJ` and `POST_DEC_OBJ` with
    /// no `out` slot written: in place where the slot holds a long or a
    /// double, which php's `zend_jit_math_helper` serves inline, a long
    /// overflowing to a double as php's does; the handler's otherwise,
    /// where php's code calls `zend_jit_assign_obj_op_helper` and its
    /// increment's kin, so the write takes no exit.
    ///
    /// A long that overflows to a double, a string the handler converts, a
    /// reference the handler steps through and a double, each a compound
    /// write, an increment or a decrement:
    ///
    /// ```php
    /// class C { public $n = PHP_INT_MAX - 2; public $s = "5"; public $r; public $f = 0.5; }
    /// function f($o, $m) { for ($i = 0; $i < $m; $i++) { $o->n++; $o->s += 1; $o->r++; $o->f -= 0.25; $o->f *= 2; --$o->s; } return $o; }
    /// $x = 1; $c = new C; $c->r = &$x; var_dump(f($c, 5), $x);
    /// ```
    /// ```expect regime=jit-force process
    /// object(C)#1 (4) {
    ///   ["n"]=>
    ///   float(9.223372036854776E+18)
    ///   ["s"]=>
    ///   int(5)
    ///   ["r"]=>
    ///   &int(6)
    ///   ["f"]=>
    ///   float(0.5)
    /// }
    /// int(6)
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class C { public $n = PHP_INT_MAX - 2; public $s = "5"; public $r; public $f = 0.5; }
    /// function f($o, $m) { for ($i = 0; $i < $m; $i++) { $o->n++; $o->s += 1; $o->r++; $o->f -= 0.25; $o->f *= 2; --$o->s; } return $o; }
    /// $x = 1; $c = new C; $c->r = &$x; var_dump(f($c, 5), $x);
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// object(C)#1 (4) {
    ///   ["n"]=>
    ///   float(9.223372036854776E+18)
    ///   ["s"]=>
    ///   int(5)
    ///   ["r"]=>
    ///   &int(6)
    ///   ["f"]=>
    ///   float(0.5)
    /// }
    /// int(6)
    /// ```
    pub(super) fn compound_obj(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let (binary, by) = match (line.opcode, line.imm) {
            (
                Opcode::AssignObjOp,
                Imm::Property {
                    binary: Some(binary),
                    ..
                },
            ) => (binary, self.scalar(at, line.data)?),
            (Opcode::PreIncObj | Opcode::PostIncObj, _) => {
                (Opcode::Add, Scalar::Long(self.b.ins().iconst(I64, 1)))
            }
            (Opcode::PreDecObj | Opcode::PostDecObj, _) => {
                (Opcode::Sub, Scalar::Long(self.b.ins().iconst(I64, 1)))
            }
            _ => return Err(mismatch("a property's compound write with no operator")),
        };
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let zval = self.property(line, Some(P.untyped()), Refuse::To(handled))?;
        let kind = self.b.ins().load(I8, trusted(), zval, TYPE_AT);
        self.compound_at((zval, kind), (binary, by, false), (handled, done))?;
        self.handled_then(at, line, (handled, done))
    }
}

/// The bits of a slot's type a write of a scalar of `kind` lands in as it
/// stands, by the handler's own test (`SlotType::admits`): its kind's, both
/// booleans' for one the value says, so a slot of `true` or of `false`
/// alone refuses it, or a slot with no type's for a value no scalar.
fn kinds_of(kind: Option<Kind>) -> u32 {
    match kind {
        Some(Kind::Long) => P.kind(ZvalType::Long),
        Some(Kind::Double) => P.kind(ZvalType::Double),
        Some(Kind::Null) => P.kind(ZvalType::Null),
        Some(Kind::False) => P.kind(ZvalType::False),
        Some(Kind::True) => P.kind(ZvalType::True),
        Some(Kind::Bool) => P.kind(ZvalType::False) | P.kind(ZvalType::True),
        None => P.untyped(),
    }
}
