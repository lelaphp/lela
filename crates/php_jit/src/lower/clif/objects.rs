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
use php_engine::tier::{self, ClassId, FRAME};
use php_heap::{Obj, ZvalType};
use php_unit::Opcode;

use super::{Emit, KIND, SLOT, Scalar, TYPE_AT, imm, mismatch, tag, trusted};
use crate::types::{Abort, ExitId, Imm, Line, Object, OpId, Operand};

impl Emit<'_> {
    /// The object `object` names, a `zend_object *`: `$this` of its frame,
    /// or what the value's slot holds, an object as its guard held it.
    fn object(&mut self, object: Object) -> Result<Value, Abort> {
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
    /// object its `op1` names, `$this` where it names none.
    fn property(&mut self, line: &Line) -> Result<Value, Abort> {
        let Imm::Property { slot, .. } = line.imm else {
            return Err(mismatch("a property opline with no slot"));
        };
        let object = match line.op1 {
            Operand::Unused => Object::This(line.frame),
            Operand::Value(value) => Object::Value(value),
            Operand::Constant(_) => return Err(mismatch("a property of a constant")),
        };
        let obj = self.object(object)?;
        let at = i64::from(imm(Obj::SLOTS_AT)) + SLOT * i64::from(slot);
        Ok(self.b.ins().iadd_imm_s(obj, at))
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
    pub(super) fn fetch_obj(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let exit = line
            .exit
            .ok_or_else(|| mismatch("a property fetch that cannot leave"))?;
        let out = line
            .out
            .ok_or_else(|| mismatch("a property fetch that writes nothing"))?;
        let zval = self.property(line)?;
        let byte = self.b.ins().load(I8, trusted(), zval, TYPE_AT);
        let undef = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, byte, tag(ZvalType::Undef));
        self.leave_if(undef, exit)?;
        match self.within(at).as_slice() {
            [] => {
                let zval = self.deref(zval);
                let info = self.b.ins().load(I32, trusted(), zval, TYPE_AT);
                let out_slot = self.value(out)?.slot;
                self.copy(zval, info, out_slot)
            }
            [in_place, through] => self.held_element([*in_place, *through], zval, out),
            _ => Err(mismatch(
                "a property fetch holds other guards than its value's two",
            )),
        }
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
        let zval = self.property(line)?;
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
        let value = match self.scalar_kind(line.data)? {
            Some(_) => Some(self.scalar(at, line.data)?),
            None => None,
        };
        let zval = self.property(line)?;
        let handled = self.b.create_block();
        let done = self.b.create_block();
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
        let zval = self.property(line)?;
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let kind = self.b.ins().load(I8, trusted(), zval, TYPE_AT);
        self.compound_at((zval, kind), (binary, by), (handled, done))?;
        self.handled_then(at, line, (handled, done))
    }
}
