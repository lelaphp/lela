//! The dimension oplines compiled inline, as php's code generator
//! compiles them over `zend_jit_fetch_dimension_address_inner`
//! (`Z/ext/opcache/jit/zend_jit_ir.c:11917-12511`): a packed array's slot
//! by a long key reached in place, as php's code reaches `arPacked[h]`,
//! and every other element through the table's own lookup, a key read off
//! its operand and handed to [`Kernel::Find`] or [`Kernel::Lookup`]. The
//! fetch (`zend_jit_fetch_dim_read`), `isset()` (`zend_jit_isset_isempty_dim`,
//! `:13157-13350`), the write (`zend_jit_assign_dim`), the compound write
//! (`zend_jit_assign_dim_op`) and the fetch for writing (`zend_jit_fetch_dim`).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I8, I32, I64};
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, StackSlot, Value};
use php_heap::{Arr, Ref, ZvalType};
use php_unit::Opcode;

use super::{COUNTED, Emit, KIND, Kernel, Scalar, TYPE_AT, imm, mismatch, tag, trusted};
use crate::types::{
    Abort, Check, Constant, ExitId, Fused, GuardAt, Imm, Layout, Leave, Line, OpId, Operand, Read,
    Types, ValueId, cannot_raise,
};

/// How a write reaches the element it writes ([`Emit::reach`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reach {
    /// A packed array's slot, by a long key.
    Slot,
    /// The table's lookup.
    Table,
    /// Either, by the array's layout.
    Either,
}

impl Emit<'_> {
    /// `isset()` of an array's element, as php's `zend_jit_isset_isempty_dim`
    /// compiles it over an array a variable holds, by a long or a string
    /// key: a packed array's slot below `nNumUsed` in place, any other
    /// element through [`Kernel::Find`], the element set where it is
    /// found and, a reference followed, more than `null`
    /// (`Z_TYPE_P(value) > IS_NULL`); the answer written, or the jump fused
    /// to it held. It calls no user code and leaves for nothing but the
    /// way its jump goes. A packed and a hashed array, read by longs and
    /// by strings, an element `null`, unset, a reference, then made a
    /// hash, and the answer jumped on and written:
    ///
    /// ```php
    /// function f($a, $h, $n) {
    ///     $p = 0; $q = 0; $k = 'b'; $w = [];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $j = $i % 5;
    ///         if (isset($a[$j])) { $p++; }
    ///         if (isset($h['a'])) { $q++; }
    ///         if (!isset($h[$k])) { $q += 10; }
    ///         $w[] = isset($h[$j]);
    ///         if ($i == 6) { unset($h['a']); $k = 'c'; }
    ///         if ($i == 8) { $a = ['x' => 1, 0 => 1, 1 => null]; }
    ///     }
    ///     return [$p, $q, json_encode($w)];
    /// }
    /// $r = 5;
    /// echo json_encode(f([1, null, 3, &$r], ['a' => 1, 'b' => null, 'c' => &$r, 2 => 0, 4 => null], 12)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [7,77,"[false,false,true,false,false,false,false,true,false,false,false,false]"]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($a, $h, $n) {
    ///     $p = 0; $q = 0; $k = 'b'; $w = [];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $j = $i % 5;
    ///         if (isset($a[$j])) { $p++; }
    ///         if (isset($h['a'])) { $q++; }
    ///         if (!isset($h[$k])) { $q += 10; }
    ///         $w[] = isset($h[$j]);
    ///         if ($i == 6) { unset($h['a']); $k = 'c'; }
    ///         if ($i == 8) { $a = ['x' => 1, 0 => 1, 1 => null]; }
    ///     }
    ///     return [$p, $q, json_encode($w)];
    /// }
    /// $r = 5;
    /// echo json_encode(f([1, null, 3, &$r], ['a' => 1, 'b' => null, 'c' => &$r, 2 => 0, 4 => null], 12)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [7,77,"[false,false,true,false,false,false,false,true,false,false,false,false]"]
    /// ```
    pub(super) fn isset_dim(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let answered = self.b.create_block();
        self.b.append_block_param(answered, I8);
        let unset = self.b.ins().iconst(I8, 0);
        let element = self.tested_element(at, line, (answered, &[BlockArg::Value(unset)]))?;
        let element = self.deref(element);
        let kind = self.b.ins().load(I8, trusted(), element, TYPE_AT);
        let set = self
            .b
            .ins()
            .icmp_imm_u(IntCC::UnsignedGreaterThan, kind, tag(ZvalType::Null));
        self.b.ins().jump(answered, &[BlockArg::Value(set)]);
        self.b.switch_to_block(answered);
        self.b.seal_block(answered);
        let answer = self.b.block_params(answered)[0];
        let jumps = match line.fused {
            Some(Fused::Jmpz) => self.b.ins().icmp_imm_s(IntCC::Equal, answer, 0),
            Some(Fused::Jmpnz) => self.b.ins().icmp_imm_s(IntCC::NotEqual, answer, 0),
            None => {
                if let Some(out) = line.out {
                    self.define(out, Scalar::Bool(answer))?;
                }
                answer
            }
        };
        self.direction(at, jumps)
    }

    /// The element `line`'s key names in the array its variable holds, for
    /// a test or a read that finds nothing silently: a packed array's slot
    /// below `nNumUsed`, as a layout guard held it, or the one
    /// [`Kernel::Find`] answers; to `missed` with its arguments where
    /// there is none, a packed array's hole aside, which the caller reads.
    fn tested_element(
        &mut self,
        at: OpId,
        line: &Line,
        (missed, args): (Block, &[BlockArg]),
    ) -> Result<Value, Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element of no array"));
        };
        let (slot, types) = {
            let held = self.value(array)?;
            (held.slot, held.types)
        };
        let arr = self.array_of(slot, self.read_of(at, array))?;
        let packed = self.packed_at(at, array, types) && self.long_at(at, line.op2)?;
        let (absent, reached) = if packed {
            let Scalar::Long(key) = self.scalar(at, line.op2)? else {
                return Err(mismatch("an element by no long key"));
            };
            (self.past_used(arr, key), Err(key))
        } else {
            let place = self.looked_up(at, line, Kernel::Find, arr)?;
            let kind = self.b.ins().stack_load(I64, I8, place, TYPE_AT);
            let unplaced = self
                .b
                .ins()
                .icmp_imm_s(IntCC::NotEqual, kind, tag(ZvalType::Indirect));
            (unplaced, Ok(place))
        };
        let on = self.b.create_block();
        self.b.ins().brif(absent, missed, args, on, &[]);
        self.b.switch_to_block(on);
        self.b.seal_block(on);
        Ok(match reached {
            Ok(place) => self.b.ins().stack_load(I64, I64, place, 0),
            Err(key) => self.slot_at(arr, key),
        })
    }

    /// `FETCH_DIM_IS`, the read an `isset()` chain, an `empty()` or a `??`
    /// makes, as php's `zend_jit_fetch_dim_read` compiles it over an array
    /// a variable holds under `BP_VAR_IS`: the element copied as
    /// `ZVAL_COPY_DEREF` copies it, and `null` where the array has not got
    /// the key, or a packed array holds a hole there, with nothing raised
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:12101-12200`).
    ///
    /// A hash and a packed array read by `??` and in `isset()`'s chain, the
    /// key there and not, an element a reference and one `null`:
    ///
    /// ```php
    /// function f($h, $p, $n) {
    ///     $s = 0; $k = 'b'; $o = [];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $s += $h['a'] ?? 100; $s += $h[$k] ?? 1000; $s += $p[$i % 4] ?? 7;
    ///         $o[] = isset($h['n'][0]); $o[] = $h['r'] ?? -1;
    ///         if ($i == 3) { $k = 'z'; unset($h['a']); }
    ///     }
    ///     return [$s, $o];
    /// }
    /// $r = 9;
    /// echo json_encode(f(['a' => 1, 'b' => 2, 'n' => [5], 'r' => &$r], [1, null, 3], 8)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [4448,[true,9,true,9,true,9,true,9,true,9,true,9,true,9,true,9]]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($h, $p, $n) {
    ///     $s = 0; $k = 'b'; $o = [];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $s += $h['a'] ?? 100; $s += $h[$k] ?? 1000; $s += $p[$i % 4] ?? 7;
    ///         $o[] = isset($h['n'][0]); $o[] = $h['r'] ?? -1;
    ///         if ($i == 3) { $k = 'z'; unset($h['a']); }
    ///     }
    ///     return [$s, $o];
    /// }
    /// $r = 9;
    /// echo json_encode(f(['a' => 1, 'b' => 2, 'n' => [5], 'r' => &$r], [1, null, 3], 8)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [4448,[true,9,true,9,true,9,true,9,true,9,true,9,true,9,true,9]]
    /// ```
    pub(super) fn fetch_is(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let out = line
            .out
            .ok_or_else(|| mismatch("a fetch that writes nothing"))?;
        let out = self.value(out)?.slot;
        let (missed, done) = (self.b.create_block(), self.b.create_block());
        let element = self.tested_element(at, line, (missed, &[]))?;
        let element = self.deref(element);
        let info = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        let kind = self.b.ins().band_imm_u(info, KIND);
        let hole = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Undef));
        self.divert_if(hole, missed);
        self.copy(element, info, out)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(missed);
        self.b.seal_block(missed);
        let (room, out_at) = self.place(out)?;
        let null = self.b.ins().iconst(I32, tag(ZvalType::Null));
        self.b.ins().store(trusted(), null, room, out_at + TYPE_AT);
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(())
    }

    /// The element `line`'s key names in the array `arr`, through
    /// `kernel`, [`Kernel::Find`] or [`Kernel::Lookup`], as php's code
    /// calls the table's lookup (`zend_jit_fetch_dimension_address_inner`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:11917-12511`): the place it
    /// answered, an `IS_INDIRECT` or `UNDEF`, in the kernel's answer slot.
    /// A throw, the memory limit's fatal past a lookup, unwinds.
    pub(super) fn looked_up(
        &mut self,
        at: OpId,
        line: &Line,
        kernel: Kernel,
        arr: Value,
    ) -> Result<StackSlot, Abort> {
        let (key, info, constant) = self.key_of(at, line)?;
        let constant = self.b.ins().iconst(I32, i64::from(constant));
        let [room, opline] = self.served(line)?;
        let slot = self.answer();
        let place = self.b.ins().stack_addr(I64, slot, 0);
        self.call(
            kernel,
            &[room, opline, arr, key, info, constant, place],
            line.unwind.map(Leave::Unwind),
        )?;
        Ok(slot)
    }

    /// The element's address a lookup answered in `slot`, on at `to`
    /// where it answered `UNDEF`.
    pub(super) fn leave_unplaced(&mut self, slot: StackSlot, to: Block) -> Value {
        let kind = self.b.ins().stack_load(I64, I8, slot, TYPE_AT);
        let unplaced = self
            .b
            .ins()
            .icmp_imm_s(IntCC::NotEqual, kind, tag(ZvalType::Indirect));
        self.divert_if(unplaced, to);
        self.b.ins().stack_load(I64, I64, slot, 0)
    }

    /// The key `line`'s `op2` names, as the lookup kernels take it: its
    /// word, its type info and whether it is the op array's constant,
    /// which a string is read as it is written.
    fn key_of(&mut self, at: OpId, line: &Line) -> Result<(Value, Value, bool), Abort> {
        if let Operand::Constant(Constant::Other(idx)) = line.op2 {
            let zval = self.constant_zval(line.frame, idx)?;
            let word = self.b.ins().load(I64, trusted(), zval, 0);
            let info = self.b.ins().load(I32, trusted(), zval, TYPE_AT);
            return Ok((word, info, true));
        }
        if self.long_at(at, line.op2)? {
            let Scalar::Long(word) = self.scalar(at, line.op2)? else {
                return Err(mismatch("a long key read as no long"));
            };
            let info = self.b.ins().iconst(I32, tag(ZvalType::Long));
            return Ok((word, info, matches!(line.op2, Operand::Constant(_))));
        }
        let Operand::Value(value) = line.op2 else {
            return Err(mismatch("a key of no value"));
        };
        let (room, key_at) = self.place(self.value(value)?.slot)?;
        let word = self.b.ins().load(I64, trusted(), room, key_at);
        let info = self.b.ins().load(I32, trusted(), room, key_at + TYPE_AT);
        Ok((word, info, false))
    }

    /// A read of a packed array's element by a long key: past `nNumUsed`
    /// or a hole leaves for the interpreter to serve it again; an element
    /// that is a reference is read through it; one of the type the op's
    /// guard holds is read, and one of another copied to the slot the
    /// fetch writes as php's `ZVAL_COPY_DEREF` copies it, for the exit to
    /// go on past the fetch. A temporary array the fetch read is freed
    /// after, as its handler frees it, what a destructor throws leaving
    /// through the exit `types` gives the free.
    pub(super) fn fetch(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let exit = line
            .exit
            .ok_or_else(|| mismatch("a fetch that cannot leave"))?;
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("a fetch from no array"));
        };
        let (slot, types) = {
            let held = self.value(array)?;
            (held.slot, held.types)
        };
        let out = line
            .out
            .ok_or_else(|| mismatch("a fetch that writes nothing"))?;
        let arr = self.array_of(slot, self.read_of(at, array))?;
        let packed = self.packed_at(at, array, types) && self.long_at(at, line.op2)?;
        let element = if packed {
            let Scalar::Long(key) = self.scalar(at, line.op2)? else {
                return Err(mismatch("a fetch by no long key"));
            };
            let element = self.element(arr, key, exit)?;
            let hole = self.b.ins().load(I8, trusted(), element, TYPE_AT);
            let hole = self
                .b
                .ins()
                .icmp_imm_s(IntCC::Equal, hole, tag(ZvalType::Undef));
            self.leave_if(hole, exit)?;
            element
        } else {
            let place = self.looked_up(at, line, Kernel::Find, arr)?;
            let to = self.left_by(Some(Leave::Exit(exit)))?;
            self.leave_unplaced(place, to)
        };
        match self.within(at).as_slice() {
            [] => {
                let element = self.deref(element);
                let info = self.b.ins().load(I32, trusted(), element, TYPE_AT);
                let out_slot = self.value(out)?.slot;
                self.copy(element, info, out_slot)?;
            }
            [in_place, through] => self.held_element([*in_place, *through], element, out)?,
            _ => {
                return Err(mismatch(
                    "a fetch holds other guards than its element's two",
                ));
            }
        }
        // `FREE_OP1`, as the interpreter takes it.
        self.free_temp(line, line.op1)
    }

    /// A write to an array's element by a long key, php's
    /// `zend_jit_assign_dim` (`Z/ext/opcache/jit/zend_jit_ir.c:13352-13526`):
    /// in place where the array is counted and held once, so php's
    /// `SEPARATE_ARRAY` has nothing to do, packed, and the key below
    /// `nNumUsed` and the element there
    /// (`zend_jit_fetch_dimension_address_inner`'s `BP_VAR_W` path,
    /// `:11953-12276`), as `zend_jit_assign_to_variable` writes it
    /// (`:5793-5950`): a scalar, or a value no scalar moved from a
    /// temporary, copied from a variable with one owner more or copied
    /// from a constant ([`Emit::copy_operand`]), and what the element held
    /// given up after ([`Emit::release`]), a destructor's throw leaving
    /// through the opline's unwind; through the handler otherwise, where
    /// php's code calls its helpers, the array's copy and the hash's
    /// lookup or insert, so the write takes no exit.
    ///
    /// An immutable array, one shared, a key past `nNumUsed`, a hole, an
    /// element that is a reference and an array turned hash are each served
    /// by the handler, in place of the write, and a counted element given
    /// up in place, its destructor called there, and the loop goes on in
    /// the trace:
    ///
    /// ```php
    /// class D { function __destruct() { echo "freed\n"; } }
    /// function w($n) {
    ///     $a = [0, 0, 0, 0]; $b = null;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if ($i == 50) { $b = $a; }
    ///         if ($i == 60) { $a[1] = new D; }
    ///         if ($i == 70) { unset($a[2]); }
    ///         if ($i == 80) { $a[1000] = -1; }
    ///         $a[$i % 6] = $i;
    ///     }
    ///     foreach ($a as $k => $v) { echo "$k=$v "; }
    ///     echo count($b), "\n";
    /// }
    /// w(100);
    /// ```
    /// ```expect regime=jit-force process
    /// freed
    /// 0=96 1=97 3=99 4=94 5=95 2=98 1000=-1 6
    /// ```
    ///
    /// The same where every guard fails once, the destructor of the
    /// element written over served where the write gives it up:
    ///
    /// ```php
    /// class D { function __destruct() { echo "freed\n"; } }
    /// function w($n) {
    ///     $a = [0, 0, 0, 0]; $b = null;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if ($i == 50) { $b = $a; }
    ///         if ($i == 60) { $a[1] = new D; }
    ///         if ($i == 70) { unset($a[2]); }
    ///         if ($i == 80) { $a[1000] = -1; }
    ///         $a[$i % 6] = $i;
    ///     }
    ///     foreach ($a as $k => $v) { echo "$k=$v "; }
    ///     echo count($b), "\n";
    /// }
    /// w(100);
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// freed
    /// 0=96 1=97 3=99 4=94 5=95 2=98 1000=-1 6
    /// ```
    ///
    /// Under exit-fuzz the handler serves the write once in each trace, in
    /// place of an exit:
    ///
    /// ```php
    /// function s($n) {
    ///     $a = []; for ($i = 0; $i < $n; $i++) { $a[] = true; }
    ///     for ($i = 2; $i < $n; $i++) {
    ///         if ($a[$i]) { for ($j = $i + $i; $j < $n; $j += $i) { $a[$j] = false; } }
    ///     }
    ///     $c = 0; foreach ($a as $p) { if ($p) { $c++; } }
    ///     return $c;
    /// }
    /// echo s(1000), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 170
    /// ```
    ///
    /// An element bound by reference to a variable is written through
    /// the reference, which the variable reads, its string and a long
    /// alike, under force and where every guard fails once:
    ///
    /// ```php
    /// function f($n) { $a = [1, 2, 3]; $r = &$a[1]; $t = 0; for ($i = 0; $i < $n; $i++) { $a[$i % 3] = $i; $t += $r; } return [$a, $r, $t]; }
    /// function g($n) { $a = ["x", "y"]; $r = &$a[1]; $t = 0; for ($i = 0; $i < $n; $i++) { $a[$i % 2] = str_repeat("s", $i % 5); $t += strlen($r); } return [$a, $r, $t]; }
    /// echo json_encode(f(3000)), json_encode(g(3000)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [[2997,2998,2999],2998,4495504][["sss","ssss"],"ssss",5997]
    /// ```
    ///
    /// ```php
    /// function f($n) { $a = [1, 2, 3]; $r = &$a[1]; $t = 0; for ($i = 0; $i < $n; $i++) { $a[$i % 3] = $i; $t += $r; } return [$a, $r, $t]; }
    /// function g($n) { $a = ["x", "y"]; $r = &$a[1]; $t = 0; for ($i = 0; $i < $n; $i++) { $a[$i % 2] = str_repeat("s", $i % 5); $t += strlen($r); } return [$a, $r, $t]; }
    /// echo json_encode(f(3000)), json_encode(g(3000)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [[2997,2998,2999],2998,4495504][["sss","ssss"],"ssss",5997]
    /// ```
    ///
    /// Fields of a line split, each trimmed back into its place, one
    /// uppercased and one a constant, the strings they held given up, under
    /// force and where every guard fails once:
    ///
    /// ```php
    /// function f($lines) { $t = 0; foreach ($lines as $line) { $f = explode(",", $line); foreach ($f as $k => $v) { $f[$k] = trim($v); } $f[1] = strtoupper($f[1]); $f[2] = "c"; $t += strlen(implode("|", $f)); } return [$t, $f]; }
    /// echo json_encode(f([" a , b ,x, d ", "e,f,g"])), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [12,["e","F","c"]]
    /// ```
    ///
    /// ```php
    /// function f($lines) { $t = 0; foreach ($lines as $line) { $f = explode(",", $line); foreach ($f as $k => $v) { $f[$k] = trim($v); } $f[1] = strtoupper($f[1]); $f[2] = "c"; $t += strlen(implode("|", $f)); } return [$t, $f]; }
    /// echo json_encode(f([" a , b ,x, d ", "e,f,g"])), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [12,["e","F","c"]]
    /// ```
    pub(super) fn assign_dim(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element write to no array"));
        };
        let value = match self.scalar_kind(line.data)? {
            Some(_) => Some(self.scalar(at, line.data)?),
            None => None,
        };
        let (slot, types) = {
            let held = self.value(array)?;
            (held.slot, held.types)
        };
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let zval = self.place(slot)?;
        let reach = self.reach(at, types, line)?;
        let write = |emit: &mut Self, element, old| {
            // Where every guard fails once, the handler serves the write.
            let never = emit.b.ins().iconst(I8, 0);
            let fuzzed = emit.fuzzed(never)?;
            emit.divert_if(fuzzed, handled);
            // An element that counts nothing is written over; one that
            // counts is given up after, but a reference is written
            // through, which `zend_jit_assign_to_variable` does past its
            // `ZVAL_DEREF` and the handler does here.
            emit.written_over(line, (element, old), value, (handled, done))
        };
        self.written_in_place((at, line), zval, (reach, Kernel::Lookup), (handled, write))?;
        self.handled_then(at, line, (handled, done))?;
        match (line.out, value) {
            (Some(out), Some(value)) => self.define(out, value),
            (Some(_), None) => Err(mismatch(
                "an element write of a value no scalar, its result used",
            )),
            (None, _) => Ok(()),
        }
    }

    /// How a write by `line`'s key reaches an element of an array of
    /// `types`: a packed array's slot by a long key, the table's lookup
    /// for a hash or a string key, and either by the array's layout where
    /// it may be of both, as php's code tests `HASH_FLAG_PACKED`
    /// (`MAY_BE_ARRAY_PACKED` and `MAY_BE_ARRAY_HASH` both).
    pub(super) fn reach(&self, at: OpId, types: Types, line: &Line) -> Result<Reach, Abort> {
        let long = self.long_at(at, line.op2)?;
        Ok(
            match (long, types.meets(Types::PACKED), types.meets(Types::HASH)) {
                (true, true, false) => Reach::Slot,
                (true, true, true) => Reach::Either,
                _ => Reach::Table,
            },
        )
    }

    /// The element `line`'s key names of the array the zval at `base +
    /// at` holds, handed with its type word to `then`, which writes it,
    /// where php's code writes it in place: the array counted and held
    /// once, so `SEPARATE_ARRAY` has nothing to do, then, as `reach` says,
    /// a packed array's slot below `nNumUsed` that holds a value, or the
    /// element `kernel`, [`Kernel::Lookup`] for a write and
    /// [`Kernel::Find`] for a compound one, answers
    /// (`zend_jit_fetch_dimension_address_inner`'s `BP_VAR_W` and
    /// `BP_VAR_RW` paths, `Z/ext/opcache/jit/zend_jit_ir.c:11953-12276`),
    /// each layout's path to a write of its own where the array may be
    /// either, as php's code merges none before it writes; to `handled`
    /// otherwise, where php's code calls the array's copy, a packed
    /// array's lookup or the warning of a key a compound write does not
    /// find.
    ///
    /// A compound write to a key the array has not got is its handler's,
    /// whose warning reaches an error handler that frees the array the reference
    /// holds, and the trace goes on with it read again through the
    /// reference, as revision 83's analysis has it (`jit-design.md` §4.2):
    ///
    /// ```php
    /// set_error_handler(function ($no, $msg) { echo $msg, "\n"; $GLOBALS['x'] = ['k' => 2]; return true; });
    /// function f(array &$a, int $n) {
    ///     $s = 0;
    ///     for ($i = 0; $i < $n; $i++) { $s += $a['k']; $a[$i + 100] += 1; $s += $a['k']; }
    ///     return $s;
    /// }
    /// $x = ['k' => 1]; echo f($x, 6), " ", json_encode($x), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// Undefined array key 100
    /// Undefined array key 101
    /// Undefined array key 102
    /// Undefined array key 103
    /// Undefined array key 104
    /// Undefined array key 105
    /// 23 {"k":2}
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// set_error_handler(function ($no, $msg) { echo $msg, "\n"; $GLOBALS['x'] = ['k' => 2]; return true; });
    /// function f(array &$a, int $n) {
    ///     $s = 0;
    ///     for ($i = 0; $i < $n; $i++) { $s += $a['k']; $a[$i + 100] += 1; $s += $a['k']; }
    ///     return $s;
    /// }
    /// $x = ['k' => 1]; echo f($x, 6), " ", json_encode($x), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// Undefined array key 100
    /// Undefined array key 101
    /// Undefined array key 102
    /// Undefined array key 103
    /// Undefined array key 104
    /// Undefined array key 105
    /// 23 {"k":2}
    /// ```
    ///
    /// A write to a shared hash is its handler's, which copies it, and a
    /// write over an object gives it up, whose destructor writes an array
    /// a reference reaches, read again at its next read:
    ///
    /// ```php
    /// class D { public function __construct(public $n) {} function __destruct() { echo "dtor {$this->n}\n"; $GLOBALS['r'] = [$this->n]; } }
    /// function f(array &$t, int $n) {
    ///     $h = ['a' => new D(0), 'b' => 0]; $s = 0;
    ///     for ($i = 0; $i < $n; $i++) { $c = $h; $h['a'] = $i; $s += $t[0]; $h['b'] = new D($i + 10); $h['b'] = $i; unset($c); }
    ///     return $s;
    /// }
    /// $r = [1]; echo f($r, 4), " ", json_encode($r), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// dtor 10
    /// dtor 0
    /// dtor 11
    /// dtor 12
    /// dtor 13
    /// 24 [13]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D { public function __construct(public $n) {} function __destruct() { echo "dtor {$this->n}\n"; $GLOBALS['r'] = [$this->n]; } }
    /// function f(array &$t, int $n) {
    ///     $h = ['a' => new D(0), 'b' => 0]; $s = 0;
    ///     for ($i = 0; $i < $n; $i++) { $c = $h; $h['a'] = $i; $s += $t[0]; $h['b'] = new D($i + 10); $h['b'] = $i; unset($c); }
    ///     return $s;
    /// }
    /// $r = [1]; echo f($r, 4), " ", json_encode($r), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// dtor 10
    /// dtor 0
    /// dtor 11
    /// dtor 12
    /// dtor 13
    /// 24 [13]
    /// ```
    pub(super) fn written_in_place(
        &mut self,
        (at, line): (OpId, &Line),
        (base, zval_at): (Value, i32),
        (reach, kernel): (Reach, Kernel),
        (handled, mut then): (
            Block,
            impl FnMut(&mut Self, Value, Value) -> Result<(), Abort>,
        ),
    ) -> Result<(), Abort> {
        let info = self.b.ins().load(I32, trusted(), base, zval_at + TYPE_AT);
        let counted = self.b.ins().band_imm_u(info, COUNTED);
        let immutable = self.b.ins().icmp_imm_s(IntCC::Equal, counted, 0);
        self.divert_if(immutable, handled);
        let arr = self.b.ins().load(I64, trusted(), base, zval_at);
        let count = self
            .b
            .ins()
            .load(I32, trusted(), arr, imm(Arr::REFCOUNT_AT));
        let shared = self.b.ins().icmp_imm_s(IntCC::NotEqual, count, 1);
        self.divert_if(shared, handled);
        let (slot, table) = match reach {
            Reach::Slot => (Some(None), None),
            Reach::Table => (None, Some(None)),
            Reach::Either => {
                let (slot, table) = (self.b.create_block(), self.b.create_block());
                let flags = self.b.ins().load(I32, trusted(), arr, imm(Arr::FLAGS_AT));
                let packed = self.b.ins().band_imm_u(flags, i64::from(Arr::PACKED));
                self.b.ins().brif(packed, slot, &[], table, &[]);
                (Some(Some(slot)), Some(Some(table)))
            }
        };
        if let Some(block) = slot {
            if let Some(block) = block {
                self.b.switch_to_block(block);
                self.b.seal_block(block);
            }
            let (element, old) = self.slot_written(at, line, arr, handled)?;
            then(self, element, old)?;
        }
        if let Some(block) = table {
            if let Some(block) = block {
                self.b.switch_to_block(block);
                self.b.seal_block(block);
            }
            let element = self.table_written(at, line, (arr, kernel), handled)?;
            let old = self.b.ins().load(I32, trusted(), element, TYPE_AT);
            then(self, element, old)?;
        }
        Ok(())
    }

    /// The slot of the packed array `arr` that `line`'s long key names,
    /// and its type word, where it is below `nNumUsed` and holds a value;
    /// to `handled` otherwise.
    fn slot_written(
        &mut self,
        at: OpId,
        line: &Line,
        arr: Value,
        handled: Block,
    ) -> Result<(Value, Value), Abort> {
        let Scalar::Long(key) = self.scalar(at, line.op2)? else {
            return Err(mismatch("an element write by no long key"));
        };
        let past = self.past_used(arr, key);
        self.divert_if(past, handled);
        let element = self.slot_at(arr, key);
        let old = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        let kind = self.b.ins().band_imm_u(old, KIND);
        let hole = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Undef));
        self.divert_if(hole, handled);
        Ok((element, old))
    }

    /// The element of `arr` `kernel` answers for `line`'s key; to
    /// `handled` where it answers none.
    fn table_written(
        &mut self,
        at: OpId,
        line: &Line,
        (arr, kernel): (Value, Kernel),
        handled: Block,
    ) -> Result<Value, Abort> {
        let place = self.looked_up(at, line, kernel, arr)?;
        Ok(self.leave_unplaced(place, handled))
    }

    /// A compound write of a number to an array's element by a long key,
    /// the array a variable's or the one the reference it holds refers
    /// to, php's `zend_jit_assign_dim_op` (`Z/ext/opcache/jit/zend_jit_ir.c:13528-13750`):
    /// in place where the array can be written in place
    /// ([`Emit::written_in_place`]) and the element is a long or a
    /// double, which php's `zend_jit_math_helper` serves inline, a long
    /// overflowing to a double as php's does; through the handler
    /// otherwise, where php's code calls its helpers (the array's copy, the
    /// hash's lookup, `zend_binary_op` over anything else), so the write
    /// takes no exit, as php's takes none where the record holds no type
    /// of the element.
    ///
    /// By-reference arrays of floats updated in a loop, as nbody updates
    /// them, an element a long that overflows, one a string, a shared
    /// array, a hole and an array turned hash each served by the handler
    /// in turn:
    ///
    /// ```php
    /// function step(array &$v, array &$w, $n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $k = $i % 4;
    ///         $v[$k] -= 0.5 * $k; $w[$k] += $k; $w[$k] *= 3;
    ///     }
    /// }
    /// $v = [1.0, 2.0, 3.0, 4.0]; $w = [0, 1, PHP_INT_MAX, 3];
    /// step($v, $w, 8);
    /// $w[1] = "7"; $copy = $v; step($v, $w, 8);
    /// unset($v[2]); step($v, $w, 4);
    /// $v["x"] = 1; step($v, $w, 4);
    /// var_dump($v, $w, $copy[1]);
    /// ```
    /// ```expect regime=jit-force process
    /// Warning: Undefined array key 2 in %s on line 5
    /// array(5) {
    ///   [0]=>
    ///   float(1)
    ///   [1]=>
    ///   float(-1)
    ///   [3]=>
    ///   float(-5)
    ///   [2]=>
    ///   float(-2)
    ///   ["x"]=>
    ///   int(1)
    /// }
    /// array(4) {
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(687)
    ///   [2]=>
    ///   float(6.723838214867132E+21)
    ///   [3]=>
    ///   int(5463)
    /// }
    /// float(1)
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function step(array &$v, array &$w, $n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $k = $i % 4;
    ///         $v[$k] -= 0.5 * $k; $w[$k] += $k; $w[$k] *= 3;
    ///     }
    /// }
    /// $v = [1.0, 2.0, 3.0, 4.0]; $w = [0, 1, PHP_INT_MAX, 3];
    /// step($v, $w, 8);
    /// $w[1] = "7"; $copy = $v; step($v, $w, 8);
    /// unset($v[2]); step($v, $w, 4);
    /// $v["x"] = 1; step($v, $w, 4);
    /// var_dump($v, $w, $copy[1]);
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// Warning: Undefined array key 2 in %s on line 5
    /// array(5) {
    ///   [0]=>
    ///   float(1)
    ///   [1]=>
    ///   float(-1)
    ///   [3]=>
    ///   float(-5)
    ///   [2]=>
    ///   float(-2)
    ///   ["x"]=>
    ///   int(1)
    /// }
    /// array(4) {
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(687)
    ///   [2]=>
    ///   float(6.723838214867132E+21)
    ///   [3]=>
    ///   int(5463)
    /// }
    /// float(1)
    /// ```
    pub(super) fn assign_dim_op(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element's compound write to no array"));
        };
        let Imm::Binary(binary) = line.imm else {
            return Err(mismatch("an ASSIGN_DIM_OP with no operator"));
        };
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let (zval, types) = self.written_array(at, array)?;
        let reach = self.reach(at, types, line)?;
        let value = match binary {
            Opcode::Concat => None,
            _ => Some(self.scalar(at, line.data)?),
        };
        let raises = !cannot_raise(binary, line.data);
        let compound = |emit: &mut Self, element, old| {
            let kind = emit.b.ins().band_imm_u(old, KIND);
            let Some(value) = value else {
                // php's `zend_jit_concat_helper` onto the element, a string.
                let string = emit
                    .b
                    .ins()
                    .icmp_imm_s(IntCC::NotEqual, kind, tag(ZvalType::String));
                emit.divert_if(string, handled);
                emit.concat_onto(line, element)?;
                emit.b.ins().jump(done, &[]);
                return Ok(());
            };
            emit.compound_at((element, kind), (binary, value, raises), (handled, done))
        };
        self.written_in_place((at, line), zval, (reach, Kernel::Find), (handled, compound))?;
        self.handled_then(at, line, (handled, done))
    }

    /// Whether the array `array`, of `types`, is read at the op at `at` as
    /// a packed one: its value's layout, or the one a guard before the op
    /// held, as php's packed guard holds what `zend_jit_fetch_reference`
    /// left of a reference (`Z/ext/opcache/jit/zend_jit_trace.c:5830-5880`).
    fn packed_at(&self, at: OpId, array: ValueId, types: Types) -> bool {
        types == Types::ARRAY.laid_out(Layout::Packed)
            || self.ir.guards.iter().any(|guard| {
                guard.at == GuardAt::Before(at)
                    && matches!(
                        guard.check,
                        Check::Layout { value, layout: Layout::Packed, .. } if value == array
                    )
            })
    }

    /// The zval that holds the array `array` names, and what is known of
    /// the array, for a write to its element at the op at `at`: the
    /// variable's own, of the layout its guard held, or the one the
    /// reference it holds refers to, of either layout, as php's
    /// `zend_jit_prepare_array_update` follows it.
    fn written_array(&mut self, at: OpId, array: ValueId) -> Result<((Value, i32), Types), Abort> {
        let (slot, types) = {
            let held = self.value(array)?;
            (held.slot, held.types)
        };
        let (room, array_at) = self.place(slot)?;
        Ok(match self.read_of(at, array) {
            Read::InPlace => ((room, array_at), types),
            Read::Through => {
                let reference = self.b.ins().load(I64, trusted(), room, array_at);
                (
                    (reference, imm(Ref::VALUE_AT)),
                    Types::of_tag(ZvalType::Array),
                )
            }
        })
    }

    /// A fetch of an array's element for writing, `FETCH_DIM_W` and
    /// `FETCH_DIM_RW`, as php's `zend_jit_fetch_dim` compiles it over an
    /// array a variable holds, or the reference it holds refers to, by a
    /// key the lookups take (`Z/ext/opcache/jit/zend_jit_ir.c:12982-13155`):
    /// the element's place, `IS_INDIRECT`, written to the fetch's `VAR`,
    /// where the write reaches it in place ([`Emit::written_in_place`]),
    /// [`Kernel::Lookup`] laying a `null` down for `_W` where the key is
    /// not there; the handler's otherwise, the array's copy, a packed
    /// array's hole and the warning `_RW` raises over a key the array has
    /// not got. What reads the `VAR` after is its handler's, as php's
    /// code calls the handler of an opline over a `VAR` its fetch left
    /// indirect.
    ///
    /// Elements of a hash and of a packed array written through a fetch,
    /// a nested array laid down, an element incremented, a key the array
    /// has not got warned of, a shared array copied and a reference:
    ///
    /// ```php
    /// function f($n) {
    ///     $h = ['a' => [], 'c' => 0, 3 => 1]; $p = [[0], [1]]; $r = 0; $h['r'] = &$r;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $h['a'][] = $i; $h[$i % 2 + 10][$i] = 1; $p[$i % 2][] = $i;
    ///         $h['c']++; $h[3]--; $h['r']++;
    ///         if ($i == 3) { $copy = $h; }
    ///         if ($i == 5) { $h['m']++; }
    ///     }
    ///     return [$h, $p, $copy['c'], $r];
    /// }
    /// echo json_encode(f(8)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// Warning: Undefined array key "m" in %s on line 8
    /// [{"a":[0,1,2,3,4,5,6,7],"c":8,"3":-7,"r":8,"10":{"0":1,"2":1,"4":1,"6":1},"11":{"1":1,"3":1,"5":1,"7":1},"m":1},[[0,0,2,4,6],[1,1,3,5,7]],4,8]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($n) {
    ///     $h = ['a' => [], 'c' => 0, 3 => 1]; $p = [[0], [1]]; $r = 0; $h['r'] = &$r;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $h['a'][] = $i; $h[$i % 2 + 10][$i] = 1; $p[$i % 2][] = $i;
    ///         $h['c']++; $h[3]--; $h['r']++;
    ///         if ($i == 3) { $copy = $h; }
    ///         if ($i == 5) { $h['m']++; }
    ///     }
    ///     return [$h, $p, $copy['c'], $r];
    /// }
    /// echo json_encode(f(8)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// Warning: Undefined array key "m" in %s on line 8
    /// [{"a":[0,1,2,3,4,5,6,7],"c":8,"3":-7,"r":8,"10":{"0":1,"2":1,"4":1,"6":1},"11":{"1":1,"3":1,"5":1,"7":1},"m":1},[[0,0,2,4,6],[1,1,3,5,7]],4,8]
    /// ```
    pub(super) fn fetch_dim_w(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element fetched for writing from no array"));
        };
        let out = line
            .out
            .ok_or_else(|| mismatch("an element fetched for writing into nothing"))?;
        let kernel = if line.opcode == Opcode::FetchDimW {
            Kernel::Lookup
        } else {
            Kernel::Find
        };
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let (zval, types) = self.written_array(at, array)?;
        let reach = self.reach(at, types, line)?;
        let out = self.value(out)?.slot;
        let placed = |emit: &mut Self, element, _| {
            let (room, out_at) = emit.place(out)?;
            let indirect = emit.b.ins().iconst(I32, tag(ZvalType::Indirect));
            emit.b.ins().store(trusted(), element, room, out_at);
            emit.b
                .ins()
                .store(trusted(), indirect, room, out_at + TYPE_AT);
            emit.b.ins().jump(done, &[]);
            Ok(())
        };
        self.written_in_place((at, line), zval, (reach, kernel), (handled, placed))?;
        self.handled_then(at, line, (handled, done))
    }

    /// The address of the slot `key` names in the packed array `arr`,
    /// leaving through `exit` where the key is past `nNumUsed`.
    pub(super) fn element(&mut self, arr: Value, key: Value, exit: ExitId) -> Result<Value, Abort> {
        let past = self.past_used(arr, key);
        self.leave_if(past, exit)?;
        Ok(self.slot_at(arr, key))
    }
}
