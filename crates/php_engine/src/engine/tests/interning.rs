//! Interned strings: the known names, what the engine's table holds at
//! build and what a request makes in its own memory.

use super::*;

/// Each known name stands at php's `zend_known_string_id` for it,
/// and the engine's table answers it by position.
#[test]
fn known_names_stand_at_their_ids() {
    let names: [(Known, &[u8]); 15] = [
        (Known::File, b"file"),
        (Known::Line, b"line"),
        (Known::Function, b"function"),
        (Known::Class, b"class"),
        (Known::Object, b"object"),
        (Known::Type, b"type"),
        (Known::ObjectOperator, b"->"),
        (Known::PaamayimNekudotayim, b"::"),
        (Known::Args, b"args"),
        (Known::Eval, b"eval"),
        (Known::Include, b"include"),
        (Known::Require, b"require"),
        (Known::IncludeOnce, b"include_once"),
        (Known::RequireOnce, b"require_once"),
        (Known::Trace, b"trace"),
    ];
    let engine = Engine::builder().build();
    for (known, bytes) in names {
        assert_eq!(known.bytes(), bytes);
        let held = engine.shared.known(known).map(|s| s.bytes().to_vec());
        assert_eq!(held.as_deref(), Some(bytes));
    }
}

/// How many strings the engine has published past the ones its build
/// interned for the constants it declares, which is what says a
/// constant of a unit was interned once.
fn interned(engine: &Engine) -> usize {
    let at_build = strings_of(&Engine::cli_builder().build());
    strings_of(engine).saturating_sub(at_build)
}

/// How many strings the engine's table holds.
fn strings_of(engine: &Engine) -> usize {
    engine.shared.interned.strings().count()
}

/// `Engine::cli_builder().build()` interns a string constant's name and
/// value into its permanent table at build, as php's `MINIT` does,
/// and no request grows the published table for reading one: that
/// table holds the hosts' names `PHP_SAPI` is read from and nothing
/// else, and each value of Core's longer than one byte is the
/// permanent string, `PHP_EOL` being the frozen one-byte string.
#[test]
fn a_string_constant_is_interned_into_the_engine_at_build() {
    let engine = Engine::cli_builder().build();
    let hosts: std::collections::BTreeSet<&[u8]> = crate::constants::HOSTS
        .iter()
        .map(|name| name.as_bytes())
        .collect();
    assert_eq!(
        strings_of(&engine),
        hosts.len(),
        "the hosts' names, each once"
    );
    for decl in &engine.shared.constants {
        assert!(engine.shared.permanent(&decl.name.0).is_some());
    }
    let rows = (engine.shared.constants.iter()).zip(&engine.shared.constant_values);
    for (decl, row) in rows {
        let Some((value, _)) = row else { continue };
        let name = &decl.name.0;
        if let Some(value) = value.zval().as_str()
            && value.len() > 1
        {
            assert!(
                engine
                    .shared
                    .permanent(value.bytes())
                    .is_some_and(|permanent| permanent.is_same(value)),
                "{} is its permanent string",
                String::from_utf8_lossy(name)
            );
        }
    }
    let version = engine.shared.constant(b"PHP_VERSION");
    assert_eq!(
        version.as_ref().and_then(Value::as_str_bytes),
        Some(crate::api::request::PHP_VERSION.as_bytes())
    );
    assert_eq!(
        engine
            .shared
            .constant(b"PHP_EOL")
            .as_ref()
            .and_then(Value::as_str_bytes),
        Some(&b"\n"[..])
    );
    assert_eq!(interned(&engine), 0);
}

/// A unit published into the cache interns its constant strings once,
/// and every request serving it reads the one `zend_string` (design
/// §5.2, §5.14, §6.3): two requests over one file write the same
/// bytes out of one allocation the engine owns, and the second
/// compiles nothing. Beside the constant the table holds the file's
/// name, which the unit's op arrays carry
/// ([`php_compile::intern_names`]).
#[test]
fn two_requests_serving_one_file_share_the_one_constant_string() {
    let engine = Engine::cli_builder().build();
    let files = Files::new("shared", &[("shared.php", "<?php echo 'shared';")]);
    let path = files.path().join("shared.php");
    let name = path.to_string_lossy().as_bytes().to_vec();
    for _ in 0..2 {
        let mut sink = Kept::default();
        let observables = engine.serve_into(
            RequestDescription {
                sapi_name: String::from("cli"),
                script_name: Some(name.clone()),
                ..RequestDescription::default()
            },
            &mut sink,
        );
        assert_eq!(sink.body(), "shared");
        assert_eq!(observables.exit_code, 0);
    }
    assert_eq!(
        interned(&engine),
        2,
        "one string for the one constant and one for the file's name, however many requests read them"
    );
    // What the cache holds after both: one unit, and the constant
    // string its `ECHO` names is the one allocation.
    let cached = || {
        engine
            .shared
            .cached(&name, Path::new(""), StackLimit::UNCHECKED)
            .expect("the unit is cached")
            .unwrap_or_else(|_| panic!("the cached unit compiled"))
    };
    let (one, again) = (cached(), cached());
    assert!(PublishedUnit::ptr_eq(&one, &again), "one published unit");
    let named = |unit: &PublishedUnit| {
        unit.main
            .literals
            .first()
            .copied()
            .and_then(php_heap::Zval::as_str)
    };
    let first = named(&one).expect("a constant string");
    let second = named(&again).expect("a constant string");
    assert_eq!(first.bytes(), b"shared");
    assert!(first.is_interned(), "a published constant is interned");
    assert!(first.is_same(second), "one `zend_string`, read by both");
}

/// What `memory_get_usage()` reads (design §5.13) after a request
/// that built strings: a request that compiles for itself makes its
/// constants in its own memory, and the next request starts where the first
/// one did, because the first request's end took every byte of it
/// (design §5.2). The native that asks is #165; the accountant it
/// asks is this one.
#[test]
fn what_memory_get_usage_reads_starts_where_it_started_for_every_request() {
    let engine = Engine::cli_builder().build();
    let source = b"<?php echo 'a constant string long enough to be allocated';";
    let mut started_at = Vec::new();
    for _ in 0..2 {
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        started_at.push(req.memory().usage(false));
        let before = req.memory().usage(false);
        let compiled = req.compiling(|strings, _, _, _, _| {
            compile(
                source,
                Origin {
                    filename: b"witness.php",
                    cwd: Path::new(""),
                    kind: SourceKind::File,
                    request: ForRequest::none(None),
                },
                CompileIni {
                    precision: 14,
                    short_open_tag: true,
                    assertions: true,
                    shebang: php_compile::Shebang::Read,
                    multibyte: false,
                },
                Declared::default(),
                StackLimit::UNCHECKED,
                strings,
                Laid {
                    arena: &mut php_compile::syntax::Arena::default(),
                    room: php_compile::Room::default(),
                },
            )
        });
        assert!(compiled.is_ok(), "the source compiles");
        assert!(
            req.memory().usage(false) > before,
            "what the request made of its constants is the request's own memory"
        );
        req.release_compiled();
    }
    // The compiler's arena, the object store's table and the VM
    // stack's first page, and nothing else, before a line runs
    // ([`crate::test::STARTED`]).
    assert_eq!(
        started_at,
        vec![crate::test::STARTED, crate::test::STARTED],
        "every request starts where the one before it did"
    );
    assert_eq!(
        interned(&engine),
        0,
        "and the engine's table never heard of it"
    );
}

/// A source the host handed over is the request's alone, as php's
/// `-r` and `eval` are: its constant strings are made in the
/// request's own memory and the engine's table never hears of them.
#[test]
fn what_the_host_handed_over_interns_into_the_request_and_not_the_engine() {
    let engine = Engine::cli_builder().build();
    let (body, _) = served(&engine, "<?php echo 'inline';");
    assert_eq!(body, "inline");
    assert_eq!(
        interned(&engine),
        0,
        "nothing a request compiled for itself outlives it"
    );
}
