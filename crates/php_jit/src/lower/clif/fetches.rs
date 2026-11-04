//! The reads php's trace compiles in line that the step served here: a
//! double's truth with the warning of a NAN in a cold arm
//! (`zend_jit_bool_jmpznz`, `Z/ext/opcache/jit/zend_jit_ir.c:7750-7768`)
//! and an array's count (`zend_jit_count`, `:11671-11713`).

use cranelift_codegen::ir::condcodes::FloatCC;
use cranelift_codegen::ir::{InstBuilder, Value};
use php_heap::Arr;

use super::{Emit, Kernel, Scalar, imm, trusted};
use crate::types::{Abort, Leave, Line, OpId, Operand};

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
}
