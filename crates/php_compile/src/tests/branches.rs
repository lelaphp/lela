//! Branches: `switch`, `match`, the conditional and `goto`, their jump
//! tables, chains, lines and refusals.

use super::*;

/// A constant subject over a jump table: `zend_emit_op` sets `op1`
/// before `op2`, so the subject's slot comes before the table's.
#[test]
fn a_constant_subject_takes_its_slot_before_the_jump_table() {
    let (unit, _strings) =
        unit_and_strings(b"<?php switch ('z') { case 'a': echo 1; break; case 'b': echo 2; }");
    let switch = unit
        .main
        .oplines
        .iter()
        .find(|op| op.opcode == Opcode::SwitchString)
        .map(|op| (op.op1_type, op.op1, op.op2_type, op.op2));
    assert_eq!(
        switch,
        Some((OperandKind::Const, 0, OperandKind::Const, 1)),
        "a string jump table over two cases"
    );
    assert_eq!(literal_text(&unit, 0).as_deref(), Some(&b"z"[..]));
    assert!(unit.main.literals[1].as_arr().is_some());
}

/// `zend_resolve_goto_label` nulls the label's slot once it has found
/// the label (`Z/Zend/zend_compile.c:5907`), and every later
/// constant is appended after it (`zend_add_literal`), the
/// same `'a'` twice in two slots, as phpdbg's `info literal` shows
/// php's table.
#[test]
fn a_goto_s_label_slot_is_nulled_and_no_slot_is_shared() {
    let (unit, _strings) = unit_and_strings(b"<?php goto a; a: echo 'a'; echo 'a';");
    assert_eq!(unit.main.literals.len(), 4);
    assert_eq!(unit.main.literals[0], php_heap::Zval::null());
    assert_eq!(literal_text(&unit, 1).as_deref(), Some(&b"a"[..]));
    assert_eq!(literal_text(&unit, 2).as_deref(), Some(&b"a"[..]));
    assert_eq!(unit.main.literals[3].as_long(), Some(1));
}

/// #256's program under the oracle's `[regime.dump]`: a `switch` of
/// fewer cases than `should_use_jumptable` asks for is the
/// `IS_EQUAL` and `JMPNZ` chain into one shared temporary, each
/// case's `break` a `JMP` to the switch's end, and a `match` of two
/// constant conditions is `MATCH` over its jump table with a
/// `QM_ASSIGN` per arm into one temporary and the live range over it.
#[test]
fn a_switch_of_few_cases_is_the_is_equal_chain_and_a_match_is_its_jump_table() {
    let source = b"<?php\n$a = 1;\nswitch ($a) { case 1: echo \"a\"; break; case 2: echo \"b\"; break; default: echo \"c\"; }\necho match ($a) { 1 => \"x\", \"s\" => \"y\", default => \"z\" };\n";
    let (unit, _strings) = unit_and_strings(source);
    expect![[r#"

        $_main:
             ; (lines=20, args=0, vars=1, tmps=4)
             ; (before optimizer)
             ; :1-5
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) int(1)
        0001 T2 = IS_EQUAL CV0($a) int(1)
        0002 JMPNZ T2 0006
        0003 T2 = IS_EQUAL CV0($a) int(2)
        0004 JMPNZ T2 0008
        0005 JMP 0010
        0006 ECHO string("a")
        0007 JMP 0011
        0008 ECHO string("b")
        0009 JMP 0011
        0010 ECHO string("c")
        0011 MATCH CV0($a) 1: 0012, "s": 0014, default: 0016
        0012 T4 = QM_ASSIGN string("x")
        0013 JMP 0018
        0014 T4 = QM_ASSIGN string("y")
        0015 JMP 0018
        0016 T4 = QM_ASSIGN string("z")
        0017 JMP 0018
        0018 ECHO T4
        0019 RETURN int(1)
        LIVE RANGES:
             4: 0017 - 0018 (tmp/var)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    let table = &unit.main.oplines[11];
    assert_eq!(table.opcode, Opcode::Match);
    assert_eq!(table.op2_type, OperandKind::Const);
    let table = unit.main.literals[table.op2 as usize].as_arr();
    assert!(
        table.is_some_and(|table| table.is_immutable()),
        "a jump table is IS_ARRAY_IMMUTABLE"
    );
    assert_eq!(table.map(|table| table.len()), Some(2));
}

/// #395's program under the oracle's `[regime.dump]`:
/// `zend_compile_conditional` is `JMPZ` past the true arm, both arms
/// writing the one temporary, and a `JMP` over the false arm;
/// `zend_compile_shorthand_conditional` is `JMP_SET` over the left
/// operand with the right operand's `QM_ASSIGN` into the same
/// temporary; and a parenthesised conditional as a condition is the
/// `JMPZ` over the temporary the inner one left.
#[test]
fn a_conditional_is_jmpz_between_two_qm_assigns_and_a_shorthand_one_is_jmp_set() {
    expect![[r#"
        0000 ASSIGN CV0($a) int(1)
        0001 JMPZ CV0($a) 0004
        0002 T2 = QM_ASSIGN string("t")
        0003 JMP 0005
        0004 T2 = QM_ASSIGN string("f")
        0005 ECHO T2
        0006 T3 = JMP_SET CV0($a) 0008
        0007 T3 = QM_ASSIGN string("f")
        0008 ECHO T3
        0009 JMPZ CV0($a) 0012
        0010 T4 = QM_ASSIGN string("t")
        0011 JMP 0013
        0012 T4 = QM_ASSIGN string("f")
        0013 JMPZ T4 0016
        0014 T5 = QM_ASSIGN string("y")
        0015 JMP 0017
        0016 T5 = QM_ASSIGN string("n")
        0017 ECHO T5
        0018 RETURN int(1)
    "#]].assert_eq(&oplines_of(
            b"<?php\n$a = 1;\necho $a ? \"t\" : \"f\";\necho $a ?: \"f\";\necho ($a ? \"t\" : \"f\") ? \"y\" : \"n\";\n"
        ));
}

/// The three sentences `zend_compile_conditional` refuses an
/// unparenthesised nested conditional with, by which of the two
/// carried a middle arm; `(a ?: b) ?: c` is the fourth grouping,
/// which php lets stand. A conditional between parentheses carries
/// php's `ZEND_PARENTHESIZED_CONDITIONAL` and is no refusal either.
#[test]
fn an_unparenthesised_nested_conditional_is_refused_in_php_s_words() {
    for (source, message) in [
        (
            &b"<?php\necho 1 ? 2 : 3 ? 4 : 5;\n"[..],
            Some(
                "Unparenthesized `a ? b : c ? d : e` is not supported. Use either `(a ? b : c) ? d : e` or `a ? b : (c ? d : e)`",
            ),
        ),
        (
            b"<?php\necho 1 ? 2 : 3 ?: 4;\n",
            Some(
                "Unparenthesized `a ? b : c ?: d` is not supported. Use either `(a ? b : c) ?: d` or `a ? b : (c ?: d)`",
            ),
        ),
        (
            b"<?php\necho 1 ?: 2 ? 3 : 4;\n",
            Some(
                "Unparenthesized `a ?: b ? c : d` is not supported. Use either `(a ?: b) ? c : d` or `a ?: (b ? c : d)`",
            ),
        ),
        (b"<?php\necho 1 ?: 2 ?: 3;\n", None),
        (b"<?php\necho (1 ? 2 : 3) ? 4 : 5;\n", None),
        (b"<?php\necho 1 ? 2 : (3 ? 4 : 5);\n", None),
    ] {
        match (refused(source), message) {
            (Some(error), Some(message)) => {
                assert_eq!(error.level, Level::CompileError, "{message}");
                assert_eq!(error.message, message);
                assert_eq!(error.line, 2, "{message}");
            }
            (None, None) => {}
            (found, wanted) => assert_eq!(
                found.map(|error| error.message.to_string()),
                wanted.map(str::to_owned),
                "{:?}",
                String::from_utf8_lossy(source)
            ),
        }
    }
}

/// Five `long` cases, the default among them, are what
/// `should_use_jumptable` asks for: a `SWITCH_LONG` over the table
/// before the chain, its `extended_value` the default's target.
#[test]
fn a_switch_of_five_long_cases_is_switch_long_over_its_table_before_the_chain() {
    expect![[r#"
        0000 ASSIGN CV0($a) int(3)
        0001 SWITCH_LONG CV0($a) 1: 0013, 2: 0015, 3: 0017, 4: 0019, 5: 0021, default: 0023
        0002 T2 = IS_EQUAL CV0($a) int(1)
        0003 JMPNZ T2 0013
        0004 T2 = IS_EQUAL CV0($a) int(2)
        0005 JMPNZ T2 0015
        0006 T2 = IS_EQUAL CV0($a) int(3)
        0007 JMPNZ T2 0017
        0008 T2 = IS_EQUAL CV0($a) int(4)
        0009 JMPNZ T2 0019
        0010 T2 = IS_EQUAL CV0($a) int(5)
        0011 JMPNZ T2 0021
        0012 JMP 0023
        0013 ECHO string("a")
        0014 JMP 0024
        0015 ECHO string("b")
        0016 JMP 0024
        0017 ECHO string("c")
        0018 JMP 0024
        0019 ECHO string("d")
        0020 JMP 0024
        0021 ECHO string("e")
        0022 JMP 0024
        0023 ECHO string("f")
        0024 RETURN int(1)
    "#]].assert_eq(&oplines_of(b"<?php\n$a = 3;\nswitch ($a) { case 1: echo \"a\"; break; case 2: echo \"b\"; break; case 3: echo \"c\"; break; case 4: echo \"d\"; break; case 5: echo \"e\"; break; default: echo \"f\"; }\n"));
}

/// Two `string` cases are a `SWITCH_STRING`; with no default the
/// table's default target is the switch's end. A numeric string
/// case, which `==` compares as a number, keeps the chain alone
/// (`determine_switch_jumptable_type`).
#[test]
fn a_switch_of_two_string_cases_is_switch_string_and_a_numeric_string_case_is_not() {
    expect![[r#"
        0000 ASSIGN CV0($a) string("y")
        0001 SWITCH_STRING CV0($a) "x": 0007, "y": 0009, default: 0011
        0002 T2 = IS_EQUAL CV0($a) string("x")
        0003 JMPNZ T2 0007
        0004 T2 = IS_EQUAL CV0($a) string("y")
        0005 JMPNZ T2 0009
        0006 JMP 0011
        0007 ECHO string("a")
        0008 JMP 0011
        0009 ECHO string("b")
        0010 JMP 0011
        0011 RETURN int(1)
    "#]].assert_eq(&oplines_of(b"<?php\n$a = \"y\";\nswitch ($a) { case \"x\": echo \"a\"; break; case \"y\": echo \"b\"; break; }\n"));
    expect![[r#"
        0000 T1 = IS_EQUAL CV0($a) string("1")
        0001 JMPNZ T1 0005
        0002 T1 = IS_EQUAL CV0($a) string("x")
        0003 JMPNZ T1 0007
        0004 JMP 0008
        0005 ECHO string("a")
        0006 JMP 0008
        0007 ECHO string("b")
        0008 RETURN int(1)
    "#]]
    .assert_eq(&oplines_of(
        b"<?php\nswitch ($a) { case \"1\": echo \"a\"; break; case \"x\": echo \"b\"; }\n",
    ));
}

/// A constant `true` or `false` subject tests each case alone, a
/// `JMPNZ` or a `JMPZ` over it; any other constant is an `IS_EQUAL`
/// over two constants, swapped where `pass_two` swaps them; and a
/// value written as two cases keeps the first target in the table,
/// `zend_hash_index_add`, while the chain tests both.
#[test]
fn a_constant_subject_is_tested_as_php_tests_it_and_a_repeated_case_keeps_its_first_target() {
    expect![[r#"
        0000 JMPNZ CV0($a) 0003
        0001 JMPNZ int(2) 0005
        0002 JMP 0006
        0003 ECHO string("a")
        0004 JMP 0006
        0005 ECHO string("b")
        0006 JMPZ CV0($a) 0008
        0007 JMP 0009
        0008 ECHO string("a")
        0009 T3 = IS_EQUAL CV0($a) int(3)
        0010 JMPNZ T3 0014
        0011 T3 = IS_EQUAL int(3) int(1)
        0012 JMPNZ T3 0015
        0013 JMP 0016
        0014 ECHO string("a")
        0015 ECHO string("b")
        0016 SWITCH_LONG CV0($a) 1: 0028, 2: 0028, 3: 0028, 4: 0028, default: 0029
        0017 T4 = IS_EQUAL CV0($a) int(1)
        0018 JMPNZ T4 0028
        0019 T4 = IS_EQUAL CV0($a) int(2)
        0020 JMPNZ T4 0028
        0021 T4 = IS_EQUAL CV0($a) int(3)
        0022 JMPNZ T4 0028
        0023 T4 = IS_EQUAL CV0($a) int(4)
        0024 JMPNZ T4 0028
        0025 T4 = IS_EQUAL CV0($a) int(4)
        0026 JMPNZ T4 0028
        0027 JMP 0029
        0028 ECHO string("x")
        0029 RETURN int(1)
    "#]].assert_eq(&oplines_of(b"<?php\nswitch (true) { case $a: echo \"a\"; break; case 2: echo \"b\"; }\nswitch (false) { case $a: echo \"a\"; }\nswitch (3) { case $a: echo \"a\"; case 1: echo \"b\"; }\nswitch ($a) { case 1: case 2: case 3: case 4: case 4: echo \"x\"; }\n"));
}

/// A temporary subject is compared by `CASE`, which keeps it alive
/// for the next case, and freed at the switch's end, which is where
/// `break` lands; `continue` in a `for` lands on the step; a
/// `break 2` or `continue 2` out of a `switch` frees its subject on
/// the way, `ZEND_FREE_ON_RETURN`, and the live range is split at
/// that `FREE` with the distance to the loop's end in its `op2`.
#[test]
fn a_temporary_subject_is_case_and_freed_where_break_lands_and_on_the_way_out_of_a_switch() {
    expect![[r#"
        0000 T2 = STRLEN CV0($a)
        0001 T3 = CASE T2 int(1)
        0002 JMPNZ T3 0006
        0003 T3 = CASE T2 int(2)
        0004 JMPNZ T3 0008
        0005 JMP 0009
        0006 ECHO string("a")
        0007 JMP 0009
        0008 ECHO string("b")
        0009 FREE T2
        0010 ASSIGN CV1($i) int(0)
        0011 JMP 0015
        0012 JMP 0014
        0013 JMP 0017
        0014 PRE_INC CV1($i)
        0015 T6 = IS_SMALLER CV1($i) int(3)
        0016 JMPNZ T6 0012
        0017 JMP 0030
        0018 T7 = STRLEN CV0($a)
        0019 T8 = CASE T7 int(1)
        0020 JMPNZ T8 0024
        0021 T8 = CASE T7 int(2)
        0022 JMPNZ T8 0026
        0023 JMP 0028
        0024 FREE T7 loop-end(+5)
        0025 JMP 0030
        0026 FREE T7 loop-end(+3)
        0027 JMP 0031
        0028 JMP 0029
        0029 FREE T7
        0030 JMPNZ CV1($i) 0018
        0031 RETURN int(1)
        LIVE RANGES:
             2: 0001 - 0009 (tmp/var)
             7: 0019 - 0024 (tmp/var)
             7: 0028 - 0029 (tmp/var)
    "#]].assert_eq(&oplines_of(b"<?php\nswitch (strlen($a)) { case 1: echo \"a\"; break; case 2: echo \"b\"; }\nfor ($i = 0; $i < 3; $i++) { continue; break 1; }\nwhile ($i) { switch (strlen($a)) { case 1: continue 2; case 2: break 2; default: break; } }\n"));
}

/// `match` as `zend_compile_match` shapes it: two conditions of one
/// arm share a target; no default arm is a `MATCH_ERROR` emitted
/// first, the table's default target; one condition is the
/// `IS_IDENTICAL` chain with its default `JMP` to the `MATCH_ERROR`;
/// a default arm alone is the `JMP` to it; and no arm at all is the
/// constant `null`.
#[test]
fn a_match_is_its_table_or_the_is_identical_chain_and_match_error_where_no_arm_is_default() {
    expect![[r#"
        0000 ASSIGN CV0($a) int(2)
        0001 MATCH CV0($a) 1: 0003, 2: 0003, "s": 0005, default: 0002
        0002 MATCH_ERROR CV0($a)
        0003 T4 = QM_ASSIGN string("x")
        0004 JMP 0007
        0005 T4 = QM_ASSIGN string("y")
        0006 JMP 0007
        0007 ECHO T4
        0008 JMP 0009
        0009 T6 = QM_ASSIGN string("d")
        0010 JMP 0011
        0011 ECHO T6
        0012 T7 = IS_IDENTICAL CV0($a) int(1)
        0013 JMPNZ T7 0016
        0014 JMP 0015
        0015 MATCH_ERROR CV0($a)
        0016 T8 = QM_ASSIGN string("one")
        0017 JMP 0018
        0018 ECHO T8
        0019 JMP 0020
        0020 MATCH_ERROR CV0($a)
        0021 ASSIGN CV1($x) null
        0022 RETURN int(1)
        LIVE RANGES:
             4: 0006 - 0007 (tmp/var)
             6: 0010 - 0011 (tmp/var)
             8: 0017 - 0018 (tmp/var)
    "#]].assert_eq(&oplines_of(b"<?php\n$a = 2;\necho match ($a) { 1, 2 => \"x\", \"s\" => \"y\" };\necho match (true) { default => \"d\" };\necho match ($a) { 1 => \"one\" };\n$x = match ($a) {};\n"));
}

/// The lines are `zend_compile_expr_inner`'s, set as each expression
/// compiles and never restored: a case's opline carries the line its
/// condition starts on, a `MATCH` and its `MATCH_ERROR` the
/// subject's, an arm's `QM_ASSIGN` and `JMP` the body's, and the
/// `ECHO` after a `match` the last arm's, which the oracle's
/// `Undefined variable` warnings and `Unhandled match case` fatals
/// place there.
#[test]
fn a_case_and_an_arm_carry_the_lines_php_gives_them() {
    let (unit, _strings) = unit_and_strings(
        b"<?php\n$a = 9;\nswitch ($a) {\n  case\n   $u:\n    echo \"a\";\n  case $v:\n    echo \"b\";\n}\necho match ($a) {\n  $w => \"x\",\n  default => $z,\n};\necho match (\n  $a\n) {\n  1 => \"x\",\n  2 => \"y\",\n};\n",
    );
    let lines: Vec<(Opcode, u32)> = unit
        .main
        .oplines
        .iter()
        .map(|op| (op.opcode, op.lineno))
        .collect();
    assert_eq!(
        lines,
        vec![
            (Opcode::Assign, 2),
            (Opcode::IsEqual, 5),
            (Opcode::Jmpnz, 5),
            (Opcode::IsEqual, 7),
            (Opcode::Jmpnz, 7),
            (Opcode::Jmp, 7),
            (Opcode::Echo, 6),
            (Opcode::Echo, 8),
            (Opcode::IsIdentical, 11),
            (Opcode::Jmpnz, 11),
            (Opcode::Jmp, 11),
            (Opcode::QmAssign, 11),
            (Opcode::Jmp, 11),
            (Opcode::QmAssign, 12),
            (Opcode::Jmp, 12),
            (Opcode::Echo, 12),
            (Opcode::Match, 15),
            (Opcode::MatchError, 15),
            (Opcode::QmAssign, 17),
            (Opcode::Jmp, 17),
            (Opcode::QmAssign, 18),
            (Opcode::Jmp, 18),
            (Opcode::Echo, 18),
            (Opcode::Return, 20),
        ]
    );
}

/// `zend_compile_break_continue`'s refusals, each at `E_COMPILE_ERROR`
/// in php's words, and `zend_compile_switch`'s and
/// `zend_compile_match`'s one default each.
#[test]
fn what_php_s_compiler_refuses_in_a_break_a_switch_and_a_match_is_refused_in_its_words() {
    for (source, message) in [
        (
            &b"<?php\nbreak;\n"[..],
            "'break' not in the 'loop' or 'switch' context",
        ),
        (
            b"<?php\ncontinue;\n",
            "'continue' not in the 'loop' or 'switch' context",
        ),
        (b"<?php\nfor (;;) { break 2; }\n", "Cannot 'break' 2 levels"),
        (
            b"<?php\nfor (;;) { continue 3; }\n",
            "Cannot 'continue' 3 levels",
        ),
        (
            b"<?php\nfor (;;) { break 0; }\n",
            "'break' operator accepts only positive integers",
        ),
        (
            b"<?php\nfor (;;) { break \"1\"; }\n",
            "'break' operator accepts only positive integers",
        ),
        (
            b"<?php\nfor (;;) { break $a; }\n",
            "'break' operator with non-integer operand is no longer supported",
        ),
        (
            b"<?php\nfor (;;) { switch ($a) { case 1: default: break; default: } }\n",
            "Switch statements may only contain one default clause",
        ),
        (
            b"<?php\necho match ($a) { 1 => 2, default => 3, default => 4 };\n",
            "Match expressions may only contain one default arm",
        ),
    ] {
        let error = refused(source).unwrap_or_else(|| {
            Refused {
                level: Level::Parse,
                message: String::from("compiled").into(),
                line: 0,
                raised: Vec::new(),
                raise: crate::Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into()
        });
        assert_eq!(error.level, Level::CompileError, "{message}");
        assert_eq!(error.message, message);
        assert_eq!(error.line, 2, "{message}");
    }
}

/// A `continue` that targets a `switch` is the `break` it is, with
/// php's warning on the unit, the hint about the loop outside where
/// there is one. php raises it as `E_WARNING`, not
/// `E_COMPILE_WARNING` (`Z/Zend/zend_compile.c:5860`), so it is one a
/// user error handler is handed.
#[test]
fn a_continue_targeting_a_switch_is_a_break_with_php_s_warning() {
    let (unit, _strings) = unit_and_strings(
        b"<?php\nfor (;;) { switch ($a) { case 1: continue; } }\nswitch ($a) { case 1: continue; }\nfor (;;) { switch ($a) { case 1: continue 2; } }\nfor (;;) { switch ($a) { case 1: for (;;) { continue 2; } } }\n",
    );
    let raised: Vec<(Level, u32, &str)> = unit
        .diagnostics
        .iter()
        .map(|d| (d.level, d.line, &*d.message))
        .collect();
    assert_eq!(
        raised,
        vec![
            (
                Level::Warning,
                2,
                "\"continue\" targeting switch is equivalent to \"break\". Did you mean to use \"continue 2\"?"
            ),
            (
                Level::Warning,
                3,
                "\"continue\" targeting switch is equivalent to \"break\""
            ),
            (
                Level::Warning,
                5,
                "\"continue 2\" targeting switch is equivalent to \"break 2\". Did you mean to use \"continue 3\"?"
            ),
        ]
    );
    expect![[r#"
        0000 JMP 0005
        0001 T1 = IS_EQUAL CV0($a) int(1)
        0002 JMPNZ T1 0004
        0003 JMP 0005
        0004 JMP 0005
        0005 JMPNZ bool(true) 0001
        0006 T2 = IS_EQUAL CV0($a) int(1)
        0007 JMPNZ T2 0009
        0008 JMP 0010
        0009 JMP 0010
        0010 RETURN int(1)
    "#]].assert_eq(&oplines_of(b"<?php\nfor (;;) { switch ($a) { case 1: continue; } }\nswitch ($a) { case 1: continue; }\n"));
}

#[test]
fn a_goto_php_refuses_is_phps_compile_error() {
    let undefined = refused(b"<?php\ngoto missing;\n");
    assert_eq!(
        undefined.map(|e| (e.message.to_string(), e.line)),
        Some((String::from("'goto' to undefined label 'missing'"), 2))
    );
    let into = refused(b"<?php\ngoto inside;\nwhile (true) { inside: break; }\n");
    assert_eq!(
        into.map(|e| (e.message.to_string(), e.line)),
        Some((
            String::from("'goto' into loop or switch statement is disallowed"),
            2
        ))
    );
    let twice = refused(b"<?php\na:\na:\n");
    assert_eq!(
        twice.map(|e| (e.message.to_string(), e.line)),
        Some((String::from("Label 'a' already defined"), 3))
    );
    let out = refused(b"<?php\ntry { echo 1; } finally { goto out; }\nout: echo 2;\n");
    assert_eq!(
        out.map(|e| e.message.to_string()),
        Some(String::from("jump out of a finally block is disallowed"))
    );
    let inside = refused(b"<?php\ngoto inside;\ntry { echo 1; } finally { inside: echo 2; }\n");
    assert_eq!(
        inside.map(|e| e.message.to_string()),
        Some(String::from("jump into a finally block is disallowed"))
    );
}
