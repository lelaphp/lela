//! Constants: the persistent ones, the special ones, `const`, a
//! constant the compiler cannot fold, and what the constant evaluator
//! leaves.

use super::*;

#[test]
fn a_persistent_constant_stands_for_its_value_where_it_is_written() {
    assert_eq!(
        echoed_literal(b"<?php echo INF;").as_double(),
        Some(f64::INFINITY)
    );
    assert_eq!(
        echoed_literal(b"<?php echo -INF;").as_double(),
        Some(f64::NEG_INFINITY)
    );
    assert_eq!(
        echoed_literal(b"<?php echo \\NAN;")
            .as_double()
            .map(f64::is_nan),
        Some(true),
        "a leading separator names the same global constant"
    );
}

#[test]
fn the_engine_s_persistent_constants_stand_for_their_values_where_they_are_written() {
    use php_unit::decl::ConstExpr;
    let mut strings = php_heap::Strings::new();
    let options = Options {
        persistent: indexed(vec![
            ConstDecl {
                name: Name(b"T_LNUMBER".to_vec().into_boxed_slice()),
                value: ConstExpr::long(260),
                attributes: Vec::new(),
            },
            ConstDecl {
                name: Name(b"PHP_OS".to_vec().into_boxed_slice()),
                value: ConstExpr::Str(b"Darwin".to_vec().into_boxed_slice()),
                attributes: Vec::new(),
            },
        ]),
        ..Options::default()
    };
    let compiled = compile_parsed(
        b"<?php echo T_LNUMBER - 1, \\PHP_OS;",
        &options,
        &mut strings,
    );
    let unit = compiled.expect("the source compiles");
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=3, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n\
         0000 ECHO int(259)\n\
         0001 ECHO string(\"Darwin\")\n\
         0002 RETURN int(1)\n"
    );
    assert_eq!(unit.main.literals[0].as_long(), Some(259));
    assert_eq!(
        unit.main.literals[1].as_str().map(|s| s.bytes().to_vec()),
        Some(b"Darwin".to_vec())
    );
    let lowered = compile_parsed(b"<?php echo t_lnumber;", &options, &mut strings);
    assert_eq!(
        lowered.expect("the source compiles").dump(&[]).text(),
        "\n$_main:\n     ; (lines=3, args=0, vars=0, tmps=1)\n\
         \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n\
         0000 T0 = FETCH_CONSTANT string(\"t_lnumber\")\n\
         0001 ECHO T0\n\
         0002 RETURN int(1)\n",
        "a constant's name is case-sensitive, so the lowered spelling folds to nothing \
         and is fetched as the program serves"
    );
}

/// `zend_try_ct_eval_magic_const`'s `T_DIR` arm (#366): `__DIR__` is
/// `zend_dirname` over the file's name, and where that answers `.`,
/// the working directory of the request, `VCWD_GETCWD`, which is per
/// request under ZTS and so the option's, never the process's. Two
/// units compiled for two requests in one process read two
/// directories.
#[test]
fn dir_is_the_option_s_working_directory_where_the_name_has_no_directory() {
    fn dir(filename: &[u8], cwd: &[u8]) -> Option<Vec<u8>> {
        let mut strings = php_heap::Strings::new();
        let options = Options {
            filename,
            cwd,
            ..Options::default()
        };
        let unit = compile_parsed(b"<?php echo __DIR__;", &options, &mut strings)
            .expect("the source compiles");
        literal_text(&unit, 0)
    }
    assert_eq!(
        dir(b"witness.php", b"/srv/one").as_deref(),
        Some(b"/srv/one".as_slice())
    );
    assert_eq!(
        dir(b"witness.php", b"/srv/two").as_deref(),
        Some(b"/srv/two".as_slice()),
        "the same name under another request's directory is that directory"
    );
    assert_eq!(
        dir(b"/var/www/index.php", b"/srv/one").as_deref(),
        Some(b"/var/www".as_slice()),
        "a name with a directory in it is its dirname, whatever the request's"
    );
}

/// A constant no table here holds was refused until #380 and is now
/// `zend_compile_const`'s `FETCH_CONSTANT`, looked up as the program
/// serves. The oracle's dump under `[regime.dump]` at 8.5.9 over
/// `echo FOO;` is the same three oplines.
#[test]
fn a_constant_no_table_holds_is_fetched_as_the_program_serves() {
    let (unit, _strings) = unit_and_strings(b"<?php echo FOO;");
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=3, args=0, vars=0, tmps=1)\n\
         \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n\
         0000 T0 = FETCH_CONSTANT string(\"FOO\")\n\
         0001 ECHO T0\n\
         0002 RETURN int(1)\n"
    );
}

/// `true`, `false` and `null` written with the leading `\` are the
/// values `zend_try_ct_eval_const` substitutes, as they are without
/// it: `zend_resolve_const_name` strips the `\` and
/// `zend_get_special_const` reads the rest. The oracle's dump under
/// `[regime.dump]` at 8.5.9 over the same source.
#[test]
fn a_special_constant_written_fully_qualified_is_substituted() {
    assert_eq!(
        dump_of(b"<?php echo \\true, \\NULL, \\False, TRUE;"),
        main_dump(
            5,
            0,
            0,
            "0000 ECHO bool(true)\n\
             0001 ECHO null\n\
             0002 ECHO bool(false)\n\
             0003 ECHO bool(true)\n\
             0004 RETURN int(1)\n"
        )
    );
}

/// `zend_compile_const_decl` (#476): one `DECLARE_CONST` per
/// declarator, in written order, before anything else the file
/// does, the name prefixed with the namespace the statement stands
/// in and the value folded. The dumps are the oracle's under
/// `[regime.dump]` at 8.5.9.
#[test]
fn a_const_statement_is_one_declare_const_per_declarator_over_the_folded_value() {
    assert_eq!(
        dump_of(b"<?php const K = 1;"),
        main_dump(
            2,
            0,
            0,
            "0000 DECLARE_CONST string(\"K\") int(1)\n0001 RETURN int(1)\n"
        )
    );
    assert_eq!(
        dump_of(b"<?php const K = 1, L = 2;"),
        main_dump(
            3,
            0,
            0,
            "0000 DECLARE_CONST string(\"K\") int(1)\n\
             0001 DECLARE_CONST string(\"L\") int(2)\n\
             0002 RETURN int(1)\n"
        )
    );
    assert_eq!(
        dump_of(b"<?php const K = 1 + 2;"),
        main_dump(
            2,
            0,
            0,
            "0000 DECLARE_CONST string(\"K\") int(3)\n0001 RETURN int(1)\n"
        )
    );
    assert_eq!(
        dump_of(b"<?php namespace A; const K = 1;"),
        main_dump(
            2,
            0,
            0,
            "0000 DECLARE_CONST string(\"A\\\\K\") int(1)\n0001 RETURN int(1)\n"
        )
    );
}

/// What stands beside a `const` statement and is refused: php's
/// `zend_get_special_const` fatal over the three names no file may
/// redeclare, php's own parse error for the statement written where
/// its grammar has no `top_statement`, and what this compiler
/// cannot fold. Each sentence is the oracle's at 8.5.9.
#[test]
fn what_a_const_statement_refuses_is_what_php_refuses() {
    let refused = |source: &[u8]| {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &bundled(), &mut strings)
            .err()
            .map(|e| e.message.into_owned())
    };
    assert_eq!(
        refused(b"<?php const true = 1;").as_deref(),
        Some("Cannot redeclare constant 'true'")
    );
    assert_eq!(
        refused(b"<?php const NULL = 1;").as_deref(),
        Some("Cannot redeclare constant 'NULL'")
    );
    assert_eq!(
        refused(b"<?php if (1) { const K = 1; }").as_deref(),
        Some("syntax error, unexpected token \"const\"")
    );
    assert_eq!(
        refused(b"<?php function g() { const K = 1; }").as_deref(),
        Some("syntax error, unexpected token \"const\"")
    );
    // The `;` php's grammar requires after the list, whose absence
    // php's parser names and this one names the same way.
    assert_eq!(
        refused(b"<?php const K = 1").as_deref(),
        Some("syntax error, unexpected end of file, expecting \",\" or \";\"")
    );
}

/// `zend_compile_const`'s second half (#380), the oracle's dump
/// under `[regime.dump]` at 8.5.9: a name `zend_try_ct_eval_const`
/// cannot fold is `FETCH_CONSTANT` into a temporary over the name
/// `zend_resolve_const_name` resolved. A bare name inside a
/// namespace is the namespace's spelling under the bit
/// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE`, which the handler reads
/// to try the global spelling after the namespaced one; a name
/// written with a leading `\` is the global one and carries no
/// bit; and a name with a `\` in it takes the namespace before it,
/// so `n\Q` inside `n` is `n\n\Q`.
#[test]
fn a_constant_the_compiler_cannot_fold_is_the_oracle_s_fetch_constant() {
    assert_eq!(
        dump_of(b"<?php namespace n; define(\"n\\\\K\", 2); var_dump(K, \\W, n\\Q, NOPE);"),
        main_dump(
            15,
            0,
            6,
            "0000 INIT_NS_FCALL_BY_NAME 2 string(\"n\\\\define\")\n\
             0001 SEND_VAL_EX string(\"n\\\\K\") 1\n\
             0002 SEND_VAL_EX int(2) 2\n\
             0003 DO_FCALL_BY_NAME\n\
             0004 INIT_NS_FCALL_BY_NAME 4 string(\"n\\\\var_dump\")\n\
             0005 T1 = FETCH_CONSTANT (unqualified-in-namespace) string(\"n\\\\K\")\n\
             0006 SEND_VAL_EX T1 1\n\
             0007 T2 = FETCH_CONSTANT string(\"W\")\n\
             0008 SEND_VAL_EX T2 2\n\
             0009 T3 = FETCH_CONSTANT string(\"n\\\\n\\\\Q\")\n\
             0010 SEND_VAL_EX T3 3\n\
             0011 T4 = FETCH_CONSTANT (unqualified-in-namespace) string(\"n\\\\NOPE\")\n\
             0012 SEND_VAL_EX T4 4\n\
             0013 DO_FCALL_BY_NAME\n\
             0014 RETURN int(1)\n"
        )
    );
}

/// `zend_compile_class_const` runs `zend_eval_const_expr` over the class
/// before it compiles it, so `"ab"[0]::d` reads the class `a` the
/// dimension folds to, one `FETCH_CLASS_CONSTANT string("a")
/// string("d")`, while the same dimension under a `new`, which runs no
/// evaluator, is the `FETCH_DIM_R` and the `FETCH_CLASS` over it. The
/// dump is the oracle's under `[regime.dump]` at 8.5.9 (#2691).
#[test]
fn the_class_of_a_class_constant_is_read_as_the_constant_evaluator_folds_it() {
    assert_eq!(
        dump_of(b"<?php echo \"ab\"[0]::d; new (\"ab\"[0]);"),
        "\n$_main:\n\
         \x20    ; (lines=8, args=0, vars=0, tmps=5)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 T0 = FETCH_CLASS_CONSTANT string(\"a\") string(\"d\")\n\
         0001 ECHO T0\n\
         0002 T1 = FETCH_DIM_R string(\"ab\") int(0)\n\
         0003 V2 = FETCH_CLASS (exception) T1\n\
         0004 V3 = NEW 0 V2\n\
         0005 DO_FCALL\n\
         0006 FREE V3\n\
         0007 RETURN int(1)\n\
         LIVE RANGES:\n\
         \x20    3: 0005 - 0006 (new)\n\
         "
    );
}

/// `C::class`, which was refused as not read, as the oracle's
/// `[regime.dump]` has it: a name written out folds to its resolved
/// spelling, `self` in a known scope to the class being compiled,
/// and `static`, `self` where the scope is the includer's and a
/// value are `FETCH_CLASS_NAME`.
#[test]
fn a_class_name_folds_where_php_resolves_it_and_is_fetched_where_not() {
    assert_eq!(
        oplines_of(b"<?php namespace N; echo Foo::class, \\Foo::class, self::class, static::class, $o::class;"),
        "0000 ECHO string(\"N\\\\Foo\")\n\
         0001 ECHO string(\"Foo\")\n\
         0002 T1 = FETCH_CLASS_NAME (self)\n\
         0003 ECHO T1\n\
         0004 T2 = FETCH_CLASS_NAME (static)\n\
         0005 ECHO T2\n\
         0006 T3 = FETCH_CLASS_NAME CV0($o)\n\
         0007 ECHO T3\n\
         0008 RETURN int(1)\n"
    );
}

/// php passes a `static` variable's initializer through
/// `zend_eval_const_expr` before it compiles it, and the evaluator
/// replaces a `??` whose left side is `null` with its right side, so
/// `static $v = null ?? A[1] ?? 1;` compiles `A[1] ?? 1`: one `COALESCE`
/// over the `FETCH_DIM_IS`. Outside an initializer nothing folds the
/// `??`, and `$x = null ?? $a ?? 1;` keeps both. The dump is the oracle's
/// under `[regime.dump]` at 8.5.9 (#2681).
#[test]
fn a_static_initializer_compiles_what_the_constant_evaluator_left_of_it() {
    assert_eq!(
        dump_of(b"<?php function f() { static $v = null ?? A[1] ?? 1; } $x = null ?? $a ?? 1;"),
        "\n$_main:\n\
         \x20    ; (lines=6, args=0, vars=2, tmps=3)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 T2 = COALESCE null 0004\n\
         0001 T3 = COALESCE CV1($a) 0003\n\
         0002 T3 = QM_ASSIGN int(1)\n\
         0003 T2 = QM_ASSIGN T3\n\
         0004 ASSIGN CV0($x) T2\n\
         0005 RETURN int(1)\n\
         \nf:\n\
         \x20    ; (lines=7, args=0, vars=1, tmps=3)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 BIND_INIT_STATIC_OR_JMP CV0($v) 0006\n\
         0001 T1 = FETCH_CONSTANT string(\"A\")\n\
         0002 T2 = FETCH_DIM_IS T1 int(1)\n\
         0003 T3 = COALESCE T2 0005\n\
         0004 T3 = QM_ASSIGN int(1)\n\
         0005 BIND_STATIC (ref) CV0($v) T3\n\
         0006 RETURN null\n\
         "
    );
}

/// `zend_try_ct_eval_array` evaluates every key and value of an array
/// before it knows whether the array folds, and what the evaluator left
/// of each is what `zend_compile_array` compiles where it does not: a
/// conditional over a constant is the branch it picks, a cast over a
/// constant its value, a `??` over `null` its right side. The dump is the
/// oracle's under `[regime.dump]` at 8.5.9 (#2681).
#[test]
fn an_array_element_compiles_what_the_constant_evaluator_left_of_it() {
    assert_eq!(
        dump_of(b"<?php $x = [true ? $a : 2, $k => (int) \"4\"]; $d = [&$e, 1 ? 2 : 3]; static $s = [null ?? $b];"),
        "\n$_main:\n\
         \x20    ; (lines=10, args=0, vars=7, tmps=5)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 T7 = INIT_ARRAY 2 (packed) CV1($a) NEXT\n\
         0001 T7 = ADD_ARRAY_ELEMENT int(4) CV2($k)\n\
         0002 ASSIGN CV0($x) T7\n\
         0003 T9 = INIT_ARRAY 2 (packed) (ref) CV4($e) NEXT\n\
         0004 T9 = ADD_ARRAY_ELEMENT int(2) NEXT\n\
         0005 ASSIGN CV3($d) T9\n\
         0006 BIND_INIT_STATIC_OR_JMP CV5($s) 0009\n\
         0007 T11 = INIT_ARRAY 1 (packed) CV6($b) NEXT\n\
         0008 BIND_STATIC (ref) CV5($s) T11\n\
         0009 RETURN int(1)\n\
         LIVE RANGES:\n\
         \x20    7: 0001 - 0002 (tmp/var)\n\
         \x20    9: 0004 - 0005 (tmp/var)\n\
         "
    );
}

/// `determine_switch_jumptable_type` and `can_match_use_jumptable` read
/// each case through `zend_eval_const_expr`, which folds a conditional
/// over a constant, so five long cases the last of which is `true ? 5 :
/// 6` are a `SWITCH_LONG` and the case compiles to `int(5)`, and folds no
/// call, so a `match` over `1` and `strlen("ab")` is the chain of
/// `IS_IDENTICAL` though the compile folds the call to `int(2)`. The dump
/// is the oracle's under `[regime.dump]` at 8.5.9 (#2681).
#[test]
fn a_switch_and_a_match_read_their_cases_as_the_constant_evaluator_folds_them() {
    assert_eq!(
        dump_of(b"<?php switch ($x) { case 1: case 2: case 3: case 4: case true ? 5 : 6: echo 1; } echo match ($x) { 1 => 1, strlen(\"ab\") => 2, default => 3 };"),
        "\n$_main:\n\
         \x20    ; (lines=26, args=0, vars=1, tmps=3)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 SWITCH_LONG CV0($x) 1: 0012, 2: 0012, 3: 0012, 4: 0012, 5: 0012, default: 0013\n\
         0001 T1 = IS_EQUAL CV0($x) int(1)\n\
         0002 JMPNZ T1 0012\n\
         0003 T1 = IS_EQUAL CV0($x) int(2)\n\
         0004 JMPNZ T1 0012\n\
         0005 T1 = IS_EQUAL CV0($x) int(3)\n\
         0006 JMPNZ T1 0012\n\
         0007 T1 = IS_EQUAL CV0($x) int(4)\n\
         0008 JMPNZ T1 0012\n\
         0009 T1 = IS_EQUAL CV0($x) int(5)\n\
         0010 JMPNZ T1 0012\n\
         0011 JMP 0013\n\
         0012 ECHO int(1)\n\
         0013 T2 = IS_IDENTICAL CV0($x) int(1)\n\
         0014 JMPNZ T2 0018\n\
         0015 T2 = IS_IDENTICAL CV0($x) int(2)\n\
         0016 JMPNZ T2 0020\n\
         0017 JMP 0022\n\
         0018 T3 = QM_ASSIGN int(1)\n\
         0019 JMP 0024\n\
         0020 T3 = QM_ASSIGN int(2)\n\
         0021 JMP 0024\n\
         0022 T3 = QM_ASSIGN int(3)\n\
         0023 JMP 0024\n\
         0024 ECHO T3\n\
         0025 RETURN int(1)\n\
         LIVE RANGES:\n\
         \x20    3: 0023 - 0024 (tmp/var)\n\
         "
    );
}
