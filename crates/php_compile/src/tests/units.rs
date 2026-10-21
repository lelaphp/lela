//! A file as one unit: the empty file, the implicit return, the halt,
//! the short open tag, the room, the entry safepoint, `declare`, and
//! the scanner's and compiler's refusals.

use super::*;

#[test]
fn an_empty_file_compiles_the_way_php_compiles_one() {
    let unit = unit_of(b"");
    assert_eq!(unit.main.oplines.len(), 1);
    assert_eq!(unit.main.oplines[0].opcode, Opcode::Return);
    assert_eq!(unit.main.oplines[0].op1_type, OperandKind::Const);
    assert_eq!(
        unit.main.oplines[0].extended_value,
        u32::MAX,
        "php sets the implicit final return's extended_value to -1"
    );
    assert!(unit.functions.is_empty());
}

#[test]
fn echo_of_a_number_is_one_echo_and_the_final_return() {
    let (unit, _strings) = unit_and_strings(b"<?php echo 1;");
    // the constant 1 is echoed and returned
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO int(1)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(
        (unit.main.oplines[0].op1, unit.main.oplines[1].op1),
        (0, 1),
        "zend_add_literal gives each its own slot"
    );
}

/// The implicit final return stands on the file's last line, where
/// the scanner stopped (`CG(zend_lineno) = last_lineno`,
/// `Z/Zend/zend_language_scanner.l:622`), which phpdbg's `print` and
/// its stepping name: `L0003 0001 RETURN int(1)` for a file whose
/// last statement is on line 2 and which ends in a newline.
#[test]
fn the_implicit_return_stands_on_the_file_s_last_line() {
    let (unit, _strings) = unit_and_strings(b"<?php\necho 1;\n");
    let lines: Vec<u32> = unit.main.oplines.iter().map(|op| op.lineno).collect();
    assert_eq!(lines, [2, 3]);
    assert_eq!((unit.main.line_start, unit.main.line_end), (1, 3));
}

/// The scanner stops at `__halt_compiler();`, so the main op array
/// of a halted file ends on the halt's line and the data after it
/// counts none, as php's `halt01.phpt` dump reads `:1-5`.
#[test]
fn a_halted_file_ends_on_the_halt_s_line() {
    let (unit, _strings) = unit_and_strings(b"<?php echo 1;\n__halt_compiler();\ndata\n");
    assert_eq!((unit.main.line_start, unit.main.line_end), (1, 2));
    let (unit, _strings) = unit_and_strings(b"<?php echo 1;\n__halt_compiler() ?>\ndata\n");
    assert_eq!(unit.main.line_end, 3);
}

/// The dump of one source compiled through [`compile_source`] under
/// `short_open_tag`, which is the route the lexer is told it by.
fn dump_under_short_open_tag(source: &[u8], short_open_tag: bool) -> String {
    let mut strings = php_heap::Strings::new();
    let options = Options {
        short_open_tag,
        ..bundled()
    };
    compile_source(&mut Arena::new(), source, &options, &mut strings)
        .expect("the source compiles")
        .dump(&[])
        .text()
}

/// [`Options::short_open_tag`] reaching the scanner (#608): with the
/// entry off the `<?` and everything after it is the one
/// `T_INLINE_HTML` php's `inline_char_handler` takes it into, and
/// with it on the same source is the code inside the tag. The
/// oracle answers the off side beside [`compile_source`], which is
/// the value the pin's recipe passes; the on side is
/// `tests/lang/short_tags.001.phpt`, whose `--INI--` turns it on.
#[test]
fn a_short_tag_opens_code_only_while_short_open_tag_is_on() {
    let source = b"<?\necho 1;\n";
    // off, the file echoes its own text
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-3
             ; return  [] RANGE[0..0]
        0000 ECHO string("<?\necho 1;\n")
        0001 RETURN int(1)
    "#]]
    .assert_eq(&dump_under_short_open_tag(source, false));
    // on, the tag opens code and the 1 inside it is echoed
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-3
             ; return  [] RANGE[0..0]
        0000 ECHO int(1)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&dump_under_short_open_tag(source, true));
}

/// A unit laid down in the room of one given up (#3028) is the unit
/// laid down in none, and an `eval` of one line in a loop takes its
/// oplines', its constants' and its file name's room from the last.
#[test]
fn a_unit_laid_down_in_a_room_is_the_unit_laid_down_in_none() {
    let options = Options {
        kind: SourceKind::EVAL,
        filename: b"/a.php(2) : eval()'d code",
        ..Options::default()
    };
    let mut arena = Arena::new();
    let mut strings = php_heap::Strings::new();
    let mut compile = |source: &[u8], room: Room| {
        compile_source_into(&mut arena, source, &options, &mut strings, room)
    };
    let fresh = compile(b"echo 'a', 'b'; return 1;", Room::default()).expect("the source compiles");
    let mut given_up =
        compile(b"echo 'c'; return 2;", Room::default()).expect("the source compiles");
    assert!(!given_up.main.oplines.is_empty() && !fresh.main.oplines.is_empty());
    let oplines = given_up.main.oplines.as_ptr();
    let filename = given_up.main.filename.0.as_ptr();
    let room = Room::of(&mut given_up.main);
    let laid = compile(b"echo 'a', 'b'; return 1;", room).expect("the source compiles");
    assert_eq!(fresh, laid);
    assert_eq!(
        laid.main.oplines.as_ptr(),
        oplines,
        "the oplines' room is the last unit's"
    );
    assert_eq!(
        laid.main.filename.0.as_ptr(),
        filename,
        "the file's name is written over"
    );
}

#[test]
fn the_entry_is_safepoint_zero_and_the_return_carries_its_own_number() {
    let (unit, _strings) = unit_and_strings(b"<?php echo 1;");
    assert_eq!(
        &*unit.main.safepoints,
        &[OplineIdx(0), OplineIdx(1)],
        "the entry at opline zero, then the return"
    );
    assert_eq!(
        unit.main.oplines[1].out, 1,
        "a site opcode carries its safepoint number in the field ops.toml names"
    );
}

/// `print` is `zend_compile_print`'s `ECHO` and the constant `1` as
/// the expression's value, which a condition over it reads as it
/// stands, the oracle's `[regime.dump]` of the same source.
#[test]
fn print_is_an_echo_whose_value_is_the_constant_one() {
    expect![[r#"
        0000 ECHO string("a")
        0001 ASSIGN CV0($r) int(1)
        0002 ECHO CV0($r)
        0003 JMPZ int(1) 0005
        0004 ECHO string("y")
        0005 RETURN int(1)
    "#]]
    .assert_eq(&oplines_of(
        b"<?php $r = print \"a\"; if (print $r) echo \"y\";",
    ));
}

/// The scanner's deprecations and warning stand on the unit before
/// the compiler's own warning, each at its line, as php raised them:
/// the whole file is scanned before `zend_compile_top_stmt` runs.
#[test]
fn the_scanners_diagnostics_stand_on_the_unit_before_the_compilers() {
    let (unit, _strings) = unit_and_strings(
        b"<?php\n$x = 1;\n(integer)$x;\n(float)1;\n$s = (string)$x;\n(boolean)$x;\necho (int)\"12abc\", \"\\777\", NAN . \"z\";\n",
    );
    let raised = |level, line, message: &str| Diagnostic {
        level,
        line,
        message: message.into(),
    };
    assert_eq!(
        unit.diagnostics,
        vec![
            raised(
                Level::Deprecated,
                3,
                "Non-canonical cast (integer) is deprecated, use the (int) cast instead"
            ),
            raised(
                Level::Deprecated,
                6,
                "Non-canonical cast (boolean) is deprecated, use the (bool) cast instead"
            ),
            raised(
                Level::CompileWarning,
                7,
                "Octal escape sequence overflow \\777 is greater than \\377"
            ),
            raised(
                Level::Warning,
                7,
                "unexpected NAN value was coerced to string"
            ),
        ]
    );
}

/// A fatal carries what was raised before it: the scanner's warning
/// up to the token the parser refused, and none past it, since php's
/// scanner never reached there; every one of them for the `(unset)`
/// cast, whose `E_COMPILE_ERROR` php raises once the file has parsed.
#[test]
fn a_fatal_carries_what_the_scanner_raised_before_it() {
    let refused = |source: &[u8]| {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &Options::default(), &mut strings).err()
    };
    let overflow = |line| Diagnostic {
        level: Level::CompileWarning,
        line,
        message: "Octal escape sequence overflow \\777 is greater than \\377".into(),
    };
    assert_eq!(
        refused(b"<?php\necho \"\\777\";\necho 1 +;\necho \"\\777\";"),
        Some(CompileError {
            level: Level::Parse,
            message: "syntax error, unexpected token \";\"".into(),
            line: 3,
            raised: vec![overflow(2)],
            thrown: false,
            stage: crate::Stage::Compiling,
            declared: None,
            keys_made: 0,
        })
    );
    assert_eq!(
        refused(b"<?php\necho \"\\777\";\n(unset)$x;\necho \"\\777\";"),
        Some(CompileError {
            level: Level::CompileError,
            message: "The (unset) cast is no longer supported".into(),
            line: 3,
            raised: vec![overflow(2), overflow(4)],
            thrown: false,
            stage: crate::Stage::Compiling,
            declared: None,
            keys_made: 0,
        })
    );
}

/// Two units compiled into one engine's table share the one string
/// for a constant both carry, and a constant carried twice in one
/// unit is interned once: that is what interning is for, and it is
/// what a published unit hands every request serving it (design
/// §5.2, §6.3).
#[test]
fn two_units_interned_into_one_table_share_their_constant_string() {
    let mut strings = php_heap::Strings::new();
    let compiled = [
        compile_source(
            &mut Arena::new(),
            b"<?php echo 'shared';",
            &Options::default(),
            &mut strings,
        ),
        compile_source(
            &mut Arena::new(),
            b"<?php echo 'shared', 'shared';",
            &Options::default(),
            &mut strings,
        ),
    ];
    assert_eq!(strings.count(), 1, "one string, carried by two units");
    let [one, other] = compiled.each_ref().map(|unit| {
        let unit = unit.as_ref().expect("the source compiles");
        let literal = unit.main.literals.first().copied();
        literal
            .and_then(php_heap::Zval::as_str)
            .expect("a string literal")
    });
    assert_eq!(one.bytes(), b"shared");
    assert!(one.is_same(other), "one `zend_string`, named by both units");
}

/// `zend_compile_namespace`: a first `namespace` after any statement
/// is php's `E_COMPILE_ERROR` in php's words at the statement's line.
#[test]
fn a_namespace_after_a_statement_is_php_s_compile_error() {
    let mut strings = php_heap::Strings::new();
    let trouble = compile_parsed(b"<?php 1;\nnamespace Foo;", &bundled(), &mut strings).err();
    assert_eq!(
        trouble,
        Some(CompileError {
            level: Level::CompileError,
            message: "Namespace declaration statement has to be the very first statement or after any declare call in the script".into(),
            line: 2,
            raised: Vec::new(),
            thrown: false,
            stage: crate::Stage::Compiling,
            declared: None,
            keys_made: 0,
        })
    );
}

#[test]
fn what_the_reader_cannot_read_is_a_compile_error_at_its_line() {
    let mut strings = php_heap::Strings::new();
    let trouble = compile_parsed(b"<?php\n$x . list($a);", &Options::default(), &mut strings).err();
    assert_eq!(trouble.map(|e| e.line), Some(2));
}

/// A file php's parser refuses is refused here in php's words, the
/// first diagnostic `parse` handed back, before the walk is asked
/// anything: `echo 1 +;` is php's `unexpected token ";"` and not the
/// walk's sentence about the expression.
#[test]
fn what_php_refuses_is_the_first_parse_error_in_phps_words() {
    let mut strings = php_heap::Strings::new();
    let trouble = compile_parsed(b"<?php\necho 1 +;", &Options::default(), &mut strings).err();
    assert_eq!(
        trouble,
        Some(CompileError {
            level: Level::Parse,
            message: "syntax error, unexpected token \";\"".into(),
            line: 2,
            raised: Vec::new(),
            thrown: false,
            stage: crate::Stage::Compiling,
            declared: None,
            keys_made: 0,
        })
    );
}

/// An expression the walk cannot emit yet inside parentheses: the
/// refusal names the expression itself, in the sentence the phpt
/// lane reads as a construct not read yet, where the reader named
/// the parenthesis before (#395). A parenthesis nothing closes is
/// the parser's own refusal, raised before the walk runs.
#[test]
fn an_unread_expression_inside_parentheses_names_the_expression() {
    let compiled = || CompileError {
        level: Level::Parse,
        message: String::from("compiled").into(),
        line: 0,
        raised: Vec::new(),
        thrown: false,
        stage: crate::Stage::Compiling,
        declared: None,
        keys_made: 0,
    };
    let error = refused(b"<?php\n$a = (function () { declare(ticks=0): enddeclare; });\n")
        .unwrap_or_else(compiled);
    assert_eq!(error.level, Level::Parse);
    assert_eq!(
        error.message,
        "a `declare` in the alternative syntax this compiler cannot read yet"
    );
    assert_eq!(error.line, 2);
    let unclosed = refused(b"<?php\n$a = (1 + 2;\n").unwrap_or_else(compiled);
    assert_eq!(unclosed.message, "syntax error, unexpected token \";\"");
}

/// #3029: a refusal carries what landed in php's tables before it,
/// each top-level function and each class bound early in the order
/// it landed, beside what was raised by then; a class its opline
/// binds, and a closure slot the refused statement took, are not.
#[test]
fn a_refusal_carries_the_functions_and_the_early_bound_classes_that_landed() {
    use php_unit::TopLevel;
    let source = b"<?php\nfunction f(int $a = null) { return fn() => 1; }\nclass A {}\ninterface I {}\nclass C implements I {}\nfunction g() {}\n$h = function () { break 2; };\n";
    let declared = refused(source)
        .and_then(|error| error.declared)
        .expect("the refusal carries what landed");
    assert_eq!(
        declared.unit.top_level,
        vec![
            TopLevel::Function(0),
            TopLevel::Class(0),
            TopLevel::Class(1),
            TopLevel::Function(1),
        ]
    );
    assert_eq!(declared.raised_before, vec![1, 1, 1, 1]);
    assert_eq!(declared.unit.dynamic.len(), 1, "f's closure, not $h's");
    assert_eq!(declared.unit.runtime_keys.len(), 1, "C, its opline's");
    let keys = refused(source).map(|error| error.keys_made);
    assert_eq!(keys, Some(1), "the key counted with what landed");
}

/// #3029: a refusal php throws is its parser's, before `zend_compile`
/// declared anything, so it carries nothing that landed.
#[test]
fn a_thrown_refusal_carries_nothing_declared() {
    let source = b"<?php\nfunction f() {}\nclass A {}\nif (1) { __HALT_COMPILER(); }\n";
    let refused = refused(source);
    assert!(refused.as_ref().is_some_and(|error| error.thrown));
    assert!(refused.is_some_and(|error| error.declared.is_none()));
}

/// #3041: a refusal carries the keys its compile had made whatever it
/// declared, an anonymous class's among them and one whose body the
/// compile refused in; a parse error made none.
#[test]
fn a_refusal_carries_the_keys_it_made_declared_or_not() {
    let source = b"<?php\n$o = new class {};\n$p = new class { function f() { break 2; } };\n";
    let error = refused(source);
    assert!(error.as_ref().is_some_and(|error| error.declared.is_none()));
    assert_eq!(error.map(|error| error.keys_made), Some(2));
    let parse = refused(b"<?php\n$o = new class {};\n$p = ;\n").map(|error| error.keys_made);
    assert_eq!(parse, Some(0), "a parse error compiled nothing");
}

/// `zend_compile_declare` (#411): the statement leaves no opline,
/// so the oracle's dump of a file that opens with one is the dump of
/// the file without it, and `declare(strict_types=1)` shows in the
/// bit every op array of the file then carries. The dumps are the
/// oracle's under `[regime.dump]` at 8.5.9.
#[test]
fn a_declare_leaves_no_opline_and_strict_types_marks_every_op_array_of_the_file() {
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO int(1)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php declare(strict_types=1); echo 1;"));
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO int(1)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php declare(ticks=0, strict_types=1); echo 1;"));
    let (unit, _strings) = unit_and_strings(b"<?php declare(strict_types=1); function f() {}");
    assert!(unit.main.strict_types);
    assert!(unit.functions.iter().all(|(_, code)| code.strict_types));
    let (unit, _strings) = unit_and_strings(b"<?php declare(strict_types=0); function f() {}");
    assert!(!unit.main.strict_types);
    assert!(unit.functions.iter().all(|(_, code)| !code.strict_types));
    let (unit, _strings) = unit_and_strings(b"<?php function f() {}");
    assert!(!unit.main.strict_types);
}

/// What `zend_compile_declare` refuses, in php's words, and what it
/// warns about and compiles on from; each measured against the
/// oracle at 8.5.9. The value is held to `ZEND_AST_ZVAL` before the
/// name is looked at, so `1 + 0` and `true` are the same fatal as
/// each other and not a fold.
#[test]
fn what_a_declare_refuses_is_what_php_refuses() {
    let refused = |source: &[u8]| {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &bundled(), &mut strings)
            .err()
            .map(|e| e.message.into_owned())
    };
    assert_eq!(
        refused(b"<?php echo 1; declare(strict_types=1);").as_deref(),
        Some("strict_types declaration must be the very first statement in the script")
    );
    assert_eq!(
        refused(b"<?php namespace A; declare(strict_types=1);").as_deref(),
        Some("strict_types declaration must be the very first statement in the script")
    );
    assert_eq!(
        refused(b"<?php function g() { declare(strict_types=1); }").as_deref(),
        Some("strict_types declaration must be the very first statement in the script")
    );
    assert_eq!(
        refused(b"<?php declare(strict_types=1) { }").as_deref(),
        Some("strict_types declaration must not use block mode")
    );
    assert_eq!(
        refused(b"<?php declare(strict_types=1) echo 1;").as_deref(),
        Some("strict_types declaration must not use block mode")
    );
    assert_eq!(
        refused(b"<?php declare(strict_types=2);").as_deref(),
        Some("strict_types declaration must have 0 or 1 as its value")
    );
    assert_eq!(
        refused(b"<?php declare(strict_types=\"1\");").as_deref(),
        Some("strict_types declaration must have 0 or 1 as its value")
    );
    assert_eq!(
        refused(b"<?php declare(strict_types=1+0);").as_deref(),
        Some("declare(strict_types) value must be a literal")
    );
    assert_eq!(
        refused(b"<?php declare(strict_types=true);").as_deref(),
        Some("declare(strict_types) value must be a literal")
    );
    assert_eq!(
        refused(b"<?php declare(ticks=-1);").as_deref(),
        Some("declare(ticks) value must be a literal")
    );
    // What php compiles and this compiler does not: the alternative
    // syntax. A tick count was one of them until #1190, the encoding
    // pragma until #1272, which is held to `zend_is_first_statement`
    // without `allow_nop` (the parser's own reading of it is
    // `multibyte`'s tests).
    assert_eq!(refused(b"<?php declare(ticks=1); echo 1;").as_deref(), None);
    let first = Some("Encoding declaration pragma must be the very first statement in the script");
    assert_eq!(
        refused(b"<?php declare(ticks=1); declare(encoding=\"x\");").as_deref(),
        None
    );
    assert_eq!(
        refused(b"<?php ; declare(encoding=\"x\");").as_deref(),
        first
    );
    assert_eq!(
        refused(b"<?php ?><?php declare(encoding=\"x\");").as_deref(),
        first
    );
    assert_eq!(
        refused(b"<?php echo 1; declare(encoding=\"x\");").as_deref(),
        first
    );
    assert_eq!(
        refused(b"<?php declare(encoding=\"x\") { declare(encoding=\"y\"); }").as_deref(),
        first
    );
    assert_eq!(
        refused(b"<?php declare(ticks=0): echo 1; enddeclare;").as_deref(),
        Some("a `declare` in the alternative syntax this compiler cannot read yet")
    );
    // `E_COMPILE_WARNING` and the file compiles on, which is the
    // diagnostic the unit carries for the engine to raise.
    let (unit, _strings) = unit_and_strings(b"<?php declare(foo=1); echo 1;");
    assert_eq!(
        unit.diagnostics
            .iter()
            .map(|d| (d.level, d.message.to_string()))
            .collect::<Vec<_>>(),
        vec![(
            Level::CompileWarning,
            String::from("Unsupported declare 'foo'")
        )]
    );
}
