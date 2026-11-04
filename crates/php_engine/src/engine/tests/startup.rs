//! Engine and request startup: the startup ini entries, the bundle's
//! startup handlers and what a refused startup serves.

use super::*;

use crate::api::abi::OnModify;

/// What the pinned oracle answers for
/// `var_dump(ini_get('error_append_string'))` under
/// `php_request_startup` starts the buffer the ini asks for before
/// the program runs: the handler `output_handler` names, which must
/// be a function by then, php's warning where it is not, or else
/// `output_buffering`'s default handler.
#[test]
fn the_request_starts_the_buffer_output_handler_or_output_buffering_names() {
    let program = "<?php function h($s) { return '[' . $s . ']'; } echo 'x'; echo 'y';";
    let serve = |name: &str, value: &str| {
        let engine = Engine::cli_builder().ini(name, value).build();
        let out = engine.serve_capture(program, crate::test::Capture::default());
        out.stdout_text()
    };
    assert_eq!(
        serve("output_handler", "h"),
        "\nWarning: PHP Request Startup: function \"h\" not found or invalid function name in Unknown on line 0\nxy"
    );
    assert_eq!(serve("output_buffering", "4096"), "xy");
    assert_eq!(serve("output_handler", ""), "xy");
}

/// A startup diagnostic is its message's bytes: `extension_dir` set to
/// `a/<0xE4>/w`, which a host's ini text can carry and `-d` does not
/// yet (#1686), is written as php writes it,
/// `php -n -d extension_dir=a/$'\xe4'/w -d extension=x -r ''` with
/// the `dlopen` parentheses the engine cannot make.
#[test]
fn a_startup_diagnostic_writes_its_bytes() {
    let mut builder = Engine::cli_builder();
    builder
        .ini_entries
        .extend_from_slice(b"extension_dir=\"a/\xe4/w\"\nextension=x\n");
    let out = builder
        .build()
        .serve_capture("", crate::test::Capture::default());
    let expected: &[u8] = b"\nWarning: PHP Startup: Unable to load dynamic library 'x' (tried: a/\xe4/w/x (dynamic modules are not supported), a/\xe4/w/x.so (dynamic modules are not supported)) in Unknown on line 0\n";
    assert_eq!(out.stdout, expected);
}

/// `zend_register_ini_entries_ex` sets each entry before it walks to
/// the next, so a handler at startup sees the host's value of an
/// entry declared before its own, and what it writes to the error
/// stream is kept for the host to write before it serves, as
/// opcache's handlers log a refused setting.
#[test]
fn a_startup_handler_sees_the_entries_before_it_and_its_stderr_is_kept() {
    let second: OnModify = |req, value| {
        let mut line = req.ini().get(b"t.first").unwrap_or_default().to_vec();
        line.extend_from_slice(b" then ");
        line.extend_from_slice(value.unwrap_or_default());
        line.push(b'\n');
        req.response_mut().write_stderr(&line);
        Ok(true)
    };
    let first: OnModify = |_, _| Ok(true);
    let entry = |name: &str| php_unit::decl::IniDecl {
        name: Name(name.as_bytes().into()),
        default: Some(b"0"[..].into()),
        user: true,
        per_dir: true,
        system: true,
    };
    let extension = crate::api::abi::ExtensionBuilder::default()
        .ini(vec![entry("t.first"), entry("t.second")])
        .on_modify(b"t.first", first)
        .on_modify(b"t.second", second)
        .build("t");
    let engine = Engine::cli_builder()
        .extensions(&[extension])
        .ini("t.second", "2")
        .ini("t.first", "1")
        .build();
    assert!(engine.startup_errors().ends_with(b"1 then 2\n"));
}

/// Two extensions that intercept one native stack in bundle order,
/// each at its `MINIT`: the later serves
/// first and passes through to the earlier, the earlier to the
/// native, and each hands on the call's own arguments.
#[test]
fn interceptions_of_one_native_stack_in_bundle_order() {
    let earlier: Entry = |req| {
        req.response_mut().write(b"earlier ");
        crate::api::call::passthrough(req, None)
    };
    let later: Entry = |req| {
        req.response_mut().write(b"later ");
        crate::api::call::passthrough(req, None)
    };
    let extensions = [
        crate::api::abi::ExtensionBuilder::default()
            .intercept(b"strlen", earlier)
            .build("earlier"),
        crate::api::abi::ExtensionBuilder::default()
            .intercept(b"strlen", later)
            .build("later"),
    ];
    let served = Engine::cli_builder()
        .extensions(&extensions)
        .build()
        .serve_capture(
            "<?php $f = 'strlen'; echo $f('abcd'), ' ', $f('ab');",
            crate::test::Capture::default(),
        );
    assert_eq!(served.stdout_text(), "later earlier 4 later earlier 2");
}

/// A startup an extension refuses serves nothing, and every request
/// ends with the status it named.
#[test]
fn a_refused_startup_serves_nothing() {
    let refusing = crate::api::abi::ExtensionBuilder::default()
        .post_startup(|req, _| {
            req.response_mut().write_stderr(b"refused\n");
            crate::api::abi::PostStartup::Refuse { status: -2 }
        })
        .build("t");
    let engine = Engine::cli_builder().extensions(&[refusing]).build();
    let first = engine.serve_capture("<?php echo 1;", crate::test::Capture::default());
    let second = engine.serve_capture("<?php echo 2;", crate::test::Capture::default());
    assert_eq!(
        (
            first.stdout.as_slice(),
            first.stderr.as_slice(),
            first.exit_code
        ),
        (&b""[..], &b"refused\n"[..], -2)
    );
    assert_eq!((second.stderr.as_slice(), second.exit_code), (&b""[..], -2));
}

/// What the pinned oracle answers for `ini_get` under `-n` and one
/// `-d` whose value carries a newline, so the text holds a section:
/// after `[PATH=/x]` no entry reaches a directive, after `[foo]` or a
/// bare `[PATH]` every one does. And a constant at startup is Zend's
/// or `main`'s, a double one written at one digit as
/// `zend_double_to_str` writes it before `precision` is registered,
/// while `M_PI`, `ext/standard`'s, is not there yet and stands.
#[test]
fn the_startup_entries_are_read_as_php_init_config_reads_them() {
    let read = |name: &str, value: &str, probe: &[u8]| {
        let engine = Engine::cli_builder().ini(name, value).build();
        engine
            .shared
            .ini
            .master(probe)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    };
    assert_eq!(
        read("x", "1\n[PATH=/x]\nprecision=3", b"precision").as_deref(),
        Some("14")
    );
    assert_eq!(
        read("x", "1\n[foo]\nprecision=3", b"precision").as_deref(),
        Some("3")
    );
    assert_eq!(
        read("x", "1\n[PATH]\nprecision=3", b"precision").as_deref(),
        Some("3")
    );
    assert_eq!(
        read("error_log", "PHP_FLOAT_EPSILON", b"error_log").as_deref(),
        Some("2.0E-16")
    );
    assert_eq!(
        read("docref_root", "PHP_INT_SIZE | 1", b"docref_root").as_deref(),
        Some("9")
    );
    assert_eq!(
        read("precision", "M_PI", b"precision").as_deref(),
        Some("M_PI")
    );
}

/// The `[PATH=]` and `[HOST=]` tables as the pinned oracle keeps them:
/// `php -n -d 'x=1<newline>[PATH=/w]…'` dumps `get_cfg_var('/w')` as
/// `["precision"]=>"5"` and `["arr"]=>[0=>"a","k"=>"b"]`, a `name[]=`
/// nested in the section's table; and php-cgi serving `/w/sub/t.php`
/// for `SERVER_NAME=Example.com` reads `precision` 3 and
/// `serialize_precision` 7, the host's table and then each directory
/// from the root down, the nested array set as nothing.
#[test]
fn a_section_s_table_nests_its_arrays_and_activates_its_strings() {
    let text = "1\n[PATH=/w]\nprecision=5\narr[]=a\narr[k]=b\n\
                [PATH=/w/sub/]\nprecision=3\n[HOST=Example.COM]\nserialize_precision=7";
    let engine = Engine::cli_builder().ini("x", text).build();
    let str = |s: &str| Configured::Str(s.as_bytes().to_vec());
    let name = |s: &str| ConfiguredKey::Name(s.as_bytes().to_vec());
    assert_eq!(
        engine.shared.configured(b"/w"),
        Some(&Configured::Array(vec![
            (name("precision"), str("5")),
            (
                name("arr"),
                Configured::Array(vec![
                    (ConfiguredKey::Index(0), str("a")),
                    (name("k"), str("b")),
                ])
            ),
        ]))
    );
    let row = |n: &str, v: &str| {
        (
            n.as_bytes().to_vec(),
            v.as_bytes().to_vec(),
            IniOrigin::Admin,
        )
    };
    assert_eq!(
        engine.activated_ini(Some(b"Example.com"), b"/w/sub/t.php"),
        vec![
            row("serialize_precision", "7"),
            row("precision", "5"),
            row("precision", "3"),
        ]
    );
    assert_eq!(engine.activated_ini(Some(b"other"), b"sub/t.php"), vec![]);
    let none = Engine::cli_builder().ini("arr[]", "a").build();
    assert_eq!(none.activated_ini(Some(b"arr"), b"/arr/t.php"), vec![]);
}

/// What the pinned oracle answers under `-n -d disable_functions=<list>`
/// for [`disabled_functions`]'s cases: a disabled native is no
/// function to a call, a dynamic call, `function_exists()`,
/// `get_defined_functions()` or a callback's check; the list splits
/// at spaces and commas alone; a name is matched as the table's
/// lowered key; an unknown one is passed over; `exit` and `die` stay,
/// each with php's startup warning;
/// and a user function may take a disabled native's name (Zend's
/// `bug79382.phpt`). A list a host lays over the started engine, as
/// php-fpm's child lays its pool's (`fpm_php_apply_defines_ex`), takes
/// the same natives out and raises nothing a request shows (#2273).
#[test]
fn disable_functions_takes_its_natives_out_of_the_function_table() {
    const PROBE: &str = r#"<?php
            $listed = false;
            foreach (get_defined_functions()['internal'] as $name) { $listed = $listed || $name === 'strlen'; }
            var_dump(function_exists('strlen'), function_exists('defined'), function_exists('exit'), $listed);
            try { strlen('a'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
            try { $f = 'defined'; $f('A'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
            try { set_error_handler('strlen'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
        "#;
    const UNDEFINED_STRLEN: &str = "Call to undefined function strlen()\n";
    const UNDEFINED_DEFINED: &str = "Call to undefined function defined()\n";
    const CALLBACK: &str = "set_error_handler(): Argument #1 ($callback) must be a valid callback or null, function \"strlen\" not found or invalid function name\n";
    let served_under = |list: &str| {
        let engine = Engine::cli_builder().ini("disable_functions", list).build();
        served(&engine, PROBE).0
    };
    let laid = |list: &str| {
        let mut engine = Engine::cli_builder().build();
        let entry = (b"disable_functions".to_vec(), list.as_bytes().to_vec());
        let laid = engine.alter_master_ini(vec![(entry.0, entry.1, IniOrigin::Admin)]);
        assert_eq!(laid, Ok(Vec::new()));
        engine
    };
    let answers = |strlen: bool, defined: bool| {
        let mut expected = format!("bool({strlen})\nbool({defined})\nbool(true)\nbool({strlen})\n");
        if !strlen {
            expected.push_str(UNDEFINED_STRLEN);
        }
        if !defined {
            expected.push_str(UNDEFINED_DEFINED);
        }
        if !strlen {
            expected.push_str(CALLBACK);
        }
        expected
    };
    for (list, strlen, defined) in [
        ("", true, true),
        ("strlen", false, true),
        (" strlen , defined", false, false),
        ("strlen,,defined", false, false),
        ("STRLEN", true, true),
        ("nosuchfn,strlen", false, true),
        ("\tstrlen", true, true),
        ("die,strlen", false, true),
        ("exit", true, true),
    ] {
        // `zend_disable_function`'s startup warning for a name it keeps.
        let mut kept = String::new();
        for name in list.split([' ', ',']) {
            if matches!(name, "exit" | "die" | "clone") {
                kept.push_str("\nWarning: Cannot disable function ");
                kept.push_str(name);
                kept.push_str("() in Unknown on line 0\n");
            }
        }
        let expected = kept + &answers(strlen, defined);
        assert_eq!(served_under(list), expected, "{list:?}");
        let (out, _) = served(&laid(list), PROBE);
        assert_eq!(out, answers(strlen, defined), "laid {list:?}");
    }
    let taken = "<?php function strlen($x) { return 42; } var_dump(strlen('foobar'));";
    let engine = Engine::cli_builder()
        .ini("disable_functions", "strlen")
        .build();
    assert_eq!(served(&engine, taken).0, "int(42)\n");
    assert_eq!(served(&laid("strlen"), taken).0, "int(42)\n");
}

/// `zend_ini_parse_quantity` (`Z/Zend/zend_ini.c:613-841`), which is
/// how `opcache.opt_debug_level=0x10000` is a bit and not a zero: a
/// base prefix is read as `strtoul` with base zero reads it, a
/// leading `0` before a digit is octal, a `K`, `M` or `G` multiplies,
/// and every shape php interprets as zero for backward compatibility
/// is zero.
#[test]
fn a_whole_number_ini_value_is_read_as_php_s_quantity_reads_it() {
    let read = |value: &str| crate::ini_quantity::parse(value.as_bytes()).0;
    assert_eq!(
        read("0x10000"),
        0x1_0000,
        "the dump bit, as the pin writes it"
    );
    assert_eq!(
        read("  0X10 "),
        16,
        "whitespace either side, and either case"
    );
    assert_eq!(read("65536"), 65536, "the same number in decimal");
    assert_eq!(read("0777"), 511, "a leading zero before a digit is octal");
    assert_eq!(read("0o17"), 15, "and the explicit octal prefix");
    assert_eq!(read("0b101"), 5, "and the binary one");
    assert_eq!(read("-1"), -1, "the sign is read before the digits");
    assert_eq!(read("2M"), 2 << 20, "a multiplier over decimal digits");
    assert_eq!(read("0"), 0, "a bare zero is no prefix at all");
    assert_eq!(read(""), 0, "nothing at all");
    assert_eq!(read("off"), 0, "no valid leading digits");
    assert_eq!(read("0z1"), 0, "an unknown prefix");
    assert_eq!(read("0x"), 0, "no digits after the prefix");
    assert_eq!(read("09"), 0, "octal that holds no octal digit");
}

/// Each table `EngineBuilder::build` and `link_classes` lay themselves
/// is one allocation at the length it ends at, as `zend_hash_init` sizes
/// a startup table before it is filled (#3991, #4011, #4013, #4017): none
/// keeps room it never fills. The names and rows a row owns are each
/// their own (#4027).
#[test]
fn the_tables_build_and_link_classes_lay_are_each_one_allocation_at_its_length() {
    let engine = Engine::cli_builder().build();
    let s = &engine.shared;
    let laid = [
        ("extensions", s.extensions.len(), s.extensions.capacity()),
        ("functions", s.functions.len(), s.functions.capacity()),
        ("entries", s.entries.len(), s.entries.capacity()),
        (
            "intercepted_by",
            s.intercepted_by.len(),
            s.intercepted_by.capacity(),
        ),
        (
            "function_modules",
            s.function_modules.len(),
            s.function_modules.capacity(),
        ),
        (
            "function_flags",
            s.function_flags.len(),
            s.function_flags.capacity(),
        ),
        ("frameless", s.frameless.len(), s.frameless.capacity()),
        (
            "frameless_entries",
            s.frameless_entries.len(),
            s.frameless_entries.capacity(),
        ),
        ("classes", s.classes.len(), s.classes.capacity()),
        (
            "class_modules",
            s.class_modules.len(),
            s.class_modules.capacity(),
        ),
        ("handlers", s.handlers.len(), s.handlers.capacity()),
        ("natives", s.natives.len(), s.natives.capacity()),
        ("methods", s.methods.len(), s.methods.capacity()),
        ("trampolines", s.trampolines.len(), s.trampolines.capacity()),
        ("parents", s.parents.len(), s.parents.capacity()),
        ("placed", s.placed.len(), s.placed.capacity()),
        ("constants", s.constants.len(), s.constants.capacity()),
        (
            "constant_modules",
            s.constant_modules.len(),
            s.constant_modules.capacity(),
        ),
        (
            "constant_values",
            s.constant_values.len(),
            s.constant_values.capacity(),
        ),
    ];
    let slack: Vec<_> = (laid.iter()).filter(|(_, len, room)| len != room).collect();
    assert_eq!(
        slack,
        [] as [&(&str, usize, usize); 0],
        "laid past their length"
    );
    // `parameter_classes`, `method_functions` and `trampoline_tables` are
    // two boxes each and `replaced` one, at their length as made (#4027,
    // #4098).
    assert_eq!(laid.len(), 19, "the tables build and link_classes lay");
    assert!(
        laid.iter().all(|(_, len, _)| *len > 0),
        "each holds Core's rows"
    );
}

/// The bytes a hand-laid native's entries spell its names with, held in
/// statics so each has one address to compare against.
static NAMED: &[u8] = b"named_by_its_entry";
static SLOT: &[u8] = b"slot";
static MARK: &[u8] = b"Deprecated";
static CONSTANT: &[u8] = b"NAMED_BY_ITS_ENTRY";

/// A native's function, parameter, attribute and constant names point
/// at the bytes its static entries spell them with and copy none, as
/// php's `zend_register_functions` names a function by its
/// `zend_function_entry`'s `const char *` and opcache re-points every
/// other startup name at one permanent copy (#4027): startup allocates
/// nothing per name.
#[test]
fn a_natives_names_are_its_entries_bytes_and_no_copy() {
    const FUNCTIONS: &[crate::api::__private::NativeFunction] =
        &[crate::api::__private::NativeFunction {
            name: NAMED,
            parameters: &[crate::api::__private::NativeParameter {
                name: SLOT,
                ty: || None,
                by_ref: false,
                prefer_ref: false,
                variadic: false,
                passed: false,
                default: None,
                attributes: &[],
            }],
            return_type: || None,
            frameless: &[],
            attributes: &[crate::api::__private::attribute(MARK, &[])],
            entry: crate::api::__private::abstract_entry,
        }];
    const CONSTANTS: &[crate::api::__private::NativeConst] =
        &[crate::api::__private::NativeConst {
            name: CONSTANT,
            value: || php_unit::decl::ConstExpr::long(1),
            attributes: &[],
        }];
    let extension = crate::api::abi::ExtensionBuilder::default()
        .functions(FUNCTIONS)
        .constants(CONSTANTS)
        .build("named");
    let engine = Engine::cli_builder().extensions(&[extension]).build();
    let s = &engine.shared;
    let function = (s.functions.iter()).find(|decl| *decl.name.0 == *NAMED);
    let function = function.expect("the native is declared");
    assert_eq!(function.name.0.as_ptr(), NAMED.as_ptr());
    assert_eq!(function.parameters[0].name.0.as_ptr(), SLOT.as_ptr());
    assert_eq!(function.attributes[0].name.0.as_ptr(), MARK.as_ptr());
    let constant = (s.constants.iter()).find(|decl| *decl.name.0 == *CONSTANT);
    let constant = constant.expect("the constant is declared");
    assert_eq!(constant.name.0.as_ptr(), CONSTANT.as_ptr());
}

/// The bytes an interception names the native it replaces with, held in
/// a static so it has one address to compare against.
static INTERCEPTED: &[u8] = b"strlen";

/// A core ini entry's name and default are the core table's bytes, as
/// `zend_register_ini_entries_ex` interns a static `zend_ini_entry_def`'s
/// `const char *` that opcache moves into its one buffer, and an
/// interception's name is the bytes its extension spells it with,
/// as phar's `PHAR_G(orig_fopen)` names nothing it allocates: neither
/// the table nor the supplies resolved from the builders copy them
/// (#4098).
#[test]
fn a_core_entry_and_an_interception_are_named_by_the_binarys_bytes() {
    let extension = crate::api::abi::ExtensionBuilder::default()
        .intercept(INTERCEPTED, crate::api::__private::abstract_entry)
        .build("intercepting");
    let engine = Engine::cli_builder().extensions(&[extension]).build();
    let s = &engine.shared;
    for &(name, _) in CORE_INI {
        let row = s
            .ini
            .find(name.as_bytes())
            .expect("a core entry is registered");
        assert_eq!(row.name.as_ptr(), name.as_ptr(), "{name}");
    }
    let precision = s
        .ini
        .find(b"precision")
        .and_then(|row| row.value.as_deref());
    let declared = CORE_INI.iter().find(|(name, _)| *name == "precision");
    let declared = declared
        .and_then(|(_, value)| *value)
        .expect("precision has a default");
    assert_eq!(precision.map(<[u8]>::as_ptr), Some(declared.as_ptr()));
    let intercepted = (s.supplied.intercepts.iter()).find(|(_, name, _)| *name.0 == *INTERCEPTED);
    let intercepted = intercepted.expect("the interception is supplied");
    assert_eq!(intercepted.1.0.as_ptr(), INTERCEPTED.as_ptr());
}
