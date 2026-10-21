//! Observation of a declared set of opcodes (design §7.1, as #1780
//! ruled), as php's `zend_test` prints it with
//! `zend_test.observer.show_opcode_in_user_handler`: an extension
//! declares from its startup ini which opcodes it observes, is told of
//! each opline of one of them before its handler runs, and the handler
//! runs as it does with nothing declared. The sequences are the
//! oracle's, under the `zend-test` variant, with `zend_test`'s
//! `<!-- init … -->` lines left out, since these extensions declare no
//! call observer.

use php_unit::decl::{IniDecl, Name};

use super::observer::serve_main;
use super::stepped::Stepper;
use super::test_ext::observed_name;
use crate::api::abi::{Beneath, Extension, ExtensionBuilder, Observed, Opcodes, Start, chain};
use crate::engine::{Engine, EngineBuilder};
use crate::observer::{Declared, Untold};
use crate::test::Files;
use crate::tier::{Loop, Nop, execute};
use php_unit::Opcode;

/// An extension that observes the opcodes its own system ini entry,
/// `t.opcodes_N`, names, separated by spaces or commas as `zend_test`
/// parses `observer.show_opcode_in_user_handler`, and writes one line
/// for each opline it is told of.
struct Shows<const N: u8>;

impl<const N: u8> Shows<N> {
    fn ini_name() -> String {
        format!("t.opcodes_{N}")
    }

    fn extension() -> Extension {
        let entry = IniDecl {
            name: Name(Self::ini_name().into_bytes().into()),
            default: Some(b""[..].into()),
            user: false,
            per_dir: false,
            system: true,
        };
        ExtensionBuilder::default()
            .ini(vec![entry])
            .opcodes(Self::declared, Self::told)
            .build(&format!("shows_{N}"))
    }

    fn declared(start: &Start) -> Opcodes {
        let name = Self::ini_name();
        let names = start
            .ini()
            .entries
            .iter()
            .find(|(entry, _)| entry.as_slice() == name.as_bytes())
            .and_then(|(_, value)| value.clone())
            .unwrap_or_default();
        let mut opcodes = Opcodes::default();
        for name in names.split(|&b| b == b' ' || b == b',') {
            if !name.is_empty() {
                opcodes.insert(name);
            }
        }
        opcodes
    }

    /// `<!-- opcode: 'ZEND_ECHO' in user handler -->` for the first
    /// extension, as `zend_test` writes it; the others name themselves
    /// and where the frame stands.
    fn told(observed: &mut Observed<'_>, beneath: Beneath<'_, chain::Opcode>) {
        let opcode = beneath.opcode();
        let line = if N == 0 {
            format!("<!-- opcode: '{opcode}' in user handler -->\n")
        } else {
            let at = observed.frame().map_or(0, |frame| frame.line());
            let name = observed_name(observed);
            format!("<!-- {N}: '{opcode}' {name}:{at} -->\n")
        };
        observed.response_mut().write(line.as_bytes());
        beneath.serve(observed);
    }
}

/// `source` served as `main.php` under `builder` with `extensions`, the
/// startup ini set from `ini`; what it wrote.
/// What `source` wrote, served as `main.php` in a tree named `name`,
/// one of the test's own, by an engine of `builder` with `extensions`
/// and `ini`.
fn served(
    name: &str,
    builder: EngineBuilder,
    extensions: &[Extension],
    ini: &[(&str, &str)],
    source: &str,
) -> String {
    let mut builder = builder.extensions(extensions);
    for (name, value) in ini {
        builder = builder.ini(name, value);
    }
    let engine = builder.build();
    let files = Files::new(&format!("opcodes-{name}"), &[]);
    let dir = files.path();
    let served = serve_main(&engine, dir, source);
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// A call under `@`, an `echo` of two values, and an undefined variable
/// read under `@`: the silence ops `ext/opcache/tests/jit/ignored_opcodes.phpt`
/// observes and the `ECHO` each value is written by.
const SILENCED: &str = "<?php\nfunction test(): int { return 0; }\n$r = @test();\necho $r, \"\\n\";\necho @$undefined, \"done\\n\";\n";

/// php's order: each declared opline told before its handler, so a
/// line comes before the value its `ECHO` writes, and nothing of a
/// `RETURN` or an `ASSIGN`, which are not declared. Each handler does
/// what it does with nothing declared: the program writes what it
/// writes without the extension, the warning `@` silences still
/// silenced.
#[test]
fn a_declared_opcode_is_told_before_its_handler_and_the_handler_runs() {
    let ini = [(
        "t.opcodes_0",
        "ZEND_BEGIN_SILENCE, ZEND_END_SILENCE, ZEND_ECHO",
    )];
    let observed = served(
        "declared",
        Engine::cli_builder(),
        &[Shows::<0>::extension()],
        &ini,
        SILENCED,
    );
    assert_eq!(
        observed,
        "<!-- opcode: 'ZEND_BEGIN_SILENCE' in user handler -->\n\
         <!-- opcode: 'ZEND_END_SILENCE' in user handler -->\n\
         <!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         0<!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         \n\
         <!-- opcode: 'ZEND_BEGIN_SILENCE' in user handler -->\n\
         <!-- opcode: 'ZEND_END_SILENCE' in user handler -->\n\
         <!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         <!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         done\n"
    );
    let plain = served("declared-plain", Engine::cli_builder(), &[], &[], SILENCED);
    assert_eq!(plain, "0\ndone\n");
    let told: String = observed
        .split_inclusive('\n')
        .map(|line| match line.find("<!--") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect();
    assert_eq!(told, plain);
}

/// A function's prologue is php's: the `RECV` of an argument a call
/// passed to a function that declares no type is skipped, so it is
/// never told of; the `RETURN` of each frame is, the main file's last.
#[test]
fn a_skipped_recv_is_never_told_and_each_return_is() {
    let ini = [("t.opcodes_0", "ZEND_ECHO ZEND_RETURN ZEND_RECV")];
    let source = "<?php\nfunction f($a) {\n    echo $a;\n    return $a;\n}\necho f(1);\nf(2);\n";
    assert_eq!(
        served(
            "skipped-recv",
            Engine::cli_builder(),
            &[Shows::<0>::extension()],
            &ini,
            source
        ),
        "<!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         1<!-- opcode: 'ZEND_RETURN' in user handler -->\n\
         <!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         1<!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         2<!-- opcode: 'ZEND_RETURN' in user handler -->\n\
         <!-- opcode: 'ZEND_RETURN' in user handler -->\n"
    );
}

/// The `RECV*` a frame's prologue serves are told of as php's VM
/// dispatches them: every one of a function that declares a type, and
/// past the passed arguments of one that declares none, so a call that
/// passes every argument to such a function is told of none.
#[test]
fn each_recv_a_prologue_serves_is_told() {
    let ini = [("t.opcodes_0", "ZEND_RECV,ZEND_RECV_INIT")];
    let source = "<?php\nfunction t(int $a, $b = 2) { return $a; }\nfunction u($a, $b = 2) { return $a; }\nt(1);\nu(1);\nu(1, 3);\n";
    assert_eq!(
        served(
            "prologue",
            Engine::cli_builder(),
            &[Shows::<0>::extension()],
            &ini,
            source
        ),
        "<!-- opcode: 'ZEND_RECV' in user handler -->\n\
         <!-- opcode: 'ZEND_RECV_INIT' in user handler -->\n\
         <!-- opcode: 'ZEND_RECV_INIT' in user handler -->\n"
    );
}

/// A call that passes more arguments than a function without a type
/// declares skips every declared parameter's `RECV`, as php's
/// `zend_copy_extra_args` moves the opline past them, and still serves
/// the `RECV_VARIADIC` that collects the rest.
#[test]
fn a_call_with_extra_arguments_skips_each_declared_recv() {
    let ini = [("t.opcodes_0", "ZEND_RECV ZEND_RECV_VARIADIC")];
    let source = "<?php\nfunction f($a) { return $a; }\nf(1, 2);\nfunction g($a, ...$r) { return $r; }\ng(1, 2, 3);\ng(1);\n";
    assert_eq!(
        served(
            "extra-arguments",
            Engine::cli_builder(),
            &[Shows::<0>::extension()],
            &ini,
            source
        ),
        "<!-- opcode: 'ZEND_RECV_VARIADIC' in user handler -->\n\
         <!-- opcode: 'ZEND_RECV_VARIADIC' in user handler -->\n"
    );
}

/// Two extensions that declare one opcode are each told, the later
/// first, as php's handler that saved the one it set over calls it after
/// its own; an opcode one of them declares is told to it alone. Each is
/// handed the frame standing at the opline, its function and its line.
#[test]
fn extensions_that_declare_one_opcode_are_each_told_the_later_first() {
    let ini = [
        ("t.opcodes_1", "ZEND_ECHO"),
        ("t.opcodes_2", "ZEND_ECHO,ZEND_RETURN"),
    ];
    let source = "<?php\nfunction f() {\n    echo 'f';\n    return 1;\n}\necho f();\n";
    assert_eq!(
        served(
            "later-first",
            Engine::cli_builder(),
            &[Shows::<1>::extension(), Shows::<2>::extension()],
            &ini,
            source,
        ),
        "<!-- 2: 'ZEND_ECHO' f:3 -->\n\
         <!-- 1: 'ZEND_ECHO' f:3 -->\n\
         f<!-- 2: 'ZEND_RETURN' f:4 -->\n\
         <!-- 2: 'ZEND_ECHO' file 'main.php':6 -->\n\
         <!-- 1: 'ZEND_ECHO' file 'main.php':6 -->\n\
         1<!-- 2: 'ZEND_RETURN' file 'main.php':7 -->\n"
    );
}

/// A stepping host is told as the loop is: [`crate::tier::step`] tells
/// the observers of the opline it dispatches, so a debugger that steps
/// every serving one handler at a time writes what the loop writes. The
/// `ECHO` of an undefined variable is told of before its handler raises
/// the warning, so the line comes before the error handler's own.
#[test]
fn a_stepping_host_is_told_as_the_loop_is() {
    let ini = [("t.opcodes_0", "ZEND_ECHO ZEND_RETURN")];
    let source = "<?php\nfunction f($a) {\n    echo $a;\n    return $a;\n}\necho f(1);\nset_error_handler('f');\necho $u;\n";
    let plain = served(
        "stepping-plain",
        Engine::cli_builder(),
        &[Shows::<0>::extension()],
        &ini,
        source,
    );
    let stepped = served(
        "stepping",
        Engine::cli_builder().tier::<Stepper<false>>(),
        &[Shows::<0>::extension()],
        &ini,
        source,
    );
    assert_eq!(
        plain,
        "<!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         1<!-- opcode: 'ZEND_RETURN' in user handler -->\n\
         <!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         1<!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         <!-- opcode: 'ZEND_ECHO' in user handler -->\n\
         2<!-- opcode: 'ZEND_RETURN' in user handler -->\n\
         <!-- opcode: 'ZEND_RETURN' in user handler -->\n"
    );
    assert_eq!(stepped, plain);
}

/// With nothing declared, by no extension or by one whose ini names no
/// opcode, as `zend_test` sets no handler while its entry is empty, the
/// engine binds the loop with no test for declared opcodes, `Untold`'s
/// instantiation, and one opcode declared binds `Declared`'s (design
/// §5.8, revision 107). The entries are compared as the functions they
/// are: each instantiation is one function of this crate.
#[test]
fn with_none_declared_the_engine_binds_the_loop_that_tells_no_one() {
    let untold: Loop = execute::<Nop, Untold>;
    let telling: Loop = execute::<Nop, Declared>;
    for engine in [
        Engine::cli_builder().build(),
        Engine::cli_builder()
            .extensions(&[Shows::<0>::extension()])
            .build(),
    ] {
        assert!(!engine.shared.opcodes.declares_any());
        assert!(std::ptr::fn_addr_eq(engine.shared.loops.entry, untold));
        assert!(!std::ptr::fn_addr_eq(engine.shared.loops.entry, telling));
    }
    let declared = Engine::cli_builder()
        .extensions(&[Shows::<0>::extension()])
        .ini("t.opcodes_0", "ZEND_ECHO")
        .build();
    assert!(declared.shared.opcodes.declares_any());
    assert!(std::ptr::fn_addr_eq(declared.shared.loops.entry, telling));
}

/// php's `zend_get_opcode_id`: the opcode of the lowest number whose name
/// opens with the name handed, so a name that is a prefix of several is
/// the first of them, and one no opcode's name opens with adds nothing.
#[test]
fn an_opcode_is_named_as_php_finds_it() {
    let mut opcodes = Opcodes::default();
    assert!(opcodes.is_empty());
    assert!(opcodes.insert(b"ZEND_ASSIGN"));
    assert!(opcodes.insert(b"ZEND_BEGIN"));
    assert!(!opcodes.insert(b"ZEND_NOPE"));
    assert!(opcodes.contains(Opcode::Assign));
    assert!(!opcodes.contains(Opcode::AssignDim));
    assert!(opcodes.contains(Opcode::BeginSilence));
    assert!(!opcodes.contains(Opcode::EndSilence));
    // php's search stops short of its last opcode, which no name finds.
    assert!(!opcodes.insert(b"ZEND_DECLARE_ATTRIBUTED_CONST"));
}
