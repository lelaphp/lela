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
use php_heap::{Arr, ZvalType};
use php_unit::Opcode;

use super::{COUNTED, Emit, KIND, Kernel, Kind, Scalar, TYPE_AT, imm, mismatch, tag, trusted};
use crate::types::{
    Abort, Check, Constant, ExitId, Fused, GuardAt, Home, Imm, Layout, Leave, Line, OpId, Operand,
    Read, Slot, Types, ValueId, cannot_raise,
};

/// Where a read goes on that finds no element ([`Emit::reached`]): the
/// opline's exit, for the interpreter to serve it again and warn, or a
/// block of the caller's with its arguments.
#[derive(Clone, Copy)]
enum Missing<'a> {
    Exit(ExitId),
    To(Block, &'a [BlockArg]),
}

/// An element's key, read once for the slot and the lookup kernels
/// ([`Emit::key_of`]).
#[derive(Debug, Clone, Copy)]
enum Key {
    /// A long, the slot's index and the table's hash; `constant` where
    /// it is the op array's.
    Long { word: Value, constant: bool },
    /// Any other zval, by its word and type info; `constant` where it is
    /// the op array's, which a string is read as it is written.
    Zval {
        word: Value,
        info: Value,
        constant: bool,
    },
}

/// The long a packed slot is written by: [`Emit::reach`] gives a slot only
/// where the key is one.
fn long(key: Key) -> Result<Value, Abort> {
    match key {
        Key::Long { word, .. } => Ok(word),
        Key::Zval { .. } => Err(mismatch("an element write by no long key")),
    }
}

/// How a write reaches the element it writes ([`Emit::reach`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// A packed array's slot, by a long key.
    Slot,
    /// The table's lookup.
    Table,
    /// Either, by the array's layout.
    Either,
}

impl Emit<'_> {
    /// `isset()` of an array's element, as php's `zend_jit_isset_isempty_dim`
    /// compiles it over an array a variable or a temporary holds, by a
    /// long or a string key: a packed array's slot below `nNumUsed` in
    /// place, any other element through [`Kernel::Find`], the element set
    /// where it is found and, a reference followed, more than `null`
    /// (`Z_TYPE_P(value) > IS_NULL`); a temporary freed, as php's
    /// `jit_FREE_OP` frees it before the jump (`:13294`, `:13326`); the
    /// answer written, or the
    /// jump fused to it held. It leaves for nothing but the way its jump
    /// goes. [`Emit::fetch_is`]'s witnesses test an element of a
    /// temporary. A packed and a hashed array, read by longs and
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
    ///
    /// The temporary a magic `__get` made freed before the jump, its last
    /// owner, whose element's destructor writes the variable a reference
    /// reaches and a global array, which the trace reads again where it
    /// reads them next, under force and where every guard fails once
    /// (`jit-design.md` §4.2, revision 83):
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { global $g; $g = $this->n; $GLOBALS['l'][0] = $this->n; } }
    /// class M { function __isset($p) { return true; } function __get($p) { static $c = 0; return ['k' => new D(++$c)]; } }
    /// function f(&$r, $o, $n) {
    ///     global $l; $s = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r = 0; $l = [1];
    ///         if (isset($o->a['k'])) { $s += 100; }
    ///         $s += $r + $l[0];
    ///     }
    ///     return $s;
    /// }
    /// $g = 0; $l = [];
    /// echo f($g, new M, 30), " ", $g, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 3930 30
    /// ```
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { global $g; $g = $this->n; $GLOBALS['l'][0] = $this->n; } }
    /// class M { function __isset($p) { return true; } function __get($p) { static $c = 0; return ['k' => new D(++$c)]; } }
    /// function f(&$r, $o, $n) {
    ///     global $l; $s = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r = 0; $l = [1];
    ///         if (isset($o->a['k'])) { $s += 100; }
    ///         $s += $r + $l[0];
    ///     }
    ///     return $s;
    /// }
    /// $g = 0; $l = [];
    /// echo f($g, new M, 30), " ", $g, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 3930 30
    /// ```
    pub(super) fn isset_dim(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let answered = self.b.create_block();
        self.b.append_block_param(answered, I8);
        let unset = self.b.ins().iconst(I8, 0);
        let element = self.reached(at, line, Missing::To(answered, &[BlockArg::Value(unset)]))?;
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
        // `FREE_OP1`, before the jump, as php's code frees it.
        self.free_temp(line, line.op1)?;
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

    /// The element `line`'s key names in the array its variable holds,
    /// for a read, a test or a read that finds nothing silently: a packed
    /// array's slot below `nNumUsed` that holds a value, where a layout
    /// guard held the array packed and the key is a long, or the place
    /// [`Kernel::Find`] answers; where there is none, on as `missing` says.
    fn reached(&mut self, at: OpId, line: &Line, missing: Missing<'_>) -> Result<Value, Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element of no array"));
        };
        let held = self.value(array)?;
        let arr = self.array_of(held.slot, self.read_of(at, array))?;
        if self.packed_at(at, array, held.types) && self.long_at(at, line.op2)? {
            let Scalar::Long(key) = self.scalar(at, line.op2)? else {
                return Err(mismatch("an element by no long key"));
            };
            let past = self.past_used(arr, key);
            self.missing_if(past, missing)?;
            let element = self.slot_at(arr, key);
            let kind = self.b.ins().load(I8, trusted(), element, TYPE_AT);
            let hole = self
                .b
                .ins()
                .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Undef));
            self.missing_if(hole, missing)?;
            return Ok(element);
        }
        let key = self.key_of(at, line)?;
        let place = self.looked_up(line, (arr, Kernel::Find), key)?;
        let unplaced = self.unplaced(place);
        self.missing_if(unplaced, missing)?;
        Ok(self.placed_at(place))
    }

    /// On as `missing` says where `cond` is not zero, a key the array has
    /// not got; on in place otherwise.
    fn missing_if(&mut self, cond: Value, missing: Missing<'_>) -> Result<(), Abort> {
        match missing {
            Missing::Exit(exit) => self.leave_if(cond, exit),
            Missing::To(to, args) => {
                let on = self.b.create_block();
                self.b.ins().brif(cond, to, args, on, &[]);
                self.b.switch_to_block(on);
                self.b.seal_block(on);
                Ok(())
            }
        }
    }

    /// `FETCH_DIM_IS`, the read an `isset()` chain, an `empty()` or a `??`
    /// makes, as php's `zend_jit_fetch_dim_read` compiles it over an array
    /// a variable or a temporary holds under `BP_VAR_IS`: the element
    /// copied as `ZVAL_COPY_DEREF` copies it, or held to the type the next
    /// opline holds it to ([`Emit::read_element`]), and `null` where the
    /// array has not got the key, or a packed array holds a hole there,
    /// with nothing raised (`Z/ext/opcache/jit/zend_jit_ir.c:12101-12200`),
    /// leaving past the fetch through the opline's exit where its element
    /// is held, php's `not_found_exit_addr` (`:12576-12610`), the one exit
    /// `types` gives this opline; a temporary freed after, as the handler
    /// frees it, a destructor's throw unwinding.
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
    ///
    /// A temporary freed after its read, under `??` and in `isset()`'s
    /// chain, the last owner of an object whose destructor writes the
    /// variable a reference reaches and a global array, which the trace
    /// reads again where it reads them next, under force and where every
    /// guard fails once (`jit-design.md` §4.2, revision 83):
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { global $g; $g = $this->n; $GLOBALS['l'][0] = $this->n; } }
    /// function f(&$r, $n) {
    ///     global $l; $s = 0; $h = ['a' => ['b' => 1]];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r = 0; $l = [1];
    ///         $s += [new D($i), 5][1] ?? 0; $s += $r + $l[0];
    ///         $s += isset($h['a']['b']) + ($h['a']['c'] ?? 2);
    ///     }
    ///     return $s;
    /// }
    /// $g = 0; $l = [];
    /// echo f($g, 30), " ", $g, "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 1110 29
    /// ```
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { global $g; $g = $this->n; $GLOBALS['l'][0] = $this->n; } }
    /// function f(&$r, $n) {
    ///     global $l; $s = 0; $h = ['a' => ['b' => 1]];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r = 0; $l = [1];
    ///         $s += [new D($i), 5][1] ?? 0; $s += $r + $l[0];
    ///         $s += isset($h['a']['b']) + ($h['a']['c'] ?? 2);
    ///     }
    ///     return $s;
    /// }
    /// $g = 0; $l = [];
    /// echo f($g, 30), " ", $g, "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 1110 29
    /// ```
    ///
    /// An element the next fetch or test holds the type of, not found, a
    /// reference and of another type, each leaving past the fetch, the
    /// one not found with `null` in its slot, a temporary's freed where
    /// it lands (revision 114), under force and where every guard fails
    /// once:
    ///
    /// ```php
    /// function f($h, $n) {
    ///     $s = 0; $r = ['b' => 7];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if (isset($h['a']['b'])) { $s += 1; }
    ///         $s += $h['a']['b'] ?? 100;
    ///         $s += isset([$i => ['b' => 1]][$i % 2]['b']) ? 10 : 1000;
    ///         if ($i == 3) { unset($h['a']); }
    ///         if ($i == 5) { $h['a'] = &$r; }
    ///         if ($i == 7) { $h['a'] = 5; }
    ///     }
    ///     return $s;
    /// }
    /// echo f(['a' => ['b' => 2]], 12), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// 10648
    /// ```
    ///
    /// ```php
    /// function f($h, $n) {
    ///     $s = 0; $r = ['b' => 7];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         if (isset($h['a']['b'])) { $s += 1; }
    ///         $s += $h['a']['b'] ?? 100;
    ///         $s += isset([$i => ['b' => 1]][$i % 2]['b']) ? 10 : 1000;
    ///         if ($i == 3) { unset($h['a']); }
    ///         if ($i == 5) { $h['a'] = &$r; }
    ///         if ($i == 7) { $h['a'] = 5; }
    ///     }
    ///     return $s;
    /// }
    /// echo f(['a' => ['b' => 2]], 12), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// 10648
    /// ```
    pub(super) fn fetch_is(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let (missed, done) = (self.b.create_block(), self.b.create_block());
        let out = self.read_element(at, line, Missing::To(missed, &[]))?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(missed);
        self.b.seal_block(missed);
        let (room, out_at) = self.place(out)?;
        let null = self.b.ins().iconst(I32, tag(ZvalType::Null));
        self.b.ins().store(trusted(), null, room, out_at + TYPE_AT);
        // Past the fetch with the `null`, where a guard holds the element
        // to another type, as php's not-found exit leaves.
        let (to, args) = match line.exit {
            Some(exit) => self.guard_exit(exit)?,
            None => (done, Vec::new()),
        };
        self.b.ins().jump(to, &args);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        self.free_temp(line, line.op1)
    }

    /// The element a read reaches ([`Emit::reached`]), on as `missing`
    /// says where there is none, written to the slot the fetch writes:
    /// copied as `ZVAL_COPY_DEREF` copies it ([`Emit::copied_element`]),
    /// or held to its type by its one or two guards
    /// ([`Emit::held_element`]). The slot it wrote.
    fn read_element(&mut self, at: OpId, line: &Line, missing: Missing<'_>) -> Result<Slot, Abort> {
        let out = line
            .out
            .ok_or_else(|| mismatch("a fetch that writes nothing"))?;
        let element = self.reached(at, line, missing)?;
        match self.within(at).as_slice() {
            [] => self.copied_element(element, out)?,
            [in_place] => self.held_element(*in_place, None, element, out)?,
            [in_place, through] => self.held_element(*in_place, Some(*through), element, out)?,
            _ => {
                return Err(mismatch(
                    "a fetch holds other guards than its element's one or two",
                ));
            }
        }
        Ok(self.value(out)?.slot)
    }

    /// An element no guard holds, at `element`, copied through the
    /// reference it may be to the slot of `out`, as php's
    /// `zend_jit_fetch_dim_read` copies one with no `MAY_BE_GUARD`
    /// (`ZVAL_COPY_DEREF`, `Z/ext/opcache/jit/zend_jit_ir.c:12695-12704`).
    ///
    /// Its value is never one a register carries, so the slot is all it
    /// writes. php's analysis types every read of an array's element as
    /// what the array holds or `null` (`zend_array_element_type`,
    /// `Z/Zend/Optimizer/zend_inference.c:2147-2151`, `php_ssa`'s
    /// `element_type`), and `types` compiles a fetch inline only over an
    /// array, so the types of one no guard holds are one scalar only where
    /// they are `null` alone, which `types` keeps in a register that is a
    /// constant and defines nothing. A long, a double or a boolean is kept
    /// in a register only under the guard within the fetch that holds it
    /// to the type the trace recorded, which [`Emit::held_element`]
    /// defines. php comes to the same: it keeps a fetched element's
    /// register under its `MAY_BE_GUARD` too
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:3051-3062`), but gives a
    /// register to a long or a double alone (`zend_jit_var_supports_reg`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:17724-17728`), which an unguarded
    /// fetch, typed with `null` beside what the array holds, never is.
    /// Such a register here would be a fetch that wrote its slot alone,
    /// #3665's defect, so it is refused as php's `COMPILER_ERROR`.
    ///
    /// Elements of an array of `null`s, by a long key and by a string, the
    /// second through `??`, each kept in a register that is a constant:
    ///
    /// ```php
    /// function f($n) {
    ///     $z = [null, null]; $h = ['k' => null];
    ///     for ($i = 0; $i < $n; $i++) { echo $z[$i & 1] === null ? "null" : "set", " ", $h['k'] ?? $i, "\n"; }
    /// }
    /// f(3);
    /// ```
    /// ```expect regime=jit-inline process
    /// null 0
    /// null 1
    /// null 2
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:6
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %s:4
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 2 f() %s:5
    /// ---- TRACE 2 start (side trace 1/2) f() %s:5
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    /// ```
    ///
    /// The same beside the elements a guard holds, a long, a double and a
    /// boolean, each read on the compiler's thread once its trace is
    /// entered, none read wrong:
    ///
    /// ```php
    /// function f($n) {
    ///     $z = [null, null]; $h = ['k' => null]; $l = [0, 0, 0]; $d = ['x' => 0.0]; $b = [false, true]; $bad = 0;
    ///     for ($i = 1; $i < $n; $i++) {
    ///         $l[$i % 3] = $i; $d['x'] = $i * 0.5;
    ///         if ($z[$i & 1] !== null || ($h['k'] ?? -1) !== -1 || $l[$i % 3] !== $i || $d['x'] !== $i * 0.5 || $b[$i & 1] !== (($i & 1) === 1)) { $bad++; }
    ///     }
    ///     return $bad;
    /// }
    /// echo f(200000), "\n";
    /// ```
    /// ```expect regime=jit-tracing process
    /// 0
    /// ```
    fn copied_element(&mut self, element: Value, out: ValueId) -> Result<(), Abort> {
        let held = self.value(out)?;
        if held.home == Home::Register && Kind::of(held.types).is_some_and(Kind::carries) {
            return Err(mismatch(
                "an element no guard holds kept in a register that carries a value",
            ));
        }
        let element = self.deref(element);
        let info = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        self.copy(element, info, held.slot)
    }

    /// The element `key` names in the array `arr`, through `kernel`,
    /// [`Kernel::Find`] or [`Kernel::Lookup`], served at `line`, as php's code
    /// calls the table's lookup (`zend_jit_fetch_dimension_address_inner`,
    /// `Z/ext/opcache/jit/zend_jit_ir.c:11917-12511`): the place it
    /// answered, an `IS_INDIRECT` or `UNDEF`, in the kernel's answer slot.
    /// The memory limit's fatal past a lookup, which the request ends in,
    /// unwinds.
    fn looked_up(
        &mut self,
        line: &Line,
        (arr, kernel): (Value, Kernel),
        key: Key,
    ) -> Result<StackSlot, Abort> {
        let (key, info, of_unit) = match key {
            Key::Long { word, constant } => {
                let info = self.b.ins().iconst(I32, tag(ZvalType::Long));
                (word, info, constant)
            }
            Key::Zval {
                word,
                info,
                constant,
            } => (word, info, constant),
        };
        let constant = self.b.ins().iconst(I32, i64::from(of_unit));
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

    /// Whether a lookup answered no place in `slot`, `UNDEF`.
    fn unplaced(&mut self, slot: StackSlot) -> Value {
        let kind = self.b.ins().stack_load(I64, I8, slot, TYPE_AT);
        self.b
            .ins()
            .icmp_imm_s(IntCC::NotEqual, kind, tag(ZvalType::Indirect))
    }

    /// The element's address a lookup answered in `slot`.
    fn placed_at(&mut self, slot: StackSlot) -> Value {
        self.b.ins().stack_load(I64, I64, slot, 0)
    }

    /// The key `line`'s `op2` names, as the lookup kernels take it: its
    /// word, its type info and whether it is the op array's constant,
    /// which a string is read as it is written.
    fn key_of(&mut self, at: OpId, line: &Line) -> Result<Key, Abort> {
        if let Operand::Constant(Constant::Other(idx)) = line.op2 {
            let zval = self.constant_zval(line.frame, idx)?;
            let word = self.b.ins().load(I64, trusted(), zval, 0);
            let info = self.b.ins().load(I32, trusted(), zval, TYPE_AT);
            return Ok(Key::Zval {
                word,
                info,
                constant: true,
            });
        }
        if self.long_at(at, line.op2)? {
            let Scalar::Long(word) = self.scalar(at, line.op2)? else {
                return Err(mismatch("a long key read as no long"));
            };
            return Ok(Key::Long {
                word,
                constant: matches!(line.op2, Operand::Constant(_)),
            });
        }
        let Operand::Value(value) = line.op2 else {
            return Err(mismatch("a key of no value"));
        };
        let (room, key_at) = self.place(self.value(value)?.slot)?;
        let word = self.b.ins().load(I64, trusted(), room, key_at);
        let info = self.b.ins().load(I32, trusted(), room, key_at + TYPE_AT);
        Ok(Key::Zval {
            word,
            info,
            constant: false,
        })
    }

    /// The two words of the key `line`'s `op2` names, as a kernel that
    /// reads it as a zval takes them.
    pub(super) fn key_words(&mut self, at: OpId, line: &Line) -> Result<(Value, Value), Abort> {
        Ok(match self.key_of(at, line)? {
            Key::Long { word, .. } => (word, self.b.ins().iconst(I32, tag(ZvalType::Long))),
            Key::Zval { word, info, .. } => (word, info),
        })
    }

    /// A read of an array's element by a long or a string key, php's
    /// `zend_jit_fetch_dim_read` under `BP_VAR_R` as its tracing tier takes it
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:12513-12700`): a key the array has
    /// not got, past a packed array's `nNumUsed`, a hole or none the
    /// table's lookup finds, leaves for the interpreter to serve it again
    /// and warn ([`Emit::reached`]); an element that is a reference is read
    /// through it; one of the type the op's guard holds is read, and one
    /// of another copied to the slot the fetch writes as php's
    /// `ZVAL_COPY_DEREF` copies it, for the exit to go on past the fetch.
    /// A temporary array the fetch read is freed after, as its handler
    /// frees it, what a destructor throws leaving through the exit `types`
    /// gives the free.
    pub(super) fn fetch(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let exit = line
            .exit
            .ok_or_else(|| mismatch("a fetch that cannot leave"))?;
        self.read_element(at, line, Missing::Exit(exit))?;
        // `FREE_OP1`, as the interpreter takes it.
        self.free_temp(line, line.op1)
    }

    /// A write to an array's element by a long or a string key, the array
    /// a variable's, the one the reference it holds refers to or the one
    /// an indirect `VAR` points at ([`Emit::written_array`]), php's
    /// `zend_jit_assign_dim` (`Z/ext/opcache/jit/zend_jit_ir.c:13352-13526`):
    /// in place where [`Emit::written_in_place`] reaches the element, a
    /// packed slot or the one [`Kernel::Lookup`] finds or lays down, as
    /// `zend_jit_assign_to_variable` writes it
    /// (`:5793-5950`): a scalar, or a value no scalar moved from a
    /// temporary, copied from a variable with one owner more or copied
    /// from a constant ([`Emit::copy_operand`]), and what the element held
    /// given up after ([`Emit::release`]), a destructor's throw leaving
    /// through the opline's unwind; through the handler otherwise, where
    /// php's code copies the array, so the write takes no exit.
    ///
    /// An immutable array, one shared and an element that is a reference
    /// are each served by the handler, in place of the write, a key past
    /// `nNumUsed`, a hole and an array turned hash by [`Kernel::Lookup`],
    /// and a counted element given up in place, its destructor called
    /// there, and the loop goes on in the trace:
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
    ///
    /// A write to the array a reference holds is made in place through
    /// the reference, as php's `zend_jit_prepare_array_update` follows it
    /// ([`Emit::written_array`]), the array written over by the loop
    /// between, under force and where every guard fails once:
    ///
    /// ```php
    /// function f(array &$r, $n) { for ($i = 0; $i < $n; $i++) { $r['k'] = $i; $r[$i & 1] = "v$i"; if ($i == 2) { $r = ['z' => 1]; } } }
    /// $x = ['k' => 0]; $y = &$x; f($x, 6); echo json_encode([$x, $y]), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [{"z":1,"k":5,"1":"v5","0":"v4"},{"z":1,"k":5,"1":"v5","0":"v4"}]
    /// ```
    ///
    /// ```php
    /// function f(array &$r, $n) { for ($i = 0; $i < $n; $i++) { $r['k'] = $i; $r[$i & 1] = "v$i"; if ($i == 2) { $r = ['z' => 1]; } } }
    /// $x = ['k' => 0]; $y = &$x; f($x, 6); echo json_encode([$x, $y]), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [{"z":1,"k":5,"1":"v5","0":"v4"},{"z":1,"k":5,"1":"v5","0":"v4"}]
    /// ```
    pub(super) fn assign_dim(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let Operand::Value(array) = line.op1 else {
            return Err(mismatch("an element write to no array"));
        };
        let value = match self.scalar_kind(line.data)? {
            Some(_) => Some(self.scalar(at, line.data)?),
            None => None,
        };
        let handled = self.b.create_block();
        let done = self.b.create_block();
        let (zval, types) = self.written_array(at, array)?;
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
    fn reach(&self, at: OpId, types: Types, line: &Line) -> Result<Reach, Abort> {
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
    /// zval_at` holds, handed with its type word to `then`, which writes it,
    /// where php's code writes it in place: the array counted and held
    /// once, so `SEPARATE_ARRAY` has nothing to do, then, as `reach` says,
    /// a packed array's slot below `nNumUsed` that holds a value, or the
    /// element `kernel`, [`Kernel::Lookup`] for a write and
    /// [`Kernel::Find`] for a compound one, answers
    /// (`zend_jit_fetch_dimension_address_inner`'s `BP_VAR_W` and
    /// `BP_VAR_RW` paths, `Z/ext/opcache/jit/zend_jit_ir.c:11953-12276`),
    /// each layout's path to a write of its own where the array may be
    /// either, as php's code merges none before it writes, a packed
    /// array's slot a write has not got through [`Kernel::Lookup`] too, as
    /// php's code calls `zend_hash_index_lookup` for it; to `handled`
    /// otherwise, where php's code calls the array's copy or the warning
    /// of a key a compound write does not find.
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
    fn written_in_place(
        &mut self,
        (at, line): (OpId, &Line),
        (base, zval_at): (Value, i32),
        (reach, kernel): (Reach, Kernel),
        (handled, mut then): (
            Block,
            impl FnMut(&mut Self, Value, Value) -> Result<(), Abort>,
        ),
    ) -> Result<(), Abort> {
        // The key read once, before the array's tests, for both layouts'
        // paths, as php's code reads `hval` before it tests the layout.
        let key = self.key_of(at, line)?;
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
        if reach == Reach::Table {
            let (element, old) = self.table_written(line, (arr, kernel), key, handled)?;
            return then(self, element, old);
        }
        // The packed slot first, as php's code lays it, and the table's
        // arm cold, as it calls a kernel: the slot's path falls through.
        // The table's arm serves a hash where the array may be either, and
        // a write's slot the packed array has not got, past `nNumUsed` or a
        // hole, which php's code looks up with `zend_hash_index_lookup`
        // (`Z/ext/opcache/jit/zend_jit_ir.c:12261-12285`); a compound
        // write's is the handler's, which warns.
        let index = long(key)?;
        let table = (reach == Reach::Either || kernel == Kernel::Lookup).then(|| {
            let table = self.b.create_block();
            self.b.set_cold_block(table);
            table
        });
        if reach == Reach::Either {
            let (slot, table) = (self.b.create_block(), table.unwrap_or(handled));
            let flags = self.b.ins().load(I32, trusted(), arr, imm(Arr::FLAGS_AT));
            let packed = self.b.ins().band_imm_u(flags, i64::from(Arr::PACKED));
            let hashed = self.b.ins().icmp_imm_s(IntCC::Equal, packed, 0);
            self.b.ins().brif(hashed, table, &[], slot, &[]);
            self.b.switch_to_block(slot);
            self.b.seal_block(slot);
        }
        let missed = if kernel == Kernel::Lookup {
            table.unwrap_or(handled)
        } else {
            handled
        };
        let (element, old) = self.slot_written(arr, index, missed);
        then(self, element, old)?;
        let Some(table) = table else {
            return Ok(());
        };
        self.b.switch_to_block(table);
        self.b.seal_block(table);
        let (element, old) = self.table_written(line, (arr, kernel), key, handled)?;
        then(self, element, old)
    }

    /// The slot of the packed array `arr` that the long `key` names, and
    /// its type word, where it is below `nNumUsed` and holds a value; to
    /// `missed` otherwise.
    fn slot_written(&mut self, arr: Value, key: Value, missed: Block) -> (Value, Value) {
        let past = self.past_used(arr, key);
        self.divert_if(past, missed);
        let element = self.slot_at(arr, key);
        let old = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        let kind = self.b.ins().band_imm_u(old, KIND);
        let hole = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Undef));
        self.divert_if(hole, missed);
        (element, old)
    }

    /// The element of `arr` `kernel` answers for `key`, and its type
    /// word; to `handled` where it answers none.
    fn table_written(
        &mut self,
        line: &Line,
        (arr, kernel): (Value, Kernel),
        key: Key,
        handled: Block,
    ) -> Result<(Value, Value), Abort> {
        let place = self.looked_up(line, (arr, kernel), key)?;
        let unplaced = self.unplaced(place);
        self.divert_if(unplaced, handled);
        let element = self.placed_at(place);
        let old = self.b.ins().load(I32, trusted(), element, TYPE_AT);
        Ok((element, old))
    }

    /// A compound write to an array's element by a long or a string key,
    /// the array a variable's or the one the reference it holds refers
    /// to, php's `zend_jit_assign_dim_op` (`Z/ext/opcache/jit/zend_jit_ir.c:13528-13750`):
    /// in place where [`Emit::written_in_place`] reaches the element, a
    /// packed slot or the one [`Kernel::Find`] finds, and the element is
    /// of a kind the operator serves inline ([`Emit::compound_at`]), or a
    /// string `.=` writes onto through [`Kernel::AssignConcat`]; through
    /// the handler otherwise, where php's code copies the array, warns of
    /// a key the array has not got or calls `zend_binary_op` over anything
    /// else, so the write takes no exit, as php's takes none where the
    /// record holds no type of the element.
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
    /// variable's own, of the layout its guard held, the one the
    /// reference it holds refers to, as php's
    /// `zend_jit_prepare_array_update` follows it
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:5084-5096`), or the one the
    /// indirect `VAR` points at, as php's `zend_jit_fetch_indirect_var`
    /// follows it (`Z/ext/opcache/jit/zend_jit_ir.c:17087-17141`), and on
    /// through the reference it points at, as `zend_jit_fetch_reference`
    /// follows it (`:17021-17085`, #3410), each of either layout.
    ///
    /// Elements written, compounded and fetched for writing through the
    /// `VAR` a fetch for writing left indirect: an inner array copied
    /// where it is shared, an object written over and destroyed, an
    /// element turned `null`, which the guard on what the `VAR` points at
    /// leaves at, and one that is a reference, written through it,
    /// under force and where every guard fails once:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d"; } }
    /// function f($n) {
    ///     $g = ['a' => [0, 0, 0, 0], 'b' => ['x' => 0, 'y' => 1.5], 'c' => [[0, 0], [0, 0]]]; $q = [7]; $g['q'] = &$q;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $g['a'][$i & 3] = $i; $g['b']['x'] += $i; $g['b']['y'] *= 2; $g['c'][$i & 1][$i & 1] = $i; $g['q'][0] = $i;
    ///         if ($i == 5) { $copy = $g['a']; }
    ///         if ($i == 7) { $g['a'][1] = new D; }
    ///         if ($i == 9) { $g['c'][1] = null; }
    ///     }
    ///     return json_encode([$g, $q, $copy]);
    /// }
    /// echo f(14), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// d[{"a":[12,13,10,11],"b":{"x":91,"y":24576},"c":[[12,0],{"1":13}],"q":[13]},[13],[4,5,2,3]]
    /// ```
    ///
    /// ```php
    /// class D { function __destruct() { echo "d"; } }
    /// function f($n) {
    ///     $g = ['a' => [0, 0, 0, 0], 'b' => ['x' => 0, 'y' => 1.5], 'c' => [[0, 0], [0, 0]]]; $q = [7]; $g['q'] = &$q;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $g['a'][$i & 3] = $i; $g['b']['x'] += $i; $g['b']['y'] *= 2; $g['c'][$i & 1][$i & 1] = $i; $g['q'][0] = $i;
    ///         if ($i == 5) { $copy = $g['a']; }
    ///         if ($i == 7) { $g['a'][1] = new D; }
    ///         if ($i == 9) { $g['c'][1] = null; }
    ///     }
    ///     return json_encode([$g, $q, $copy]);
    /// }
    /// echo f(14), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// d[{"a":[12,13,10,11],"b":{"x":91,"y":24576},"c":[[12,0],{"1":13}],"q":[13]},[13],[4,5,2,3]]
    /// ```
    ///
    /// A write, a compound write and a fetch for writing through a `VAR`
    /// that points at a reference, read back through the variable the
    /// reference is bound to: the array copied where `$copy` shares it,
    /// and the element no reference once it is unset and written again,
    /// which the guard on the reference leaves at, under force and where
    /// every guard fails once:
    ///
    /// ```php
    /// function f($n) {
    ///     $q = [0, 0, [0, 0]]; $g = ['q' => 0]; $g['q'] = &$q; $copy = null;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $g['q'][0] = $i; $g['q'][1] += $i; $g['q'][2][$i & 1] = $i;
    ///         if ($i == 4) { $copy = $q; }
    ///         if ($i == 9) { unset($g['q']); $g['q'] = [0, 0, [0, 0]]; }
    ///     }
    ///     return json_encode([$g, $q, $copy]);
    /// }
    /// echo f(14), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [{"q":[13,46,[12,13]]},[9,45,[8,9]],[4,10,[4,3]]]
    /// ```
    ///
    /// ```php
    /// function f($n) {
    ///     $q = [0, 0, [0, 0]]; $g = ['q' => 0]; $g['q'] = &$q; $copy = null;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $g['q'][0] = $i; $g['q'][1] += $i; $g['q'][2][$i & 1] = $i;
    ///         if ($i == 4) { $copy = $q; }
    ///         if ($i == 9) { unset($g['q']); $g['q'] = [0, 0, [0, 0]]; }
    ///     }
    ///     return json_encode([$g, $q, $copy]);
    /// }
    /// echo f(14), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [{"q":[13,46,[12,13]]},[9,45,[8,9]],[4,10,[4,3]]]
    /// ```
    fn written_array(&mut self, at: OpId, array: ValueId) -> Result<((Value, i32), Types), Abort> {
        let held = self.value(array)?;
        let read = self.read_of(at, array);
        let zval = self.zval_of(held.slot, read)?;
        let types = match read {
            Read::InPlace => held.types,
            Read::Through | Read::Indirect(_) => Types::of_tag(ZvalType::Array),
        };
        Ok((zval, types))
    }

    /// A fetch of an array's element for writing, `FETCH_DIM_W` and
    /// `FETCH_DIM_RW`, as php's `zend_jit_fetch_dim` compiles it over an
    /// array a variable holds, the reference it holds refers to or the
    /// indirect `VAR` a fetch before it left points at, by a key the
    /// lookups take (`Z/ext/opcache/jit/zend_jit_ir.c:12982-13155`): the
    /// element's place, `IS_INDIRECT`, written to the fetch's `VAR`,
    /// where the write reaches it in place ([`Emit::written_in_place`]),
    /// [`Kernel::Lookup`] laying a `null` down for `_W` where the key is
    /// not there; the handler's otherwise, the array's copy, a packed
    /// array's hole `_RW` reads and the warning it raises over a key the
    /// array has not got. A write, a compound write and a fetch for
    /// writing over the `VAR` follow it ([`Emit::written_array`]); an
    /// increment over it is its handler's, as php's code calls the handler
    /// of an increment over a `VAR` its fetch left indirect.
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
}
