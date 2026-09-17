//! The compiler (design §4.1, §6.1): one pass from the typed tree to a
//! [`Unit`], after `zend_compile.c`, with the constant-expression tree,
//! early binding, compile-time fatals at the site that owns each
//! construct, and safepoint numbering with its invariant asserted. What is
//! here is the first slice of Lane B (design §11): the statements the
//! engine's loop serves, `echo` and a call of a named function, emitted as
//! php emits them, with the entry safepoint and the numbered return. The
//! tree it walks is `read`'s while `php_syntax` is a stub (design §4.9);
//! what it cannot read is refused as a [`CompileError`].
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod emit;
mod live;
mod read;
mod special;
mod stack;

pub use stack::StackLimit;

/// The token stream and the tree (design §4.9), handed up for
/// `php_engine::api::syntax` and nothing of the compiler's own: the
/// lexer, its stream and php's names for its kinds, which
/// `token_get_all` spells, and the parser with the tree's shape and its
/// diagnostics, which `token_get_all` under `TOKEN_PARSE` reads as php's
/// `tokenize_parse` reads the parser's events. It passes through here
/// for the same reason [`compile_source`] does: the engine's edges reach
/// this crate and not the syntax crate (design §4.2).
pub mod syntax {
    pub use php_syntax::tree::{Element, TokenId, Tree};
    pub use php_syntax::{
        Diagnostic, FileId, Level, LexOptions, Lexed, Options, Parsed, SyntaxKind, Token, amp_name,
        cast_name, int_overflows, lex, magic_const_name, operator_name, parse,
    };
}

use php_heap::Intern;
use php_syntax::Parsed;
use php_unit::decl::{ClassDecl, ConstDecl, FunctionDecl, Name};
use php_unit::{Diagnostic, Level, Unit};

/// What the compiler is told beside the source (design §4.1).
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// The file name the unit and its errors carry.
    pub filename: Name,
    /// Whether the caller's `declare(strict_types=1)` is in force.
    pub strict_types: bool,
    /// php's `precision` ini entry as it stands while the file compiles,
    /// which `_convert_to_string` renders a double constant at when the
    /// compiler folds a `.` over it (`zend_double_to_str`, which reads
    /// `EG(precision)`); php's default is 14.
    pub precision: i32,
    /// The persistent constants the engine's extensions declared
    /// (`CONST_PERSISTENT`), which `zend_try_ct_eval_const` substitutes
    /// where they are written; a name not here is fetched as the program serves.
    pub persistent: Vec<ConstDecl>,
    /// The natives the engine's bundle declared, php's `CG(function_table)`
    /// as `zend_compile_call` reads it: what `zend_try_compile_special_func`
    /// requires a name to be one of before it serves the call itself
    /// (`fbc->type == ZEND_INTERNAL_FUNCTION`), and what a name must be
    /// in for `INIT_FCALL`; a name the bundle has not is the call by name,
    /// `INIT_FCALL_BY_NAME`, resolved as the program serves. Every
    /// function here is a native; a user function is declared by nothing
    /// this compiler reads yet.
    pub functions: Vec<FunctionDecl>,
    /// The classes the engine's bundle declared, php's `CG(class_table)`
    /// as `zend_compile_static_call` reads it: a class the table has
    /// with a public method of the name written is a callee the
    /// compiler knows (`zend_get_compatible_func_or_null`), whose
    /// arguments are sent as a known callee's are; any other class or
    /// method is resolved as the program serves. Every class here is a
    /// native one; a class the file itself declares stands beside them.
    pub classes: Vec<ClassDecl>,
    /// The stack limit of the thread the compiler runs on, php's
    /// `EG(stack_limit)`, which the engine works out once per request
    /// (`zend_call_stack_init`) and the compiler's recursion is held to
    /// (`zend_check_stack_limit`); unchecked where a caller says nothing.
    pub stack: StackLimit,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            filename: Name::default(),
            strict_types: false,
            precision: 14,
            persistent: Vec::new(),
            functions: Vec::new(),
            classes: Vec::new(),
            stack: StackLimit::UNCHECKED,
        }
    }
}

/// A compile-time fatal: php's message, at php's line, at the level
/// php raises it, which is how the request ends. `E_PARSE` for the
/// parser's first `syntax error, …` where php's parser refused the file,
/// and for the compiler's own sentence where php's would have compiled it
/// and this compiler cannot read it yet; `E_COMPILE_ERROR` for what
/// php's compiler refuses after parsing, the `(unset)` cast and the
/// stack limit ([`StackLimit::check`]) among them.
///
/// `raised` is what `zend_error` raised before the fatal and php had
/// already rendered when it bailed out: the scanner's warning and
/// deprecation (design §6.1) up to the token the parser refused, and
/// for a fatal after the parse every one of them, then the compiler's
/// own; each in the order raised, for the engine to render before the
/// fatal as php's request shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub level: Level,
    pub message: String,
    pub line: u32,
    pub raised: Vec<Diagnostic>,
}

/// Parse and compile one source file: what the engine calls for the
/// file it serves, an `include` or an `eval`, since its edges reach the compiler
/// and not the syntax crate (design §4.2).
///
/// `strings` is where the unit's constant strings are interned (design
/// §5.2): the engine's table for a unit bound for the unit cache, whose
/// strings every request serving it then shares, and the request's own
/// heap for a unit that is one request's alone. php decides the same
/// thing by which handler is installed when `zend_compile_file` runs
/// (`Z/Zend/zend_string.c:363-373`).
pub fn compile_source(
    source: &[u8],
    options: &Options,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    compile(
        &php_syntax::parse(source, &php_syntax::Options::default()),
        options,
        strings,
    )
}

/// Compile one parsed file into a unit, interning its constant strings
/// into `strings`.
pub fn compile(
    parsed: &Parsed,
    options: &Options,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    // `Parsed::diagnostics` is in php's order (design §6.1): the
    // scanner's and the parser's by position, the compiler's one after
    // them all. php's scanner raises as it scans and its parser stops at
    // the token it did not want, so what stands before the first fatal
    // was raised and what stands after it never was; php compiles
    // nothing of a file its parser refused, and the first fatal is the
    // compile error, in php's words at php's line, with what was raised
    // before it carried beside it.
    let mut raised = Vec::new();
    for diagnostic in &parsed.diagnostics {
        let level = level_of(diagnostic.level);
        if diagnostic.level.is_fatal() {
            return Err(CompileError {
                level,
                message: diagnostic.message.clone(),
                line: diagnostic.line,
                raised,
            });
        }
        raised.push(Diagnostic {
            level,
            line: diagnostic.line,
            message: diagnostic.message.as_str().into(),
        });
    }
    let read = read::read(&parsed.source, options.stack).map_err(|trouble| CompileError {
        level: trouble.level,
        message: trouble.what,
        line: trouble.line,
        raised: raised.clone(),
    })?;
    let lines = u32::try_from(parsed.source.split(|b| *b == b'\n').count()).unwrap_or(u32::MAX);
    // The scanner's diagnostics stand before the compiler's on the unit,
    // as php raised them: the whole file is scanned before
    // `zend_compile_top_stmt` runs, and opcache records both kinds on a
    // `persistent_script` to replay on a hit.
    match emit::unit(&read, options, lines, strings) {
        Ok(mut unit) => {
            raised.append(&mut unit.diagnostics);
            unit.diagnostics = raised;
            Ok(unit)
        }
        Err(mut refused) => {
            raised.append(&mut refused.raised);
            refused.raised = raised;
            Err(refused)
        }
    }
}

/// php's level for what the scanner or the parser raised: the same
/// `E_*` under the unit's name for it.
fn level_of(level: php_syntax::Level) -> Level {
    match level {
        php_syntax::Level::Parse => Level::Parse,
        php_syntax::Level::CompileError => Level::CompileError,
        php_syntax::Level::CompileWarning => Level::CompileWarning,
        php_syntax::Level::Deprecated => Level::Deprecated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_unit::decl::{ConstExpr, Parameter, Type, Visibility};
    use php_unit::{Opcode, OperandKind, OplineIdx, frameless_table};

    /// A unit whose constant strings are one engine's, which is what the
    /// unit cache holds (design §6.3).
    fn unit_of(source: &[u8]) -> Unit {
        unit_and_strings(source).0
    }

    /// php's dump of a one-line file's main op array, the file unnamed
    /// as `Options::default` leaves it: the header with these counts,
    /// then the oplines.
    fn main_dump(lines: usize, vars: u32, tmps: u32, oplines: &str) -> String {
        format!(
            "\n$_main:\n     ; (lines={lines}, args=0, vars={vars}, tmps={tmps})\n\
             \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n{oplines}"
        )
    }

    /// The dump of one source, read while the table its strings were
    /// interned into still stands, under the frameless table of the
    /// bundle the source compiled against, as the engine hands it to
    /// `compile-identity`.
    fn dump_of(source: &[u8]) -> String {
        let (unit, _strings) = unit_and_strings(source);
        let functions = bundled().functions;
        let names: Vec<Name> = frameless_table(&functions)
            .iter()
            .map(|entry| functions[entry.function as usize].name.clone())
            .collect();
        unit.dump(&names)
    }

    fn unit_and_strings(source: &[u8]) -> (Unit, php_heap::Strings) {
        let mut strings = php_heap::Strings::new();
        let unit = compile(
            &php_syntax::parse(source, &php_syntax::Options::default()),
            &bundled(),
            &mut strings,
        )
        .unwrap_or_default();
        (unit, strings)
    }

    /// The natives the tests below call that are in no row of
    /// `special::TABLE`: php's `-n` has each, so its dump reads them off
    /// `CG(function_table)` as `INIT_FCALL` and `DO_ICALL`.
    const CALLED: [&[u8]; 5] = [
        b"var_dump",
        b"extension_loaded",
        b"str_repeat",
        b"set_time_limit",
        b"memory_get_usage",
    ];

    /// The natives with a frameless entry the tests below call, as
    /// `ext/standard` declares them: the name, the parameters as php's
    /// stub has them with a default where the stub has one, and the
    /// arities of `frameless_function_infos_<name>`.
    fn frameless_natives() -> Vec<FunctionDecl> {
        let parameter = |name: &str, ty: Type, default: Option<ConstExpr>| Parameter {
            name: Name::from(name),
            ty: Some(ty),
            by_ref: false,
            variadic: false,
            default,
            attributes: Vec::new(),
        };
        [
            (
                "trim",
                vec![
                    parameter("string", Type::String, None),
                    parameter(
                        "characters",
                        Type::String,
                        Some(ConstExpr::Str(b" ".as_slice().into())),
                    ),
                ],
                vec![1, 2],
            ),
            (
                "str_contains",
                vec![
                    parameter("haystack", Type::String, None),
                    parameter("needle", Type::String, None),
                ],
                vec![2],
            ),
            (
                "substr",
                vec![
                    parameter("string", Type::String, None),
                    parameter("offset", Type::Int, None),
                    parameter(
                        "length",
                        Type::Nullable(Box::new(Type::Int)),
                        Some(ConstExpr::Null),
                    ),
                ],
                vec![2, 3],
            ),
        ]
        .into_iter()
        .map(|(name, parameters, frameless)| FunctionDecl {
            name: Name::from(name),
            parameters,
            return_type: None,
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            frameless,
        })
        .collect()
    }

    /// The options of an engine whose bundle declares every name of
    /// `special::TABLE` and every name of [`CALLED`] as a native, which
    /// php's `-n` has, and then [`frameless_natives`]: what
    /// `zend_compile_call` reads as `CG(function_table)`, so the tests
    /// below compile under the table php compiles under.
    fn bundled() -> Options {
        Options {
            functions: special::TABLE
                .iter()
                .map(|(name, _)| *name)
                .chain(CALLED)
                .map(|name| FunctionDecl {
                    name: Name(name.into()),
                    parameters: Vec::new(),
                    return_type: None,
                    returns_by_ref: false,
                    attributes: Vec::new(),
                    doc_comment: None,
                    file: Name(Box::new([])),
                    line: 0,
                    frameless: Vec::new(),
                })
                .chain(frameless_natives())
                .collect(),
            ..Options::default()
        }
    }

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
        assert_eq!(
            unit.dump(&[]),
            main_dump(2, 0, 0, "0000 ECHO int(1)\n0001 RETURN int(1)\n"),
            "the constant 1 is echoed and returned"
        );
        assert_eq!(
            (unit.main.oplines[0].op1, unit.main.oplines[1].op1),
            (0, 0),
            "php keeps one slot for it"
        );
    }

    #[test]
    fn a_nested_call_sends_what_the_inner_call_returned() {
        let (unit, _strings) = unit_and_strings(b"<?php var_dump(extension_loaded('demo'));");
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                7,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"var_dump\")\n\
                 0001 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0002 SEND_VAL string(\"demo\") 1\n\
                 0003 V0 = DO_ICALL\n\
                 0004 SEND_VAR V0 1\n\
                 0005 DO_ICALL\n\
                 0006 RETURN int(1)\n"
            ),
            "php's, the inner call's `V0 = ` and the outer's bare"
        );
        assert_eq!(unit.main.oplines[0].extended_value, 1, "one argument");
        assert_eq!(
            unit.main.oplines[0].op1, 96,
            "zend_vm_calc_used_stack: five header slots and one argument, sixteen bytes each"
        );
        assert_eq!(unit.main.oplines[1].op1, 96);
        assert_eq!(
            unit.main.cache_size, 16,
            "one cache slot per INIT_FCALL, a pointer each"
        );
        assert_eq!(
            unit.main.num_tmps, 2,
            "one temporary per call, as php counts them here"
        );
    }

    #[test]
    fn the_entry_is_safepoint_zero_and_the_return_carries_its_own_number() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1;");
        assert_eq!(
            unit.main.safepoints.as_ref(),
            &[OplineIdx(0), OplineIdx(1)],
            "the entry at opline zero, then the return"
        );
        assert_eq!(
            unit.main.oplines[1].out, 1,
            "a site opcode carries its safepoint number in the field ops.toml names"
        );
    }

    /// The one constant a single-expression `echo` leaves in the table.
    fn echoed_literal(source: &[u8]) -> php_heap::Zval {
        let (unit, _strings) = unit_and_strings(source);
        unit.main
            .literals
            .first()
            .copied()
            .unwrap_or(php_heap::Zval::UNDEF)
    }

    #[test]
    fn arithmetic_over_two_constants_is_folded_the_way_php_folds_it() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1 + 2;");
        assert_eq!(
            unit.dump(&[]),
            main_dump(2, 0, 0, "0000 ECHO int(3)\n0001 RETURN int(1)\n"),
            "the sum is one literal and no ZEND_ADD"
        );
        assert_eq!(echoed_literal(b"<?php echo 1 + 2;").as_long(), Some(3));
        // `div_function`: a long where the division comes out whole, a
        // double where it does not.
        assert_eq!(echoed_literal(b"<?php echo 4/2;").as_long(), Some(2));
        assert_eq!(
            echoed_literal(b"<?php echo 1/3;").as_double(),
            Some(1.0 / 3.0)
        );
        assert_eq!(
            echoed_literal(b"<?php echo 0.1 + 0.2;").as_double(),
            Some(0.1 + 0.2)
        );
        // `fast_long_add_function`: the sum that overflows the long is
        // the double php leaves.
        assert_eq!(
            echoed_literal(b"<?php echo 9223372036854775807 + 1;").as_double(),
            Some(9_223_372_036_854_775_807_f64 + 1.0)
        );
        // `zend_try_ct_eval_unary_pm`: a multiplication by -1, which
        // keeps the sign of a zero.
        assert_eq!(echoed_literal(b"<?php echo -2;").as_long(), Some(-2));
        assert_eq!(
            echoed_literal(b"<?php echo -0.0;")
                .as_double()
                .map(f64::is_sign_negative),
            Some(true)
        );
    }

    /// `zend_try_ct_eval_binary_op` converts an operand as the opcode's
    /// kernel converts it, through the one reader `php_heap` holds, so a
    /// numeric string, a bool and null fold where they stand and the op
    /// array is php's single `ECHO` of the folded constant.
    #[test]
    fn an_operand_php_converts_is_folded_where_it_stands() {
        assert_eq!(
            dump_of(b"<?php echo \"1\" + 1;"),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n")
        );
        assert_eq!(echoed_literal(b"<?php echo \"1\" + 1;").as_long(), Some(2));
        assert_eq!(
            dump_of(b"<?php echo true + 1;"),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n")
        );
        assert_eq!(echoed_literal(b"<?php echo true + 1;").as_long(), Some(2));
        // The folded 1 and the return's 1 are one literal, as
        // `zend_add_literal` keeps them.
        let (unit, _strings) = unit_and_strings(b"<?php echo null + 1;");
        assert_eq!(
            unit.dump(&[]),
            main_dump(2, 0, 0, "0000 ECHO int(1)\n0001 RETURN int(1)\n")
        );
        assert_eq!(unit.main.literals.len(), 1);
        assert_eq!(echoed_literal(b"<?php echo null + 1;").as_long(), Some(1));
        assert_eq!(echoed_literal(b"<?php echo false + 1;").as_long(), Some(1));
        // `_is_numeric_string_ex`: whitespace on either side, a sign, a
        // fraction, an exponent, and a long's edge.
        assert_eq!(
            echoed_literal(b"<?php echo \" 1 \" + \"+2\";").as_long(),
            Some(3)
        );
        assert_eq!(
            echoed_literal(b"<?php echo \"1e3\" + \".5\";").as_double(),
            Some(1000.5)
        );
        assert_eq!(
            echoed_literal(b"<?php echo \"9223372036854775808\" + 0;").as_double(),
            Some(9_223_372_036_854_775_808.0)
        );
        assert_eq!(
            echoed_literal(b"<?php echo -\"1.5\";").as_double(),
            Some(-1.5)
        );
        assert_eq!(echoed_literal(b"<?php echo -null;").as_long(), Some(0));
        assert_eq!(
            echoed_literal(b"<?php echo \"3\" ** \"2\";").as_long(),
            Some(9)
        );
    }

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
            persistent: vec![
                ConstDecl {
                    name: Name(b"T_LNUMBER".to_vec().into_boxed_slice()),
                    value: ConstExpr::Long(260),
                },
                ConstDecl {
                    name: Name(b"PHP_OS".to_vec().into_boxed_slice()),
                    value: ConstExpr::Str(b"Darwin".to_vec().into_boxed_slice()),
                },
            ],
            ..Options::default()
        };
        let compiled = compile(
            &php_syntax::parse(
                b"<?php echo T_LNUMBER - 1, \\PHP_OS;",
                &php_syntax::Options::default(),
            ),
            &options,
            &mut strings,
        );
        assert!(compiled.is_ok(), "{compiled:?}");
        let unit = compiled.unwrap_or_default();
        assert_eq!(
            unit.dump(&[]),
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
        assert!(
            compile(
                &php_syntax::parse(b"<?php echo t_lnumber;", &php_syntax::Options::default()),
                &options,
                &mut strings
            )
            .is_err(),
            "a constant's name is case-sensitive"
        );
    }

    #[test]
    fn a_modulo_and_a_power_over_constants_fold_the_way_php_folds_them() {
        assert_eq!(echoed_literal(b"<?php echo 7 % 2;").as_long(), Some(1));
        assert_eq!(echoed_literal(b"<?php echo -7 % 2;").as_long(), Some(-1));
        assert_eq!(echoed_literal(b"<?php echo 7 % -2;").as_long(), Some(1));
        // `zend_is_op_long_compatible`: a double or a numeric string
        // that is a whole number in range casts to its long and folds.
        assert_eq!(echoed_literal(b"<?php echo 2.0 % 1;").as_long(), Some(0));
        assert_eq!(echoed_literal(b"<?php echo 7 % 2.0;").as_long(), Some(1));
        assert_eq!(
            echoed_literal(b"<?php echo \"1.0\" % 1;").as_long(),
            Some(0)
        );
        assert_eq!(
            echoed_literal(b"<?php echo \"1e3\" % 7;").as_long(),
            Some(6)
        );
        assert_eq!(echoed_literal(b"<?php echo 2 ** 3;").as_long(), Some(8));
        assert_eq!(
            echoed_literal(b"<?php echo 2 ** 3 ** 2;").as_long(),
            Some(512),
            "`**` reads right to left"
        );
        assert_eq!(
            echoed_literal(b"<?php echo -2 ** 2;").as_long(),
            Some(-4),
            "`**` binds tighter than unary `-`"
        );
        assert_eq!(
            echoed_literal(b"<?php echo 2 ** -1;").as_double(),
            Some(0.5)
        );
        // `pow_function_base`: the step that overflows leaves php's double.
        assert_eq!(
            echoed_literal(b"<?php echo 2 ** 63;").as_double(),
            Some(9_223_372_036_854_775_808_f64)
        );
    }

    #[test]
    fn arithmetic_php_does_not_fold_is_emitted_as_the_opcode_php_emits() {
        assert_eq!(
            dump_of(b"<?php echo 1 + extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 V0 = DO_ICALL\n\
                 0003 T1 = ADD int(1) V0\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            ),
            "php's own op array, its literals in the order the oplines name them"
        );
        // `zend_binary_op_produces_error`: a string that opens with no
        // number, or carries anything past the one it opens with, is left
        // to the opcode, which throws or warns where it stands.
        // The return's `1` shares the slot of an operand that is the
        // same `1`, as `zend_add_literal` keeps them.
        for (source, left, right, returned) in [
            (
                &b"<?php echo \"abc\" + 1;"[..],
                "string(\"abc\")",
                "int(1)",
                1,
            ),
            (b"<?php echo \"1abc\" + 1;", "string(\"1abc\")", "int(1)", 1),
            (b"<?php echo \"0x1A\" + 1;", "string(\"0x1A\")", "int(1)", 1),
            (b"<?php echo \"1e\" + 1;", "string(\"1e\")", "int(1)", 1),
            (b"<?php echo 1 + \"\";", "int(1)", "string(\"\")", 0),
        ] {
            let (unit, _strings) = unit_and_strings(source);
            assert_eq!(
                unit.dump(&[]),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 T0 = ADD {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
            assert_eq!(
                unit.main.oplines[2].op1,
                returned,
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // The divisor php's `zval_get_double` reads as zero, whatever
        // its type, and the negative power of a zero.
        for (source, divisor) in [
            (&b"<?php echo 2 / \"0\";"[..], "string(\"0\")"),
            (b"<?php echo 2 / \"0.0\";", "string(\"0.0\")"),
            (b"<?php echo 2 / false;", "bool(false)"),
            (b"<?php echo 2 / null;", "null"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 T0 = DIV int(2) {divisor}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // `-"abc"` is the multiplication by -1 php emits for an operand
        // it will not fold.
        assert_eq!(
            dump_of(b"<?php echo -\"1abc\";"),
            main_dump(
                3,
                0,
                1,
                "0000 T0 = MUL string(\"1abc\") int(-1)\n0001 ECHO T0\n0002 RETURN int(1)\n"
            )
        );
        // `zend_binary_op_produces_error`: the division php refuses to
        // fold, so that `DivisionByZeroError` is thrown where it stands.
        assert_eq!(
            dump_of(b"<?php echo 1/0;"),
            main_dump(
                3,
                0,
                1,
                "0000 T0 = DIV int(1) int(0)\n0001 ECHO T0\n0002 RETURN int(1)\n"
            )
        );
        // `zend_compile_unary_pm`: the multiplication by -1 php emits
        // where the operand is not a constant.
        assert_eq!(
            dump_of(b"<?php echo -extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 V0 = DO_ICALL\n\
                 0003 T1 = MUL V0 int(-1)\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            )
        );
    }

    #[test]
    fn a_modulo_and_a_power_php_does_not_fold_are_mod_and_pow_where_they_stand() {
        // The modulo php refuses to fold, by whatever `zval_get_long`
        // reads as zero and over an operand `zend_is_op_long_compatible`
        // refuses, and the negative power of a zero: each is the opcode,
        // to throw or deprecate where it stands.
        for (source, left, right) in [
            (&b"<?php echo 1 % 0;"[..], "int(1)", "int(0)"),
            (b"<?php echo 1 % \"0\";", "int(1)", "string(\"0\")"),
            (b"<?php echo 1 % null;", "int(1)", "null"),
            (b"<?php echo 1.5 % 1;", "float(1.5)", "int(1)"),
            (b"<?php echo \"1.5\" % 1;", "string(\"1.5\")", "int(1)"),
            (b"<?php echo 1e19 % 1;", "float(1e+19)", "int(1)"),
            (
                b"<?php echo \"9223372036854775808\" % 1;",
                "string(\"9223372036854775808\")",
                "int(1)",
            ),
            (b"<?php echo INF % 1;", "float(inf)", "int(1)"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 T0 = MOD {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for (source, base) in [
            (&b"<?php echo 0 ** -1;"[..], "int(0)"),
            (b"<?php echo \"0\" ** -1;", "string(\"0\")"),
            (b"<?php echo 0.0 ** -1;", "float(0)"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 T0 = POW {base} int(-1)\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // `%` and `**` over a call, php's `MOD` and `POW` where `ADD`
        // stood above.
        for (source, opcode, left) in [
            (
                &b"<?php echo 7 % extension_loaded('Core');"[..],
                "MOD",
                "int(7)",
            ),
            (
                b"<?php echo 2 ** extension_loaded('Core');",
                "POW",
                "int(2)",
            ),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    6,
                    0,
                    2,
                    &format!(
                        "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                         0001 SEND_VAL string(\"Core\") 1\n\
                         0002 V0 = DO_ICALL\n\
                         0003 T1 = {opcode} {left} V0\n\
                         0004 ECHO T1\n\
                         0005 RETURN int(1)\n"
                    )
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `shift_left_function` and `shift_right_function` over two
    /// constants, as php folds them: the shift over the longs both sides
    /// are worth, zero or the sign past the long's width, and the opcode
    /// left standing for a negative count, an operand
    /// `zend_is_op_long_compatible` refuses, or a call.
    #[test]
    fn a_shift_over_constants_folds_the_way_php_folds_it_and_the_rest_is_sl_or_sr() {
        assert_eq!(echoed_literal(b"<?php echo 1 << 3;").as_long(), Some(8));
        assert_eq!(echoed_literal(b"<?php echo 1 << 64;").as_long(), Some(0));
        assert_eq!(echoed_literal(b"<?php echo -1 >> 64;").as_long(), Some(-1));
        assert_eq!(echoed_literal(b"<?php echo 8 >> 1;").as_long(), Some(4));
        assert_eq!(echoed_literal(b"<?php echo \"2\" << 1;").as_long(), Some(4));
        assert_eq!(echoed_literal(b"<?php echo 1 << 2.0;").as_long(), Some(4));
        assert_eq!(
            echoed_literal(b"<?php echo 1 + 2 << 3;").as_long(),
            Some(24),
            "`+` binds tighter than `<<`"
        );
        for (source, opcode, left, right) in [
            (&b"<?php echo 1 << -1;"[..], "SL", "int(1)", "int(-1)"),
            (b"<?php echo 1 >> -1;", "SR", "int(1)", "int(-1)"),
            (b"<?php echo 1.5 << 1;", "SL", "float(1.5)", "int(1)"),
            (b"<?php echo 1 << \"1x\";", "SL", "int(1)", "string(\"1x\")"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!(
                        "0000 T0 = {opcode} {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n"
                    )
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        assert_eq!(
            dump_of(b"<?php echo 1 << extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 V0 = DO_ICALL\n\
                 0003 T1 = SL int(1) V0\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            )
        );
    }

    /// The text of the one constant a single-expression `echo` leaves,
    /// read while the table it was interned into still stands.
    fn echoed_string(source: &[u8]) -> Vec<u8> {
        let (unit, _strings) = unit_and_strings(source);
        literal_text(&unit, 0).unwrap_or_default()
    }

    /// The text of the constant at `at`, for a string; nothing otherwise.
    fn literal_text(unit: &Unit, at: usize) -> Option<Vec<u8>> {
        unit.main
            .literals
            .get(at)
            .and_then(|z| z.as_str())
            .map(|s| s.bytes().to_vec())
    }

    /// `zend_compile_binary_op`'s `ZEND_CONCAT` arm: the fold over two
    /// constants through `concat_function`, each constant converted as
    /// `_convert_to_string` converts it, so the op array is php's single
    /// `ECHO` of the folded string; and a constant beside what does not
    /// fold converted where it stands, so the `CONCAT` reads a string.
    #[test]
    fn a_concatenation_over_constants_folds_to_php_s_one_string_and_the_rest_is_concat() {
        for (source, joined) in [
            (&b"<?php echo 'a' . 'b';"[..], "ab"),
            (b"<?php echo \"1\" . 2;", "12"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    2,
                    0,
                    0,
                    &format!("0000 ECHO string(\"{joined}\")\n0001 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        assert_eq!(echoed_string(b"<?php echo 'a' . 'b';"), b"ab");
        assert_eq!(echoed_string(b"<?php echo \"1\" . 2;"), b"12");
        assert_eq!(echoed_string(b"<?php echo 2.5 . 'x';"), b"2.5x");
        assert_eq!(echoed_string(b"<?php echo 1.0 . 'y';"), b"1y");
        assert_eq!(
            echoed_string(b"<?php echo 1 / 3 . '';"),
            b"0.33333333333333"
        );
        assert_eq!(echoed_string(b"<?php echo true . false . null;"), b"1");
        assert_eq!(echoed_string(b"<?php echo 'a' . 'b' . 'c';"), b"abc");
        assert_eq!(
            echoed_string(b"<?php echo 1 . 2 << 3;"),
            b"116",
            "`<<` binds tighter than `.`"
        );
        assert_eq!(
            echoed_literal(b"<?php echo (1 . 2) + 3;").as_long(),
            Some(15),
            "a folded `.` is a constant to the arithmetic around it"
        );
        // The precision the double is rendered at is the compile-time
        // ini entry, as `zend_double_to_str` reads `EG(precision)`.
        let mut strings = php_heap::Strings::new();
        let unit = compile(
            &php_syntax::parse(b"<?php echo 1 / 3 . '';", &php_syntax::Options::default()),
            &Options {
                precision: 3,
                ..Options::default()
            },
            &mut strings,
        )
        .unwrap_or_default();
        assert_eq!(literal_text(&unit, 0), Some(b"0.333".to_vec()));
        // A constant beside a CV is converted and the `CONCAT` stands.
        let (unit, _strings) = unit_and_strings(
            b"<?php $a = 1; echo 2 . $a; echo $a . 2.5; echo 'a' . 'b' . $a; echo NAN . $a;",
        );
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                10,
                1,
                5,
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 T2 = CONCAT string(\"2\") CV0($a)\n\
                 0002 ECHO T2\n\
                 0003 T3 = CONCAT CV0($a) string(\"2.5\")\n\
                 0004 ECHO T3\n\
                 0005 T4 = CONCAT string(\"ab\") CV0($a)\n\
                 0006 ECHO T4\n\
                 0007 T5 = CONCAT string(\"NAN\") CV0($a)\n\
                 0008 ECHO T5\n\
                 0009 RETURN int(1)\n"
            ),
            "php's own op array"
        );
        assert_eq!(
            literal_text(&unit, 1),
            Some(b"2".to_vec()),
            "the long is the string `2`"
        );
        assert_eq!(
            literal_text(&unit, 2),
            Some(b"2.5".to_vec()),
            "the double is the string `2.5`"
        );
        assert_eq!(
            literal_text(&unit, 3),
            Some(b"ab".to_vec()),
            "the left fold is one string"
        );
        assert_eq!(
            literal_text(&unit, 4),
            Some(b"NAN".to_vec()),
            "the NAN is the string `zend_double_to_str` renders"
        );
    }

    /// `zend_double_to_str` over a NAN raises the coercion warning while
    /// the file compiles, and the unit carries it as opcache carries a
    /// `persistent_script`'s `warnings`: `E_WARNING`, php's text, at the
    /// line the fold stood on, once per operand converted and in the
    /// order they were, with the folded string in the op array. A unit
    /// with nothing raised carries nothing.
    #[test]
    fn a_nan_a_fold_converts_is_the_warning_the_unit_carries() {
        use php_unit::{Diagnostic, Level};
        let (unit, _strings) =
            unit_and_strings(b"<?php\necho 'a';\necho NAN . 'z';\necho 1 . NAN . NAN;");
        assert_eq!(
            unit.dump(&[]),
            "\n$_main:\n     ; (lines=4, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :1-4\n     ; return  [] RANGE[0..0]\n\
             0000 ECHO string(\"a\")\n\
             0001 ECHO string(\"NANz\")\n\
             0002 ECHO string(\"1NANNAN\")\n\
             0003 RETURN int(1)\n"
        );
        let coerced = |line| Diagnostic {
            level: Level::Warning,
            line,
            message: "unexpected NAN value was coerced to string".into(),
        };
        assert_eq!(
            unit.diagnostics,
            vec![coerced(3), coerced(4), coerced(4)],
            "one per operand converted, at the statement's line"
        );
        assert!(unit_of(b"<?php echo 'a' . 'b';").diagnostics.is_empty());
        assert!(
            unit_of(b"<?php echo NAN;").diagnostics.is_empty(),
            "an `ECHO` over a NAN converts nothing at compile time; the kernel warns"
        );
    }

    /// `zend_compile_cast` under the dump recipe: `CAST` with php's type
    /// constant over the operand into a temporary, `BOOL` for `(bool)`,
    /// nothing folded and the `FREE` after one nothing reads, which
    /// `zend_do_free` leaves off a `BOOL`; and the scanner's escapes as
    /// the bytes php reads.
    #[test]
    fn a_cast_is_php_s_opline_and_an_escape_is_php_s_byte() {
        assert_eq!(
            dump_of(
                b"<?php\n$x = 1;\n(integer)$x;\n(float)1;\n$s = (string)$x;\n(boolean)$x;\necho (int)\"12abc\", \"\\777\", NAN . \"z\";\n",
            ),
            "\n$_main:\n     ; (lines=13, args=0, vars=2, tmps=7)\n\
             \x20    ; (before optimizer)\n     ; :1-8\n     ; return  [] RANGE[0..0]\n\
             0000 ASSIGN CV0($x) int(1)\n\
             0001 T3 = CAST (long) CV0($x)\n\
             0002 FREE T3\n\
             0003 T4 = CAST (double) int(1)\n\
             0004 FREE T4\n\
             0005 T5 = CAST (string) CV0($x)\n\
             0006 ASSIGN CV1($s) T5\n\
             0007 T7 = BOOL CV0($x)\n\
             0008 T8 = CAST (long) string(\"12abc\")\n\
             0009 ECHO T8\n\
             0010 ECHO string(\"\\xFF\")\n\
             0011 ECHO string(\"NANz\")\n\
             0012 RETURN int(1)\n"
        );
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
            compile(
                &php_syntax::parse(source, &php_syntax::Options::default()),
                &Options::default(),
                &mut strings,
            )
            .err()
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
            })
        );
        assert_eq!(
            refused(b"<?php\necho \"\\777\";\n(unset)$x;\necho \"\\777\";"),
            Some(CompileError {
                level: Level::CompileError,
                message: "The (unset) cast is no longer supported".into(),
                line: 3,
                raised: vec![overflow(2), overflow(4)],
            })
        );
    }

    /// `zend_compile_for` over `for (;;) {}`, php's own oplines under
    /// the dump recipe: the `JMP` forward to the condition, and the
    /// condition, the constant `true` for an empty one, as the `JMPNZ`
    /// back to the body's first opline, which is itself here. php folds
    /// nothing, so the back-edge is a `JMPNZ` and not a `JMP`.
    #[test]
    fn an_empty_for_is_php_s_forward_jump_and_a_conditional_jump_to_itself() {
        let (unit, _strings) = unit_and_strings(b"<?php set_time_limit(1); for (;;) {}");
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                6,
                0,
                1,
                "0000 INIT_FCALL 1 96 string(\"set_time_limit\")\n\
                 0001 SEND_VAL int(1) 1\n\
                 0002 DO_ICALL\n\
                 0003 JMP 0004\n\
                 0004 JMPNZ bool(true) 0004\n\
                 0005 RETURN int(1)\n"
            ),
            "the oracle's dump, byte for byte"
        );
        assert_eq!(
            unit.main.oplines[3].op1, 4,
            "the JMP lands on the condition"
        );
        assert_eq!(
            unit.main.oplines[4].op2, 4,
            "the JMPNZ lands on the body's first opline, which is itself"
        );
        assert_eq!(
            unit.main.literals[2],
            php_heap::Zval::bool(true),
            "an empty condition is php's constant true"
        );
        assert_eq!(
            unit.main.safepoints.as_ref(),
            &[OplineIdx(0), OplineIdx(4), OplineIdx(5)],
            "the entry, the back-edge, the return"
        );
        assert_eq!(
            unit.main.oplines[4].out, 1,
            "the back-edge's safepoint number"
        );
        assert_eq!(unit.main.oplines[5].out, 2, "the return's");
    }

    /// The three expression lists and the body, in the order php lays
    /// them down: initialisers, the jump, the body, the step, then the
    /// condition, whose last expression is what the `JMPNZ` tests. The
    /// oracle's dump of this source, under the recipe, is the same
    /// sixteen oplines with `V3 = DO_ICALL` at 13, the one call whose
    /// value is read.
    #[test]
    fn a_for_lays_its_lists_down_in_php_s_order() {
        let (unit, _strings) = unit_and_strings(
            b"<?php for (extension_loaded('i'); extension_loaded('a'), extension_loaded('b'); extension_loaded('s')) echo 'body';",
        );
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                16,
                0,
                4,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"i\") 1\n\
                 0002 DO_ICALL\n\
                 0003 JMP 0008\n\
                 0004 ECHO string(\"body\")\n\
                 0005 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0006 SEND_VAL string(\"s\") 1\n\
                 0007 DO_ICALL\n\
                 0008 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0009 SEND_VAL string(\"a\") 1\n\
                 0010 DO_ICALL\n\
                 0011 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0012 SEND_VAL string(\"b\") 1\n\
                 0013 V3 = DO_ICALL\n\
                 0014 JMPNZ V3 0004\n\
                 0015 RETURN int(1)\n"
            )
        );
        assert_eq!(
            unit.main.oplines[3].op1, 8,
            "the JMP lands on the condition"
        );
        assert_eq!(unit.main.oplines[14].op2, 4, "the JMPNZ lands on the body");
    }

    /// php's own loop, the shape #156's bench runs and the witness beside
    /// `php_heap::Str::bytes` is, under the dump recipe: the `$i++` in
    /// the step is `PRE_INC` with its temporary unread, which is
    /// `zend_do_free`'s conversion, and the condition is `IS_SMALLER`
    /// into the temporary the `JMPNZ` tests, `T3 = ` before it.
    #[test]
    fn php_s_own_for_loop_is_an_is_smaller_and_a_pre_inc() {
        let (unit, _strings) =
            unit_and_strings(b"<?php for ($i = 0; $i < 1000; $i++) { echo 'dax'; }");
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                7,
                1,
                3,
                "0000 ASSIGN CV0($i) int(0)\n\
                 0001 JMP 0004\n\
                 0002 ECHO string(\"dax\")\n\
                 0003 PRE_INC CV0($i)\n\
                 0004 T3 = IS_SMALLER CV0($i) int(1000)\n\
                 0005 JMPNZ T3 0002\n\
                 0006 RETURN int(1)\n"
            )
        );
        assert_eq!(unit.main.oplines[3].op2_type, OperandKind::Unused);
        assert_eq!(unit.main.oplines[3].out, 1, "the increment's temporary");
        assert_eq!(
            unit.main.oplines[4].out, 2,
            "the comparison's, which the JMPNZ tests"
        );
    }

    /// The oracle's dump under the pin's `dump` regime, `vars=1, tmps=4`:
    /// a `<` over a CV is `IS_SMALLER` into a temporary, `$a++` and
    /// `++$a` whose values are read are `POST_INC` and `PRE_INC`, and a
    /// `<` over two constants folds to the bool `zend_compare` orders
    /// them to, a string pair and a double against a string included.
    #[test]
    fn a_comparison_is_is_smaller_where_it_does_not_fold_and_the_bool_where_it_does() {
        assert_eq!(
            dump_of(
                b"<?php echo $a < 2, 1 < $a, $a++, ++$a, 1 < 2, 1.5 < 2, \"a\" < \"b\", 2 < 1, \"10\" < \"9\", 1.5 < \"abc\", null < true;"
            ),
            main_dump(
                16,
                1,
                4,
                "0000 T1 = IS_SMALLER CV0($a) int(2)\n\
                 0001 ECHO T1\n\
                 0002 T2 = IS_SMALLER int(1) CV0($a)\n\
                 0003 ECHO T2\n\
                 0004 T3 = POST_INC CV0($a)\n\
                 0005 ECHO T3\n\
                 0006 T4 = PRE_INC CV0($a)\n\
                 0007 ECHO T4\n\
                 0008 ECHO bool(true)\n\
                 0009 ECHO bool(true)\n\
                 0010 ECHO bool(true)\n\
                 0011 ECHO bool(false)\n\
                 0012 ECHO bool(false)\n\
                 0013 ECHO bool(true)\n\
                 0014 ECHO bool(true)\n\
                 0015 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump, `vars=1, tmps=19`: `<=` is `IS_SMALLER_OR_EQUAL`
    /// as written, `>` and `>=` are `IS_SMALLER` and `IS_SMALLER_OR_EQUAL`
    /// with the operands swapped (`zend_compile_greater`), and the
    /// equality level is its own opcode each, `<>` being `IS_NOT_EQUAL`.
    /// `2 == $a` dumps as `IS_EQUAL CV0($a) int(2)`, and so do `2 * $a`
    /// and the other three equalities, since `pass_two` swaps the
    /// operands of a `COMMUTATIVE` opcode whose first is the lesser
    /// kind; `2 <=> $a` and `2 + $a` keep their order.
    #[test]
    fn the_rest_of_the_comparison_level_is_the_opcode_php_emits_with_php_s_operand_order() {
        assert_eq!(
            dump_of(
                b"<?php echo $a <= 2, 2 <= $a, $a > 2, 2 > $a, $a >= 2, 2 >= $a, $a == 2, 2 == $a, $a != 2, 2 != $a, $a <> 2, $a === 2, 2 === $a, $a !== 2, 2 !== $a, $a <=> 2, 2 <=> $a, 2 * $a, 2 + $a;"
            ),
            main_dump(
                39,
                1,
                19,
                "0000 T1 = IS_SMALLER_OR_EQUAL CV0($a) int(2)\n\
                 0001 ECHO T1\n\
                 0002 T2 = IS_SMALLER_OR_EQUAL int(2) CV0($a)\n\
                 0003 ECHO T2\n\
                 0004 T3 = IS_SMALLER int(2) CV0($a)\n\
                 0005 ECHO T3\n\
                 0006 T4 = IS_SMALLER CV0($a) int(2)\n\
                 0007 ECHO T4\n\
                 0008 T5 = IS_SMALLER_OR_EQUAL int(2) CV0($a)\n\
                 0009 ECHO T5\n\
                 0010 T6 = IS_SMALLER_OR_EQUAL CV0($a) int(2)\n\
                 0011 ECHO T6\n\
                 0012 T7 = IS_EQUAL CV0($a) int(2)\n\
                 0013 ECHO T7\n\
                 0014 T8 = IS_EQUAL CV0($a) int(2)\n\
                 0015 ECHO T8\n\
                 0016 T9 = IS_NOT_EQUAL CV0($a) int(2)\n\
                 0017 ECHO T9\n\
                 0018 T10 = IS_NOT_EQUAL CV0($a) int(2)\n\
                 0019 ECHO T10\n\
                 0020 T11 = IS_NOT_EQUAL CV0($a) int(2)\n\
                 0021 ECHO T11\n\
                 0022 T12 = IS_IDENTICAL CV0($a) int(2)\n\
                 0023 ECHO T12\n\
                 0024 T13 = IS_IDENTICAL CV0($a) int(2)\n\
                 0025 ECHO T13\n\
                 0026 T14 = IS_NOT_IDENTICAL CV0($a) int(2)\n\
                 0027 ECHO T14\n\
                 0028 T15 = IS_NOT_IDENTICAL CV0($a) int(2)\n\
                 0029 ECHO T15\n\
                 0030 T16 = SPACESHIP CV0($a) int(2)\n\
                 0031 ECHO T16\n\
                 0032 T17 = SPACESHIP int(2) CV0($a)\n\
                 0033 ECHO T17\n\
                 0034 T18 = MUL CV0($a) int(2)\n\
                 0035 ECHO T18\n\
                 0036 T19 = ADD int(2) CV0($a)\n\
                 0037 ECHO T19\n\
                 0038 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump, `vars=0, tmps=0`: every comparison over two
    /// constants folds, to the bool `zend_compare` orders them to, the
    /// long `compare_function` writes for `<=>`, and `zend_is_identical`
    /// for `===` and `!==`, which reads the type before the value, so
    /// `1 === "1"` and `null === false` are false where `==` says true.
    #[test]
    fn every_comparison_over_two_constants_folds_to_the_value_php_folds_it_to() {
        assert_eq!(
            dump_of(
                b"<?php echo 1 <= 2, 2 <= 1, 1 > 2, 2 > 1, 1 >= 1, 1 == \"1\", 1 != \"1\", \"abc\" == \"ABC\", 1 === 1, 1 === \"1\", 1 !== 1.0, 1 <=> 2, 2 <=> 1, 1 <=> 1, \"10\" <=> \"9\", 1.5 <=> \"abc\", null == false, null === false, \"1e3\" == \"1000\", \"abc\" <= \"abd\", 1.5 >= \"1.5\", 2 > \"10\";"
            ),
            main_dump(
                23,
                0,
                0,
                "0000 ECHO bool(true)\n\
                 0001 ECHO bool(false)\n\
                 0002 ECHO bool(false)\n\
                 0003 ECHO bool(true)\n\
                 0004 ECHO bool(true)\n\
                 0005 ECHO bool(true)\n\
                 0006 ECHO bool(false)\n\
                 0007 ECHO bool(false)\n\
                 0008 ECHO bool(true)\n\
                 0009 ECHO bool(false)\n\
                 0010 ECHO bool(true)\n\
                 0011 ECHO int(-1)\n\
                 0012 ECHO int(1)\n\
                 0013 ECHO int(0)\n\
                 0014 ECHO int(1)\n\
                 0015 ECHO int(-1)\n\
                 0016 ECHO bool(true)\n\
                 0017 ECHO bool(false)\n\
                 0018 ECHO bool(true)\n\
                 0019 ECHO bool(true)\n\
                 0020 ECHO bool(true)\n\
                 0021 ECHO bool(false)\n\
                 0022 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump, `vars=2, tmps=7`: a `===` or `!==` against a
    /// constant null, false or true on either side is the `TYPE_CHECK`
    /// `zend_compile_binary_op` emits in its place, the constant's
    /// `1 << IS_*` in `extended_value` for `===` and `MAY_BE_ANY` less
    /// that bit for `!==`; against any other constant, or a variable,
    /// it is the opcode itself, and a `==` against null is `IS_EQUAL`.
    #[test]
    fn an_identity_against_null_or_a_bool_is_the_type_check_php_emits_in_its_place() {
        assert_eq!(
            dump_of(
                b"<?php echo $a === null, null === $a, $a !== false, true !== $a, $a === 1, $a == null, $a !== $b;"
            ),
            main_dump(
                15,
                2,
                7,
                "0000 T2 = TYPE_CHECK (null) CV0($a)\n\
                 0001 ECHO T2\n\
                 0002 T3 = TYPE_CHECK (null) CV0($a)\n\
                 0003 ECHO T3\n\
                 0004 T4 = TYPE_CHECK TYPE [null, true, long, double, string, array, object, resource] CV0($a)\n\
                 0005 ECHO T4\n\
                 0006 T5 = TYPE_CHECK TYPE [null, false, long, double, string, array, object, resource] CV0($a)\n\
                 0007 ECHO T5\n\
                 0008 T6 = IS_IDENTICAL CV0($a) int(1)\n\
                 0009 ECHO T6\n\
                 0010 T7 = IS_EQUAL CV0($a) null\n\
                 0011 ECHO T7\n\
                 0012 T8 = IS_NOT_IDENTICAL CV0($a) CV1($b)\n\
                 0013 ECHO T8\n\
                 0014 RETURN int(1)\n"
            )
        );
    }

    /// `zend_do_free` over every place php frees a value: an expression
    /// statement, each expression of a `for`'s three lists but the last
    /// of the condition, and the last of the initialisers and the step.
    /// Each `$a++` there is php's `PRE_INC`; the one an `ASSIGN` reads
    /// stays `POST_INC`. The oracle's dump is the same thirteen oplines,
    /// `vars=2, tmps=10`.
    #[test]
    fn a_post_inc_nothing_reads_is_the_pre_inc_zend_do_free_makes_it() {
        assert_eq!(
            dump_of(b"<?php $a++; ++$a; $b = $a++; for ($a++, $b++; $a++, $a < 1; $a++, ++$a) {}"),
            main_dump(
                13,
                2,
                10,
                "0000 PRE_INC CV0($a)\n\
                 0001 PRE_INC CV0($a)\n\
                 0002 T4 = POST_INC CV0($a)\n\
                 0003 ASSIGN CV1($b) T4\n\
                 0004 PRE_INC CV0($a)\n\
                 0005 PRE_INC CV1($b)\n\
                 0006 JMP 0009\n\
                 0007 PRE_INC CV0($a)\n\
                 0008 PRE_INC CV0($a)\n\
                 0009 PRE_INC CV0($a)\n\
                 0010 T11 = IS_SMALLER CV0($a) int(1)\n\
                 0011 JMPNZ T11 0007\n\
                 0012 RETURN int(1)\n"
            )
        );
    }

    /// The same over `--`: `$a--;` as a statement is php's `PRE_DEC
    /// CV0($a)`, `zend_do_free`'s conversion of the `POST_DEC` nothing
    /// reads, and the ones an `ASSIGN`, an `ECHO` or a comparison reads
    /// stay `POST_DEC`. The oracle's dump is the same fifteen oplines,
    /// `vars=2, tmps=10`.
    #[test]
    fn a_post_dec_nothing_reads_is_the_pre_dec_zend_do_free_makes_it() {
        assert_eq!(
            dump_of(b"<?php $a--; --$a; $b = $a--; echo --$a, $a--; for ($a--; $a-- > 1; --$a) {}"),
            main_dump(
                15,
                2,
                10,
                "0000 PRE_DEC CV0($a)\n\
                 0001 PRE_DEC CV0($a)\n\
                 0002 T4 = POST_DEC CV0($a)\n\
                 0003 ASSIGN CV1($b) T4\n\
                 0004 T6 = PRE_DEC CV0($a)\n\
                 0005 ECHO T6\n\
                 0006 T7 = POST_DEC CV0($a)\n\
                 0007 ECHO T7\n\
                 0008 PRE_DEC CV0($a)\n\
                 0009 JMP 0011\n\
                 0010 PRE_DEC CV0($a)\n\
                 0011 T10 = POST_DEC CV0($a)\n\
                 0012 T11 = IS_SMALLER int(1) T10\n\
                 0013 JMPNZ T11 0010\n\
                 0014 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_while`, and a block whose statements keep their own
    /// lines while the loop's own oplines keep the statement's, as php's
    /// `CG(zend_lineno)` is the condition's again once the condition is
    /// compiled; the final return takes the line the last opline left,
    /// as `zend_emit_final_return` does.
    #[test]
    fn a_while_is_the_same_shape_and_the_body_keeps_its_lines() {
        let (unit, _strings) =
            unit_and_strings(b"<?php\nwhile (extension_loaded('a')) {\n  echo 1;\n}");
        assert_eq!(
            unit.dump(&[]),
            "\n$_main:\n     ; (lines=7, args=0, vars=0, tmps=1)\n\
             \x20    ; (before optimizer)\n     ; :1-4\n     ; return  [] RANGE[0..0]\n\
             0000 JMP 0002\n\
             0001 ECHO int(1)\n\
             0002 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
             0003 SEND_VAL string(\"a\") 1\n\
             0004 V0 = DO_ICALL\n\
             0005 JMPNZ V0 0001\n\
             0006 RETURN int(1)\n"
        );
        assert_eq!(unit.main.oplines[0].op1, 2);
        assert_eq!(unit.main.oplines[5].op2, 1);
        let lines: Vec<u32> = unit.main.oplines.iter().map(|op| op.lineno).collect();
        assert_eq!(lines, vec![2, 3, 2, 2, 2, 2, 2]);
    }

    #[test]
    fn what_this_compiler_cannot_reach_is_refused_rather_than_mis_compiled() {
        // A constant no table here holds.
        let mut strings = php_heap::Strings::new();
        assert!(
            compile(
                &php_syntax::parse(b"<?php echo FOO;", &php_syntax::Options::default()),
                &Options::default(),
                &mut strings
            )
            .is_err()
        );
    }

    /// The oracle's dump under the pin's `dump` regime of
    /// `$a = 1; echo $a, -$a, $A; $c = $b = $a + 1;`, `vars=4, tmps=5`:
    ///
    /// ```text
    /// 0000 ASSIGN CV0($a) int(1)
    /// 0001 ECHO CV0($a)
    /// 0002 T5 = MUL CV0($a) int(-1)
    /// 0003 ECHO T5
    /// 0004 ECHO CV1($A)
    /// 0005 T6 = ADD CV0($a) int(1)
    /// 0006 T7 = ASSIGN CV3($b) T6
    /// 0007 ASSIGN CV2($c) T7
    /// 0008 RETURN int(1)
    /// ```
    ///
    /// `lookup_cv` numbers a name when it is first met, `$a` and `$A`
    /// apart, and `zend_compile_assign` looks the variable up before it
    /// compiles the value, so `$c` is met before `$b`. Each `ASSIGN`
    /// takes a temporary, read or not, which is the five php counts.
    #[test]
    fn a_variable_is_its_cv_and_an_assignment_is_php_s_assign_in_php_s_order() {
        use OperandKind::{Const, Cv, Tmp, Unused};
        let unit = unit_of(b"<?php $a = 1; echo $a, -$a, $A; $c = $b = $a + 1;");
        assert_eq!(
            unit.main.vars.iter().map(|v| &*v.0).collect::<Vec<_>>(),
            [b"a".as_slice(), b"A", b"c", b"b"]
        );
        assert_eq!(unit.main.num_cvs, 4);
        assert_eq!(unit.main.num_tmps, 5);
        let shape: Vec<_> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.op1_type, op.op1, op.op2_type, op.op2, op.out))
            .collect();
        assert_eq!(
            shape,
            [
                (Opcode::Assign, Cv, 0, Const, 0, 0),
                (Opcode::Echo, Cv, 0, Unused, 0, 0),
                (Opcode::Mul, Cv, 0, Const, 1, 1),
                (Opcode::Echo, Tmp, 1, Unused, 0, 0),
                (Opcode::Echo, Cv, 1, Unused, 0, 0),
                (Opcode::Add, Cv, 0, Const, 0, 2),
                (Opcode::Assign, Cv, 3, Tmp, 2, 3),
                (Opcode::Assign, Cv, 2, Tmp, 3, 4),
                (Opcode::Return, Const, 0, Unused, 0, 1),
            ]
        );
    }

    /// `zend_do_free` over the oracle's dump of `$a = 1; $a + 1;
    /// str_repeat("a", 3) . "b"; extension_loaded('Core'); $a++;`,
    /// `vars=1, tmps=6`:
    ///
    /// ```text
    /// 0000 ASSIGN CV0($a) int(1)
    /// 0001 T2 = ADD CV0($a) int(1)
    /// 0002 FREE T2
    /// 0003 INIT_FCALL 2 112 string("str_repeat")
    /// 0004 SEND_VAL string("a") 1
    /// 0005 SEND_VAL int(3) 2
    /// 0006 V3 = DO_ICALL
    /// 0007 T4 = CONCAT V3 string("b")
    /// 0008 FREE T4
    /// 0009 INIT_FCALL 1 96 string("extension_loaded")
    /// 0010 SEND_VAL string("Core") 1
    /// 0011 DO_ICALL
    /// 0012 PRE_INC CV0($a)
    /// 0013 RETURN int(1)
    /// ```
    ///
    /// The `ASSIGN`, the call and the `PRE_INC` are marked `UNUSED` in
    /// `out_type` and keep their numbers; the `ADD` and the `CONCAT`,
    /// which php does not mark, are followed by a `FREE` of their
    /// temporary.
    #[test]
    fn zend_do_free_marks_an_assign_a_call_and_a_pre_inc_and_frees_the_rest() {
        use OperandKind::{Tmp, Unused, Var};
        let (unit, _strings) = unit_and_strings(
            b"<?php $a = 1; $a + 1; str_repeat(\"a\", 3) . \"b\"; extension_loaded('Core'); $a++;",
        );
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                14,
                1,
                6,
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 T2 = ADD CV0($a) int(1)\n\
                 0002 FREE T2\n\
                 0003 INIT_FCALL 2 112 string(\"str_repeat\")\n\
                 0004 SEND_VAL string(\"a\") 1\n\
                 0005 SEND_VAL int(3) 2\n\
                 0006 V3 = DO_ICALL\n\
                 0007 T4 = CONCAT V3 string(\"b\")\n\
                 0008 FREE T4\n\
                 0009 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0010 SEND_VAL string(\"Core\") 1\n\
                 0011 DO_ICALL\n\
                 0012 PRE_INC CV0($a)\n\
                 0013 RETURN int(1)\n"
            )
        );
        let marks: Vec<_> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.out_type, op.out))
            .collect();
        assert_eq!(marks[0], (Opcode::Assign, Unused, 0), "numbered, unread");
        assert_eq!(marks[1], (Opcode::Add, Tmp, 1));
        assert_eq!(marks[2], (Opcode::Free, Unused, 0));
        assert_eq!(unit.main.oplines[2].op1_type, Tmp);
        assert_eq!(unit.main.oplines[2].op1, 1, "the ADD's temporary");
        assert_eq!(marks[6], (Opcode::DoIcall, Var, 2));
        assert_eq!(marks[11], (Opcode::DoIcall, Unused, 4), "numbered, unread");
        assert_eq!(marks[12], (Opcode::PreInc, Unused, 5), "numbered, unread");
        assert_eq!(
            unit.main.oplines[13].out_type, Unused,
            "a site keeps its number bare"
        );
    }

    /// `zend_calc_live_ranges` over the oracle's dump of
    /// `echo str_repeat("a", 2) . str_repeat("b", 2);`, `vars=0,
    /// tmps=3`, which ends in the live-range table:
    ///
    /// ```text
    /// 0000 INIT_FCALL 2 112 string("str_repeat")
    /// 0001 SEND_VAL string("a") 1
    /// 0002 SEND_VAL int(2) 2
    /// 0003 V0 = DO_ICALL
    /// 0004 INIT_FCALL 2 112 string("str_repeat")
    /// 0005 SEND_VAL string("b") 1
    /// 0006 SEND_VAL int(2) 2
    /// 0007 V1 = DO_ICALL
    /// 0008 T2 = CONCAT V0 V1
    /// 0009 ECHO T2
    /// 0010 RETURN int(1)
    /// LIVE RANGES:
    ///      0: 0004 - 0008 (tmp/var)
    /// ```
    ///
    /// The first call's `V0` is defined at 0003 and read at the `CONCAT`
    /// four oplines on, so it is live from the opline after its
    /// definition to its use; `V1` and `T2` are each read by the next
    /// opline, the trivial range php stores nothing for.
    #[test]
    fn a_temporary_read_past_the_next_opline_has_php_s_live_range() {
        use php_unit::{LiveKind, LiveRange};
        let (unit, _strings) =
            unit_and_strings(b"<?php echo str_repeat(\"a\", 2) . str_repeat(\"b\", 2);");
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                11,
                0,
                3,
                "0000 INIT_FCALL 2 112 string(\"str_repeat\")\n\
                 0001 SEND_VAL string(\"a\") 1\n\
                 0002 SEND_VAL int(2) 2\n\
                 0003 V0 = DO_ICALL\n\
                 0004 INIT_FCALL 2 112 string(\"str_repeat\")\n\
                 0005 SEND_VAL string(\"b\") 1\n\
                 0006 SEND_VAL int(2) 2\n\
                 0007 V1 = DO_ICALL\n\
                 0008 T2 = CONCAT V0 V1\n\
                 0009 ECHO T2\n\
                 0010 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0004 - 0008 (tmp/var)\n"
            )
        );
        assert_eq!(
            *unit.main.live_ranges,
            [LiveRange {
                var: 0,
                kind: LiveKind::TmpVar,
                start: OplineIdx(4),
                end: OplineIdx(8),
            }]
        );
    }

    /// A CV is sent by `SEND_VAR` as a call's `VAR` is
    /// (`zend_compile_args`), and the value a native returned is
    /// assigned from its `VAR`, which is the shape #174's witness
    /// compiles to.
    #[test]
    fn a_cv_is_sent_by_send_var_and_assigned_from_a_call_s_var() {
        use OperandKind::{Const, Cv, Unused, Var};
        let unit = unit_of(b"<?php $b = memory_get_usage(); var_dump(memory_get_usage() - $b);");
        let shape: Vec<_> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.op1_type, op.op2_type))
            .collect();
        assert_eq!(
            shape,
            [
                (Opcode::InitFcall, Unused, Const),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Assign, Cv, Var),
                (Opcode::InitFcall, Unused, Const),
                (Opcode::InitFcall, Unused, Const),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Sub, Var, Cv),
                (Opcode::SendVal, OperandKind::Tmp, Unused),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Return, Const, Unused),
            ]
        );
        let sent = unit_of(b"<?php $a = 1; var_dump($a);");
        assert_eq!(sent.main.oplines[2].opcode, Opcode::SendVar);
        assert_eq!(sent.main.oplines[2].op1_type, Cv);
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
            compile_source(b"<?php echo 'shared';", &Options::default(), &mut strings),
            compile_source(
                b"<?php echo 'shared', 'shared';",
                &Options::default(),
                &mut strings,
            ),
        ];
        assert_eq!(strings.count(), 1, "one string, carried by two units");
        let named: Vec<Option<php_heap::Str>> = compiled
            .iter()
            .map(|unit| {
                unit.as_ref()
                    .ok()
                    .and_then(|unit| unit.main.literals.first().copied())
                    .and_then(php_heap::Zval::as_str)
            })
            .collect();
        assert!(named.iter().all(Option::is_some), "{named:?}");
        assert_eq!(
            named[0].map(|s| s.bytes().to_vec()),
            Some(b"shared".to_vec())
        );
        let [Some(one), Some(other)] = [named[0], named[1]] else {
            return;
        };
        assert!(one.is_same(other), "one `zend_string`, named by both units");
    }

    /// The oracle's dump under the pin's `dump` regime of #162's third
    /// program, `echo 1 + strlen('a');`:
    ///
    /// ```text
    /// 0000 ECHO int(2)
    /// 0001 RETURN int(1)
    /// ```
    ///
    /// `zend_try_compile_special_func` reaches `zend_compile_func_strlen`,
    /// which leaves the constant string's length where the call stood,
    /// and `zend_try_ct_eval_binary_op` folds the sum over it.
    #[test]
    fn strlen_over_a_constant_string_is_its_length_and_the_sum_folds_over_it() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1 + strlen('a');");
        assert_eq!(
            unit.dump(&[]),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n"),
            "one ECHO of a constant, no INIT_FCALL and no ADD"
        );
        assert_eq!(unit.main.literals[0].as_long(), Some(2));
    }

    /// The constant a single-expression `echo` leaves, where the whole
    /// expression folded: the first slot, of two with the return's.
    fn folded(source: &[u8]) -> Option<php_heap::Zval> {
        let (unit, _strings) = unit_and_strings(source);
        (unit.main.oplines.len() == 2 && unit.main.oplines[0].opcode == Opcode::Echo)
            .then(|| unit.main.literals.first().copied())
            .flatten()
    }

    /// Whether the source compiles to the ordinary call of `name`: an
    /// `INIT_FCALL` first, over that lowered name, and a `DO_ICALL`.
    fn ordinary_call(source: &[u8], name: &[u8]) -> bool {
        let (unit, _strings) = unit_and_strings(source);
        let oplines = &unit.main.oplines;
        oplines.first().map(|op| op.opcode) == Some(Opcode::InitFcall)
            && literal_text(&unit, 0).as_deref() == Some(name)
            && oplines.iter().any(|op| op.opcode == Opcode::DoIcall)
    }

    /// The four rows of php's table that fold to a constant with no
    /// opcode, each held to what the oracle dumps: `strlen` over a
    /// constant string, `chr` over a long written 0 to 255, `ord` over a
    /// one-byte string, `defined` over a name the compiler substitutes,
    /// and `.` over two literals counting as written, since php's parser
    /// folds it before the compiler looks (`zend_ast_create_concat_op`).
    #[test]
    fn the_four_special_names_fold_to_the_constant_php_dumps() {
        for (source, long) in [
            (&b"<?php echo strlen('a' . 'b');"[..], 2),
            (b"<?php echo strlen('a' . 1);", 2),
            (b"<?php echo strlen('');", 0),
            (b"<?php echo \\strlen('ab');", 2),
            (b"<?php echo STRLEN('ab');", 2),
            (b"<?php echo strlen(chr(65));", 1),
            (b"<?php echo ord('a');", 97),
            (b"<?php echo ord('a' . '');", 97),
        ] {
            assert_eq!(
                folded(source).and_then(php_heap::Zval::as_long),
                Some(long),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for (source, text) in [
            (&b"<?php echo chr(65);"[..], &b"A"[..]),
            (b"<?php echo chr(0);", b"\0"),
            (b"<?php echo chr(255);", b"\xff"),
        ] {
            assert_eq!(
                echoed_string(source),
                text,
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for source in [
            &b"<?php echo defined('INF');"[..],
            b"<?php echo defined('NAN');",
            b"<?php echo defined('true');",
            b"<?php echo defined('TRUE');",
            b"<?php echo defined('null');",
            b"<?php echo defined('IN' . 'F');",
        ] {
            assert_eq!(
                folded(source).map(php_heap::Zval::kind),
                Some(php_heap::ZvalType::True),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// What php leaves to the call itself is the ordinary call here,
    /// and never a constant or an opcode it is not: a `chr` whose long
    /// is not written as such or is outside the byte, an `ord` over any
    /// other string, a `defined` over a namespaced or a class name or
    /// over no written value, a wrong count of arguments, and a name
    /// whose row has no kernel yet.
    #[test]
    fn a_special_name_php_leaves_to_the_call_is_the_ordinary_call() {
        for (source, name) in [
            (&b"<?php echo strlen();"[..], &b"strlen"[..]),
            (b"<?php echo strlen('a', 'b');", b"strlen"),
            (b"<?php echo chr(1 + 64);", b"chr"),
            (b"<?php echo chr(-0);", b"chr"),
            (b"<?php echo chr(-1);", b"chr"),
            (b"<?php echo chr(256);", b"chr"),
            (b"<?php echo chr('65');", b"chr"),
            (b"<?php echo ord('ab');", b"ord"),
            (b"<?php echo ord('');", b"ord"),
            (b"<?php echo ord(65);", b"ord"),
            (b"<?php echo defined('A\\\\B');", b"defined"),
            (b"<?php echo defined('A::B');", b"defined"),
            (b"<?php echo defined(null);", b"defined"),
            (b"<?php echo defined(INF);", b"defined"),
            (b"<?php echo defined();", b"defined"),
            (b"<?php echo count($a, 1);", b"count"),
            (b"<?php echo is_int();", b"is_int"),
            (b"<?php echo is_int($a, $b);", b"is_int"),
            (b"<?php echo intval($a);", b"intval"),
            (b"<?php echo gettype($a);", b"gettype"),
        ] {
            assert!(
                ordinary_call(source, name),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_compile_call` reads `fbc` off `CG(function_table)` before
    /// `zend_try_compile_special_func` reads its table, so under a bundle
    /// that declares none of the names, which `Options::default` is,
    /// every one is the call by name, the four constant shapes included.
    #[test]
    fn a_special_name_the_bundle_has_not_is_the_call_by_name() {
        for (source, name) in [
            (&b"<?php echo strlen($a);"[..], &b"strlen"[..]),
            (b"<?php echo strlen('a');", b"strlen"),
            (b"<?php echo chr(65);", b"chr"),
            (b"<?php echo ord('a');", b"ord"),
            (b"<?php echo defined('INF');", b"defined"),
            (b"<?php echo defined('NOPE');", b"defined"),
            (b"<?php echo is_int($a);", b"is_int"),
            (b"<?php echo count($a);", b"count"),
        ] {
            let mut strings = php_heap::Strings::new();
            let unit = compile(
                &php_syntax::parse(source, &php_syntax::Options::default()),
                &Options::default(),
                &mut strings,
            )
            .unwrap_or_default();
            let oplines = &unit.main.oplines;
            assert!(
                oplines.first().map(|op| op.opcode) == Some(Opcode::InitFcallByName)
                    && literal_text(&unit, 0).as_deref() == Some(name)
                    && oplines.iter().any(|op| op.opcode == Opcode::DoFcallByName),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// The oracle's dump of `echo nope($a);` (#252), a name php's
    /// function table has not: `zend_compile_dynamic_call`'s
    /// `INIT_FCALL_BY_NAME` with the argument count and no used stack,
    /// `SEND_VAR_EX` for the CV and `DO_FCALL_BY_NAME` into the `VAR`.
    /// The constant pair is `zend_add_func_name_literal`'s, the name as
    /// written and then its lowered form in the slot after it, two slots
    /// even where the two are one string.
    #[test]
    fn a_name_the_bundle_has_not_is_init_fcall_by_name_over_the_constant_pair() {
        let (unit, _strings) = unit_and_strings(b"<?php echo nope($a);");
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                5,
                1,
                1,
                "0000 INIT_FCALL_BY_NAME 1 string(\"nope\")\n\
                 0001 SEND_VAR_EX CV0($a) 1\n\
                 0002 V1 = DO_FCALL_BY_NAME\n\
                 0003 ECHO V1\n\
                 0004 RETURN int(1)\n"
            )
        );
        let init = unit.main.oplines[0];
        assert_eq!(
            init.op1, 0,
            "zend_compile_call_common writes used_stack for INIT_FCALL alone"
        );
        assert_eq!(init.op1_type, OperandKind::Unused);
        assert_eq!((init.op2_type, init.op2), (OperandKind::Const, 0));
        assert_eq!(init.out, 0, "one cache slot, zend_alloc_cache_slot");
        assert_eq!(unit.main.cache_size, 8);
        assert_eq!(literal_text(&unit, 0).as_deref(), Some(&b"nope"[..]));
        assert_eq!(
            literal_text(&unit, 1).as_deref(),
            Some(&b"nope"[..]),
            "the lowered name is the slot after the written one"
        );
        assert_eq!(
            unit.main.literals.len(),
            3,
            "the pair and the final return's 1"
        );
    }

    /// The oracle's dump of `echo NoPe(nope2($a), 1 + $a, 'x');`: the
    /// pair keeps the case the name was written in and lowers it beside,
    /// what the inner call returned is `SEND_VAR_NO_REF_EX`, a temporary
    /// and a constant are `SEND_VAL_EX`, and the inner call is its own
    /// by-name sequence inside the outer's.
    #[test]
    fn a_call_by_name_sends_each_argument_as_php_sends_it_for_an_unknown_callee() {
        let (unit, _strings) = unit_and_strings(b"<?php echo NoPe(nope2($a), 1 + $a, 'x');");
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                11,
                1,
                3,
                "0000 INIT_FCALL_BY_NAME 3 string(\"NoPe\")\n\
                 0001 INIT_FCALL_BY_NAME 1 string(\"nope2\")\n\
                 0002 SEND_VAR_EX CV0($a) 1\n\
                 0003 V1 = DO_FCALL_BY_NAME\n\
                 0004 SEND_VAR_NO_REF_EX V1 1\n\
                 0005 T2 = ADD int(1) CV0($a)\n\
                 0006 SEND_VAL_EX T2 2\n\
                 0007 SEND_VAL_EX string(\"x\") 3\n\
                 0008 V3 = DO_FCALL_BY_NAME\n\
                 0009 ECHO V3\n\
                 0010 RETURN int(1)\n"
            )
        );
        assert_eq!(literal_text(&unit, 0).as_deref(), Some(&b"NoPe"[..]));
        assert_eq!(literal_text(&unit, 1).as_deref(), Some(&b"nope"[..]));
        assert_eq!(literal_text(&unit, 2).as_deref(), Some(&b"nope2"[..]));
        assert_eq!(literal_text(&unit, 3).as_deref(), Some(&b"nope2"[..]));
        assert_eq!(
            unit.main.cache_size, 16,
            "one cache slot per INIT_FCALL_BY_NAME"
        );
    }

    /// A name the bundle has keeps `INIT_FCALL` and `DO_ICALL` whatever
    /// its arguments: `strlen('a', 'b')` is in `special::TABLE` and takes
    /// two arguments, which the fold refuses, so it is the ordinary call
    /// over the lowered name with php's used stack, as the oracle dumps
    /// it: `INIT_FCALL 2 112 string("strlen")`.
    #[test]
    fn a_name_the_bundle_has_keeps_init_fcall_and_do_icall() {
        let (unit, _strings) = unit_and_strings(b"<?php echo StrLen('a', 'b');");
        assert_eq!(
            unit.dump(&[]),
            main_dump(
                6,
                0,
                1,
                "0000 INIT_FCALL 2 112 string(\"strlen\")\n\
                 0001 SEND_VAL string(\"a\") 1\n\
                 0002 SEND_VAL string(\"b\") 2\n\
                 0003 V0 = DO_ICALL\n\
                 0004 ECHO V0\n\
                 0005 RETURN int(1)\n"
            )
        );
        assert_eq!(
            unit.main.literals.len(),
            4,
            "the lowered name once, no pair"
        );
    }

    /// The oracle's dump under the pin's `dump` regime of #219's program
    /// and of its kin: `STRLEN` over the CV, over a constant
    /// that is not a string and over what a `CONCAT` left, `COUNT` for
    /// `count` and `sizeof` alike, `TYPE_CHECK` with the mask each
    /// `is_*` writes, and `DEFINED` over the name as a string with its
    /// cache slot, `1.5` written as `"1.5"`; and the name in any case,
    /// with or without a leading separator.
    #[test]
    fn the_opcode_shapes_of_the_special_table_are_php_s() {
        assert_eq!(
            dump_of(b"<?php echo strlen($a), count($a), is_int($a), defined('NOPE');"),
            main_dump(
                9,
                1,
                4,
                "0000 T1 = STRLEN CV0($a)\n\
                 0001 ECHO T1\n\
                 0002 T2 = COUNT CV0($a)\n\
                 0003 ECHO T2\n\
                 0004 T3 = TYPE_CHECK (long) CV0($a)\n\
                 0005 ECHO T3\n\
                 0006 T4 = DEFINED string(\"NOPE\")\n\
                 0007 ECHO T4\n\
                 0008 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(
                b"<?php echo strlen(1), strlen(INF), strlen(null), strlen($a . $b), STRLEN('ab');"
            ),
            main_dump(
                11,
                2,
                5,
                "0000 T2 = STRLEN int(1)\n\
                 0001 ECHO T2\n\
                 0002 T3 = STRLEN float(inf)\n\
                 0003 ECHO T3\n\
                 0004 T4 = STRLEN null\n\
                 0005 ECHO T4\n\
                 0006 T5 = CONCAT CV0($a) CV1($b)\n\
                 0007 T6 = STRLEN T5\n\
                 0008 ECHO T6\n\
                 0009 ECHO int(2)\n\
                 0010 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(
                b"<?php echo defined('inf'), defined(1.5), defined('NOPE'), $x = defined('NOPE');"
            ),
            main_dump(
                10,
                1,
                5,
                "0000 T1 = DEFINED string(\"inf\")\n\
                 0001 ECHO T1\n\
                 0002 T2 = DEFINED string(\"1.5\")\n\
                 0003 ECHO T2\n\
                 0004 T3 = DEFINED string(\"NOPE\")\n\
                 0005 ECHO T3\n\
                 0006 T4 = DEFINED string(\"NOPE\")\n\
                 0007 T5 = ASSIGN CV0($x) T4\n\
                 0008 ECHO T5\n\
                 0009 RETURN int(1)\n"
            )
        );
        let (unit, _strings) =
            unit_and_strings(b"<?php echo sizeof($a), defined('A'), defined('B');");
        let extended: Vec<_> = unit
            .main
            .oplines
            .iter()
            .filter(|op| matches!(op.opcode, Opcode::Count | Opcode::Defined))
            .map(|op| (op.opcode, op.extended_value))
            .collect();
        assert_eq!(
            extended,
            [
                (Opcode::Count, 1),
                (Opcode::Defined, 0),
                (Opcode::Defined, 8)
            ],
            "sizeof's bit, and one cache slot per DEFINED, eight bytes each"
        );
        assert_eq!(unit.main.cache_size, 16);
    }

    /// The oracle's dump of every `is_*` row and of `count` and `sizeof`
    /// over a constant and a CV: `TYPE_CHECK` with the mask each name
    /// writes, `is_bool` two bits and `is_scalar` five, over a constant
    /// where one is written, and `COUNT` the same for both names.
    #[test]
    fn the_type_check_and_count_rows_are_php_s_masks() {
        assert_eq!(
            dump_of(
                b"<?php echo sizeof($a), is_scalar($a), is_bool($a), is_int(1), count(\"x\"), is_null($u), \\is_integer($a), Is_Float($a), is_double($a), is_string($a), is_array($a), is_object($a), is_resource($a), is_long($a);"
            ),
            main_dump(
                29,
                2,
                14,
                "0000 T2 = COUNT CV0($a)\n\
                 0001 ECHO T2\n\
                 0002 T3 = TYPE_CHECK TYPE [bool, long, double, string] CV0($a)\n\
                 0003 ECHO T3\n\
                 0004 T4 = TYPE_CHECK (bool) CV0($a)\n\
                 0005 ECHO T4\n\
                 0006 T5 = TYPE_CHECK (long) int(1)\n\
                 0007 ECHO T5\n\
                 0008 T6 = COUNT string(\"x\")\n\
                 0009 ECHO T6\n\
                 0010 T7 = TYPE_CHECK (null) CV1($u)\n\
                 0011 ECHO T7\n\
                 0012 T8 = TYPE_CHECK (long) CV0($a)\n\
                 0013 ECHO T8\n\
                 0014 T9 = TYPE_CHECK (double) CV0($a)\n\
                 0015 ECHO T9\n\
                 0016 T10 = TYPE_CHECK (double) CV0($a)\n\
                 0017 ECHO T10\n\
                 0018 T11 = TYPE_CHECK (string) CV0($a)\n\
                 0019 ECHO T11\n\
                 0020 T12 = TYPE_CHECK (array) CV0($a)\n\
                 0021 ECHO T12\n\
                 0022 T13 = TYPE_CHECK (object) CV0($a)\n\
                 0023 ECHO T13\n\
                 0024 T14 = TYPE_CHECK (resource) CV0($a)\n\
                 0025 ECHO T14\n\
                 0026 T15 = TYPE_CHECK (long) CV0($a)\n\
                 0027 ECHO T15\n\
                 0028 RETURN int(1)\n"
            )
        );
    }

    /// `zend_do_free` over what the four opcodes leave: none of them is
    /// one php marks `UNUSED`, so a value nothing reads is a `FREE`.
    #[test]
    fn a_special_opcode_nothing_reads_is_freed() {
        assert_eq!(
            dump_of(b"<?php $n = strlen($a); strlen($a); is_int($a); count($a); defined('NOPE');"),
            main_dump(
                11,
                2,
                6,
                "0000 T2 = STRLEN CV1($a)\n\
                 0001 ASSIGN CV0($n) T2\n\
                 0002 T4 = STRLEN CV1($a)\n\
                 0003 FREE T4\n\
                 0004 T5 = TYPE_CHECK (long) CV1($a)\n\
                 0005 FREE T5\n\
                 0006 T6 = COUNT CV1($a)\n\
                 0007 FREE T6\n\
                 0008 T7 = DEFINED string(\"NOPE\")\n\
                 0009 FREE T7\n\
                 0010 RETURN int(1)\n"
            )
        );
    }

    /// The table is `zend_try_compile_special_func_ex`'s names in its
    /// order, `strlen` first and `clone` last, and a name is found by the
    /// row it stands in.
    #[test]
    fn the_special_table_is_php_s_names_in_php_s_order() {
        use special::{Special, TABLE, row};
        assert_eq!(TABLE.len(), 35);
        assert_eq!(TABLE.first().map(|(n, _)| *n), Some(&b"strlen"[..]));
        assert_eq!(TABLE.last().map(|(n, _)| *n), Some(&b"clone"[..]));
        assert_eq!(row(b"defined"), Some(Special::Defined));
        assert_eq!(row(b"is_integer"), Some(Special::TypeCheck(0x10)));
        assert_eq!(row(b"is_scalar"), Some(Special::TypeCheck(0x7c)));
        assert_eq!(row(b"sizeof"), Some(Special::Count { sizeof: true }));
        assert_eq!(row(b"intval"), Some(Special::Opcode));
        assert_eq!(row(b"var_dump"), None);
        let names: Vec<&[u8]> = TABLE.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "each name once");
    }

    /// The oracle's dump under `[regime.dump]` of a call of each
    /// frameless arity `ext/standard` declares: `str_contains` of two,
    /// `trim` of one and of two, `substr` of two, and `str_contains`
    /// over a temporary; each a `FRAMELESS_ICALL_<n>` into a temporary
    /// with the table's row in `extended_value`, the one nothing reads
    /// freed by a `FREE` (`zend_do_free`), and the temporaries numbered
    /// as php numbers them, one skipped per assignment.
    #[test]
    fn a_call_a_frameless_entry_serves_is_frameless_icall_with_its_row() {
        let source = b"<?php $a = \"x\"; $b = str_contains($a, \"y\"); $t = trim($a); $u = trim($a, \"x\"); substr($a, 1); echo str_contains($a . \"z\", \"z\");";
        assert_eq!(
            dump_of(source),
            main_dump(
                13,
                4,
                10,
                "0000 ASSIGN CV0($a) string(\"x\")\n\
                 0001 T5 = FRAMELESS_ICALL_2(str_contains) CV0($a) string(\"y\")\n\
                 0002 ASSIGN CV1($b) T5\n\
                 0003 T7 = FRAMELESS_ICALL_1(trim) CV0($a)\n\
                 0004 ASSIGN CV2($t) T7\n\
                 0005 T9 = FRAMELESS_ICALL_2(trim) CV0($a) string(\"x\")\n\
                 0006 ASSIGN CV3($u) T9\n\
                 0007 T11 = FRAMELESS_ICALL_2(substr) CV0($a) int(1)\n\
                 0008 FREE T11\n\
                 0009 T12 = CONCAT CV0($a) string(\"z\")\n\
                 0010 T13 = FRAMELESS_ICALL_2(str_contains) T12 string(\"z\")\n\
                 0011 ECHO T13\n\
                 0012 RETURN int(1)\n"
            )
        );
        let unit = unit_of(source);
        let rows: Vec<(Opcode, u32)> = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode.is_frameless_icall())
            .map(|op| (op.opcode, op.extended_value))
            .collect();
        assert_eq!(
            rows,
            [
                (Opcode::FramelessIcall2, 2),
                (Opcode::FramelessIcall1, 0),
                (Opcode::FramelessIcall2, 1),
                (Opcode::FramelessIcall2, 3),
                (Opcode::FramelessIcall2, 2),
            ],
            "zend_flf_functions in registration order: trim 1, trim 2, str_contains 2, substr 2, substr 3"
        );
    }

    /// A third argument rides in an `OP_DATA` after the opline
    /// (`zend_emit_op_data`), and `zend_do_free` steps back over it to
    /// the call that left the value. The oracle's dump prints a live
    /// range for the temporary across the `OP_DATA`, #255's table, and
    /// the dump here is the oracle's under `[regime.dump]`, live ranges
    /// and all.
    #[test]
    fn a_third_argument_is_an_op_data_after_the_frameless_call() {
        let source = b"<?php $a = \"x\"; $s = substr($a, 1, 2); substr($a, 1, 2);";
        assert_eq!(
            dump_of(source),
            main_dump(
                8,
                2,
                4,
                "0000 ASSIGN CV0($a) string(\"x\")\n\
                 0001 T3 = FRAMELESS_ICALL_3(substr) CV0($a) int(1)\n\
                 0002 OP_DATA int(2)\n\
                 0003 ASSIGN CV1($s) T3\n\
                 0004 T5 = FRAMELESS_ICALL_3(substr) CV0($a) int(1)\n\
                 0005 OP_DATA int(2)\n\
                 0006 FREE T5\n\
                 0007 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    3: 0002 - 0003 (tmp/var)\n\
                 \x20    5: 0005 - 0006 (tmp/var)\n"
            )
        );
    }

    /// A count no entry serves is the ordinary call, whatever the
    /// name: `str_contains` of one argument is below its required
    /// count, `trim` of three is above every arity, and a name the
    /// bundle has not is the call by name.
    #[test]
    fn a_count_no_entry_serves_is_the_ordinary_call() {
        assert_eq!(
            dump_of(b"<?php str_contains($a); trim($a, $b, $c); nope($a, $b);"),
            main_dump(
                13,
                3,
                3,
                "0000 INIT_FCALL 1 96 string(\"str_contains\")\n\
                 0001 SEND_VAR CV0($a) 1\n\
                 0002 DO_ICALL\n\
                 0003 INIT_FCALL 3 128 string(\"trim\")\n\
                 0004 SEND_VAR CV0($a) 1\n\
                 0005 SEND_VAR CV1($b) 2\n\
                 0006 SEND_VAR CV2($c) 3\n\
                 0007 DO_ICALL\n\
                 0008 INIT_FCALL_BY_NAME 2 string(\"nope\")\n\
                 0009 SEND_VAR_EX CV0($a) 1\n\
                 0010 SEND_VAR_EX CV1($b) 2\n\
                 0011 DO_FCALL_BY_NAME\n\
                 0012 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump of `zend_compile_ns_call`'s shape for an
    /// unqualified name in a namespace: `JMP_FRAMELESS` over the
    /// lowered name with its cache slot, printed in bytes, the call by
    /// name through `INIT_NS_FCALL_BY_NAME`, `SEND_*_EX` and
    /// `DO_FCALL_BY_NAME`, the `JMP` past the branch, and the
    /// `FRAMELESS_ICALL_<n>` into the same `VAR`, freed by a `FREE`
    /// where nothing reads it. A `\`-qualified name resolves at compile
    /// time as outside a namespace, and a call nested in the branch
    /// takes no branch of its own (`in_jmp_frameless_branch`).
    #[test]
    fn an_unqualified_name_in_a_namespace_is_jmp_frameless_around_the_call_by_name() {
        assert_eq!(
            dump_of(b"<?php namespace Foo; $a = \"x\"; $b = str_contains($a, \"y\"); \\trim($a); trim(nope($a));"),
            main_dump(
                25,
                2,
                7,
                "0000 ASSIGN CV0($a) string(\"x\")\n\
                 0001 JMP_FRAMELESS 8 string(\"foo\\\\str_contains\") 0007\n\
                 0002 INIT_NS_FCALL_BY_NAME 2 string(\"Foo\\\\str_contains\")\n\
                 0003 SEND_VAR_EX CV0($a) 1\n\
                 0004 SEND_VAL_EX string(\"y\") 2\n\
                 0005 V3 = DO_FCALL_BY_NAME\n\
                 0006 JMP 0008\n\
                 0007 V3 = FRAMELESS_ICALL_2(str_contains) CV0($a) string(\"y\")\n\
                 0008 ASSIGN CV1($b) V3\n\
                 0009 T5 = FRAMELESS_ICALL_1(trim) CV0($a)\n\
                 0010 FREE T5\n\
                 0011 JMP_FRAMELESS 40 string(\"foo\\\\trim\") 0019\n\
                 0012 INIT_NS_FCALL_BY_NAME 1 string(\"Foo\\\\trim\")\n\
                 0013 INIT_NS_FCALL_BY_NAME 1 string(\"Foo\\\\nope\")\n\
                 0014 SEND_VAR_EX CV0($a) 1\n\
                 0015 V6 = DO_FCALL_BY_NAME\n\
                 0016 SEND_VAR_NO_REF_EX V6 1\n\
                 0017 V7 = DO_FCALL_BY_NAME\n\
                 0018 JMP 0023\n\
                 0019 INIT_NS_FCALL_BY_NAME 1 string(\"Foo\\\\nope\")\n\
                 0020 SEND_VAR_EX CV0($a) 1\n\
                 0021 V8 = DO_FCALL_BY_NAME\n\
                 0022 V7 = FRAMELESS_ICALL_1(trim) V8\n\
                 0023 FREE V7\n\
                 0024 RETURN int(1)\n"
            )
        );
    }

    /// The three constants `zend_add_ns_func_name_literal` lays down,
    /// in a row: the name with its namespace as written, lowered, and
    /// the lowered name alone; a second `namespace` statement moves the
    /// namespace, and a qualified name is the namespace before it,
    /// resolved at compile time as `INIT_FCALL_BY_NAME`.
    #[test]
    fn a_namespace_call_carries_php_s_three_constants() {
        let (unit, _strings) =
            unit_and_strings(b"<?php namespace A; namespace B\\C; Str_Contains(\"a\", \"b\"); D\\e(); namespace\\f();");
        let literal = |at: usize| {
            unit.main.literals[at]
                .as_str()
                .map(|s| String::from_utf8_lossy(s.bytes()).into_owned())
        };
        assert_eq!(
            unit.main.oplines[1].opcode,
            Opcode::InitNsFcallByName,
            "after the JMP_FRAMELESS"
        );
        let at = unit.main.oplines[1].op2 as usize;
        assert_eq!(literal(at).as_deref(), Some("B\\C\\Str_Contains"));
        assert_eq!(literal(at + 1).as_deref(), Some("b\\c\\str_contains"));
        assert_eq!(literal(at + 2).as_deref(), Some("str_contains"));
        let by_name: Vec<String> = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode == Opcode::InitFcallByName)
            .filter_map(|op| literal(op.op2 as usize))
            .collect();
        assert_eq!(by_name, ["B\\C\\D\\e", "B\\C\\f"]);
    }

    /// `zend_compile_namespace`: a first `namespace` after any statement
    /// is php's `E_COMPILE_ERROR` in php's words at the statement's line.
    #[test]
    fn a_namespace_after_a_statement_is_php_s_compile_error() {
        let mut strings = php_heap::Strings::new();
        let trouble = compile(
            &php_syntax::parse(b"<?php 1;\nnamespace Foo;", &php_syntax::Options::default()),
            &bundled(),
            &mut strings,
        )
        .err();
        assert_eq!(
            trouble,
            Some(CompileError {
                level: Level::CompileError,
                message: "Namespace declaration statement has to be the very first statement or after any declare call in the script".into(),
                line: 2,
                raised: Vec::new(),
            })
        );
    }

    #[test]
    fn what_the_reader_cannot_read_is_a_compile_error_at_its_line() {
        let mut strings = php_heap::Strings::new();
        let trouble = compile(
            &php_syntax::parse(b"<?php\n$x | 1;", &php_syntax::Options::default()),
            &Options::default(),
            &mut strings,
        )
        .err();
        assert_eq!(trouble.map(|e| e.line), Some(2));
    }

    /// A file php's parser refuses is refused here in php's words, the
    /// first diagnostic `parse` handed back, before the reader is asked
    /// anything: `echo 1 +;` is php's `unexpected token ";"` and not the
    /// reader's sentence about the expression.
    #[test]
    fn what_php_refuses_is_the_first_parse_error_in_phps_words() {
        let mut strings = php_heap::Strings::new();
        let trouble = compile(
            &php_syntax::parse(b"<?php\necho 1 +;", &php_syntax::Options::default()),
            &Options::default(),
            &mut strings,
        )
        .err();
        assert_eq!(
            trouble,
            Some(CompileError {
                level: Level::Parse,
                message: "syntax error, unexpected token \";\"".into(),
                line: 2,
                raised: Vec::new(),
            })
        );
    }

    /// The oplines and the live-range table of one source's main op
    /// array, the header left off: what the oracle's dump prints after
    /// `; return  [] RANGE[0..0]`.
    fn oplines_of(source: &[u8]) -> String {
        let dump = dump_of(source);
        dump.split_once("RANGE[0..0]\n")
            .map_or(dump.clone(), |(_, rest)| rest.to_owned())
    }

    /// The compile error one source refuses with, or nothing.
    fn refused(source: &[u8]) -> Option<CompileError> {
        let mut strings = php_heap::Strings::new();
        compile(
            &php_syntax::parse(source, &php_syntax::Options::default()),
            &bundled(),
            &mut strings,
        )
        .err()
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
        assert_eq!(
            unit.dump(&[]),
            "\n$_main:\n     ; (lines=20, args=0, vars=1, tmps=4)\n\
             \x20    ; (before optimizer)\n     ; :1-5\n     ; return  [] RANGE[0..0]\n\
             0000 ASSIGN CV0($a) int(1)\n\
             0001 T2 = IS_EQUAL CV0($a) int(1)\n\
             0002 JMPNZ T2 0006\n\
             0003 T2 = IS_EQUAL CV0($a) int(2)\n\
             0004 JMPNZ T2 0008\n\
             0005 JMP 0010\n\
             0006 ECHO string(\"a\")\n\
             0007 JMP 0011\n\
             0008 ECHO string(\"b\")\n\
             0009 JMP 0011\n\
             0010 ECHO string(\"c\")\n\
             0011 MATCH CV0($a) 1: 0012, \"s\": 0014, default: 0016\n\
             0012 T4 = QM_ASSIGN string(\"x\")\n\
             0013 JMP 0018\n\
             0014 T4 = QM_ASSIGN string(\"y\")\n\
             0015 JMP 0018\n\
             0016 T4 = QM_ASSIGN string(\"z\")\n\
             0017 JMP 0018\n\
             0018 ECHO T4\n\
             0019 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    4: 0017 - 0018 (tmp/var)\n"
        );
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

    /// Five `long` cases, the default among them, are what
    /// `should_use_jumptable` asks for: a `SWITCH_LONG` over the table
    /// before the chain, its `extended_value` the default's target.
    #[test]
    fn a_switch_of_five_long_cases_is_switch_long_over_its_table_before_the_chain() {
        assert_eq!(
            oplines_of(b"<?php\n$a = 3;\nswitch ($a) { case 1: echo \"a\"; break; case 2: echo \"b\"; break; case 3: echo \"c\"; break; case 4: echo \"d\"; break; case 5: echo \"e\"; break; default: echo \"f\"; }\n"),
            "0000 ASSIGN CV0($a) int(3)\n\
             0001 SWITCH_LONG CV0($a) 1: 0013, 2: 0015, 3: 0017, 4: 0019, 5: 0021, default: 0023\n\
             0002 T2 = IS_EQUAL CV0($a) int(1)\n\
             0003 JMPNZ T2 0013\n\
             0004 T2 = IS_EQUAL CV0($a) int(2)\n\
             0005 JMPNZ T2 0015\n\
             0006 T2 = IS_EQUAL CV0($a) int(3)\n\
             0007 JMPNZ T2 0017\n\
             0008 T2 = IS_EQUAL CV0($a) int(4)\n\
             0009 JMPNZ T2 0019\n\
             0010 T2 = IS_EQUAL CV0($a) int(5)\n\
             0011 JMPNZ T2 0021\n\
             0012 JMP 0023\n\
             0013 ECHO string(\"a\")\n\
             0014 JMP 0024\n\
             0015 ECHO string(\"b\")\n\
             0016 JMP 0024\n\
             0017 ECHO string(\"c\")\n\
             0018 JMP 0024\n\
             0019 ECHO string(\"d\")\n\
             0020 JMP 0024\n\
             0021 ECHO string(\"e\")\n\
             0022 JMP 0024\n\
             0023 ECHO string(\"f\")\n\
             0024 RETURN int(1)\n"
        );
    }

    /// Two `string` cases are a `SWITCH_STRING`; with no default the
    /// table's default target is the switch's end. A numeric string
    /// case, which `==` compares as a number, keeps the chain alone
    /// (`determine_switch_jumptable_type`).
    #[test]
    fn a_switch_of_two_string_cases_is_switch_string_and_a_numeric_string_case_is_not() {
        assert_eq!(
            oplines_of(b"<?php\n$a = \"y\";\nswitch ($a) { case \"x\": echo \"a\"; break; case \"y\": echo \"b\"; break; }\n"),
            "0000 ASSIGN CV0($a) string(\"y\")\n\
             0001 SWITCH_STRING CV0($a) \"x\": 0007, \"y\": 0009, default: 0011\n\
             0002 T2 = IS_EQUAL CV0($a) string(\"x\")\n\
             0003 JMPNZ T2 0007\n\
             0004 T2 = IS_EQUAL CV0($a) string(\"y\")\n\
             0005 JMPNZ T2 0009\n\
             0006 JMP 0011\n\
             0007 ECHO string(\"a\")\n\
             0008 JMP 0011\n\
             0009 ECHO string(\"b\")\n\
             0010 JMP 0011\n\
             0011 RETURN int(1)\n"
        );
        assert_eq!(
            oplines_of(
                b"<?php\nswitch ($a) { case \"1\": echo \"a\"; break; case \"x\": echo \"b\"; }\n"
            ),
            "0000 T1 = IS_EQUAL CV0($a) string(\"1\")\n\
             0001 JMPNZ T1 0005\n\
             0002 T1 = IS_EQUAL CV0($a) string(\"x\")\n\
             0003 JMPNZ T1 0007\n\
             0004 JMP 0008\n\
             0005 ECHO string(\"a\")\n\
             0006 JMP 0008\n\
             0007 ECHO string(\"b\")\n\
             0008 RETURN int(1)\n"
        );
    }

    /// A constant `true` or `false` subject tests each case alone, a
    /// `JMPNZ` or a `JMPZ` over it; any other constant is an `IS_EQUAL`
    /// over two constants, swapped where `pass_two` swaps them; and a
    /// value written as two cases keeps the first target in the table,
    /// `zend_hash_index_add`, while the chain tests both.
    #[test]
    fn a_constant_subject_is_tested_as_php_tests_it_and_a_repeated_case_keeps_its_first_target() {
        assert_eq!(
            oplines_of(b"<?php\nswitch (true) { case $a: echo \"a\"; break; case 2: echo \"b\"; }\nswitch (false) { case $a: echo \"a\"; }\nswitch (3) { case $a: echo \"a\"; case 1: echo \"b\"; }\nswitch ($a) { case 1: case 2: case 3: case 4: case 4: echo \"x\"; }\n"),
            "0000 JMPNZ CV0($a) 0003\n\
             0001 JMPNZ int(2) 0005\n\
             0002 JMP 0006\n\
             0003 ECHO string(\"a\")\n\
             0004 JMP 0006\n\
             0005 ECHO string(\"b\")\n\
             0006 JMPZ CV0($a) 0008\n\
             0007 JMP 0009\n\
             0008 ECHO string(\"a\")\n\
             0009 T3 = IS_EQUAL CV0($a) int(3)\n\
             0010 JMPNZ T3 0014\n\
             0011 T3 = IS_EQUAL int(3) int(1)\n\
             0012 JMPNZ T3 0015\n\
             0013 JMP 0016\n\
             0014 ECHO string(\"a\")\n\
             0015 ECHO string(\"b\")\n\
             0016 SWITCH_LONG CV0($a) 1: 0028, 2: 0028, 3: 0028, 4: 0028, default: 0029\n\
             0017 T4 = IS_EQUAL CV0($a) int(1)\n\
             0018 JMPNZ T4 0028\n\
             0019 T4 = IS_EQUAL CV0($a) int(2)\n\
             0020 JMPNZ T4 0028\n\
             0021 T4 = IS_EQUAL CV0($a) int(3)\n\
             0022 JMPNZ T4 0028\n\
             0023 T4 = IS_EQUAL CV0($a) int(4)\n\
             0024 JMPNZ T4 0028\n\
             0025 T4 = IS_EQUAL CV0($a) int(4)\n\
             0026 JMPNZ T4 0028\n\
             0027 JMP 0029\n\
             0028 ECHO string(\"x\")\n\
             0029 RETURN int(1)\n"
        );
    }

    /// A temporary subject is compared by `CASE`, which keeps it alive
    /// for the next case, and freed at the switch's end, which is where
    /// `break` lands; `continue` in a `for` lands on the step; a
    /// `break 2` or `continue 2` out of a `switch` frees its subject on
    /// the way, `ZEND_FREE_ON_RETURN`, and the live range is split at
    /// that `FREE` with the distance to the loop's end in its `op2`.
    #[test]
    fn a_temporary_subject_is_case_and_freed_where_break_lands_and_on_the_way_out_of_a_switch() {
        assert_eq!(
            oplines_of(b"<?php\nswitch (strlen($a)) { case 1: echo \"a\"; break; case 2: echo \"b\"; }\nfor ($i = 0; $i < 3; $i++) { continue; break 1; }\nwhile ($i) { switch (strlen($a)) { case 1: continue 2; case 2: break 2; default: break; } }\n"),
            "0000 T2 = STRLEN CV0($a)\n\
             0001 T3 = CASE T2 int(1)\n\
             0002 JMPNZ T3 0006\n\
             0003 T3 = CASE T2 int(2)\n\
             0004 JMPNZ T3 0008\n\
             0005 JMP 0009\n\
             0006 ECHO string(\"a\")\n\
             0007 JMP 0009\n\
             0008 ECHO string(\"b\")\n\
             0009 FREE T2\n\
             0010 ASSIGN CV1($i) int(0)\n\
             0011 JMP 0015\n\
             0012 JMP 0014\n\
             0013 JMP 0017\n\
             0014 PRE_INC CV1($i)\n\
             0015 T6 = IS_SMALLER CV1($i) int(3)\n\
             0016 JMPNZ T6 0012\n\
             0017 JMP 0030\n\
             0018 T7 = STRLEN CV0($a)\n\
             0019 T8 = CASE T7 int(1)\n\
             0020 JMPNZ T8 0024\n\
             0021 T8 = CASE T7 int(2)\n\
             0022 JMPNZ T8 0026\n\
             0023 JMP 0028\n\
             0024 FREE T7 loop-end(+5)\n\
             0025 JMP 0030\n\
             0026 FREE T7 loop-end(+3)\n\
             0027 JMP 0031\n\
             0028 JMP 0029\n\
             0029 FREE T7\n\
             0030 JMPNZ CV1($i) 0018\n\
             0031 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    2: 0001 - 0009 (tmp/var)\n\
             \x20    7: 0019 - 0024 (tmp/var)\n\
             \x20    7: 0028 - 0029 (tmp/var)\n"
        );
    }

    /// `match` as `zend_compile_match` shapes it: two conditions of one
    /// arm share a target; no default arm is a `MATCH_ERROR` emitted
    /// first, the table's default target; one condition is the
    /// `IS_IDENTICAL` chain with its default `JMP` to the `MATCH_ERROR`;
    /// a default arm alone is the `JMP` to it; and no arm at all is the
    /// constant `null`.
    #[test]
    fn a_match_is_its_table_or_the_is_identical_chain_and_match_error_where_no_arm_is_default() {
        assert_eq!(
            oplines_of(b"<?php\n$a = 2;\necho match ($a) { 1, 2 => \"x\", \"s\" => \"y\" };\necho match (true) { default => \"d\" };\necho match ($a) { 1 => \"one\" };\n$x = match ($a) {};\n"),
            "0000 ASSIGN CV0($a) int(2)\n\
             0001 MATCH CV0($a) 1: 0003, 2: 0003, \"s\": 0005, default: 0002\n\
             0002 MATCH_ERROR CV0($a)\n\
             0003 T4 = QM_ASSIGN string(\"x\")\n\
             0004 JMP 0007\n\
             0005 T4 = QM_ASSIGN string(\"y\")\n\
             0006 JMP 0007\n\
             0007 ECHO T4\n\
             0008 JMP 0009\n\
             0009 T6 = QM_ASSIGN string(\"d\")\n\
             0010 JMP 0011\n\
             0011 ECHO T6\n\
             0012 T7 = IS_IDENTICAL CV0($a) int(1)\n\
             0013 JMPNZ T7 0016\n\
             0014 JMP 0015\n\
             0015 MATCH_ERROR CV0($a)\n\
             0016 T8 = QM_ASSIGN string(\"one\")\n\
             0017 JMP 0018\n\
             0018 ECHO T8\n\
             0019 JMP 0020\n\
             0020 MATCH_ERROR CV0($a)\n\
             0021 ASSIGN CV1($x) null\n\
             0022 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    4: 0006 - 0007 (tmp/var)\n\
             \x20    6: 0010 - 0011 (tmp/var)\n\
             \x20    8: 0017 - 0018 (tmp/var)\n"
        );
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
                (Opcode::Return, 18),
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
            let error = refused(source).unwrap_or_else(|| CompileError {
                level: Level::Parse,
                message: String::from("compiled"),
                line: 0,
                raised: Vec::new(),
            });
            assert_eq!(error.level, Level::CompileError, "{message}");
            assert_eq!(error.message, message);
            assert_eq!(error.line, 2, "{message}");
        }
    }

    /// A `continue` that targets a `switch` is the `break` it is, with
    /// php's compile-time warning on the unit, the hint about the loop
    /// outside where there is one.
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
                    Level::CompileWarning,
                    2,
                    "\"continue\" targeting switch is equivalent to \"break\". Did you mean to use \"continue 2\"?"
                ),
                (
                    Level::CompileWarning,
                    3,
                    "\"continue\" targeting switch is equivalent to \"break\""
                ),
                (
                    Level::CompileWarning,
                    5,
                    "\"continue 2\" targeting switch is equivalent to \"break 2\". Did you mean to use \"continue 3\"?"
                ),
            ]
        );
        assert_eq!(
            oplines_of(b"<?php\nfor (;;) { switch ($a) { case 1: continue; } }\nswitch ($a) { case 1: continue; }\n"),
            "0000 JMP 0005\n\
             0001 T1 = IS_EQUAL CV0($a) int(1)\n\
             0002 JMPNZ T1 0004\n\
             0003 JMP 0005\n\
             0004 JMP 0005\n\
             0005 JMPNZ bool(true) 0001\n\
             0006 T2 = IS_EQUAL CV0($a) int(1)\n\
             0007 JMPNZ T2 0009\n\
             0008 JMP 0010\n\
             0009 JMP 0010\n\
             0010 RETURN int(1)\n"
        );
    }

    /// `zend_compile_class_decl` for the class the doc-comment witness
    /// of `php_syntax` declares (#257): bound at compile time with no
    /// opline, so the main op array is the oracle's `ECHO` and `RETURN`
    /// alone; the declaration among the unit's classes with each
    /// declarator's doc comment as `backup_doc_comment` took it (probed
    /// 8.5.9: `/** P */ public $x, $y;` gives P to `$x` alone), the
    /// constants' values as written, and a `}` clearing what is pending.
    #[test]
    fn a_class_with_properties_and_constants_is_bound_with_no_opline() {
        let (unit, _strings) = unit_and_strings(
            b"<?php\nclass M { /** P */ public $x, $y = 1 /** P2 */; /** K */ const A = 1, B = 2; static $s; /** F */ final protected const C = -A; }\n/** D */ { } class N /** D2 */ { var $v /** V */; }\necho 1;\n",
        );
        assert_eq!(
            unit.dump(&[]),
            "\n$_main:\n     ; (lines=2, args=0, vars=0, tmps=0)\n     ; (before optimizer)\n     ; :1-5\n     ; return  [] RANGE[0..0]\n0000 ECHO int(1)\n0001 RETURN int(1)\n",
            "the class emits no opline; php binds it at compile time"
        );
        assert_eq!(unit.classes.len(), 2, "two classes, got {:?}", unit.classes);
        let (m, n) = (&unit.classes[0], &unit.classes[1]);
        assert_eq!(
            (m.name.0.as_ref(), m.line, m.doc_comment.as_deref()),
            (b"M".as_slice(), 2, None)
        );
        let props: Vec<_> = m
            .properties
            .iter()
            .map(|p| {
                (
                    p.name.0.as_ref(),
                    p.is_instance,
                    p.default.clone(),
                    p.doc_comment.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            props,
            vec![
                (b"x".as_slice(), true, None, Some(b"/** P */".as_slice())),
                (b"y", true, Some(ConstExpr::Long(1)), Some(b"/** P2 */")),
                (b"s", false, None, None),
            ]
        );
        let consts: Vec<_> = m
            .constants
            .iter()
            .map(|c| {
                (
                    c.name.0.as_ref(),
                    c.visibility,
                    c.is_final,
                    c.value.clone(),
                    c.doc_comment.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            consts,
            vec![
                (
                    b"A".as_slice(),
                    Visibility::Public,
                    false,
                    ConstExpr::Long(1),
                    Some(b"/** K */".as_slice())
                ),
                (b"B", Visibility::Public, false, ConstExpr::Long(2), None),
                (
                    b"C",
                    Visibility::Protected,
                    true,
                    ConstExpr::Unary {
                        op: Name(b"-".as_slice().into()),
                        operand: Box::new(ConstExpr::Constant(Name(b"A".as_slice().into()))),
                    },
                    Some(b"/** F */"),
                ),
            ]
        );
        assert_eq!(
            (n.name.0.as_ref(), n.line, n.doc_comment.as_deref()),
            (b"N".as_slice(), 3, Some(b"/** D2 */".as_slice())),
            "the close brace before it cleared D, and a class takes before its open brace"
        );
        assert_eq!(
            n.properties[0].doc_comment.as_deref(),
            Some(b"/** V */".as_slice())
        );
        assert_eq!(
            n.properties[0].visibility,
            Visibility::Public,
            "`var` is public"
        );
    }

    /// What `zend_compile_prop_decl`, `zend_compile_class_const_decl`,
    /// `zend_assert_valid_class_name` and `zend_add_member_modifier`
    /// refuse, in php's words at the declarator's line, and what this
    /// compiler does not read yet, refused as such rather than compiled
    /// to something else.
    #[test]
    fn what_php_s_compiler_refuses_in_a_class_is_refused_in_its_words() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile(
                &php_syntax::parse(source, &php_syntax::Options::default()),
                &bundled(),
                &mut strings,
            )
            .err()
            .map(|e| (e.level, e.message, e.line))
        };
        let compile_error =
            |message: &str, line| Some((Level::CompileError, message.to_string(), line));
        assert_eq!(
            refused(b"<?php\nclass M {\n public $x;\n public $x; }"),
            compile_error("Cannot redeclare M::$x", 4)
        );
        assert_eq!(
            refused(b"<?php\nclass M { const A = 1;\n const A = 2; }"),
            compile_error("Cannot redefine class constant M::A", 3)
        );
        assert_eq!(
            refused(b"<?php\nclass Int {}"),
            compile_error("Cannot use \"Int\" as a class name as it is reserved", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { public $x = $y; }"),
            compile_error("Constant expression contains invalid operations", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { const A = f(); }"),
            compile_error("Constant expression contains invalid operations", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { public private $x; }"),
            compile_error("Multiple access type modifiers are not allowed", 2)
        );
        let not_yet = |message: &str, line| Some((Level::Parse, message.to_string(), line));
        assert_eq!(
            refused(b"<?php\nclass M {}\nclass m {}"),
            not_yet("a class declared twice this compiler cannot read yet", 3)
        );
        assert_eq!(
            refused(b"<?php\nclass M extends N {}"),
            not_yet(
                "a class with a parent, an interface or an attribute this compiler cannot read yet",
                2
            )
        );
        assert_eq!(
            refused(b"<?php\nclass M { public function f() {} }"),
            not_yet("a class member this compiler cannot read yet", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { public int $x; }"),
            not_yet("a class member this compiler cannot read yet", 2)
        );
    }

    /// `zend_compile_new`, `zend_compile_static_call`,
    /// `zend_compile_method_call` and `zend_compile_instanceof` as the
    /// oracle at 8.5.9 dumps them under `opcache.opt_debug_level=0x10000`,
    /// each source read once: `NEW` into a `VAR` with its `DO_FCALL`
    /// unused and the `FREE` or the live range `zend_do_free` leaves;
    /// the two `INIT_*_METHOD_CALL`s with their arguments sent for a
    /// callee nothing is known of; `INSTANCEOF` into a temporary, a
    /// chain of them grouped to the left with the second over the
    /// first's temporary, and `false` where the left side is a constant.
    #[test]
    fn new_compiles_to_php_s_op_array() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $o = new E;",
                4,
                1,
                3,
                "0000 V1 = NEW 0 string(\"E\")\n\
                 0001 DO_FCALL\n\
                 0002 ASSIGN CV0($o) V1\n\
                 0003 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    1: 0001 - 0002 (new)\n",
            ),
            (
                b"<?php new E;",
                4,
                0,
                2,
                "0000 V0 = NEW 0 string(\"E\")\n\
                 0001 DO_FCALL\n\
                 0002 FREE V0\n\
                 0003 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0002 (new)\n",
            ),
            (
                b"<?php echo (new E(1))->m();",
                7,
                0,
                3,
                "0000 V0 = NEW 1 string(\"E\")\n\
                 0001 SEND_VAL_EX int(1) 1\n\
                 0002 DO_FCALL\n\
                 0003 INIT_METHOD_CALL 0 V0 string(\"m\")\n\
                 0004 V2 = DO_FCALL\n\
                 0005 ECHO V2\n\
                 0006 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0003 (new)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        let (unit, _strings) = unit_and_strings(b"<?php new E; $o instanceof E;");
        assert_eq!(
            unit.main.cache_size, 16,
            "NEW and INSTANCEOF take one cache slot each, eight bytes a slot"
        );
    }

    /// The two `INIT_*_METHOD_CALL`s, read as
    /// [`new_compiles_to_php_s_op_array`]'s cases were, and `INSTANCEOF`
    /// as an argument sent.
    #[test]
    fn the_method_calls_compile_to_php_s_op_arrays() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $x = E::m();",
                4,
                1,
                2,
                "0000 INIT_STATIC_METHOD_CALL 0 string(\"E\") string(\"m\")\n\
                 0001 V1 = DO_FCALL\n\
                 0002 ASSIGN CV0($x) V1\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php E::__construct();",
                3,
                0,
                1,
                "0000 INIT_STATIC_METHOD_CALL 0 string(\"E\") CONSTRUCTOR\n\
                 0001 DO_FCALL\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; $o->m(1, $o)->n();",
                8,
                1,
                3,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 INIT_METHOD_CALL 2 CV0($o) string(\"m\")\n\
                 0002 SEND_VAL_EX int(1) 1\n\
                 0003 SEND_VAR_EX CV0($o) 2\n\
                 0004 V2 = DO_FCALL\n\
                 0005 INIT_METHOD_CALL 0 V2 string(\"n\")\n\
                 0006 DO_FCALL\n\
                 0007 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; $o->m(f($o));",
                8,
                1,
                3,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 INIT_METHOD_CALL 1 CV0($o) string(\"m\")\n\
                 0002 INIT_FCALL_BY_NAME 1 string(\"f\")\n\
                 0003 SEND_VAR_EX CV0($o) 1\n\
                 0004 V2 = DO_FCALL_BY_NAME\n\
                 0005 SEND_VAR_NO_REF_EX V2 1\n\
                 0006 DO_FCALL\n\
                 0007 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; \\E::m($o instanceof \\E);",
                6,
                1,
                3,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 INIT_STATIC_METHOD_CALL 1 string(\"E\") string(\"m\")\n\
                 0002 T2 = INSTANCEOF CV0($o) string(\"E\")\n\
                 0003 SEND_VAL_EX T2 1\n\
                 0004 DO_FCALL\n\
                 0005 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        let (unit, _strings) = unit_and_strings(b"<?php E::m(); $o->n();");
        assert_eq!(
            unit.main.cache_size, 32,
            "each INIT takes two cache slots, the class and the method, eight bytes a slot"
        );
    }

    /// `INSTANCEOF` as the oracle at 8.5.9 dumps it: into a temporary,
    /// freed where the test is a statement; `false` where the left side
    /// is a constant; a chain grouped to the left, the second over the
    /// first's temporary.
    #[test]
    fn instanceof_compiles_to_php_s_op_array() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $o = 1; $o instanceof E;",
                4,
                1,
                2,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 T2 = INSTANCEOF CV0($o) string(\"E\")\n\
                 0002 FREE T2\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; echo 1 instanceof E, strlen('a') instanceof E;",
                4,
                1,
                1,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 ECHO bool(false)\n\
                 0002 ECHO bool(false)\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; echo $o instanceof E instanceof F;",
                5,
                1,
                3,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 T2 = INSTANCEOF CV0($o) string(\"E\")\n\
                 0002 T3 = INSTANCEOF T2 string(\"F\")\n\
                 0003 ECHO T3\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_delayed_compile_prop` as the oracle at 8.5.9 dumps it under
    /// `opcache.opt_debug_level=0x10000`: `FETCH_OBJ_R` into a temporary
    /// with the name as one constant, the receiver's operand whatever it
    /// is, the temporary freed where the fetch is a statement and sent
    /// by `SEND_VAL` to a callee the table has.
    #[test]
    fn property_fetches_compile_to_php_s_op_arrays() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $x = $o->p;",
                3,
                2,
                2,
                "0000 T2 = FETCH_OBJ_R CV1($o) string(\"p\")\n\
                 0001 ASSIGN CV0($x) T2\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $x = $o->p + $o->q;",
                5,
                2,
                4,
                "0000 T2 = FETCH_OBJ_R CV1($o) string(\"p\")\n\
                 0001 T3 = FETCH_OBJ_R CV1($o) string(\"q\")\n\
                 0002 T4 = ADD T2 T3\n\
                 0003 ASSIGN CV0($x) T4\n\
                 0004 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    2: 0001 - 0002 (tmp/var)\n",
            ),
            (
                b"<?php (new E)->p;",
                5,
                0,
                3,
                "0000 V0 = NEW 0 string(\"E\")\n\
                 0001 DO_FCALL\n\
                 0002 T2 = FETCH_OBJ_R V0 string(\"p\")\n\
                 0003 FREE T2\n\
                 0004 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0002 (new)\n",
            ),
            (
                b"<?php $o->p->q()->r;",
                6,
                1,
                3,
                "0000 T1 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
                 0001 INIT_METHOD_CALL 0 T1 string(\"q\")\n\
                 0002 V2 = DO_FCALL\n\
                 0003 T3 = FETCH_OBJ_R V2 string(\"r\")\n\
                 0004 FREE T3\n\
                 0005 RETURN int(1)\n",
            ),
            (
                b"<?php var_dump($o->p);",
                5,
                1,
                2,
                "0000 INIT_FCALL 1 96 string(\"var_dump\")\n\
                 0001 T1 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
                 0002 SEND_VAL T1 1\n\
                 0003 DO_ICALL\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        let (unit, _strings) = unit_and_strings(b"<?php $o->p;");
        assert_eq!(
            unit.main.cache_size, 24,
            "a property fetch takes three cache slots (zend_alloc_cache_slots(3)), eight bytes a slot"
        );
    }

    /// `FETCH_THIS` as the oracle dumps it: `$this` into a temporary
    /// where it is read and into a `VAR` where it is a receiver or is
    /// sent to a callee nothing is known of (`zend_compile_simple_var`,
    /// `zend_compile_args`).
    #[test]
    fn this_compiles_to_fetch_this_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php echo $this;",
                3,
                0,
                1,
                "0000 T0 = FETCH_THIS\n\
                 0001 ECHO T0\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $this->p;",
                4,
                0,
                2,
                "0000 V0 = FETCH_THIS\n\
                 0001 T1 = FETCH_OBJ_R V0 string(\"p\")\n\
                 0002 FREE T1\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $this->m();",
                4,
                0,
                2,
                "0000 V0 = FETCH_THIS\n\
                 0001 INIT_METHOD_CALL 0 V0 string(\"m\")\n\
                 0002 DO_FCALL\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php f($this);",
                5,
                0,
                2,
                "0000 INIT_FCALL_BY_NAME 1 string(\"f\")\n\
                 0001 V0 = FETCH_THIS\n\
                 0002 SEND_VAR_EX V0 1\n\
                 0003 DO_FCALL_BY_NAME\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_get_compatible_func_or_null`: a public method of a class
    /// the engine declared is a callee the compiler knows, so its
    /// arguments are sent as `SEND_VAL` and `SEND_VAR`; a private one,
    /// a class the table has not and a class the file declares with no
    /// such method are sent for a callee nothing is known of. The
    /// class's name is matched whatever its case, as `CG(class_table)`
    /// is keyed by the lowered name.
    #[test]
    fn a_public_method_of_a_declared_class_is_a_known_callee() {
        use php_unit::decl::{
            ClassDecl, ClassFlags, ClassKind, EnumBacking, MethodDecl, Visibility,
        };
        let method = |name: &[u8], visibility| MethodDecl {
            function: FunctionDecl {
                name: Name(name.into()),
                parameters: Vec::new(),
                return_type: None,
                returns_by_ref: false,
                attributes: Vec::new(),
                doc_comment: None,
                file: Name(Box::new([])),
                line: 0,
                frameless: Vec::new(),
            },
            visibility,
            is_instance: false,
            is_abstract: false,
            is_final: false,
            tentative_return: false,
        };
        let options = Options {
            classes: vec![ClassDecl {
                name: Name(b"PhpToken".as_slice().into()),
                kind: ClassKind::Class,
                flags: ClassFlags::default(),
                parent: None,
                interfaces: Vec::new(),
                traits: Vec::new(),
                backing: EnumBacking::Pure,
                constants: Vec::new(),
                properties: Vec::new(),
                methods: vec![
                    method(b"tokenize", Visibility::Public),
                    method(b"hidden", Visibility::Private),
                ],
                attributes: Vec::new(),
                doc_comment: None,
                file: Name(Box::new([])),
                line: 0,
            }],
            ..Options::default()
        };
        let dump = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile(
                &php_syntax::parse(source, &php_syntax::Options::default()),
                &options,
                &mut strings,
            )
            .unwrap_or_default()
            .dump(&[])
        };
        assert_eq!(
            dump(b"<?php $a = 1; phptoken::TOKENIZE('<?php ', $a);"),
            main_dump(
                6,
                1,
                2,
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 INIT_STATIC_METHOD_CALL 2 string(\"phptoken\") string(\"TOKENIZE\")\n\
                 0002 SEND_VAL string(\"<?php \") 1\n\
                 0003 SEND_VAR CV0($a) 2\n\
                 0004 DO_FCALL\n\
                 0005 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump(b"<?php PhpToken::hidden(1); Nope::m(1); class M {} M::m(1);"),
            main_dump(
                10,
                0,
                3,
                "0000 INIT_STATIC_METHOD_CALL 1 string(\"PhpToken\") string(\"hidden\")\n\
                 0001 SEND_VAL_EX int(1) 1\n\
                 0002 DO_FCALL\n\
                 0003 INIT_STATIC_METHOD_CALL 1 string(\"Nope\") string(\"m\")\n\
                 0004 SEND_VAL_EX int(1) 1\n\
                 0005 DO_FCALL\n\
                 0006 INIT_STATIC_METHOD_CALL 1 string(\"M\") string(\"m\")\n\
                 0007 SEND_VAL_EX int(1) 1\n\
                 0008 DO_FCALL\n\
                 0009 RETURN int(1)\n"
            )
        );
    }

    /// What stands beside these four and is not read yet is refused as
    /// such.
    #[test]
    fn what_stands_beside_new_and_the_calls_is_refused_as_not_yet_read() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile(
                &php_syntax::parse(source, &php_syntax::Options::default()),
                &bundled(),
                &mut strings,
            )
            .err()
            .map(|e| e.message)
        };
        assert_eq!(
            refused(b"<?php new $c;").as_deref(),
            Some("a `new` of what is no class name this compiler cannot read yet")
        );
        assert_eq!(
            refused(b"<?php echo E::A;").as_deref(),
            Some("a class constant or a static property this compiler cannot read yet")
        );
        assert_eq!(
            refused(b"<?php echo $o->$p;").as_deref(),
            Some("a property named at run time this compiler cannot read yet")
        );
        assert_eq!(
            refused(b"<?php f($o->p);").as_deref(),
            Some(
                "a property fetch sent to a callee the table has not this compiler cannot read yet"
            )
        );
        assert_eq!(
            refused(b"<?php echo $o instanceof $c;").as_deref(),
            Some("an `instanceof` of what is no class name this compiler cannot read yet")
        );
        assert_eq!(
            refused(b"<?php new self;").as_deref(),
            Some("a class reference of the scope this compiler cannot read yet")
        );
    }
}
