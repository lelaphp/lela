//! `zend_func_info.c`, with the return information of `zend_inference.c`
//! it reads: what a call returns as php knows its callee
//! (`zend_get_func_info`), from php's table of natives
//! (`zend_func_infos.h`, rendered by `build.rs` from
//! `data/func-infos.toml`), from what the last analysis of a user
//! function found it returns, or from the callee's declared return type
//! alone (`zend_get_return_info_from_signature_only`).

use php_unit::may_be;

use crate::calls::Call;
use crate::infer::Infer;
use crate::types::{ClassOf, Range, Returns, Types, of_declared};
use crate::{Body, Callee, Function, LinkedTable};

include!(concat!(env!("OUT_DIR"), "/func_infos.rs"));

/// `zend_get_return_info_from_signature_only`: the declared return type,
/// a class it names taken as the class or one below it, where it declares
/// one and, for a tentative type, `use_tentative` allows it; anything but
/// a reference otherwise; a reference where the callee returns one.
pub(crate) fn signature_only<T: LinkedTable>(
    callee: &Callee<'_, T::Class>,
    table: &T,
    use_tentative: bool,
) -> (Types, Option<ClassOf<T::Class>>) {
    let tentative = callee.method.is_some_and(|m| m.tentative_return);
    let (mut types, mut class) = match callee.return_type {
        Some(ty) if use_tentative || !tentative => {
            let (types, class) = of_declared(Some(ty), table);
            // php sets `ce_is_instanceof` from the address it writes the
            // class to, never null.
            let class = class.map(|class| ClassOf {
                class,
                instanceof: true,
            });
            (types, class)
        }
        _ => (Types::UNKNOWN, None),
    };
    // A generator's reference return is what it yields.
    if callee.returns_by_ref && !callee.generator {
        types |= Types::REF;
        class = None;
    }
    (types, class)
}

/// `zend_init_func_return_info`: what a function that declares its return
/// type returns, before inference learns more; nothing for one that
/// declares none.
pub(crate) fn init_return_info<T: LinkedTable>(
    function: &Function<'_>,
    table: &T,
) -> Returns<T::Class> {
    let Some(callee) = function.callee() else {
        return Returns::default();
    };
    if callee.return_type.is_none() {
        return Returns::default();
    }
    let (types, class) = signature_only(&callee, table, true);
    Returns {
        types,
        class,
        range: Range::default(),
        has_range: false,
    }
}

/// `func_info[]`'s row for the native `name`.
fn table_info(name: &[u8]) -> Option<Types> {
    let at = FUNC_INFOS
        .binary_search_by(|(row, _)| (*row).cmp(name))
        .ok()?;
    FUNC_INFOS.get(at).map(|&(_, mask)| Types(mask))
}

impl<T: LinkedTable> Infer<'_, T> {
    /// `zend_get_func_info`: what `call` returns, and the class of the
    /// object it returns where php knows it.
    ///
    /// A user function's call takes what the callee's analysis found it
    /// returns where the trace's build analysed the callee first, its types
    /// and its range: `twice` and `low` are entered before `plus` is, so
    /// `plus`'s calls read them.
    ///
    /// ```php
    /// function twice(int $x) { return $x * 2; }
    /// function low(int $x) { return $x & 7; }
    /// function plus(int $x) { return twice($x) + low($x); }
    /// $t = 0;
    /// for ($k = 0; $k < 5; $k++) {
    ///     $t += twice($k) + low($k) + plus($k);
    /// }
    /// echo $t, "\n";
    /// ```
    /// ```expect regime=jit-ssa process capture
    /// 60
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:5
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) $main() %SCRATCH%/witness.php:6
    /// ---- TRACE 1 stop (loop)
    ///
    /// $_main:
    ///      ; (lines=21, args=0, vars=2, tmps=10, ssa_vars=0)
    ///      ; (JIT)
    ///      ; %SCRATCH%/witness.php:1-9
    ///      ; return  [] RANGE[0..0]
    /// BB0:
    ///      ; start lines=[0-2]
    ///      ; to=(BB5)
    ///      ; level=0
    ///      ; children=(BB5)
    /// 0000 ASSIGN CV0($t) int(0)
    /// 0001 ASSIGN CV1($k) int(0)
    /// 0002 JMP BB5
    ///
    /// BB1:
    ///      ; target lines=[3-5]
    ///      ; from=(BB5)
    ///      ; to=(BB2)
    ///      ; idom=BB5
    ///      ; level=2
    ///      ; loop_header=5
    ///      ; children=(BB2)
    /// 0003 INIT_FCALL 1 112 string("twice")
    /// 0004 SEND_VAR CV1($k) 1
    /// 0005 V4 = DO_UCALL
    ///
    /// BB2:
    ///      ; follow entry lines=[6-8]
    ///      ; from=(BB1)
    ///      ; to=(BB3)
    ///      ; idom=BB1
    ///      ; level=3
    ///      ; loop_header=5
    ///      ; children=(BB3)
    /// 0006 INIT_FCALL 1 112 string("low")
    /// 0007 SEND_VAR CV1($k) 1
    /// 0008 V5 = DO_UCALL
    ///
    /// BB3:
    ///      ; follow entry lines=[9-12]
    ///      ; from=(BB2)
    ///      ; to=(BB4)
    ///      ; idom=BB2
    ///      ; level=4
    ///      ; loop_header=5
    ///      ; children=(BB4)
    /// 0009 T6 = ADD V4 V5
    /// 0010 INIT_FCALL 1 144 string("plus")
    /// 0011 SEND_VAR CV1($k) 1
    /// 0012 V7 = DO_UCALL
    ///
    /// BB4:
    ///      ; follow entry lines=[13-15]
    ///      ; from=(BB3)
    ///      ; to=(BB5)
    ///      ; idom=BB3
    ///      ; level=5
    ///      ; loop_header=5
    /// 0013 T8 = ADD T6 V7
    /// 0014 ASSIGN_OP (ADD) CV0($t) T8
    /// 0015 PRE_INC CV1($k)
    ///
    /// BB5:
    ///      ; follow target loop_header lines=[16-17]
    ///      ; from=(BB0, BB4)
    ///      ; to=(BB1, BB6)
    ///      ; idom=BB0
    ///      ; level=1
    ///      ; children=(BB1, BB6)
    /// 0016 T11 = IS_SMALLER CV1($k) int(5)
    /// 0017 JMPNZ T11 BB1
    ///
    /// BB6:
    ///      ; follow exit lines=[18-20]
    ///      ; from=(BB5)
    ///      ; idom=BB5
    ///      ; level=2
    /// 0018 ECHO CV0($t)
    /// 0019 ECHO string("\n")
    /// 0020 RETURN int(1)
    ///
    /// twice:
    ///      ; (lines=4, args=1, vars=1, tmps=1, ssa_vars=3)
    ///      ; (JIT)
    ///      ; %SCRATCH%/witness.php:2-2
    ///      ; return  [long, double]
    ///      ; #0.CV0($x) NOVAL [undef]
    /// BB0:
    ///      ; start lines=[0-0]
    ///      ; to=(BB1)
    ///      ; level=0
    ///      ; children=(BB1)
    /// 0000 #1.CV0($x) [long] RANGE[MIN..MAX] = RECV 1
    ///
    /// BB1:
    ///      ; recv follow exit entry lines=[1-2]
    ///      ; from=(BB0)
    ///      ; idom=BB0
    ///      ; level=1
    /// 0001 #2.T1 [long, double] = MUL #1.CV0($x) [long] RANGE[MIN..MAX] int(2)
    /// 0002 RETURN #2.T1 [long, double]
    ///
    /// low:
    ///      ; (lines=4, args=1, vars=1, tmps=1, ssa_vars=3)
    ///      ; (JIT)
    ///      ; %SCRATCH%/witness.php:3-3
    ///      ; return  [long] RANGE[0..7]
    ///      ; #0.CV0($x) NOVAL [undef]
    /// BB0:
    ///      ; start lines=[0-0]
    ///      ; to=(BB1)
    ///      ; level=0
    ///      ; children=(BB1)
    /// 0000 #1.CV0($x) [long] RANGE[MIN..MAX] = RECV 1
    ///
    /// BB1:
    ///      ; recv follow exit entry lines=[1-2]
    ///      ; from=(BB0)
    ///      ; idom=BB0
    ///      ; level=1
    /// 0001 #2.T1 [long] RANGE[0..7] = BW_AND #1.CV0($x) [long] RANGE[MIN..MAX] int(7)
    /// 0002 RETURN #2.T1 [long] RANGE[0..7]
    ///
    /// plus:
    ///      ; (lines=10, args=1, vars=1, tmps=3, ssa_vars=7)
    ///      ; (JIT)
    ///      ; %SCRATCH%/witness.php:4-4
    ///      ; return  [long, double]
    ///      ; #0.CV0($x) NOVAL [undef]
    /// BB0:
    ///      ; start lines=[0-0]
    ///      ; to=(BB1)
    ///      ; level=0
    ///      ; children=(BB1)
    /// 0000 #1.CV0($x) [long] RANGE[MIN..MAX] = RECV 1
    ///
    /// BB1:
    ///      ; recv follow entry lines=[1-3]
    ///      ; from=(BB0)
    ///      ; to=(BB2)
    ///      ; idom=BB0
    ///      ; level=1
    ///      ; children=(BB2)
    /// 0001 INIT_FCALL 1 112 string("twice")
    /// 0002 SEND_VAR #1.CV0($x) [long] RANGE[MIN..MAX] -> #2.CV0($x) [long] RANGE[MIN..MAX] 1
    /// 0003 #3.V1 [long, double] = DO_UCALL
    ///
    /// BB2:
    ///      ; follow entry lines=[4-6]
    ///      ; from=(BB1)
    ///      ; to=(BB3)
    ///      ; idom=BB1
    ///      ; level=2
    ///      ; children=(BB3)
    /// 0004 INIT_FCALL 1 112 string("low")
    /// 0005 SEND_VAR #2.CV0($x) [long] RANGE[MIN..MAX] -> #4.CV0($x) NOVAL [long] 1
    /// 0006 #5.V2 [long] RANGE[0..7] = DO_UCALL
    ///
    /// BB3:
    ///      ; follow exit entry lines=[7-8]
    ///      ; from=(BB2)
    ///      ; idom=BB2
    ///      ; level=3
    /// 0007 #6.T3 [long, double] = ADD #3.V1 [long, double] #5.V2 [long] RANGE[0..7]
    /// 0008 RETURN #6.T3 [long, double]
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 2 $main() %SCRATCH%/witness.php:9
    /// ---- TRACE 2 start (side trace 1/2) $main() %SCRATCH%/witness.php:9
    /// ---- TRACE 2 stop (return)
    ///
    /// $_main:
    ///      ; (lines=21, args=0, vars=2, tmps=10, ssa_vars=0)
    ///      ; (JIT)
    ///      ; %SCRATCH%/witness.php:1-9
    ///      ; return  [] RANGE[0..0]
    /// BB0:
    ///      ; start lines=[0-2]
    ///      ; to=(BB5)
    ///      ; level=0
    ///      ; children=(BB5)
    /// 0000 ASSIGN CV0($t) int(0)
    /// 0001 ASSIGN CV1($k) int(0)
    /// 0002 JMP BB5
    ///
    /// BB1:
    ///      ; target lines=[3-5]
    ///      ; from=(BB5)
    ///      ; to=(BB2)
    ///      ; idom=BB5
    ///      ; level=2
    ///      ; loop_header=5
    ///      ; children=(BB2)
    /// 0003 INIT_FCALL 1 112 string("twice")
    /// 0004 SEND_VAR CV1($k) 1
    /// 0005 V4 = DO_UCALL
    ///
    /// BB2:
    ///      ; follow entry lines=[6-8]
    ///      ; from=(BB1)
    ///      ; to=(BB3)
    ///      ; idom=BB1
    ///      ; level=3
    ///      ; loop_header=5
    ///      ; children=(BB3)
    /// 0006 INIT_FCALL 1 112 string("low")
    /// 0007 SEND_VAR CV1($k) 1
    /// 0008 V5 = DO_UCALL
    ///
    /// BB3:
    ///      ; follow entry lines=[9-12]
    ///      ; from=(BB2)
    ///      ; to=(BB4)
    ///      ; idom=BB2
    ///      ; level=4
    ///      ; loop_header=5
    ///      ; children=(BB4)
    /// 0009 T6 = ADD V4 V5
    /// 0010 INIT_FCALL 1 144 string("plus")
    /// 0011 SEND_VAR CV1($k) 1
    /// 0012 V7 = DO_UCALL
    ///
    /// BB4:
    ///      ; follow entry lines=[13-15]
    ///      ; from=(BB3)
    ///      ; to=(BB5)
    ///      ; idom=BB3
    ///      ; level=5
    ///      ; loop_header=5
    /// 0013 T8 = ADD T6 V7
    /// 0014 ASSIGN_OP (ADD) CV0($t) T8
    /// 0015 PRE_INC CV1($k)
    ///
    /// BB5:
    ///      ; follow target loop_header lines=[16-17]
    ///      ; from=(BB0, BB4)
    ///      ; to=(BB1, BB6)
    ///      ; idom=BB0
    ///      ; level=1
    ///      ; children=(BB1, BB6)
    /// 0016 T11 = IS_SMALLER CV1($k) int(5)
    /// 0017 JMPNZ T11 BB1
    ///
    /// BB6:
    ///      ; follow exit lines=[18-20]
    ///      ; from=(BB5)
    ///      ; idom=BB5
    ///      ; level=2
    /// 0018 ECHO CV0($t)
    /// 0019 ECHO string("\n")
    /// 0020 RETURN int(1)
    /// ---- TRACE 2 compiled
    /// ```
    pub(crate) fn func_info(
        &self,
        call: &Call<'_, T::Class>,
    ) -> (Types, Option<ClassOf<T::Class>>) {
        let callee = &call.callee;
        match callee.body {
            Body::Native => {
                let internal = self.internal_func_info(call);
                if !internal.is_empty() {
                    return (internal, None);
                }
                signature_only(callee, self.table, !call.is_prototype)
            }
            Body::Analysed | Body::User(_) => {
                // The op array analysed reads its own `return_info` as
                // `zend_init_func_return_info` left it, which is its
                // declared type or nothing: the signature below.
                let known = match callee.body {
                    Body::User(Some(returns)) if !call.is_prototype => returns,
                    _ => Returns::default(),
                };
                if !known.types.is_empty() {
                    return (known.types, known.class);
                }
                let (mut types, mut class) = signature_only(callee, self.table, !call.is_prototype);
                // An override may return by reference where the method
                // it overrides does not.
                if call.is_prototype && !types.without(Types::REF).is_empty() {
                    types |= Types::REF;
                    class = None;
                }
                (types, class)
            }
        }
    }

    /// `zend_get_internal_func_info`: what the table, or `range()`'s
    /// function beside it, knows a native function returns; nothing for a
    /// method or a native the table does not name.
    ///
    /// `count` with its second argument and `strlen` with a named argument are calls,
    /// not their opcodes, and are typed by their declarations; `str_repeat`
    /// by the table, `range` by its function over its arguments' types, and
    /// `twice`, analysed only after `f` in this build, by its declaration.
    ///
    /// ```php
    /// function twice($x) { return $x * 2; }
    /// function f($a, $s) {
    ///     $n = 0;
    ///     $i = 0;
    ///     do {
    ///         if ($i === 10) {
    ///             return $n;
    ///         }
    ///         $n += count($a, COUNT_NORMAL) + strlen(string: $s) + twice($i);
    ///         $t = str_repeat($s, 2);
    ///         $r = range(1, 3);
    ///         $i++;
    ///     } while (true);
    /// }
    /// echo f([1, 2], "ab"), "\n";
    /// ```
    /// ```expect regime=jit-ssa process capture
    /// 130
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:16
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:7
    /// ---- TRACE 1 stop (loop)
    ///
    /// f:
    ///      ; (lines=33, args=2, vars=6, tmps=14, ssa_vars=32)
    ///      ; (JIT)
    ///      ; %SCRATCH%/witness.php:3-15
    ///      ; return  [rc1, rcn, null, false, long, double, object]
    ///      ; #0.CV0($a) NOVAL [undef]
    ///      ; #1.CV1($s) NOVAL [undef]
    ///      ; #2.CV2($n) NOVAL [undef]
    ///      ; #3.CV3($i) NOVAL [undef]
    ///      ; #4.CV4($t) NOVAL [undef]
    ///      ; #5.CV5($r) NOVAL [undef]
    /// BB0:
    ///      ; start lines=[0-0]
    ///      ; to=(BB1)
    ///      ; level=0
    ///      ; children=(BB1)
    /// 0000 #6.CV0($a) [rc1, rcn, any] = RECV 1
    ///
    /// BB1:
    ///      ; recv follow entry lines=[1-1]
    ///      ; from=(BB0)
    ///      ; to=(BB2)
    ///      ; idom=BB0
    ///      ; level=1
    ///      ; children=(BB2)
    /// 0001 #7.CV1($s) [rc1, rcn, any] = RECV 2
    ///
    /// BB2:
    ///      ; recv follow entry lines=[2-3]
    ///      ; from=(BB1)
    ///      ; to=(BB3)
    ///      ; idom=BB1
    ///      ; level=2
    ///      ; children=(BB3)
    /// 0002 ASSIGN #2.CV2($n) NOVAL [undef] -> #8.CV2($n) [long] RANGE[0..0] int(0)
    /// 0003 ASSIGN #3.CV3($i) NOVAL [undef] -> #9.CV3($i) [long] RANGE[0..0] int(0)
    ///
    /// BB3:
    ///      ; follow target loop_header lines=[4-5]
    ///      ; from=(BB2, BB6)
    ///      ; to=(BB5, BB4)
    ///      ; idom=BB2
    ///      ; level=3
    ///      ; children=(BB4, BB5)
    ///      #10.CV0($a) [rc1, rcn, any] = Phi(#6.CV0($a) [rc1, rcn, any], #17.CV0($a) [rc1, rcn, any])
    ///      #11.CV1($s) [rc1, rcn, any] = Phi(#7.CV1($s) [rc1, rcn, any], #26.CV1($s) [rc1, rcn, any])
    ///      #12.CV2($n) [rc1, rcn, false, long, double, object] = Phi(#8.CV2($n) [long] RANGE[0..0], #25.CV2($n) [rc1, rcn, false, long, double, object])
    ///      #13.CV3($i) [long, double] RANGE[0..++] = Phi(#9.CV3($i) [long] RANGE[0..0], #31.CV3($i) [long, double] RANGE[1..++])
    ///      #14.CV4($t) NOVAL [undef, rc1, rcn, string] = Phi(#4.CV4($t) NOVAL [undef], #28.CV4($t) NOVAL [rc1, rcn, string])
    ///      #15.CV5($r) NOVAL [undef, rc1, packed array [long] of [long]] = Phi(#5.CV5($r) NOVAL [undef], #30.CV5($r) NOVAL [rc1, packed array [long] of [long]])
    /// 0004 #16.T8 [bool] = IS_IDENTICAL #13.CV3($i) [long, double] RANGE[0..++] int(10)
    /// 0005 JMPZ #16.T8 [bool] BB5
    ///
    /// BB4:
    ///      ; follow exit lines=[6-6]
    ///      ; from=(BB3)
    ///      ; idom=BB3
    ///      ; level=4
    /// 0006 RETURN #12.CV2($n) [rc1, rcn, false, long, double, object]
    ///
    /// BB5:
    ///      ; target lines=[7-17]
    ///      ; from=(BB3)
    ///      ; to=(BB6)
    ///      ; idom=BB3
    ///      ; level=4
    ///      ; loop_header=3
    ///      ; children=(BB6)
    /// 0007 INIT_FCALL 2 112 string("count")
    /// 0008 SEND_VAR #10.CV0($a) [rc1, rcn, any] -> #17.CV0($a) [rc1, rcn, any] 1
    /// 0009 SEND_VAL int(0) 2
    /// 0010 #18.V9 [long] = DO_ICALL
    /// 0011 INIT_FCALL 1 96 string("strlen")
    /// 0012 SEND_VAR #11.CV1($s) [rc1, rcn, any] -> #19.CV1($s) [rc1, rcn, any] 1
    /// 0013 #20.V10 [long] = DO_ICALL
    /// 0014 #21.T11 [long, double] = ADD #18.V9 [long] #20.V10 [long]
    /// 0015 INIT_FCALL 1 112 string("twice")
    /// 0016 SEND_VAR #13.CV3($i) [long, double] RANGE[0..++] -> #22.CV3($i) [long, double] RANGE[0..++] 1
    /// 0017 #23.V12 [rc1, rcn, any] = DO_UCALL
    ///
    /// BB6:
    ///      ; follow entry lines=[18-31]
    ///      ; from=(BB5)
    ///      ; to=(BB3, BB7)
    ///      ; idom=BB5
    ///      ; level=5
    ///      ; loop_header=3
    ///      ; children=(BB7)
    /// 0018 #24.T13 [rc1, false, long, double, object] = ADD #21.T11 [long, double] #23.V12 [rc1, rcn, any]
    /// 0019 ASSIGN_OP (ADD) #12.CV2($n) [rc1, rcn, false, long, double, object] -> #25.CV2($n) [rc1, rcn, false, long, double, object] #24.T13 [rc1, false, long, double, object]
    /// 0020 INIT_FCALL 2 112 string("str_repeat")
    /// 0021 SEND_VAR #19.CV1($s) [rc1, rcn, any] -> #26.CV1($s) [rc1, rcn, any] 1
    /// 0022 SEND_VAL int(2) 2
    /// 0023 #27.V15 [rc1, string] = DO_ICALL
    /// 0024 ASSIGN #14.CV4($t) NOVAL [undef, rc1, rcn, string] -> #28.CV4($t) NOVAL [rc1, rcn, string] #27.V15 [rc1, string]
    /// 0025 INIT_FCALL 2 112 string("range")
    /// 0026 SEND_VAL int(1) 1
    /// 0027 SEND_VAL int(3) 2
    /// 0028 #29.V17 [rc1, packed array [long] of [long]] = DO_ICALL
    /// 0029 ASSIGN #15.CV5($r) NOVAL [undef, rc1, packed array [long] of [long]] -> #30.CV5($r) NOVAL [rc1, packed array [long] of [long]] #29.V17 [rc1, packed array [long] of [long]]
    /// 0030 PRE_INC #22.CV3($i) [long, double] RANGE[0..++] -> #31.CV3($i) [long, double] RANGE[1..++]
    /// 0031 JMPNZ bool(true) BB3
    ///
    /// BB7:
    ///      ; follow exit lines=[32-32]
    ///      ; from=(BB6)
    ///      ; idom=BB6
    ///      ; level=6
    /// 0032 RETURN null
    ///
    /// twice:
    ///      ; (lines=4, args=1, vars=1, tmps=1, ssa_vars=3)
    ///      ; (JIT)
    ///      ; %SCRATCH%/witness.php:2-2
    ///      ; return  [rc1, rcn, false, long, double, object]
    ///      ; #0.CV0($x) NOVAL [undef]
    /// BB0:
    ///      ; start lines=[0-0]
    ///      ; to=(BB1)
    ///      ; level=0
    ///      ; children=(BB1)
    /// 0000 #1.CV0($x) [rc1, rcn, any] = RECV 1
    ///
    /// BB1:
    ///      ; recv follow exit entry lines=[1-2]
    ///      ; from=(BB0)
    ///      ; idom=BB0
    ///      ; level=1
    /// 0001 #2.T1 [rc1, false, long, double, object] = MUL #1.CV0($x) [rc1, rcn, any] int(2)
    /// 0002 RETURN #2.T1 [rc1, false, long, double, object]
    /// ---- TRACE 1 compiled
    ///
    ///      TRACE 1 exit 5 f() %SCRATCH%/witness.php:8
    /// ---- TRACE 2 start (side trace 1/5) f() %SCRATCH%/witness.php:8
    /// ---- TRACE 2 stop (return)
    ///
    /// f:
    ///      ; (lines=33, args=2, vars=6, tmps=14, ssa_vars=32)
    ///      ; (JIT)
    ///      ; %SCRATCH%/witness.php:3-15
    ///      ; return  [rc1, rcn, null, false, long, double, object]
    ///      ; #0.CV0($a) NOVAL [undef]
    ///      ; #1.CV1($s) NOVAL [undef]
    ///      ; #2.CV2($n) NOVAL [undef]
    ///      ; #3.CV3($i) NOVAL [undef]
    ///      ; #4.CV4($t) NOVAL [undef]
    ///      ; #5.CV5($r) NOVAL [undef]
    /// BB0:
    ///      ; start lines=[0-0]
    ///      ; to=(BB1)
    ///      ; level=0
    ///      ; children=(BB1)
    /// 0000 #6.CV0($a) [rc1, rcn, any] = RECV 1
    ///
    /// BB1:
    ///      ; recv follow entry lines=[1-1]
    ///      ; from=(BB0)
    ///      ; to=(BB2)
    ///      ; idom=BB0
    ///      ; level=1
    ///      ; children=(BB2)
    /// 0001 #7.CV1($s) [rc1, rcn, any] = RECV 2
    ///
    /// BB2:
    ///      ; recv follow entry lines=[2-3]
    ///      ; from=(BB1)
    ///      ; to=(BB3)
    ///      ; idom=BB1
    ///      ; level=2
    ///      ; children=(BB3)
    /// 0002 ASSIGN #2.CV2($n) NOVAL [undef] -> #8.CV2($n) [long] RANGE[0..0] int(0)
    /// 0003 ASSIGN #3.CV3($i) NOVAL [undef] -> #9.CV3($i) [long] RANGE[0..0] int(0)
    ///
    /// BB3:
    ///      ; follow target loop_header lines=[4-5]
    ///      ; from=(BB2, BB6)
    ///      ; to=(BB5, BB4)
    ///      ; idom=BB2
    ///      ; level=3
    ///      ; children=(BB4, BB5)
    ///      #10.CV0($a) [rc1, rcn, any] = Phi(#6.CV0($a) [rc1, rcn, any], #17.CV0($a) [rc1, rcn, any])
    ///      #11.CV1($s) [rc1, rcn, any] = Phi(#7.CV1($s) [rc1, rcn, any], #26.CV1($s) [rc1, rcn, any])
    ///      #12.CV2($n) [rc1, rcn, false, long, double, object] = Phi(#8.CV2($n) [long] RANGE[0..0], #25.CV2($n) [rc1, rcn, false, long, double, object])
    ///      #13.CV3($i) [long, double] RANGE[0..++] = Phi(#9.CV3($i) [long] RANGE[0..0], #31.CV3($i) [long, double] RANGE[1..++])
    ///      #14.CV4($t) NOVAL [undef, rc1, rcn, string] = Phi(#4.CV4($t) NOVAL [undef], #28.CV4($t) NOVAL [rc1, rcn, string])
    ///      #15.CV5($r) NOVAL [undef, rc1, packed array [long] of [long]] = Phi(#5.CV5($r) NOVAL [undef], #30.CV5($r) NOVAL [rc1, packed array [long] of [long]])
    /// 0004 #16.T8 [bool] = IS_IDENTICAL #13.CV3($i) [long, double] RANGE[0..++] int(10)
    /// 0005 JMPZ #16.T8 [bool] BB5
    ///
    /// BB4:
    ///      ; follow exit lines=[6-6]
    ///      ; from=(BB3)
    ///      ; idom=BB3
    ///      ; level=4
    /// 0006 RETURN #12.CV2($n) [rc1, rcn, false, long, double, object]
    ///
    /// BB5:
    ///      ; target lines=[7-17]
    ///      ; from=(BB3)
    ///      ; to=(BB6)
    ///      ; idom=BB3
    ///      ; level=4
    ///      ; loop_header=3
    ///      ; children=(BB6)
    /// 0007 INIT_FCALL 2 112 string("count")
    /// 0008 SEND_VAR #10.CV0($a) [rc1, rcn, any] -> #17.CV0($a) [rc1, rcn, any] 1
    /// 0009 SEND_VAL int(0) 2
    /// 0010 #18.V9 [long] = DO_ICALL
    /// 0011 INIT_FCALL 1 96 string("strlen")
    /// 0012 SEND_VAR #11.CV1($s) [rc1, rcn, any] -> #19.CV1($s) [rc1, rcn, any] 1
    /// 0013 #20.V10 [long] = DO_ICALL
    /// 0014 #21.T11 [long, double] = ADD #18.V9 [long] #20.V10 [long]
    /// 0015 INIT_FCALL 1 112 string("twice")
    /// 0016 SEND_VAR #13.CV3($i) [long, double] RANGE[0..++] -> #22.CV3($i) [long, double] RANGE[0..++] 1
    /// 0017 #23.V12 [rc1, rcn, any] = DO_UCALL
    ///
    /// BB6:
    ///      ; follow entry lines=[18-31]
    ///      ; from=(BB5)
    ///      ; to=(BB3, BB7)
    ///      ; idom=BB5
    ///      ; level=5
    ///      ; loop_header=3
    ///      ; children=(BB7)
    /// 0018 #24.T13 [rc1, false, long, double, object] = ADD #21.T11 [long, double] #23.V12 [rc1, rcn, any]
    /// 0019 ASSIGN_OP (ADD) #12.CV2($n) [rc1, rcn, false, long, double, object] -> #25.CV2($n) [rc1, rcn, false, long, double, object] #24.T13 [rc1, false, long, double, object]
    /// 0020 INIT_FCALL 2 112 string("str_repeat")
    /// 0021 SEND_VAR #19.CV1($s) [rc1, rcn, any] -> #26.CV1($s) [rc1, rcn, any] 1
    /// 0022 SEND_VAL int(2) 2
    /// 0023 #27.V15 [rc1, string] = DO_ICALL
    /// 0024 ASSIGN #14.CV4($t) NOVAL [undef, rc1, rcn, string] -> #28.CV4($t) NOVAL [rc1, rcn, string] #27.V15 [rc1, string]
    /// 0025 INIT_FCALL 2 112 string("range")
    /// 0026 SEND_VAL int(1) 1
    /// 0027 SEND_VAL int(3) 2
    /// 0028 #29.V17 [rc1, packed array [long] of [long]] = DO_ICALL
    /// 0029 ASSIGN #15.CV5($r) NOVAL [undef, rc1, packed array [long] of [long]] -> #30.CV5($r) NOVAL [rc1, packed array [long] of [long]] #29.V17 [rc1, packed array [long] of [long]]
    /// 0030 PRE_INC #22.CV3($i) [long, double] RANGE[0..++] -> #31.CV3($i) [long, double] RANGE[1..++]
    /// 0031 JMPNZ bool(true) BB3
    ///
    /// BB7:
    ///      ; follow exit lines=[32-32]
    ///      ; from=(BB6)
    ///      ; idom=BB6
    ///      ; level=6
    /// 0032 RETURN null
    /// ---- TRACE 2 compiled
    /// ```
    fn internal_func_info(&self, call: &Call<'_, T::Class>) -> Types {
        let callee = &call.callee;
        if callee.method.is_some() {
            return Types::NONE;
        }
        if callee.name == b"range" {
            return self.range_info(call);
        }
        let Some(mut info) = table_info(callee.name) else {
            return Types::NONE;
        };
        if info.has(Types::ARRAY) {
            info |= Types::EMPTY;
        }
        info
    }

    /// `zend_range_info`: the array `range()` returns, its elements typed
    /// by the arguments where it is passed two or three by position.
    fn range_info(&self, call: &Call<'_, T::Class>) -> Types {
        let n = call.args.len();
        if call.send_unpack || !(n == 2 || n == 3) {
            // May throw.
            return Types::RC1
                | Types::ARRAY
                | Types::EMPTY
                | Types::PACKED
                | Types::OF_LONG
                | Types::OF_DOUBLE
                | Types::OF_STRING;
        }
        let arg = |i: usize| {
            (call.args.get(i).copied().flatten()).map_or(Types::NONE, |at| self.op1_types(at))
        };
        let (t1, t2, t3) = (arg(0), arg(1), if n == 3 { arg(2) } else { Types::NONE });
        let mut tmp = Types::RC1 | Types::ARRAY;
        if t1.has(Types::STRING) && t2.has(Types::STRING) {
            tmp |= Types::OF_LONG | Types::OF_DOUBLE | Types::OF_STRING;
        }
        let numeric = Types::DOUBLE | Types::STRING;
        if t1.has(numeric) || t2.has(numeric) || t3.has(numeric) {
            tmp |= Types::OF_DOUBLE;
        }
        let but_double = (Types::ANY | Types::UNDEF).without(Types::DOUBLE);
        if t1.has(but_double) && t2.has(but_double) {
            tmp |= Types::OF_LONG;
        }
        if tmp.has(Types::OF_ANY) {
            tmp |= Types::PACKED;
        }
        tmp
    }

    /// `zend_inference_calc_range` for what a call returns: the range the
    /// last analysis of the user function it calls found it returns; none
    /// for any other.
    pub(crate) fn call_range(&self, at: usize) -> Option<Range> {
        let call = self.calls?.at(at)?;
        match call.callee.body {
            Body::User(Some(returns)) if !call.is_prototype && returns.has_range => {
                Some(returns.range)
            }
            _ => None,
        }
    }
}
