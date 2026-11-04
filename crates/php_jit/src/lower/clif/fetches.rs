//! The reads php's trace compiles in line that the step served here: a
//! double's truth with the warning of a NAN in a cold arm
//! (`zend_jit_bool_jmpznz`, `Z/ext/opcache/jit/zend_jit_ir.c:7750-7768`),
//! an array's count (`zend_jit_count`, `:11671-11713`), a string's offset
//! (`zend_jit_fetch_dim_read`, `:12720-12755`), a constant
//! (`zend_jit_fetch_constant`, `:8109-8177`) and a `static` property
//! (`zend_jit_fetch_static_prop`, `:16080-16225`).

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{InstBuilder, Value};
use php_engine::tier::{PLACE, PlaceLayout};
use php_heap::{Arr, ZvalType};

use super::dims::Held;
use super::{Emit, KIND, Kernel, Refuse, Scalar, TYPE_AT, imm, mismatch, tag, trusted};
use crate::types::{Abort, Leave, Line, OpId, Operand};

/// The runtime-cache slots' layout, as the fetches read them.
const P: PlaceLayout = PLACE;

impl Emit<'_> {
    /// The truth of `held`, as php's `zend_jit_bool_jmpznz` reads it: a
    /// double's with its warning of a NAN in a cold arm, the frame stood
    /// at the opline there alone, what an error handler throws leaving
    /// through the opline's unwind as the handler leaves.
    ///
    /// A double read by each of the six oplines, zero, a negative zero, a
    /// NAN, an infinity and a value, the warning raised at each:
    ///
    /// ```php
    /// function f($a, $n) {
    ///     $c = 0; $s = "";
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $d = $a[$i % 5] * 1.0;
    ///         if ($d) { $c++; }
    ///         while (!$d) { $c += 10; break; }
    ///         $x = $d && $i; $y = $d || $i; $z = (bool) $d; $w = !$d;
    ///         $s .= (int) $x . (int) $y . (int) $z . (int) $w . " ";
    ///     }
    ///     return [$c, $s];
    /// }
    /// echo json_encode(f([1.5, 0.0, -0.0, NAN, INF], 5)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// Warning: unexpected NAN value was coerced to bool in %s on line 6
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 7
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 8
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 8
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 8
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 8
    /// [23,"0110 0101 0101 1110 1110 "]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// function f($a, $n) {
    ///     $c = 0; $s = "";
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $d = $a[$i % 5] * 1.0;
    ///         if ($d) { $c++; }
    ///         while (!$d) { $c += 10; break; }
    ///         $x = $d && $i; $y = $d || $i; $z = (bool) $d; $w = !$d;
    ///         $s .= (int) $x . (int) $y . (int) $z . (int) $w . " ";
    ///     }
    ///     return [$c, $s];
    /// }
    /// echo json_encode(f([1.5, 0.0, -0.0, NAN, INF], 5)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// Warning: unexpected NAN value was coerced to bool in %s on line 6
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 7
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 8
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 8
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 8
    ///
    /// Warning: unexpected NAN value was coerced to bool in %s on line 8
    /// [23,"0110 0101 0101 1110 1110 "]
    /// ```
    ///
    /// A loop of doubles, a double's truth in the trace itself, the jump
    /// its direction guard holds, where php's trace takes the same exits:
    ///
    /// ```php
    /// function k($n) {
    ///     $c = 0; $f = 3.0;
    ///     for ($i = 1; $i < $n; $i++) {
    ///         $f = $f - 1.0;
    ///         if ($f) { $c += 1; } else { $f = 3.0; }
    ///     }
    ///     return $c;
    /// }
    /// echo k(100), "\n";
    /// ```
    /// ```expect regime=jit-inline process
    /// 66
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:10
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) k() %s:4
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 4 k() %s:6
    /// ---- TRACE 2 start (side trace 1/4) k() %s:6
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 3 k() %s:8
    /// ---- TRACE 3 start (side trace 1/3) k() %s:8
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    pub(super) fn warned_truth(&mut self, line: &Line, held: Scalar) -> Result<Value, Abort> {
        if let Scalar::Double(x) = held {
            let nan = self.b.ins().fcmp(FloatCC::Unordered, x, x);
            let warns = self.b.create_block();
            self.b.set_cold_block(warns);
            let on = self.b.create_block();
            self.b.ins().brif(nan, warns, &[], on, &[]);
            self.b.switch_to_block(warns);
            self.b.seal_block(warns);
            let [room, opline] = self.served(line)?;
            let leave = line.unwind.map(Leave::Unwind);
            self.call(Kernel::NanToBool, &[room, opline], leave)?;
            self.b.ins().jump(on, &[]);
            self.b.switch_to_block(on);
            self.b.seal_block(on);
        }
        Ok(self.truth(held))
    }

    /// The first word of what `line`'s `op1` holds, read as the guards
    /// before the op at `at` read it, a constant's where it names one.
    fn held_word(&mut self, at: OpId, line: &Line) -> Result<Value, Abort> {
        match line.op1 {
            Operand::Value(value) => {
                let slot = self.value(value)?.slot;
                self.array_of(slot, self.read_of(at, value))
            }
            _ => Ok(self.zval_words(line.frame, line.op1)?.0),
        }
    }

    /// `count()` of an array, as php's `zend_jit_count`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:11671-11713`): `nNumOfElements`
    /// read in place, what a temporary held given up after it.
    ///
    /// An array of each layout, a constant one, one read through a
    /// reference and a temporary whose last element's destructor runs at
    /// the count:
    ///
    /// ```php
    /// class D { function __destruct() { echo "freed\n"; } }
    /// function made($i) { return $i == 3 ? [new D] : [1, 2]; }
    /// function f($n) {
    ///     $p = [1, 2, 3]; $h = ["a" => 1, "b" => 2]; $r = [1]; $q = &$r; $t = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r[] = $i;
    ///         $t += count($p) + count($h) * 10 + count($q) * 100 + count([1, 2, 3, 4]) * 1000;
    ///         $t += count(made($i));
    ///     }
    ///     return $t;
    /// }
    /// echo f(5), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// freed
    /// 22124
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    /// ```php
    /// class D { function __destruct() { echo "freed\n"; } }
    /// function made($i) { return $i == 3 ? [new D] : [1, 2]; }
    /// function f($n) {
    ///     $p = [1, 2, 3]; $h = ["a" => 1, "b" => 2]; $r = [1]; $q = &$r; $t = 0;
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $r[] = $i;
    ///         $t += count($p) + count($h) * 10 + count($q) * 100 + count([1, 2, 3, 4]) * 1000;
    ///         $t += count(made($i));
    ///     }
    ///     return $t;
    /// }
    /// echo f(5), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// freed
    /// 22124
    /// ```
    ///
    /// A loop whose condition counts, its exit php's, and a destructor
    /// that throws out of a temporary the count gives up, caught in the
    /// frame that counted:
    ///
    /// ```php
    /// function k($a) { $t = 0; for ($i = 0; $i < count($a); $i++) { $t += $a[$i]; } return $t; }
    /// echo k(range(1, 200)), "\n";
    /// ```
    /// ```expect regime=jit-inline
    /// 20100
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:3
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) k() %s:2
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 4 k() %s:2
    /// ---- TRACE 2 start (side trace 1/4) k() %s:2
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    /// ```
    ///
    /// ```php
    /// class T { function __destruct() { throw new Exception("from the count"); } }
    /// function f($n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         try { $c = count([new T]); echo "past\n"; }
    ///         catch (Exception $e) { echo $e->getMessage(), "\n"; }
    ///     }
    /// }
    /// f(3);
    /// ```
    /// ```expect regime=jit-force process
    /// from the count
    /// from the count
    /// from the count
    /// ```
    pub(super) fn count(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let array = self.held_word(at, line)?;
        let count = self
            .b
            .ins()
            .uload32(trusted(), array, imm(Arr::ELEMENTS_AT));
        self.free_temp(line, line.op1)?;
        if let Some(out) = line.out {
            self.define(out, Scalar::Long(count))?;
        }
        Ok(())
    }

    /// A string's offset, as php's `zend_jit_fetch_dim_read` reads one
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:12720-12755`): the frame stood at
    /// the opline and [`Kernel::StrOffset`], php's two functions; the key
    /// then the string given up where a temporary held them, a throw
    /// tested after, and the byte into what the opline writes.
    ///
    /// By a long, from the end, by a constant and a variable numeric
    /// string, through a reference, of a temporary and of an element:
    ///
    ///
    /// ```php
    /// function f($s, $n) {
    ///     $r = &$s; $t = ""; $d = ["ab", "cd"]; $k = ["0", "1"];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $t .= $s[$i] . $s[-1 - $i] . $s["1"] . $r[$i] . ($s . $i)[$i];
    ///         $t .= $d[$i & 1][$i & 1] . $s[$k[$i & 1]] . $s[$k[$i & 1] . ""];
    ///     }
    ///     return $t;
    /// }
    /// echo f("abcd", 4), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// adbaaaaabcbbbdbbcbbccaaadabdddbb
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    ///
    /// ```php
    /// function f($s, $n) {
    ///     $r = &$s; $t = ""; $d = ["ab", "cd"]; $k = ["0", "1"];
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $t .= $s[$i] . $s[-1 - $i] . $s["1"] . $r[$i] . ($s . $i)[$i];
    ///         $t .= $d[$i & 1][$i & 1] . $s[$k[$i & 1]] . $s[$k[$i & 1] . ""];
    ///     }
    ///     return $t;
    /// }
    /// echo f("abcd", 4), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// adbaaaaabcbbbdbbcbbccaaadabdddbb
    /// ```
    ///
    /// The loop php's trace reads a byte a pass in, its exits php's:
    ///
    /// ```php
    /// function k($n, $s) {
    ///     $c = 0;
    ///     for ($i = 1; $i < $n; $i++) { if ($s[$i & 7] === 'a') { $c++; } }
    ///     return $c;
    /// }
    /// echo k(200, str_repeat('abcdefgh', 2)), "\n";
    /// ```
    /// ```expect regime=jit-inline
    /// 24
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:7
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) k() %s:4
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 4 k() %s:4
    /// ---- TRACE 2 start (side trace 1/4) k() %s:4
    /// ---- TRACE 2 stop (link to 1)
    /// ---- TRACE 2 compiled
    ///
    ///      TRACE 1 exit 3 k() %s:5
    /// ---- TRACE 3 start (side trace 1/3) k() %s:5
    /// ---- TRACE 3 stop (return)
    /// ---- TRACE 3 compiled
    /// ```
    ///
    /// Past the end, an offset's warning at its opline, a handler of it
    /// that throws, and a key refused, each caught where it was read:
    ///
    /// ```php
    /// function f($s, $n) {
    ///     $o = "";
    ///     for ($i = 0; $i < $n; $i++) {
    ///         try { $o .= "[" . $s[$i] . "]"; } catch (Exception $e) { $o .= $e->getMessage(); }
    ///     }
    ///     return $o;
    /// }
    /// function g($s, $k, $n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         try { echo $s[$k], "\n"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    ///     }
    /// }
    /// echo f("ab", 4), "\n";
    /// set_error_handler(function ($n, $m) { throw new Exception($m); });
    /// echo f("ab", 4), "\n";
    /// g("ab", "x", 3);
    /// ```
    /// ```expect regime=jit-force process
    /// Warning: Uninitialized string offset 2 in %s on line 5
    ///
    /// Warning: Uninitialized string offset 3 in %s on line 5
    /// [a][b][][]
    /// [a][b]Uninitialized string offset 2Uninitialized string offset 3
    /// Cannot access offset of type string on string
    /// Cannot access offset of type string on string
    /// Cannot access offset of type string on string
    /// ```
    pub(super) fn str_offset(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let text = self.held_word(at, line)?;
        let (key, key_info) = self.key_words(at, line)?;
        self.save_opline(line)?;
        let [room, opline] = self.served(line)?;
        let slot = self.answer();
        let answer = self.b.ins().stack_addr(I64, slot, 0);
        let status = self.call_status(
            Kernel::StrOffset,
            &[room, opline, text, key, key_info, answer],
            line.unwind.map(Leave::Unwind),
        )?;
        self.free_temp(line, line.op2)?;
        self.free_temp(line, line.op1)?;
        self.left_unless(status, line.unwind.map(Leave::Unwind))?;
        let out = line
            .out
            .ok_or_else(|| mismatch("a string's offset that writes nothing"))?;
        let (base, at) = self.place(self.value(out)?.slot)?;
        let byte = self.b.ins().stack_load(I64, I64, slot, 0);
        let info = self.b.ins().stack_load(I64, I32, slot, TYPE_AT);
        self.b.ins().store(trusted(), byte, base, at);
        self.b.ins().store(trusted(), info, base, at + TYPE_AT);
        Ok(())
    }

    /// A constant, as php's `zend_jit_fetch_constant`
    /// (`Z/ext/opcache/jit/zend_jit_ir.c:8109-8177`): its value read off
    /// the slot of the request's runtime cache where the slot holds one,
    /// through [`Kernel::GetConstant`] in a cold arm where it does not,
    /// the look-up php's `zend_jit_get_constant` makes; either way held
    /// to the type a guard within the fetch holds it to, and copied.
    ///
    /// Of each kind, counted and not, php's, the host's and the program's,
    /// by a namespaced name and one that falls back, and in a closure,
    /// whose frame reads a cache of its own:
    ///
    ///
    /// ```php
    /// namespace N { const OWN = "own"; function f($n) {
    ///     define("LATE", [1, 2]); define("S", str_repeat("s", 3));
    ///     $o = []; $c = function () { return OWN . PHP_INT_SIZE; };
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $x = E_ALL; $y = OWN; $z = LATE; $z[] = $i; $w = S . "!"; $v = PHP_SAPI === "cli";
    ///         $o[] = [$x, $y, count($z), $w, $v, \strlen(PHP_EOL), $c()];
    ///     }
    ///     return $o[$n - 1];
    /// } echo json_encode(f(4)), "\n"; }
    /// ```
    /// ```expect regime=jit-force process
    /// [30719,"own",3,"sss!",true,1,"own8"]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    ///
    /// ```php
    /// namespace N { const OWN = "own"; function f($n) {
    ///     define("LATE", [1, 2]); define("S", str_repeat("s", 3));
    ///     $o = []; $c = function () { return OWN . PHP_INT_SIZE; };
    ///     for ($i = 0; $i < $n; $i++) {
    ///         $x = E_ALL; $y = OWN; $z = LATE; $z[] = $i; $w = S . "!"; $v = PHP_SAPI === "cli";
    ///         $o[] = [$x, $y, count($z), $w, $v, \strlen(PHP_EOL), $c()];
    ///     }
    ///     return $o[$n - 1];
    /// } echo json_encode(f(4)), "\n"; }
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [30719,"own",3,"sss!",true,1,"own8"]
    /// ```
    ///
    /// The loop php's trace reads a constant a pass in, its exits php's:
    ///
    /// ```php
    /// define('TEST', null);
    /// function k($n) { for ($i = 0; $i < $n; ++$i) { $x = TEST; } return $i; }
    /// echo k(200), "\n";
    /// ```
    /// ```expect regime=jit-inline
    /// 200
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:2
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) k() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 2 k() %s:3
    /// ---- TRACE 2 start (side trace 1/2) k() %s:3
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    /// ```
    ///
    /// A name undefined until a pass defines it, whose `Error` leaves
    /// through the opline's unwind, and a deprecated constant, which no
    /// slot keeps, its deprecation raised every pass and a handler's throw
    /// caught where it was read:
    ///
    /// ```php
    /// #[\Deprecated] const OLD = 1;
    /// function f($n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         try { echo NOPE, "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         if ($i == 1) { define("NOPE", "now"); }
    ///     }
    /// }
    /// function g($n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         try { echo OLD, "\n"; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    ///     }
    /// }
    /// f(4); g(2);
    /// set_error_handler(function ($n, $m) { throw new Exception($m); });
    /// g(2);
    /// ```
    /// ```expect regime=jit-force process
    /// Undefined constant "NOPE"
    /// Undefined constant "NOPE"
    /// now
    /// now
    ///
    /// Deprecated: Constant OLD is deprecated in %s on line 11
    /// 1
    ///
    /// Deprecated: Constant OLD is deprecated in %s on line 11
    /// 1
    /// Constant OLD is deprecated
    /// Constant OLD is deprecated
    /// ```
    pub(super) fn fetch_constant(&mut self, at: OpId, line: &Line) -> Result<(), Abort> {
        let out = line
            .out
            .ok_or_else(|| mismatch("a constant's fetch that writes nothing"))?;
        let missed = self.b.create_block();
        self.b.set_cold_block(missed);
        let read = self.b.create_block();
        let element = self.b.append_block_param(read, I64);
        let place = self.cached_slot(line, P.constant(), Refuse::To(missed))?;
        let value = (self.b.ins()).iadd_imm_s(place, i64::from(P.constant_value()));
        self.b.ins().jump(read, &[value.into()]);
        self.b.switch_to_block(missed);
        self.b.seal_block(missed);
        let func = self.func_of(line.frame)?;
        let [room, opline] = self.served(line)?;
        let slot = self.answer();
        let answer = self.b.ins().stack_addr(I64, slot, 0);
        self.call(
            Kernel::GetConstant,
            &[room, opline, func, answer],
            line.unwind.map(Leave::Unwind),
        )?;
        self.b.ins().jump(read, &[answer.into()]);
        self.b.switch_to_block(read);
        self.b.seal_block(read);
        match self.within(at).as_slice() {
            [] => self.copied_element(element, out),
            [held] => self.held_element(*held, None, Held::Set(element), out),
            _ => Err(mismatch("a constant's fetch holds guards of another kind")),
        }
    }

    /// A `static` property's read, as php's `zend_jit_fetch_static_prop`
    /// under `BP_VAR_R` (`Z/ext/opcache/jit/zend_jit_ir.c:16080-16225`):
    /// the place the slot of the request's runtime cache keeps, and what
    /// it holds copied through the reference it may be, `ZVAL_COPY_DEREF`;
    /// a slot that keeps no place, and a place nothing has written, cold,
    /// to the handler, which looks the name up, primes the slot and throws
    /// where php's `zend_fetch_static_property` and
    /// `zend_jit_uninit_static_prop` do.
    ///
    /// By `self`, `parent` and a name, untyped and typed, counted and not,
    /// and one bound by reference:
    ///
    ///
    /// ```php
    /// class P { static $p = 10; protected static $q = [1]; }
    /// class C extends P {
    ///     static $a = 0; static int $t = 3; static ?string $s = null; static $r = 1;
    ///     static function f($n) {
    ///         $x = &self::$r; $o = [];
    ///         for ($i = 0; $i < $n; $i++) {
    ///             self::$a += 2; $x++;
    ///             $o[] = [self::$a, self::$t, self::$s, self::$r, parent::$p, P::$p, count(self::$q), C::$t];
    ///             self::$s = "s" . $i;
    ///         }
    ///         return $o[$n - 1];
    ///     }
    /// }
    /// echo json_encode(C::f(4)), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// [8,3,"s2",5,10,10,1,3]
    /// ```
    ///
    /// The same where every guard fails once:
    ///
    ///
    /// ```php
    /// class P { static $p = 10; protected static $q = [1]; }
    /// class C extends P {
    ///     static $a = 0; static int $t = 3; static ?string $s = null; static $r = 1;
    ///     static function f($n) {
    ///         $x = &self::$r; $o = [];
    ///         for ($i = 0; $i < $n; $i++) {
    ///             self::$a += 2; $x++;
    ///             $o[] = [self::$a, self::$t, self::$s, self::$r, parent::$p, P::$p, count(self::$q), C::$t];
    ///             self::$s = "s" . $i;
    ///         }
    ///         return $o[$n - 1];
    ///     }
    /// }
    /// echo json_encode(C::f(4)), "\n";
    /// ```
    /// ```expect regime=jit-exit-fuzz process
    /// [8,3,"s2",5,10,10,1,3]
    /// ```
    ///
    /// The loop php's trace reads a property a pass in, its exits php's:
    ///
    /// ```php
    /// class Foo {
    ///     static $a = 0;
    ///     static function k($n) { for ($i = 0; $i < $n; ++$i) { $x = self::$a; } return $i; }
    /// }
    /// echo Foo::k(200), "\n";
    /// ```
    /// ```expect regime=jit-inline
    /// 200
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:6
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) Foo::k() %s:4
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 2 Foo::k() %s:4
    /// ---- TRACE 2 start (side trace 1/2) Foo::k() %s:4
    /// ---- TRACE 2 stop (return)
    /// ---- TRACE 2 compiled
    /// ```
    ///
    /// A typed property read before it is written, php's `Error` at the
    /// opline, a closure's read in the scope it was bound to and a trait's
    /// method, which reads its class's:
    ///
    /// ```php
    /// class U { static int $t; static function f($n) {
    ///     for ($i = 0; $i < $n; $i++) {
    ///         try { echo self::$t, "\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    ///         if ($i == 1) { self::$t = 7; }
    ///     }
    /// } }
    /// U::f(4);
    /// trait R { static $v = 1; static function f($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += self::$v; } return $t; } }
    /// class A { use R; } class B { use R; }
    /// B::$v = 5;
    /// echo A::f(3), " ", B::f(3), "\n";
    /// class X { private static $w = 2; } class Y { private static $w = 3; }
    /// $c = function ($n) { $t = 0; for ($i = 0; $i < $n; $i++) { $t += self::$w; } return $t; };
    /// echo Closure::bind($c, null, X::class)(3), " ", Closure::bind($c, null, Y::class)(3), "\n";
    /// ```
    /// ```expect regime=jit-force process
    /// Typed static property U::$t must not be accessed before initialization
    /// Typed static property U::$t must not be accessed before initialization
    /// 7
    /// 7
    /// 3 15
    /// 6 9
    /// ```
    pub(super) fn fetch_static(&mut self, line: &Line) -> Result<(), Abort> {
        let out = line
            .out
            .ok_or_else(|| mismatch("a static property's fetch that writes nothing"))?;
        let slot = self.value(out)?.slot;
        let handled = self.b.create_block();
        self.b.set_cold_block(handled);
        let done = self.b.create_block();
        let cached = self.cached_slot(line, P.static_property(), Refuse::To(handled))?;
        let place = self
            .b
            .ins()
            .load(I64, trusted(), cached, imm(P.static_place() as usize));
        let info = self.b.ins().load(I32, trusted(), place, TYPE_AT);
        let kind = self.b.ins().band_imm_u(info, KIND);
        let unwritten = self
            .b
            .ins()
            .icmp_imm_s(IntCC::Equal, kind, tag(ZvalType::Undef));
        self.divert_if(unwritten, handled);
        let held = self.deref(place);
        let info = self.b.ins().load(I32, trusted(), held, TYPE_AT);
        self.copy(held, info, slot)?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(handled);
        self.b.seal_block(handled);
        let func = self.func_of(line.frame)?;
        let [room, opline] = self.served(line)?;
        self.call(
            Kernel::Handle,
            &[room, opline, func],
            line.unwind.map(Leave::Unwind),
        )?;
        self.b.ins().jump(done, &[]);
        self.b.switch_to_block(done);
        self.b.seal_block(done);
        Ok(())
    }
}
