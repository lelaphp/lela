//! `zend_ssa_inference` as php's tracing tier runs it, at an optimization
//! level of 0 (`zend_jit_op_array_analyze2`): the CVs that may hold a
//! reference (`zend_mark_cv_references`), the ranges of the integers by
//! SCC with warmup, widening and narrowing (`zend_infer_ranges`), the
//! types to their fixed point (`zend_infer_types_ex`) and what the
//! function returns (`zend_func_return_info`). `zend_type_narrowing`
//! never runs: the tier masks `ZEND_OPTIMIZER_NARROW_TO_DOUBLE` off.

use php_heap::Zval;
use php_unit::{OpArray, Opcode, OperandKind};

use crate::bits::{Bits, Passes};
use crate::calls::Calls;
use crate::cfg::Cfg;
use crate::range::{self, add_overflows};
use crate::ssa::{Alias, Constraint, PhiKind, Ssa, SsaOp, defined_at};
use crate::types::{ClassOf, Range, Returns, Types, VarInfo, of_alias, of_constant};
use crate::{Function, LinkedTable, SsaVar};

/// `RANGE_WARMUP_PASSES`.
const WARMUP_PASSES: usize = 16;

/// `ZEND_ARRAY_ELEMENT_REF`: an `extended_value` bit.
pub(crate) const ELEMENT_REF: u32 = 1;

/// `ZEND_BIND_REF`: an `extended_value` bit, the same as
/// [`ELEMENT_REF`].
pub(crate) const BIND_REF: u32 = 1;

/// Inference narrowed a type, which php reports as its own bug and
/// abandons the analysis over (`emit_type_narrowing_warning`).
#[derive(Debug)]
pub(crate) struct Narrowed;

/// What inference reads while it runs, and what it has learned so far.
pub(crate) struct Infer<'a, T: LinkedTable> {
    pub(crate) function: &'a Function<'a>,
    pub(crate) body: &'a OpArray,
    pub(crate) cfg: &'a Cfg,
    pub(crate) ssa: &'a Ssa<T::Class>,
    pub(crate) table: &'a T,
    /// The calls php maps where it reads the calls the op array makes,
    /// `func_info->call_map`; none where it maps none.
    pub(crate) calls: Option<&'a Calls<'a, T::Class>>,
    pub(crate) info: Vec<VarInfo<T::Class>>,
    /// Where each SSA op's opline stands, php's `ssa_opcodes`: none for
    /// an op array's own SSA, whose op `at` is its opline `at`; a
    /// trace's for a trace's SSA, whose ops follow the trace
    /// ([`crate::trace`]), where no worklist is kept.
    pub(crate) positions: Option<&'a [crate::trace::Position]>,
}

/// `zend_ssa_inference` over `ssa`, then `zend_func_return_info` into
/// `returns`; none where php's fails.
///
/// A counted loop: the loop variable's range found by widening and narrowing
/// over its SCC, the sum a long that may overflow into a double, and the
/// return's types and range the union of what reaches each `RETURN`:
///
/// ```php
/// function sum($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
/// echo sum(20), "\n";
/// ```
/// ```expect regime=jit-ssa process capture
/// 190
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) sum() %SCRATCH%/witness.php:2
/// ---- TRACE 1 stop (loop)
///
/// sum:
///      ; (lines=10, args=1, vars=3, tmps=5, ssa_vars=14)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:2-2
///      ; return  [long, double] RANGE[0..++]
///      ; #0.CV0($n) NOVAL [undef]
///      ; #1.CV1($s) NOVAL [undef]
///      ; #2.CV2($i) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #3.CV0($n) [rc1, rcn, any] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-3]
///      ; from=(BB0)
///      ; to=(BB3)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB3)
/// 0001 ASSIGN #1.CV1($s) NOVAL [undef] -> #4.CV1($s) [long] RANGE[0..0] int(0)
/// 0002 ASSIGN #2.CV2($i) NOVAL [undef] -> #5.CV2($i) [long] RANGE[0..0] int(0)
/// 0003 JMP BB3
///
/// BB2:
///      ; target lines=[4-5]
///      ; from=(BB3)
///      ; to=(BB3)
///      ; idom=BB3
///      ; level=3
///      ; loop_header=3
///      #10.CV0($n) [rc1, rcn, any] RANGE[1..++] = Pi<BB3>(#6.CV0($n) [rc1, rcn, any] & RANGE[#8.CV2($i) [long, double] RANGE[0..++] + 1 .. ++])
///      #11.CV2($i) [long, double] RANGE[0..++] = Pi<BB3>(#8.CV2($i) [long, double] RANGE[0..++] & RANGE[-- .. #6.CV0($n) [rc1, rcn, any] - 1])
/// 0004 ASSIGN_OP (ADD) #7.CV1($s) [long, double] RANGE[0..++] -> #12.CV1($s) [long, double] RANGE[0..++] #11.CV2($i) [long, double] RANGE[0..++]
/// 0005 PRE_INC #11.CV2($i) [long, double] RANGE[0..++] -> #13.CV2($i) [long, double] RANGE[1..++]
///
/// BB3:
///      ; follow target loop_header lines=[6-7]
///      ; from=(BB1, BB2)
///      ; to=(BB2, BB4)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB2, BB4)
///      #6.CV0($n) [rc1, rcn, any] = Phi(#3.CV0($n) [rc1, rcn, any], #10.CV0($n) [rc1, rcn, any] RANGE[1..++])
///      #7.CV1($s) [long, double] RANGE[0..++] = Phi(#4.CV1($s) [long] RANGE[0..0], #12.CV1($s) [long, double] RANGE[0..++])
///      #8.CV2($i) [long, double] RANGE[0..++] = Phi(#5.CV2($i) [long] RANGE[0..0], #13.CV2($i) [long, double] RANGE[1..++])
/// 0006 #9.T7 [bool] = IS_SMALLER #8.CV2($i) [long, double] RANGE[0..++] #6.CV0($n) [rc1, rcn, any]
/// 0007 JMPNZ #9.T7 [bool] BB2
///
/// BB4:
///      ; follow exit lines=[8-8]
///      ; from=(BB3)
///      ; idom=BB3
///      ; level=3
/// 0008 RETURN #7.CV1($s) [long, double] RANGE[0..++]
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 3 sum() %SCRATCH%/witness.php:2
/// ---- TRACE 2 start (side trace 1/3) sum() %SCRATCH%/witness.php:2
/// ---- TRACE 2 stop (return)
///
/// sum:
///      ; (lines=10, args=1, vars=3, tmps=5, ssa_vars=14)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:2-2
///      ; return  [long, double] RANGE[0..++]
///      ; #0.CV0($n) NOVAL [undef]
///      ; #1.CV1($s) NOVAL [undef]
///      ; #2.CV2($i) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #3.CV0($n) [rc1, rcn, any] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-3]
///      ; from=(BB0)
///      ; to=(BB3)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB3)
/// 0001 ASSIGN #1.CV1($s) NOVAL [undef] -> #4.CV1($s) [long] RANGE[0..0] int(0)
/// 0002 ASSIGN #2.CV2($i) NOVAL [undef] -> #5.CV2($i) [long] RANGE[0..0] int(0)
/// 0003 JMP BB3
///
/// BB2:
///      ; target lines=[4-5]
///      ; from=(BB3)
///      ; to=(BB3)
///      ; idom=BB3
///      ; level=3
///      ; loop_header=3
///      #10.CV0($n) [rc1, rcn, any] RANGE[1..++] = Pi<BB3>(#6.CV0($n) [rc1, rcn, any] & RANGE[#8.CV2($i) [long, double] RANGE[0..++] + 1 .. ++])
///      #11.CV2($i) [long, double] RANGE[0..++] = Pi<BB3>(#8.CV2($i) [long, double] RANGE[0..++] & RANGE[-- .. #6.CV0($n) [rc1, rcn, any] - 1])
/// 0004 ASSIGN_OP (ADD) #7.CV1($s) [long, double] RANGE[0..++] -> #12.CV1($s) [long, double] RANGE[0..++] #11.CV2($i) [long, double] RANGE[0..++]
/// 0005 PRE_INC #11.CV2($i) [long, double] RANGE[0..++] -> #13.CV2($i) [long, double] RANGE[1..++]
///
/// BB3:
///      ; follow target loop_header lines=[6-7]
///      ; from=(BB1, BB2)
///      ; to=(BB2, BB4)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB2, BB4)
///      #6.CV0($n) [rc1, rcn, any] = Phi(#3.CV0($n) [rc1, rcn, any], #10.CV0($n) [rc1, rcn, any] RANGE[1..++])
///      #7.CV1($s) [long, double] RANGE[0..++] = Phi(#4.CV1($s) [long] RANGE[0..0], #12.CV1($s) [long, double] RANGE[0..++])
///      #8.CV2($i) [long, double] RANGE[0..++] = Phi(#5.CV2($i) [long] RANGE[0..0], #13.CV2($i) [long, double] RANGE[1..++])
/// 0006 #9.T7 [bool] = IS_SMALLER #8.CV2($i) [long, double] RANGE[0..++] #6.CV0($n) [rc1, rcn, any]
/// 0007 JMPNZ #9.T7 [bool] BB2
///
/// BB4:
///      ; follow exit lines=[8-8]
///      ; from=(BB3)
///      ; idom=BB3
///      ; level=3
/// 0008 RETURN #7.CV1($s) [long, double] RANGE[0..++]
/// ---- TRACE 2 compiled
/// ```
///
/// Where a loop's variables each hold one type, php's trace guards fewer
/// of them than the tier's yet does (#3575), so the numbers of the exits
/// differ; these witnesses ask for the SSA dump alone
/// (`ZEND_JIT_DEBUG_SSA`), which each trace's build writes. A loop over
/// a declared `int` that shifts it down: the pi of `$n > 0` bounds it
/// below by one, the shift halves its range, and the count is a long that
/// the declared return keeps:
///
/// ```php
/// ini_set("opcache.jit_debug", 2);
/// function down(int $n): int { $c = 0; while ($n > 0) { $n >>= 1; $c++; } return $c; }
/// echo down(100), "\n";
/// ```
/// ```expect regime=jit-ssa process capture
/// 7
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:2
///
/// down:
///      ; (lines=11, args=1, vars=2, tmps=4, ssa_vars=11)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:3-3
///      ; return  [long]
///      ; #0.CV0($n) NOVAL [undef]
///      ; #1.CV1($c) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #2.CV0($n) [long] RANGE[MIN..MAX] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-2]
///      ; from=(BB0)
///      ; to=(BB3)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB3)
/// 0001 ASSIGN #1.CV1($c) NOVAL [undef] -> #3.CV1($c) [long] RANGE[0..0] int(0)
/// 0002 JMP BB3
///
/// BB2:
///      ; target lines=[3-4]
///      ; from=(BB3)
///      ; to=(BB3)
///      ; idom=BB3
///      ; level=3
///      ; loop_header=3
///      #7.CV0($n) [long] RANGE[1..MAX] = Pi<BB3>(#4.CV0($n) [long] RANGE[MIN..MAX] & RANGE[1 .. ++])
/// 0003 ASSIGN_OP (SR) #7.CV0($n) [long] RANGE[1..MAX] -> #8.CV0($n) [long] RANGE[0..4611686018427387903] int(1)
/// 0004 PRE_INC #5.CV1($c) [long, double] RANGE[0..++] -> #9.CV1($c) [long, double] RANGE[1..++]
///
/// BB3:
///      ; follow target loop_header lines=[5-6]
///      ; from=(BB1, BB2)
///      ; to=(BB2, BB4)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB2, BB4)
///      #4.CV0($n) [long] RANGE[MIN..MAX] = Phi(#2.CV0($n) [long] RANGE[MIN..MAX], #8.CV0($n) [long] RANGE[0..4611686018427387903])
///      #5.CV1($c) [long, double] RANGE[0..++] = Phi(#3.CV1($c) [long] RANGE[0..0], #9.CV1($c) [long, double] RANGE[1..++])
/// 0005 #6.T5 [bool] = IS_SMALLER int(0) #4.CV0($n) [long] RANGE[MIN..MAX]
/// 0006 JMPNZ #6.T5 [bool] BB2
///
/// BB4:
///      ; follow exit lines=[7-8]
///      ; from=(BB3)
///      ; idom=BB3
///      ; level=3
/// 0007 VERIFY_RETURN_TYPE #5.CV1($c) [long, double] RANGE[0..++] -> #10.CV1($c) [long]
/// 0008 RETURN #10.CV1($c) [long]
///
/// down:
///      ; (lines=11, args=1, vars=2, tmps=4, ssa_vars=11)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:3-3
///      ; return  [long]
///      ; #0.CV0($n) NOVAL [undef]
///      ; #1.CV1($c) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #2.CV0($n) [long] RANGE[MIN..MAX] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-2]
///      ; from=(BB0)
///      ; to=(BB3)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB3)
/// 0001 ASSIGN #1.CV1($c) NOVAL [undef] -> #3.CV1($c) [long] RANGE[0..0] int(0)
/// 0002 JMP BB3
///
/// BB2:
///      ; target lines=[3-4]
///      ; from=(BB3)
///      ; to=(BB3)
///      ; idom=BB3
///      ; level=3
///      ; loop_header=3
///      #7.CV0($n) [long] RANGE[1..MAX] = Pi<BB3>(#4.CV0($n) [long] RANGE[MIN..MAX] & RANGE[1 .. ++])
/// 0003 ASSIGN_OP (SR) #7.CV0($n) [long] RANGE[1..MAX] -> #8.CV0($n) [long] RANGE[0..4611686018427387903] int(1)
/// 0004 PRE_INC #5.CV1($c) [long, double] RANGE[0..++] -> #9.CV1($c) [long, double] RANGE[1..++]
///
/// BB3:
///      ; follow target loop_header lines=[5-6]
///      ; from=(BB1, BB2)
///      ; to=(BB2, BB4)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB2, BB4)
///      #4.CV0($n) [long] RANGE[MIN..MAX] = Phi(#2.CV0($n) [long] RANGE[MIN..MAX], #8.CV0($n) [long] RANGE[0..4611686018427387903])
///      #5.CV1($c) [long, double] RANGE[0..++] = Phi(#3.CV1($c) [long] RANGE[0..0], #9.CV1($c) [long, double] RANGE[1..++])
/// 0005 #6.T5 [bool] = IS_SMALLER int(0) #4.CV0($n) [long] RANGE[MIN..MAX]
/// 0006 JMPNZ #6.T5 [bool] BB2
///
/// BB4:
///      ; follow exit lines=[7-8]
///      ; from=(BB3)
///      ; idom=BB3
///      ; level=3
/// 0007 VERIFY_RETURN_TYPE #5.CV1($c) [long, double] RANGE[0..++] -> #10.CV1($c) [long]
/// 0008 RETURN #10.CV1($c) [long]
/// ```
///
/// The string a `.=` grows is counted once, and the return is the
/// declared `string`:
///
/// ```php
/// ini_set("opcache.jit_debug", 2);
/// function pad($n): string { $s = ""; for ($i = 0; $i < $n; $i++) { $s .= "x"; } return $s; }
/// echo pad(5), "\n";
/// ```
/// ```expect regime=jit-ssa process capture
/// xxxxx
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:2
///
/// pad:
///      ; (lines=12, args=1, vars=3, tmps=5, ssa_vars=15)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:3-3
///      ; return  [rc1, rcn, string]
///      ; #0.CV0($n) NOVAL [undef]
///      ; #1.CV1($s) NOVAL [undef]
///      ; #2.CV2($i) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #3.CV0($n) [rc1, rcn, any] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-3]
///      ; from=(BB0)
///      ; to=(BB3)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB3)
/// 0001 ASSIGN #1.CV1($s) NOVAL [undef] -> #4.CV1($s) [rc1, rcn, string] string("")
/// 0002 ASSIGN #2.CV2($i) NOVAL [undef] -> #5.CV2($i) [long] RANGE[0..0] int(0)
/// 0003 JMP BB3
///
/// BB2:
///      ; target lines=[4-5]
///      ; from=(BB3)
///      ; to=(BB3)
///      ; idom=BB3
///      ; level=3
///      ; loop_header=3
///      #10.CV0($n) [rc1, rcn, any] RANGE[1..++] = Pi<BB3>(#6.CV0($n) [rc1, rcn, any] & RANGE[#8.CV2($i) [long, double] RANGE[0..++] + 1 .. ++])
///      #11.CV2($i) [long, double] RANGE[0..++] = Pi<BB3>(#8.CV2($i) [long, double] RANGE[0..++] & RANGE[-- .. #6.CV0($n) [rc1, rcn, any] - 1])
/// 0004 ASSIGN_OP (CONCAT) #7.CV1($s) [rc1, rcn, string] -> #12.CV1($s) [rc1, rcn, string] string("x")
/// 0005 PRE_INC #11.CV2($i) [long, double] RANGE[0..++] -> #13.CV2($i) [long, double] RANGE[1..++]
///
/// BB3:
///      ; follow target loop_header lines=[6-7]
///      ; from=(BB1, BB2)
///      ; to=(BB2, BB4)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB2, BB4)
///      #6.CV0($n) [rc1, rcn, any] = Phi(#3.CV0($n) [rc1, rcn, any], #10.CV0($n) [rc1, rcn, any] RANGE[1..++])
///      #7.CV1($s) [rc1, rcn, string] = Phi(#4.CV1($s) [rc1, rcn, string], #12.CV1($s) [rc1, rcn, string])
///      #8.CV2($i) [long, double] RANGE[0..++] = Phi(#5.CV2($i) [long] RANGE[0..0], #13.CV2($i) [long, double] RANGE[1..++])
/// 0006 #9.T7 [bool] = IS_SMALLER #8.CV2($i) [long, double] RANGE[0..++] #6.CV0($n) [rc1, rcn, any]
/// 0007 JMPNZ #9.T7 [bool] BB2
///
/// BB4:
///      ; follow exit lines=[8-9]
///      ; from=(BB3)
///      ; idom=BB3
///      ; level=3
/// 0008 VERIFY_RETURN_TYPE #7.CV1($s) [rc1, rcn, string] -> #14.CV1($s) [rc1, rcn, string]
/// 0009 RETURN #14.CV1($s) [rc1, rcn, string]
///
/// pad:
///      ; (lines=12, args=1, vars=3, tmps=5, ssa_vars=15)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:3-3
///      ; return  [rc1, rcn, string]
///      ; #0.CV0($n) NOVAL [undef]
///      ; #1.CV1($s) NOVAL [undef]
///      ; #2.CV2($i) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #3.CV0($n) [rc1, rcn, any] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-3]
///      ; from=(BB0)
///      ; to=(BB3)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB3)
/// 0001 ASSIGN #1.CV1($s) NOVAL [undef] -> #4.CV1($s) [rc1, rcn, string] string("")
/// 0002 ASSIGN #2.CV2($i) NOVAL [undef] -> #5.CV2($i) [long] RANGE[0..0] int(0)
/// 0003 JMP BB3
///
/// BB2:
///      ; target lines=[4-5]
///      ; from=(BB3)
///      ; to=(BB3)
///      ; idom=BB3
///      ; level=3
///      ; loop_header=3
///      #10.CV0($n) [rc1, rcn, any] RANGE[1..++] = Pi<BB3>(#6.CV0($n) [rc1, rcn, any] & RANGE[#8.CV2($i) [long, double] RANGE[0..++] + 1 .. ++])
///      #11.CV2($i) [long, double] RANGE[0..++] = Pi<BB3>(#8.CV2($i) [long, double] RANGE[0..++] & RANGE[-- .. #6.CV0($n) [rc1, rcn, any] - 1])
/// 0004 ASSIGN_OP (CONCAT) #7.CV1($s) [rc1, rcn, string] -> #12.CV1($s) [rc1, rcn, string] string("x")
/// 0005 PRE_INC #11.CV2($i) [long, double] RANGE[0..++] -> #13.CV2($i) [long, double] RANGE[1..++]
///
/// BB3:
///      ; follow target loop_header lines=[6-7]
///      ; from=(BB1, BB2)
///      ; to=(BB2, BB4)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB2, BB4)
///      #6.CV0($n) [rc1, rcn, any] = Phi(#3.CV0($n) [rc1, rcn, any], #10.CV0($n) [rc1, rcn, any] RANGE[1..++])
///      #7.CV1($s) [rc1, rcn, string] = Phi(#4.CV1($s) [rc1, rcn, string], #12.CV1($s) [rc1, rcn, string])
///      #8.CV2($i) [long, double] RANGE[0..++] = Phi(#5.CV2($i) [long] RANGE[0..0], #13.CV2($i) [long, double] RANGE[1..++])
/// 0006 #9.T7 [bool] = IS_SMALLER #8.CV2($i) [long, double] RANGE[0..++] #6.CV0($n) [rc1, rcn, any]
/// 0007 JMPNZ #9.T7 [bool] BB2
///
/// BB4:
///      ; follow exit lines=[8-9]
///      ; from=(BB3)
///      ; idom=BB3
///      ; level=3
/// 0008 VERIFY_RETURN_TYPE #7.CV1($s) [rc1, rcn, string] -> #14.CV1($s) [rc1, rcn, string]
/// 0009 RETURN #14.CV1($s) [rc1, rcn, string]
/// ```
///
/// An array a loop appends to, packed and counted once, its elements the
/// longs or doubles a product leaves:
///
/// ```php
/// ini_set("opcache.jit_debug", 2);
/// function fill(int $n) { $a = []; for ($i = 0; $i < $n; $i++) { $a[] = $i * 2; } return $a; }
/// echo count(fill(4)), "\n";
/// ```
/// ```expect regime=jit-ssa process capture
/// 4
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:2
///
/// fill:
///      ; (lines=12, args=1, vars=3, tmps=6, ssa_vars=15)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:3-3
///      ; return  [rc1, rcn, [empty, packed, hash] array [long] of [long, double]]
///      ; #0.CV0($n) NOVAL [undef]
///      ; #1.CV1($a) NOVAL [undef]
///      ; #2.CV2($i) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #3.CV0($n) [long] RANGE[MIN..MAX] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-3]
///      ; from=(BB0)
///      ; to=(BB3)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB3)
/// 0001 ASSIGN #1.CV1($a) NOVAL [undef] -> #4.CV1($a) [rc1, rcn, empty array] array(...)
/// 0002 ASSIGN #2.CV2($i) NOVAL [undef] -> #5.CV2($i) [long] RANGE[0..0] int(0)
/// 0003 JMP BB3
///
/// BB2:
///      ; target lines=[4-7]
///      ; from=(BB3)
///      ; to=(BB3)
///      ; idom=BB3
///      ; level=3
///      ; loop_header=3
///      #10.CV0($n) [long] RANGE[MIN..MAX] = Pi<BB3>(#6.CV0($n) [long] RANGE[MIN..MAX] & RANGE[#8.CV2($i) [long] RANGE[0..MAX] + 1 .. ++])
///      #11.CV2($i) [long] RANGE[0..9223372036854775806] = Pi<BB3>(#8.CV2($i) [long] RANGE[0..MAX] & RANGE[-- .. #6.CV0($n) [long] RANGE[MIN..MAX] - 1])
/// 0004 #12.T6 [long, double] = MUL #11.CV2($i) [long] RANGE[0..9223372036854775806] int(2)
/// 0005 ASSIGN_DIM #7.CV1($a) [rc1, rcn, [empty, packed, hash] array [long] of [long, double]] -> #13.CV1($a) [rc1, [empty, packed, hash] array [long] of [long, double]] NEXT
/// 0006 OP_DATA #12.T6 [long, double]
/// 0007 PRE_INC #11.CV2($i) [long] RANGE[0..9223372036854775806] -> #14.CV2($i) [long] RANGE[1..MAX]
///
/// BB3:
///      ; follow target loop_header lines=[8-9]
///      ; from=(BB1, BB2)
///      ; to=(BB2, BB4)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB2, BB4)
///      #6.CV0($n) [long] RANGE[MIN..MAX] = Phi(#3.CV0($n) [long] RANGE[MIN..MAX], #10.CV0($n) [long] RANGE[MIN..MAX])
///      #7.CV1($a) [rc1, rcn, [empty, packed, hash] array [long] of [long, double]] = Phi(#4.CV1($a) [rc1, rcn, empty array], #13.CV1($a) [rc1, [empty, packed, hash] array [long] of [long, double]])
///      #8.CV2($i) [long] RANGE[0..MAX] = Phi(#5.CV2($i) [long] RANGE[0..0], #14.CV2($i) [long] RANGE[1..MAX])
/// 0008 #9.T8 [bool] = IS_SMALLER #8.CV2($i) [long] RANGE[0..MAX] #6.CV0($n) [long] RANGE[MIN..MAX]
/// 0009 JMPNZ #9.T8 [bool] BB2
///
/// BB4:
///      ; follow exit lines=[10-10]
///      ; from=(BB3)
///      ; idom=BB3
///      ; level=3
/// 0010 RETURN #7.CV1($a) [rc1, rcn, [empty, packed, hash] array [long] of [long, double]]
///
/// fill:
///      ; (lines=12, args=1, vars=3, tmps=6, ssa_vars=15)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:3-3
///      ; return  [rc1, rcn, [empty, packed, hash] array [long] of [long, double]]
///      ; #0.CV0($n) NOVAL [undef]
///      ; #1.CV1($a) NOVAL [undef]
///      ; #2.CV2($i) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #3.CV0($n) [long] RANGE[MIN..MAX] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-3]
///      ; from=(BB0)
///      ; to=(BB3)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB3)
/// 0001 ASSIGN #1.CV1($a) NOVAL [undef] -> #4.CV1($a) [rc1, rcn, empty array] array(...)
/// 0002 ASSIGN #2.CV2($i) NOVAL [undef] -> #5.CV2($i) [long] RANGE[0..0] int(0)
/// 0003 JMP BB3
///
/// BB2:
///      ; target lines=[4-7]
///      ; from=(BB3)
///      ; to=(BB3)
///      ; idom=BB3
///      ; level=3
///      ; loop_header=3
///      #10.CV0($n) [long] RANGE[MIN..MAX] = Pi<BB3>(#6.CV0($n) [long] RANGE[MIN..MAX] & RANGE[#8.CV2($i) [long] RANGE[0..MAX] + 1 .. ++])
///      #11.CV2($i) [long] RANGE[0..9223372036854775806] = Pi<BB3>(#8.CV2($i) [long] RANGE[0..MAX] & RANGE[-- .. #6.CV0($n) [long] RANGE[MIN..MAX] - 1])
/// 0004 #12.T6 [long, double] = MUL #11.CV2($i) [long] RANGE[0..9223372036854775806] int(2)
/// 0005 ASSIGN_DIM #7.CV1($a) [rc1, rcn, [empty, packed, hash] array [long] of [long, double]] -> #13.CV1($a) [rc1, [empty, packed, hash] array [long] of [long, double]] NEXT
/// 0006 OP_DATA #12.T6 [long, double]
/// 0007 PRE_INC #11.CV2($i) [long] RANGE[0..9223372036854775806] -> #14.CV2($i) [long] RANGE[1..MAX]
///
/// BB3:
///      ; follow target loop_header lines=[8-9]
///      ; from=(BB1, BB2)
///      ; to=(BB2, BB4)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB2, BB4)
///      #6.CV0($n) [long] RANGE[MIN..MAX] = Phi(#3.CV0($n) [long] RANGE[MIN..MAX], #10.CV0($n) [long] RANGE[MIN..MAX])
///      #7.CV1($a) [rc1, rcn, [empty, packed, hash] array [long] of [long, double]] = Phi(#4.CV1($a) [rc1, rcn, empty array], #13.CV1($a) [rc1, [empty, packed, hash] array [long] of [long, double]])
///      #8.CV2($i) [long] RANGE[0..MAX] = Phi(#5.CV2($i) [long] RANGE[0..0], #14.CV2($i) [long] RANGE[1..MAX])
/// 0008 #9.T8 [bool] = IS_SMALLER #8.CV2($i) [long] RANGE[0..MAX] #6.CV0($n) [long] RANGE[MIN..MAX]
/// 0009 JMPNZ #9.T8 [bool] BB2
///
/// BB4:
///      ; follow exit lines=[10-10]
///      ; from=(BB3)
///      ; idom=BB3
///      ; level=3
/// 0010 RETURN #7.CV1($a) [rc1, rcn, [empty, packed, hash] array [long] of [long, double]]
/// ```
///
/// A `foreach` over an untyped parameter, its key and value anything,
/// and a property of the scope, which its declared `int` types a long
/// counted nowhere, as the scope lays its objects down with php's
/// standard handlers and no `__get`:
///
/// ```php
/// class Acc { public int $t = 0; function add($a) { foreach ($a as $k => $v) { $this->t += $k * 2; } return $this->t; } }
/// $o = new Acc;
/// echo $o->add([5, 6, 7]), "\n";
/// ```
/// ```expect regime=jit-ssa process capture
/// 6
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) Acc::add() %SCRATCH%/witness.php:2
/// ---- TRACE 1 stop (loop)
///
/// Acc::add:
///      ; (lines=12, args=1, vars=3, tmps=6, ssa_vars=13)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:2-2
///      ; return  [long]
///      ; #0.CV0($a) NOVAL [undef]
///      ; #1.CV1($v) NOVAL [undef]
///      ; #2.CV2($k) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #3.CV0($a) [rc1, rcn, any] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-1]
///      ; from=(BB0)
///      ; to=(BB4, BB2)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB2, BB4)
/// 0001 #5.V3 [undef, rc1, rcn, array of [any, ref], object] = FE_RESET_R #3.CV0($a) [rc1, rcn, any] -> #4.CV0($a) NOVAL [rc1, rcn, any] BB4
///
/// BB2:
///      ; follow target loop_header lines=[2-2]
///      ; from=(BB1, BB3)
///      ; to=(BB4, BB3)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB3)
///      #6.CV1($v) NOVAL [undef, rc1, rcn, any] = Phi(#1.CV1($v) NOVAL [undef], #8.CV1($v) NOVAL [rc1, rcn, any])
///      #7.CV2($k) NOVAL [undef, rc1, rcn, any] = Phi(#2.CV2($k) NOVAL [undef], #10.CV2($k) [rc1, rcn, any])
/// 0002 #9.T4 [rc1, rcn, any] = FE_FETCH_R #5.V3 [undef, rc1, rcn, array of [any, ref], object] #6.CV1($v) NOVAL [undef, rc1, rcn, any] -> #8.CV1($v) NOVAL [rc1, rcn, any] BB4
///
/// BB3:
///      ; follow lines=[3-7]
///      ; from=(BB2)
///      ; to=(BB2)
///      ; idom=BB2
///      ; level=3
///      ; loop_header=2
/// 0003 ASSIGN #7.CV2($k) NOVAL [undef, rc1, rcn, any] -> #10.CV2($k) [rc1, rcn, any] #9.T4 [rc1, rcn, any]
/// 0004 #11.T7 [rc1, false, long, double, object] = MUL #10.CV2($k) [rc1, rcn, any] int(2)
/// 0005 ASSIGN_OBJ_OP (ADD) THIS string("t")
/// 0006 OP_DATA #11.T7 [rc1, false, long, double, object]
/// 0007 JMP BB2
///
/// BB4:
///      ; target exit lines=[8-10]
///      ; from=(BB1, BB2)
///      ; idom=BB1
///      ; level=2
/// 0008 FE_FREE #5.V3 [undef, rc1, rcn, array of [any, ref], object]
/// 0009 #12.T8 [long] = FETCH_OBJ_R THIS string("t")
/// 0010 RETURN #12.T8 [long]
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 2 Acc::add() %SCRATCH%/witness.php:2
/// ---- TRACE 2 start (side trace 1/2) Acc::add() %SCRATCH%/witness.php:2
/// ---- TRACE 2 stop (return)
///
/// Acc::add:
///      ; (lines=12, args=1, vars=3, tmps=6, ssa_vars=13)
///      ; (JIT)
///      ; %SCRATCH%/witness.php:2-2
///      ; return  [long]
///      ; #0.CV0($a) NOVAL [undef]
///      ; #1.CV1($v) NOVAL [undef]
///      ; #2.CV2($k) NOVAL [undef]
/// BB0:
///      ; start lines=[0-0]
///      ; to=(BB1)
///      ; level=0
///      ; children=(BB1)
/// 0000 #3.CV0($a) [rc1, rcn, any] = RECV 1
///
/// BB1:
///      ; recv follow entry lines=[1-1]
///      ; from=(BB0)
///      ; to=(BB4, BB2)
///      ; idom=BB0
///      ; level=1
///      ; children=(BB2, BB4)
/// 0001 #5.V3 [undef, rc1, rcn, array of [any, ref], object] = FE_RESET_R #3.CV0($a) [rc1, rcn, any] -> #4.CV0($a) NOVAL [rc1, rcn, any] BB4
///
/// BB2:
///      ; follow target loop_header lines=[2-2]
///      ; from=(BB1, BB3)
///      ; to=(BB4, BB3)
///      ; idom=BB1
///      ; level=2
///      ; children=(BB3)
///      #6.CV1($v) NOVAL [undef, rc1, rcn, any] = Phi(#1.CV1($v) NOVAL [undef], #8.CV1($v) NOVAL [rc1, rcn, any])
///      #7.CV2($k) NOVAL [undef, rc1, rcn, any] = Phi(#2.CV2($k) NOVAL [undef], #10.CV2($k) [rc1, rcn, any])
/// 0002 #9.T4 [rc1, rcn, any] = FE_FETCH_R #5.V3 [undef, rc1, rcn, array of [any, ref], object] #6.CV1($v) NOVAL [undef, rc1, rcn, any] -> #8.CV1($v) NOVAL [rc1, rcn, any] BB4
///
/// BB3:
///      ; follow lines=[3-7]
///      ; from=(BB2)
///      ; to=(BB2)
///      ; idom=BB2
///      ; level=3
///      ; loop_header=2
/// 0003 ASSIGN #7.CV2($k) NOVAL [undef, rc1, rcn, any] -> #10.CV2($k) [rc1, rcn, any] #9.T4 [rc1, rcn, any]
/// 0004 #11.T7 [rc1, false, long, double, object] = MUL #10.CV2($k) [rc1, rcn, any] int(2)
/// 0005 ASSIGN_OBJ_OP (ADD) THIS string("t")
/// 0006 OP_DATA #11.T7 [rc1, false, long, double, object]
/// 0007 JMP BB2
///
/// BB4:
///      ; target exit lines=[8-10]
///      ; from=(BB1, BB2)
///      ; idom=BB1
///      ; level=2
/// 0008 FE_FREE #5.V3 [undef, rc1, rcn, array of [any, ref], object]
/// 0009 #12.T8 [long] = FETCH_OBJ_R THIS string("t")
/// 0010 RETURN #12.T8 [long]
/// ---- TRACE 2 compiled
/// ```
pub(crate) fn infer<T: LinkedTable>(
    function: &Function<'_>,
    cfg: &Cfg,
    ssa: &mut Ssa<T::Class>,
    table: &T,
    calls: Option<&Calls<'_, T::Class>>,
    returns: &mut Returns<T::Class>,
) -> Option<()> {
    let body = function.body;
    let cvs = body.num_cvs as usize;
    let info = ssa
        .vars
        .iter()
        .enumerate()
        .map(|(i, var)| {
            let mut types = Types::NONE;
            if i < cvs {
                types = Types::UNDEF;
                if var.alias != Alias::None {
                    types |= of_alias(var.alias);
                }
            }
            VarInfo {
                types,
                class: None,
                range: None,
            }
        })
        .collect();
    let mut cx = Infer {
        function,
        body,
        cfg,
        ssa: &*ssa,
        table,
        calls,
        info,
        positions: None,
    };
    cx.mark_cv_references();
    cx.infer_ranges();
    let mut worklist = Bits::new(cx.info.len());
    for j in cvs..cx.info.len() {
        worklist.incl(j);
    }
    cx.infer_types(&mut worklist).ok()?;
    cx.return_info(returns);
    let info = cx.info;
    ssa.info = info;
    Some(())
}

impl<T: LinkedTable> Infer<'_, T> {
    pub(crate) fn literal(&self, n: u32) -> Option<&Zval> {
        self.body.literals.get(n as usize)
    }

    pub(crate) fn op(&self, at: usize) -> SsaOp {
        self.ssa.ops.get(at).copied().unwrap_or_default()
    }

    /// The opline of the op array whose SSA op `at` is: its own at `at`,
    /// or a trace's at the place `ssa_opcodes[at]` names.
    pub(crate) fn opline(&self, at: usize) -> Option<&php_unit::Opline> {
        self.body.oplines.get(self.index(at)?)
    }

    /// The opline's place in its op array of SSA op `at`.
    pub(crate) fn index(&self, at: usize) -> Option<usize> {
        match self.positions {
            None => Some(at),
            Some(positions) => positions.get(at).map(|p| p.opline as usize),
        }
    }

    /// Whether SSA op `next`'s opline is the one after SSA op `at`'s in
    /// the same op array, php's `opline + 1 == ssa_opcodes[next]`.
    pub(crate) fn follows(&self, at: usize, next: usize) -> bool {
        match self.positions {
            None => next == at + 1,
            Some(positions) => match (positions.get(at), positions.get(next)) {
                (Some(a), Some(n)) => n.array == a.array && n.opline == a.opline + 1,
                _ => false,
            },
        }
    }

    /// `get_ssa_var_info`: everything for no variable.
    pub(crate) fn var_types(&self, var: Option<SsaVar>) -> Types {
        var.and_then(|v| self.info.get(v.index()))
            .map_or(Types::UNDEF | Types::REF_ANY | Types::INDIRECT, |info| {
                info.types
            })
    }

    pub(crate) fn var_info(&self, var: Option<SsaVar>) -> Option<&VarInfo<T::Class>> {
        var.and_then(|v| self.info.get(v.index()))
    }

    /// `OP1_INFO()` and its kin: a constant's types, or the variable's
    /// the opline reads there.
    pub(crate) fn operand_types(
        &self,
        kind: OperandKind,
        operand: u32,
        var: Option<SsaVar>,
    ) -> Types {
        if kind == OperandKind::Const {
            self.literal(operand).map_or(Types::NONE, of_constant)
        } else {
            self.var_types(var)
        }
    }

    /// `OP1_HAS_RANGE()` and its kin: a long constant's value, or the
    /// variable's range where it has one.
    pub(crate) fn operand_range(
        &self,
        kind: OperandKind,
        operand: u32,
        var: Option<SsaVar>,
    ) -> Option<Range> {
        match kind {
            OperandKind::Const => self.literal(operand)?.as_long().map(|n| Range {
                min: n,
                max: n,
                underflow: false,
                overflow: false,
            }),
            OperandKind::Unused => None,
            _ => self.var_info(var)?.range,
        }
    }

    fn has_ref(&self, var: usize) -> bool {
        self.info.get(var).is_some_and(|i| i.types.has(Types::REF))
    }

    /// `FOR_EACH_VAR_USAGE`: the variables defined where `var` is read,
    /// by an opline or a phi.
    pub(crate) fn usages(&self, var: SsaVar) -> Vec<SsaVar> {
        let mut all: Vec<SsaVar> = self
            .ssa
            .uses(var)
            .flat_map(|at| defined_at(self.ssa, self.body, at as usize))
            .collect();
        all.extend(
            self.ssa
                .phi_uses(var)
                .filter_map(|p| self.ssa.phi(p).map(|p| p.ssa_var)),
        );
        all
    }

    /// `zend_mark_cv_references`: a CV whose definition makes a
    /// reference, and every variable a reference flows into, may be one.
    fn mark_cv_references(&mut self) {
        let body = self.body;
        let cvs = body.num_cvs;
        let n = self.info.len();
        let mut worklist = Bits::new(n);
        for var in 0..n {
            let Some(v) = self.ssa.vars.get(var) else {
                continue;
            };
            let id = SsaVar::of(var);
            if let Some(def) = v.definition
                && v.var.0 < cvs
            {
                let at = def as usize;
                let Some(op) = body.oplines.get(at) else {
                    continue;
                };
                let ssa_op = self.op(at);
                let makes = if ssa_op.out_def == Some(id) {
                    matches!(op.opcode, Opcode::Recv | Opcode::RecvInit)
                        && self
                            .parameter(op.op1)
                            .is_some_and(|p| p.by_ref || p.prefer_ref)
                } else if ssa_op.op1_def == Some(id) {
                    match op.opcode {
                        Opcode::AssignRef
                        | Opcode::MakeRef
                        | Opcode::FeResetRw
                        | Opcode::BindGlobal
                        | Opcode::SendRef
                        | Opcode::SendVarEx
                        | Opcode::SendFuncArg
                        | Opcode::BindInitStaticOrJmp => true,
                        Opcode::InitArray | Opcode::AddArrayElement => {
                            op.extended_value & ELEMENT_REF != 0
                        }
                        Opcode::BindStatic => op.extended_value & BIND_REF != 0,
                        Opcode::Yield => self.function.decl.is_some_and(|d| d.returns_by_ref),
                        Opcode::OpData => at
                            .checked_sub(1)
                            .and_then(|p| body.oplines.get(p))
                            .is_some_and(|prev| {
                                matches!(
                                    prev.opcode,
                                    Opcode::AssignObjRef | Opcode::AssignStaticPropRef
                                )
                            }),
                        _ => false,
                    }
                } else if ssa_op.op2_def == Some(id) {
                    match op.opcode {
                        Opcode::AssignRef | Opcode::FeFetchRw => true,
                        Opcode::BindLexical => op.extended_value & BIND_REF != 0,
                        _ => false,
                    }
                } else {
                    false
                };
                if makes {
                    worklist.incl(var);
                }
            } else if self.has_ref(var) || v.alias == Alias::Symtable {
                worklist.incl(var);
            }
        }
        let mut passes = Passes::new();
        while let Some(var) = passes.next(&mut worklist) {
            if let Some(info) = self.info.get_mut(var) {
                info.types |= Types::REF_ANY;
            }
            let id = SsaVar::of(var);
            for phi in self.ssa.phi_uses(id) {
                if let Some(p) = self.ssa.phi(phi)
                    && !self.has_ref(p.ssa_var.index())
                {
                    worklist.incl(p.ssa_var.index());
                }
            }
            for at in self.ssa.uses(id) {
                let op = self.op(at as usize);
                if op.op1_use == Some(id)
                    && let Some(def) = op.op1_def
                    && !self.has_ref(def.index())
                {
                    // Unset breaks references (outside global scope).
                    let unset = body
                        .oplines
                        .get(at as usize)
                        .is_some_and(|o| o.opcode == Opcode::UnsetCv);
                    if unset && self.function.name().is_some() {
                        continue;
                    }
                    worklist.incl(def.index());
                }
                if op.op2_use == Some(id)
                    && let Some(def) = op.op2_def
                    && !self.has_ref(def.index())
                {
                    worklist.incl(def.index());
                }
                if op.out_use == Some(id)
                    && let Some(def) = op.out_def
                    && !self.has_ref(def.index())
                {
                    worklist.incl(def.index());
                }
            }
        }
    }

    /// `op_array->arg_info[num - 1]`, the parameter a `RECV` of `num`
    /// receives.
    pub(crate) fn parameter(&self, num: u32) -> Option<&php_unit::decl::Parameter> {
        let at = (num as usize).checked_sub(1)?;
        self.function.decl?.parameters.get(at)
    }

    // ------------------------------------------------------------ ranges

    /// `zend_inference_calc_range`: the range `var` takes from its
    /// definition, `narrowing` as php's bool of the same name.
    #[allow(clippy::too_many_lines)]
    fn calc_range(&self, var: SsaVar, narrowing: bool) -> Option<Range> {
        let v = self.ssa.var(var)?;
        let Some(phi) = v.definition_phi else {
            let def = v.definition?;
            return self.propagate_range(def as usize, var);
        };
        let p = self.ssa.phi(phi)?;
        let mut tmp = Range {
            min: i64::MAX,
            max: i64::MIN,
            underflow: false,
            overflow: false,
        };
        let range_of = |var: Option<SsaVar>| self.var_info(var).and_then(|i| i.range);
        match p.kind {
            PhiKind::Pi {
                constraint: Constraint::Range(c),
                ..
            } => {
                let src = p.sources.first().copied();
                if c.negative != crate::Negative::None {
                    if let Some(r) = range_of(src) {
                        tmp = r;
                        if c.range.min == c.range.max
                            && !c.range.underflow
                            && !c.range.overflow
                            && c.min_ssa_var.is_none()
                            && c.max_ssa_var.is_none()
                            && let Some(def) =
                                src.and_then(|s| self.ssa.var(s)).and_then(|s| s.definition)
                        {
                            // A constrained induction variable.
                            self.induction(def as usize, var, &c.range, &mut tmp);
                        }
                    } else if narrowing {
                        tmp = Range::ANY;
                    }
                } else if let Some(r) = range_of(src) {
                    // Intersection.
                    tmp = r;
                    match c.min_ssa_var {
                        None => {
                            tmp.underflow = c.range.underflow && tmp.underflow;
                            tmp.min = c.range.min.max(tmp.min);
                        }
                        Some(m) => {
                            if narrowing && let Some(mr) = range_of(Some(m)) {
                                tmp.underflow = mr.underflow && tmp.underflow;
                                if !add_overflows(mr.min, c.range.min) {
                                    tmp.min = (mr.min + c.range.min).max(tmp.min);
                                }
                            }
                        }
                    }
                    match c.max_ssa_var {
                        None => {
                            tmp.max = c.range.max.min(tmp.max);
                            tmp.overflow = c.range.overflow && tmp.overflow;
                        }
                        Some(m) => {
                            if narrowing && let Some(mr) = range_of(Some(m)) {
                                if !add_overflows(mr.max, c.range.max) {
                                    tmp.max = (mr.max + c.range.max).min(tmp.max);
                                }
                                tmp.overflow = mr.overflow && tmp.overflow;
                            }
                        }
                    }
                } else if narrowing {
                    match c.min_ssa_var.map(|m| range_of(Some(m))) {
                        None => {
                            tmp.underflow = c.range.underflow;
                            tmp.min = c.range.min;
                        }
                        Some(Some(mr)) => {
                            if add_overflows(mr.min, c.range.min) {
                                tmp.underflow = true;
                                tmp.min = i64::MIN;
                            } else {
                                tmp.underflow = mr.underflow;
                                tmp.min = mr.min + c.range.min;
                            }
                        }
                        Some(None) => {
                            tmp.underflow = true;
                            tmp.min = i64::MIN;
                        }
                    }
                    match c.max_ssa_var.map(|m| range_of(Some(m))) {
                        None => {
                            tmp.max = c.range.max;
                            tmp.overflow = c.range.overflow;
                        }
                        Some(Some(mr)) => {
                            if add_overflows(mr.max, c.range.max) {
                                tmp.overflow = true;
                                tmp.max = i64::MAX;
                            } else {
                                tmp.max = mr.max + c.range.max;
                                tmp.overflow = mr.overflow;
                            }
                        }
                        Some(None) => {
                            tmp.max = i64::MAX;
                            tmp.overflow = true;
                        }
                    }
                }
            }
            // A pi of a type constraint unions its sources, all one
            // variable, as a phi does.
            PhiKind::Pi { .. } | PhiKind::Phi => {
                for &source in &p.sources {
                    if let Some(r) = range_of(Some(source)) {
                        tmp.underflow |= r.underflow;
                        tmp.min = tmp.min.min(r.min);
                        tmp.max = tmp.max.max(r.max);
                        tmp.overflow |= r.overflow;
                    } else if narrowing {
                        tmp = Range::ANY;
                    }
                }
            }
        }
        (tmp.min <= tmp.max).then_some(tmp)
    }

    /// The constrained induction variable of `zend_inference_calc_range`:
    /// a pi `var` of `$i != N` on a variable that steps by one from the
    /// loop's phi, whose start lies past `N`, never passes it.
    fn induction(&self, def: usize, var: SsaVar, c: &Range, tmp: &mut Range) {
        let Some(op) = self.body.oplines.get(def) else {
            return;
        };
        let loop_phi = || {
            let used = self.op(def).op1_use?;
            let p = self.ssa.phi(self.ssa.var(used)?.definition_phi?)?;
            let preds = self.cfg.block(p.block)?.predecessors.len();
            (matches!(p.kind, PhiKind::Phi) && preds == 2 && p.sources.get(1) == Some(&var))
                .then_some(p)?
                .sources
                .first()
                .and_then(|s| self.var_info(Some(*s)))
                .and_then(|i| i.range)
        };
        match op.opcode {
            Opcode::PreDec | Opcode::PostDec if !tmp.underflow => {
                if let Some(start) = loop_phi()
                    && start.min > c.max
                {
                    tmp.min = c.max + 1;
                }
            }
            Opcode::PreInc | Opcode::PostInc if !tmp.overflow => {
                if let Some(start) = loop_phi()
                    && start.max < c.min
                {
                    tmp.max = c.min - 1;
                }
            }
            _ => {}
        }
    }

    /// `zend_inference_propagate_range`: the range of `var`, which the
    /// opline at `at` defines.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn propagate_range(&self, at: usize, var: SsaVar) -> Option<Range> {
        let op = self.opline(at)?;
        let ssa_op = self.op(at);
        let var = Some(var);
        let mut tmp = Range::default();
        let op1 = || self.operand_range(op.op1_type, op.op1, ssa_op.op1_use);
        let op2 = || self.operand_range(op.op2_type, op.op2, ssa_op.op2_use);
        let is_result = ssa_op.out_def == var;
        let is_op1 = ssa_op.op1_def == var;
        match op.opcode {
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Sl
            | Opcode::Sr
            | Opcode::BwOr
            | Opcode::BwAnd
            | Opcode::BwXor
                if is_result =>
            {
                range::binary(op.opcode, op1(), op2(), &mut tmp).then_some(tmp)
            }
            Opcode::BwNot if is_result => {
                let r = op1()?;
                if r.underflow || r.overflow {
                    tmp.min = i64::MIN;
                    tmp.max = i64::MAX;
                } else {
                    tmp.min = !r.max;
                    tmp.max = !r.min;
                }
                Some(tmp)
            }
            Opcode::Cast | Opcode::SendVar | Opcode::UnsetDim | Opcode::UnsetObj if is_op1 => op1(),
            Opcode::Cast if is_result => {
                if op.extended_value != LONG {
                    return None;
                }
                (tmp.min, tmp.max) = op1().map_or((i64::MIN, i64::MAX), |r| (r.min, r.max));
                Some(tmp)
            }
            Opcode::QmAssign | Opcode::JmpSet | Opcode::Coalesce | Opcode::CopyTmp
                if is_op1 || is_result =>
            {
                op1()
            }
            Opcode::PreInc if is_op1 || is_result => {
                let mut r = op1()?;
                if r.max < i64::MAX {
                    r.max += 1;
                } else {
                    r.overflow = true;
                }
                if r.min < i64::MAX && !r.underflow {
                    r.min += 1;
                }
                Some(r)
            }
            Opcode::PreDec if is_op1 || is_result => {
                let mut r = op1()?;
                if r.min > i64::MIN {
                    r.min -= 1;
                } else {
                    r.underflow = true;
                }
                if r.max > i64::MIN && !r.overflow {
                    r.max -= 1;
                }
                Some(r)
            }
            Opcode::PostInc if is_op1 || is_result => {
                let mut r = op1()?;
                if is_result {
                    return Some(r);
                }
                if r.max < i64::MAX {
                    r.max += 1;
                } else {
                    r.overflow = true;
                }
                if r.min < i64::MAX && !r.underflow {
                    r.min += 1;
                }
                Some(r)
            }
            Opcode::PostDec if is_op1 || is_result => {
                let mut r = op1()?;
                if is_result {
                    return Some(r);
                }
                if r.min > i64::MIN {
                    r.min -= 1;
                } else {
                    r.underflow = true;
                }
                if r.max > i64::MIN && !r.overflow {
                    r.max -= 1;
                }
                Some(r)
            }
            Opcode::Assign if is_op1 || ssa_op.op2_def == var || is_result => op2(),
            Opcode::AssignOp if is_op1 || is_result => {
                let opcode = binary_opcode(op.extended_value)?;
                if matches!(opcode, Opcode::Concat | Opcode::Pow) {
                    return None;
                }
                range::binary(opcode, op1(), op2(), &mut tmp).then_some(tmp)
            }
            Opcode::OpData if is_op1 => {
                let prev = at.checked_sub(1).and_then(|p| self.opline(p))?;
                if matches!(
                    prev.opcode,
                    Opcode::AssignDim
                        | Opcode::AssignObj
                        | Opcode::AssignStaticProp
                        | Opcode::AssignDimOp
                        | Opcode::AssignObjOp
                        | Opcode::AssignStaticPropOp
                ) {
                    op1()
                } else {
                    None
                }
            }
            Opcode::Recv | Opcode::RecvInit if is_result => {
                let typed = op.op1 <= self.function.num_args()
                    && self
                        .parameter(op.op1)
                        .and_then(|p| p.ty.as_ref())
                        .is_some_and(|ty| ty.mask() & !Types::NULL.0 == Types::LONG.0);
                typed.then_some(Range::LONG)
            }
            Opcode::Strlen if is_result => {
                tmp.min = 0;
                tmp.max = i64::MAX;
                Some(tmp)
            }
            Opcode::FuncNumArgs => {
                tmp.min = 0;
                tmp.max = i64::MAX;
                Some(tmp)
            }
            Opcode::Count => {
                tmp.min = i64::MIN;
                tmp.max = i64::MAX;
                Some(tmp)
            }
            Opcode::DoFcall | Opcode::DoIcall | Opcode::DoUcall | Opcode::DoFcallByName
                if is_result =>
            {
                self.call_range(at)
            }
            _ => None,
        }
    }

    fn init_range(&mut self, var: usize, r: Range) {
        let mut r = r;
        if r.underflow {
            r.min = i64::MIN;
        }
        if r.overflow {
            r.max = i64::MAX;
        }
        if let Some(info) = self.info.get_mut(var) {
            info.range = Some(r);
        }
    }

    /// `zend_inference_widening_meet`.
    fn widening_meet(&mut self, var: usize, mut r: Range) -> bool {
        let Some(info) = self.info.get_mut(var) else {
            return false;
        };
        if let Some(old) = info.range {
            if r.underflow || old.underflow || r.min < old.min {
                r.underflow = true;
                r.min = i64::MIN;
            } else {
                r.min = old.min;
            }
            if r.overflow || old.overflow || r.max > old.max {
                r.overflow = true;
                r.max = i64::MAX;
            } else {
                r.max = old.max;
            }
            if old == r {
                return false;
            }
        }
        info.range = Some(r);
        true
    }

    /// `zend_inference_narrowing_meet`.
    fn narrowing_meet(&mut self, var: usize, mut r: Range) -> bool {
        let Some(info) = self.info.get_mut(var) else {
            return false;
        };
        if let Some(old) = info.range {
            if !r.underflow && !old.underflow && old.min < r.min {
                r.min = old.min;
            }
            if !r.overflow && !old.overflow && old.max > r.max {
                r.max = old.max;
            }
            if r.underflow {
                r.min = i64::MIN;
            }
            if r.overflow {
                r.max = i64::MAX;
            }
            if old == r {
                return false;
            }
        }
        info.range = Some(r);
        true
    }

    fn widening(&mut self, var: usize) -> bool {
        self.calc_range(SsaVar::of(var), false)
            .is_some_and(|r| self.widening_meet(var, r))
    }

    fn narrowing(&mut self, var: usize) -> bool {
        self.calc_range(SsaVar::of(var), true)
            .is_some_and(|r| self.narrowing_meet(var, r))
    }

    /// `ADD_SCC_VAR` over each usage of `var`.
    fn add_scc_usages(&self, var: usize, scc: u32, worklist: &mut Bits, visited: Option<&Bits>) {
        for used in self.usages(SsaVar::of(var)) {
            self.add_scc_var(used.index(), scc, worklist, visited);
        }
    }

    /// `ADD_SCC_VAR`, and `ADD_SCC_VAR_1` where `visited` is given.
    fn add_scc_var(&self, var: usize, scc: u32, worklist: &mut Bits, visited: Option<&Bits>) {
        let in_scc = self.ssa.vars.get(var).is_some_and(|v| v.scc == Some(scc));
        if in_scc && !self.has_ref(var) && !visited.is_some_and(|v| v.has(var)) {
            worklist.incl(var);
        }
    }

    /// `zend_infer_ranges_warmup`.
    fn warmup(&mut self, members: &[usize], scc: u32) {
        let n = self.info.len();
        let mut worklist = Bits::new(n);
        let mut visited = Bits::new(n);
        for _ in 0..WARMUP_PASSES {
            for &j in members {
                let entry = self.ssa.vars.get(j).is_some_and(|v| v.scc_entry);
                if entry && !self.has_ref(j) {
                    worklist.incl(j);
                }
            }
            visited.clear();
            let mut passes = Passes::new();
            while let Some(j) = passes.next(&mut worklist) {
                if let Some(r) = self.calc_range(SsaVar::of(j), false)
                    && self.narrowing_meet(j, r)
                {
                    visited.incl(j);
                    self.add_scc_usages(j, scc, &mut worklist, Some(&visited));
                }
            }
        }
    }

    /// `zend_infer_ranges`: each SCC in topological order, a lone
    /// variable at once, a cycle by warmup, widening and narrowing.
    fn infer_ranges(&mut self) {
        let n = self.info.len();
        let sccs = self.ssa.sccs as usize;
        // Each SCC's variables, the last numbered first, as php links them.
        let mut members: Vec<Vec<usize>> = vec![Vec::new(); sccs];
        for (j, v) in self.ssa.vars.iter().enumerate() {
            if let Some(list) = v.scc.and_then(|s| members.get_mut(s as usize)) {
                list.insert(0, j);
            }
        }
        let mut worklist = Bits::new(n);
        for (scc, members) in members.iter().enumerate() {
            let scc = u32::try_from(scc).unwrap_or(u32::MAX);
            let &[j] = members.as_slice() else {
                if members.is_empty() {
                    continue;
                }
                worklist.clear();
                for &j in members {
                    let entry = self.ssa.vars.get(j).is_some_and(|v| v.scc_entry);
                    if entry && !self.has_ref(j) {
                        worklist.incl(j);
                    }
                }
                self.warmup(members, scc);
                for &j in members {
                    if !self.has_ref(j) {
                        worklist.incl(j);
                    }
                }
                self.widen(&mut worklist, scc);
                for &j in members {
                    let missing = self.info.get(j).is_some_and(|i| i.range.is_none());
                    if missing && !self.has_ref(j) {
                        self.init_range(j, Range::ANY);
                        self.add_scc_usages(j, scc, &mut worklist, None);
                    }
                }
                self.widen(&mut worklist, scc);
                for &j in members {
                    let phi = self
                        .ssa
                        .vars
                        .get(j)
                        .and_then(|v| v.definition_phi)
                        .and_then(|p| self.ssa.phi(p))
                        .is_some_and(|p| matches!(p.kind, PhiKind::Phi));
                    if phi && !self.has_ref(j) {
                        self.narrowing(j);
                    }
                    worklist.incl(j);
                }
                let mut passes = Passes::new();
                while let Some(j) = passes.next(&mut worklist) {
                    if self.narrowing(j) {
                        self.add_scc_usages(j, scc, &mut worklist, None);
                        let first = self.ssa.vars.get(j).and_then(|v| v.sym_use_chain);
                        let chain = std::iter::successors(first, |p| {
                            self.ssa.phi(*p).and_then(|p| p.sym_use_chain)
                        });
                        let constrained: Vec<usize> = chain
                            .filter_map(|p| self.ssa.phi(p).map(|p| p.ssa_var.index()))
                            .collect();
                        for var in constrained {
                            self.add_scc_var(var, scc, &mut worklist, None);
                        }
                    }
                }
                continue;
            };
            if self.has_ref(j) {
                continue;
            }
            let r = self.calc_range(SsaVar::of(j), true).unwrap_or(Range::ANY);
            self.init_range(j, r);
        }
    }

    /// One widening loop of `zend_infer_ranges`.
    fn widen(&mut self, worklist: &mut Bits, scc: u32) {
        let mut passes = Passes::new();
        while let Some(j) = passes.next(worklist) {
            if self.widening(j) {
                self.add_scc_usages(j, scc, worklist, None);
            }
        }
    }

    // ------------------------------------------------------------- types

    /// `UPDATE_SSA_TYPE`: `var`'s types become `t`, as a CV's and a
    /// parameter's are widened, and every variable defined from it is
    /// looked at again; a type that loses a bit is php's bug, which ends
    /// the inference.
    pub(crate) fn set_types(
        &mut self,
        t: Types,
        var: Option<SsaVar>,
        worklist: &mut Bits,
    ) -> Result<(), Narrowed> {
        let mut t = t.without(Types::GUARD);
        if t.has(Types::REF) {
            t |= Types::REF_ANY.without(Types::REF);
        }
        let Some(var) = var else {
            return Ok(());
        };
        let Some(v) = self.ssa.var(var) else {
            return Ok(());
        };
        if v.var.0 < self.function.num_args() && t.has(Types::RC1) {
            // May be captured by an exception's backtrace.
            t |= Types::RCN;
        }
        if v.var.0 < self.body.num_cvs {
            if t.has(Types::REF | Types::RCN) {
                t |= Types::RC;
            }
            if t.has(Types::RC1) && t.has(Types::STRING) {
                t |= Types::RCN;
            }
            if t.has(Types::RC1) && t.has(Types::OBJECT) {
                t |= Types::RCN;
            }
            if v.alias != Alias::None {
                t |= of_alias(v.alias);
            }
        }
        let Some(info) = self.info.get_mut(var.index()) else {
            return Ok(());
        };
        if info.types != t {
            if !info.types.without(t).is_empty() {
                return Err(Narrowed);
            }
            info.types = t;
            self.add_usages(var, worklist);
        }
        Ok(())
    }

    /// `UPDATE_SSA_OBJ_TYPE`: `var` is an object of `class`, or of a
    /// class below it where `instanceof` says so and the class is not
    /// final.
    pub(crate) fn set_class(
        &mut self,
        class: Option<T::Class>,
        instanceof: bool,
        var: Option<SsaVar>,
        worklist: &mut Bits,
    ) {
        let Some(var) = var else {
            return;
        };
        let class = class.map(|class| ClassOf {
            class,
            instanceof: instanceof && !self.table.is_final(class),
        });
        let Some(info) = self.info.get_mut(var.index()) else {
            return;
        };
        if info.class != class {
            info.class = class;
            self.add_usages(var, worklist);
        }
    }

    /// `COPY_SSA_OBJ_TYPE`.
    pub(crate) fn copy_class(
        &mut self,
        from: Option<SsaVar>,
        to: Option<SsaVar>,
        worklist: &mut Bits,
    ) {
        let from = self
            .var_info(from)
            .filter(|i| i.types.has(Types::OBJECT))
            .and_then(|i| i.class);
        let to_ref = self.var_info(to).is_some_and(|i| i.types.has(Types::REF));
        match from {
            Some(c) if !to_ref => self.set_class(Some(c.class), c.instanceof, to, worklist),
            _ => self.set_class(None, false, to, worklist),
        }
    }

    /// `add_usages`.
    fn add_usages(&self, var: SsaVar, worklist: &mut Bits) {
        // A trace's inference keeps no worklist (`update_worklist` 0).
        if self.positions.is_some() {
            return;
        }
        for used in self.usages(var) {
            worklist.incl(used.index());
        }
    }

    /// `safe_instanceof`.
    fn safe_instanceof(&self, a: T::Class, b: T::Class) -> bool {
        a == b || self.table.instance_of(a, b)
    }

    /// `get_class_entry_rank`.
    fn rank(&self, class: T::Class) -> u32 {
        let mut rank = 0;
        let mut at = class;
        while let Some(parent) = self.table.parent(at) {
            rank += 1;
            at = parent;
        }
        rank
    }

    /// `join_class_entries`: the nearest class both are of.
    fn join(
        &self,
        a: Option<T::Class>,
        b: Option<T::Class>,
        instanceof: &mut bool,
    ) -> Option<T::Class> {
        if a == b {
            return a;
        }
        let (mut a, mut b) = (a?, b?);
        let (mut ra, mut rb) = (self.rank(a), self.rank(b));
        while ra != rb {
            if ra > rb {
                a = self.table.parent(a)?;
                ra -= 1;
            } else {
                b = self.table.parent(b)?;
                rb -= 1;
            }
        }
        while a != b {
            a = self.table.parent(a)?;
            b = self.table.parent(b)?;
        }
        *instanceof = true;
        Some(a)
    }

    /// `zend_infer_types_ex`: the lowest variable of the worklist at a
    /// time, until none is left.
    fn infer_types(&mut self, worklist: &mut Bits) -> Result<(), Narrowed> {
        while let Some(j) = worklist.first() {
            worklist.excl(j);
            let var = SsaVar::of(j);
            let Some(v) = self.ssa.var(var) else {
                continue;
            };
            if let Some(phi) = v.definition_phi.and_then(|p| self.ssa.phi(p)) {
                match phi.kind {
                    PhiKind::Pi { constraint, .. } => {
                        let source = phi.sources.first().copied();
                        let from = self.var_info(source);
                        let mut class = from.and_then(|i| i.class.map(|c| c.class));
                        let mut instanceof =
                            from.and_then(|i| i.class).is_some_and(|c| c.instanceof);
                        let mut t = self.var_types(source);
                        if let Constraint::Type {
                            mask,
                            class: wanted,
                        } = constraint
                        {
                            t &= mask;
                            if !t
                                .has(Types::STRING | Types::ARRAY | Types::OBJECT | Types::RESOURCE)
                            {
                                t = t.without(Types::RC);
                            }
                            if t.has(Types::OBJECT)
                                && let Some(wanted) = wanted
                                && class != Some(wanted)
                            {
                                match class {
                                    None => {
                                        class = Some(wanted);
                                        instanceof = true;
                                    }
                                    Some(have)
                                        if instanceof && self.safe_instanceof(wanted, have) =>
                                    {
                                        class = Some(wanted);
                                    }
                                    // Either the class is below the
                                    // constraint's or they are unrelated.
                                    Some(_) => {}
                                }
                            }
                        }
                        self.set_types(t, Some(var), worklist)?;
                        if t.has(Types::REF) {
                            self.set_class(None, false, Some(var), worklist);
                        } else {
                            self.set_class(class, instanceof, Some(var), worklist);
                        }
                    }
                    PhiKind::Phi => {
                        let sources = phi.sources.clone();
                        let t = sources
                            .iter()
                            .fold(Types::NONE, |t, s| t | self.var_types(Some(*s)));
                        self.set_types(t, Some(var), worklist)?;
                        let mut first = true;
                        let mut instanceof = false;
                        let mut class = None;
                        for s in sources {
                            let Some(info) = self.var_info(Some(s)) else {
                                continue;
                            };
                            if !info.types.has(Types::OBJECT) {
                                continue;
                            }
                            let (c, inst) = (
                                info.class.map(|c| c.class),
                                info.class.is_some_and(|c| c.instanceof),
                            );
                            if first {
                                class = c;
                                instanceof = inst;
                                first = false;
                            } else {
                                instanceof |= inst;
                                class = self.join(class, c, &mut instanceof);
                            }
                        }
                        self.set_class(class, class.is_some() && instanceof, Some(var), worklist);
                    }
                }
            } else if let Some(def) = v.definition {
                self.update(def as usize, worklist)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ return

    /// `is_recursive_tail_call`: `returned` is what a `DO_UCALL` of the
    /// op array analysed itself returned.
    fn is_recursive_tail_call(&self, returned: Option<SsaVar>) -> bool {
        let Some(calls) = self.calls else {
            return false;
        };
        let defined = returned
            .and_then(|v| self.ssa.var(v))
            .and_then(|v| v.definition);
        defined.is_some_and(|at| {
            let at = at as usize;
            self.body
                .oplines
                .get(at)
                .is_some_and(|op| op.opcode == Opcode::DoUcall)
                && calls.at(at).is_some_and(|call| call.callee.is_analysed())
        })
    }

    /// `zend_func_return_info`, recursive and not widening, as
    /// `zend_infer_types` asks it: what each reachable `RETURN` returns,
    /// joined, within what `ret` holds already, but a return of a
    /// recursive tail call.
    fn return_info(&self, ret: &mut Returns<T::Class>) {
        if ret.types.is_empty() {
            ret.types = Types::REF_ANY;
        }
        let mut t = Types::NONE;
        // `tmp_ce` and `tmp_is_instanceof`, none before the first return.
        let mut class: Option<(Option<T::Class>, bool)> = None;
        let mut range = Range::default();
        // `tmp_has_range`: none before the first return.
        let mut has_range: Option<bool> = None;
        for block in &self.cfg.blocks {
            if !block.marks.reachable || block.len == 0 {
                continue;
            }
            let at = (block.start + block.len - 1) as usize;
            let Some(op) = self.body.oplines.get(at) else {
                continue;
            };
            if !matches!(op.opcode, Opcode::Return | Opcode::ReturnByRef) {
                continue;
            }
            let ssa_op = self.op(at);
            if self.is_recursive_tail_call(ssa_op.op1_use) {
                continue;
            }
            let mut t1 = self.operand_types(op.op1_type, op.op1, ssa_op.op1_use);
            if t1.has(Types::UNDEF) {
                t1 |= Types::NULL;
            }
            if op.opcode == Opcode::Return {
                if t1.has(Types::RC1) {
                    t1 |= Types::RCN;
                }
                t1 = t1.without(Types::UNDEF | Types::REF);
            } else {
                t1 |= Types::REF;
                t1 = t1.without(Types::UNDEF | Types::RC);
            }
            t |= t1;
            let used = self.var_info(ssa_op.op1_use);
            let arg = used
                .and_then(|i| i.class)
                .filter(|_| !t1.has(Types::REF) && op.op1_type != OperandKind::Const);
            let (arg_class, arg_instanceof) =
                (arg.map(|c| c.class), arg.is_some_and(|c| c.instanceof));
            class = Some(match class {
                None => (arg_class, arg_instanceof),
                Some((have, inst)) if arg_class.is_some() && arg_class == have => {
                    (have, inst || arg_instanceof)
                }
                Some(_) => (None, false),
            });
            if op.op1_type == OperandKind::Const {
                match self.literal(op.op1).and_then(|z| z.as_long()) {
                    Some(n) => match has_range {
                        None => {
                            has_range = Some(true);
                            range = Range {
                                min: n,
                                max: n,
                                underflow: false,
                                overflow: false,
                            };
                        }
                        Some(true) => {
                            if !range.underflow {
                                range.min = range.min.min(n);
                            }
                            if !range.overflow {
                                range.max = range.max.max(n);
                            }
                        }
                        Some(false) => {}
                    },
                    None => has_range = Some(false),
                }
            } else if let Some(info) = used {
                if let Some(r) = info.range {
                    match has_range {
                        None => {
                            has_range = Some(true);
                            range = r;
                        }
                        Some(true) => {
                            if r.underflow {
                                range.underflow = true;
                                range.min = i64::MIN;
                            } else {
                                range.min = range.min.min(r.min);
                            }
                            if r.overflow {
                                range.overflow = true;
                                range.max = i64::MAX;
                            } else {
                                range.max = range.max.max(r.max);
                            }
                        }
                        Some(false) => {}
                    }
                } else {
                    has_range = Some(true);
                    range = Range::ANY;
                }
            } else {
                has_range = Some(false);
            }
        }
        let typed = self.function.decl.is_some_and(|d| d.return_type.is_some());
        if !typed {
            ret.class = class.and_then(|(c, inst)| {
                c.map(|class| ClassOf {
                    class,
                    instanceof: inst,
                })
            });
        }
        ret.types &= t;
        ret.range = range;
        // php's one-bit field takes the -1 of no return as set.
        ret.has_range = has_range.unwrap_or(typed);
    }
}

/// `IS_LONG`, a `CAST`'s `extended_value`.
const LONG: u32 = 4;

/// The binary operation an `ASSIGN_OP`'s `extended_value` names.
pub(crate) fn binary_opcode(extended_value: u32) -> Option<Opcode> {
    Opcode::from_u8(u8::try_from(extended_value).ok()?)
}
