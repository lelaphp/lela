use super::*;
use php_heap::{Fatal, FrameRef};

use crate::api::abi::OnModify;
use crate::api::request::{Aside, Kept};
use crate::observer::Untold;
use crate::probe::{Asked, Probe};
use crate::tier::Site;

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

fn description(source: &str) -> RequestDescription {
    RequestDescription {
        sapi_name: String::from("cli"),
        script_name: Some(b"witness.php".to_vec()),
        source: Some(source.as_bytes().to_vec()),
        ..RequestDescription::default()
    }
}

/// `sapi_send_headers` under a host whose `header_handler` takes the
/// line: `expose_php`'s `X-Powered-By` laid at request start, and the
/// default `Content-type` after it once the first body byte reaches
/// the block; under the CLI's description the block
/// carries nothing, since `sapi_cli_header_handler` answers zero and
/// the line is freed (#289).
#[test]
fn the_default_content_type_reaches_a_web_host_s_block_and_not_the_cli_s() {
    let engine = Engine::cli_builder().build();
    let mut web = crate::test::Capture::default();
    engine.serve_into(
        RequestDescription {
            web: Some(WebRequest {
                response_code: 200,
                ..WebRequest::default()
            }),
            sapi_name: String::from("cgi-fcgi"),
            ..description("<?php echo 'x';")
        },
        &mut web,
    );
    assert_eq!(String::from_utf8_lossy(&web.stdout), "x");
    assert_eq!(
        web.headers,
        b"X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n"
    );
    let mut cli = crate::test::Capture::default();
    engine.serve_into(description("<?php echo 'x';"), &mut cli);
    assert_eq!(String::from_utf8_lossy(&cli.stdout), "x");
    assert_eq!(cli.headers, b"");
}

/// `__DIR__` where the request's name has no directory is the
/// request's working directory, `VCWD_GETCWD` (#366): two requests
/// in one process described with two directories read two, which is
/// php's under ZTS and what design §3's threads need; the process's
/// own directory is neither.
#[test]
fn dir_is_the_described_request_s_working_directory_not_the_process_s() {
    let engine = Engine::cli_builder().build();
    let dir_under = |cwd: &str| {
        let mut sink = Kept::default();
        engine.serve_into(
            RequestDescription {
                cwd: PathBuf::from(cwd),
                ..description("<?php echo __DIR__;")
            },
            &mut sink,
        );
        sink.body()
    };
    assert_eq!(dir_under("/srv/one"), "/srv/one");
    assert_eq!(dir_under("/srv/two"), "/srv/two");
}

/// `error_log=syslog` with no `request::Syslog` registered, as
/// without ext/standard: the entry reaches the host's log (#1364).
#[test]
fn error_log_syslog_without_a_registered_syslog_reaches_the_host_s_log() {
    let engine = Engine::builder().ini("error_log", "syslog").build();
    let mut sink = Kept::default();
    let source = "<?php ini_set('log_errors', 1); ini_set('display_errors', 0); echo $u;";
    engine.serve_into(description(source), &mut sink);
    assert_eq!(
        sink.logged(),
        "PHP Warning:  Undefined variable $u in witness.php on line 1\n"
    );
}

fn served(engine: &Engine, source: &str) -> (String, Observables) {
    let mut sink = Kept::default();
    let observables = engine.serve_into(description(source), &mut sink);
    (sink.body(), observables)
}

/// A unit compiled from source the host handed over is the request's
/// alone, and so is every `eval`'s: their op arrays take runtime
/// caches of the request's own and no map-pointer offset of the
/// engine's, so `CG(map_ptr_last)` moves only for what the engine
/// publishes, as php's moves only for what opcache persists, and a
/// request that evaluates in a loop grows it by nothing (#528).
#[test]
fn a_unit_compiled_for_one_request_moves_the_engine_s_map_pointer_count_by_nothing() {
    let engine = Engine::cli_builder().build();
    let count = || engine.shared.map_ptr_last.load(Ordering::Relaxed);
    let before = count();
    let (body, _) = served(
        &engine,
        "<?php function f($s) { return strlen($s); } echo f('ab'), f('abc');\n\
         for ($i = 0; $i < 3; $i++) { echo eval('return $i;'); }",
    );
    assert_eq!(body, "23012");
    assert_eq!(count(), before, "the engine's count is where it was");
}

/// A file the unit cache compiles inside `opcache.file_update_protection`
/// and holds nowhere is the request's alone, as php keeps a file it
/// does not persist in process memory: requests that include it move
/// the engine's map-pointer count by nothing, where each moved it by
/// its op arrays (GH-8646, #2624). The same file held moves it once,
/// at the request that held it.
#[test]
fn a_file_the_cache_holds_nowhere_moves_the_engine_s_map_pointer_count_by_nothing() {
    use crate::api::abi::{Admission, ExtensionBuilder, Lookup};
    let files = Files::new(
        "map-ptr",
        &[(
            "declares.inc",
            "<?php class GhMapPtr {} function gh_f() {} return 1;",
        )],
    );
    let file = files.path().join("declares.inc");
    let program = format!("<?php echo include '{}';", file.display());
    let cache = |admission: fn(&mut Request, &[u8]) -> Admission| {
        let extension = ExtensionBuilder::default().admission(admission).build("t");
        Engine::cli_builder().extensions(&[extension]).build()
    };
    let count = |engine: &Engine| engine.shared.map_ptr_last.load(Ordering::Relaxed);
    let protected = cache(|_, _| Admission::Held {
        validation: Validation::Never,
        lookup: Lookup::Resolved,
        protection: 3600,
        file_cache: None,
    });
    let before = count(&protected);
    let bodies: Vec<String> = (0..3).map(|_| served(&protected, &program).0).collect();
    assert_eq!(bodies, ["1", "1", "1"]);
    assert_eq!(count(&protected), before, "nothing held, nothing counted");
    let holding = cache(|_, _| Admission::Held {
        validation: Validation::Never,
        lookup: Lookup::Resolved,
        protection: 0,
        file_cache: None,
    });
    let before = count(&holding);
    assert_eq!(served(&holding, &program).0, "1");
    let after_first = count(&holding);
    assert_eq!(served(&holding, &program).0, "1");
    assert!(after_first > before, "the held file took its offsets");
    assert_eq!(count(&holding), after_first, "and took them once");
}

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

/// `sapi_module.treat_data` replaced, as mbstring replaces it: the
/// query string is handed to the extension's treater, whose variables
/// are registered; a source it leaves, the cookie header here, is
/// `php_default_treat_data`'s.
#[test]
fn a_treat_data_hook_reads_the_sources_it_takes_and_leaves_the_rest() {
    use crate::api::request::Input;
    let upper: crate::api::abi::TreatDataHook = |_, input, source| {
        Ok((input == Input::Get).then(|| vec![(b"q".to_vec(), source.to_ascii_uppercase())]))
    };
    let extension = crate::api::abi::ExtensionBuilder::default()
        .treat_data(upper)
        .build("t");
    let engine = Engine::cli_builder().extensions(&[extension]).build();
    let mut sink = crate::test::Capture::default();
    engine.serve_into(
        RequestDescription {
            web: Some(WebRequest {
                query: Some(b"a=1&b=2".to_vec()),
                cookies: Some(b"c=3".to_vec()),
                ..WebRequest::default()
            }),
            sapi_name: String::from("cgi-fcgi"),
            ..description(
                "<?php echo $_GET['q'], ' ', isset($_GET['a']) ? 'a' : '-', ' ', $_COOKIE['c'];",
            )
        },
        &mut sink,
    );
    assert_eq!(String::from_utf8_lossy(&sink.stdout), "A=1&B=2 - 3");
}

/// `-d error_append_string=<value>`: `Off`, `OFF`, `oFf`, `FALSE`,
/// `none`, `None`, `NULL` and `off ` are `string(0) ""`; `On`, `YES`
/// and `true` are `string(1) "1"`; `0`, `2`, `abc`, `stderr`, `1M`,
/// `of` and ` off` stand as written.
#[test]
fn a_host_s_ini_value_is_read_as_the_scanner_reads_a_bare_word() {
    let read = |value: &str| {
        let engine = Engine::cli_builder()
            .ini("error_append_string", value)
            .build();
        engine
            .shared
            .ini_default(b"error_append_string")
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    };
    for off in ["Off", "OFF", "oFf", "FALSE", "none", "None", "NULL", "off "] {
        assert_eq!(read(off).as_deref(), Some(""), "{off:?}");
    }
    for on in ["On", "YES", "true"] {
        assert_eq!(read(on).as_deref(), Some("1"), "{on:?}");
    }
    for written in ["0", "2", "abc", "stderr", "1M", "of", " off", ""] {
        assert_eq!(read(written).as_deref(), Some(written), "{written:?}");
    }
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

/// An extension that names a file to preload as the engine
/// finishes starting up, reading it from an ini entry of its own,
/// and holds every file in the cache, never validated, as opcache
/// holds one with `opcache.validate_timestamps=0`.
fn preloading(script: &Path) -> Extension {
    let entry = php_unit::decl::IniDecl {
        name: Name(b"t.preload"[..].into()),
        default: Some(b""[..].into()),
        user: false,
        per_dir: false,
        system: true,
    };
    crate::api::abi::ExtensionBuilder::default()
        .ini(vec![entry])
        .admission(|_, _| crate::api::abi::Admission::Held {
            validation: Validation::Never,
            lookup: crate::api::abi::Lookup::Resolved,
            protection: 0,
            file_cache: None,
        })
        .post_startup(|req, _| {
            let script = req.ini().get(b"t.preload").unwrap_or_default().to_vec();
            if script.is_empty() {
                crate::api::abi::PostStartup::Serve
            } else {
                crate::api::abi::PostStartup::Preload {
                    script,
                    thread: None,
                }
            }
        })
        .build(&script.to_string_lossy())
}

/// `opcache.preload` as the unit cache answers it (design §6.3): the
/// file is served once, before the first request, which writes after
/// it; what it declared stands in every request, a class it left
/// unlinked is php's warning, and the file included again declares
/// nothing the preload holds.
#[test]
fn a_preloaded_script_runs_once_and_its_declarations_stand_in_every_request() {
    let files = Files::new(
        "preload",
        &[(
            "preload.inc",
            "<?php\necho \"preloading\\n\";\nfunction pre_f() { return 'f'; }\n\
             class PreA { function m() { return 'm'; } }\nclass PreB extends PreA {}\n\
             interface PreI {}\nclass PreC implements PreI {}\nreturn;\n\
             class Late extends Missing {}\n",
        )],
    );
    let script = files.path().join("preload.inc");
    let engine = Engine::cli_builder()
        .extensions(&[preloading(&script)])
        .ini("t.preload", script.as_os_str().as_encoded_bytes())
        .build();
    let program = format!(
        "<?php echo pre_f(), (new PreB)->m(), (new PreC) instanceof PreI ? 'y' : 'n', \
         class_exists('Late', false) ? 'y' : 'n', \"\\n\"; include '{}'; echo \"again\\n\";",
        script.display()
    );
    let first = engine.serve_capture(&program, crate::test::Capture::default());
    let second = engine.serve_capture(&program, crate::test::Capture::default());
    let resolved = script.canonicalize().expect("the preload file resolves");
    assert_eq!(
        first.stdout_text(),
        format!(
            "preloading\n\nWarning: Can't preload unlinked class Late: Unknown parent Missing in {} on line 9\nfmyn\npreloading\nagain\n",
            resolved.display()
        )
    );
    assert_eq!(second.stdout_text(), "fmyn\npreloading\nagain\n");
}

/// An opcache restart past the preload (design §5.14): the next
/// request is handed a new epoch, and the preload's classes and
/// functions stand in it at the numbers they had, the very rows.
#[test]
fn a_restart_keeps_the_preloads_rows_at_their_numbers() {
    let files = Files::new(
        "preload-restart",
        &[(
            "preload.inc",
            "<?php\nfunction pre_g() { return 'g'; }\nclass PreD { function m() { return 'd'; } }\n",
        )],
    );
    let script = files.path().join("preload.inc");
    let engine = Engine::cli_builder()
        .extensions(&[preloading(&script)])
        .ini("t.preload", script.as_os_str().as_encoded_bytes())
        .build();
    let program = "<?php echo pre_g(), (new PreD)->m();";
    let first = engine.serve_capture(program, crate::test::Capture::default());
    let before = engine.shared.arena.current();
    let scheduled = before.files.schedule_restart();
    let second = engine.serve_capture(program, crate::test::Capture::default());
    let after = engine.shared.arena.current();
    assert_eq!(scheduled, crate::api::request::Restart::Scheduled);
    assert_eq!(first.stdout_text(), "gd");
    assert_eq!(second.stdout_text(), "gd");
    assert!(!Arc::ptr_eq(&before, &after), "a new epoch");
    let preloaded = engine.shared.units.preloaded();
    let functions =
        preloaded.map_or_else(Vec::new, |preloaded| preloaded.functions.rows().to_vec());
    assert!(!functions.is_empty());
    for function in functions {
        let (was, is) = (before.function(function), after.function(function));
        assert!(was.zip(is).is_some_and(|(was, is)| std::ptr::eq(was, is)));
    }
}

/// An extension that keeps every file in the cache, never validated,
/// and names a file cache from an ini entry of its own.
fn file_caching(dir: &Path) -> Extension {
    let entry = php_unit::decl::IniDecl {
        name: Name(b"t.file_cache"[..].into()),
        default: Some(b""[..].into()),
        user: false,
        per_dir: false,
        system: true,
    };
    crate::api::abi::ExtensionBuilder::default()
        .ini(vec![entry])
        .admission(|req, _| {
            let dir = req.ini().get(b"t.file_cache").unwrap_or_default();
            crate::api::abi::Admission::Held {
                validation: Validation::Never,
                lookup: crate::api::abi::Lookup::Resolved,
                protection: 0,
                file_cache: Some(crate::api::abi::FileCache {
                    directory: std::path::PathBuf::from(String::from_utf8_lossy(dir).into_owned()),
                    read_only: false,
                }),
            }
        })
        .build(&dir.to_string_lossy())
}

/// `opcache.file_cache` as the unit cache's second level (design
/// §6.3): a file one engine compiled is kept on disk, and another
/// engine of the same build reads it back instead of compiling the
/// file, so what it serves is what was kept though the file has
/// changed since; a kept file another build wrote is never read.
#[test]
fn a_kept_unit_is_read_back_by_the_next_engine_and_another_builds_is_not() {
    let files = Files::new("file-cache", &[("kept.inc", "<?php return 'kept';")]);
    let dir = files.path();
    let cache = dir.join("cache");
    std::fs::create_dir(&cache).expect("the tree takes the cache's directory");
    let script = dir
        .join("kept.inc")
        .canonicalize()
        .expect("the kept file resolves");
    let engine = || {
        Engine::cli_builder()
            .extensions(&[file_caching(dir)])
            .ini("t.file_cache", cache.as_os_str().as_encoded_bytes())
            .build()
    };
    let program = format!("<?php echo include '{}';", script.display());
    let first = engine().serve_capture(&program, crate::test::Capture::default());
    files.write("kept.inc", "<?php return 'compiled';");
    let second = engine().serve_capture(&program, crate::test::Capture::default());
    let kept = engine();
    let kept_at = crate::unit_cache::Disk {
        directory: &cache,
        read_only: false,
        build: &kept.shared.units.build,
    }
    .path(script.as_os_str().as_encoded_bytes());
    let mut stale = std::fs::read(&kept_at).expect("the kept file is read");
    stale[8] ^= 1;
    std::fs::write(&kept_at, &stale).expect("the kept file is written over");
    let third = kept.serve_capture(&program, crate::test::Capture::default());
    assert_eq!(first.stdout_text(), "kept");
    assert_eq!(second.stdout_text(), "kept");
    assert_eq!(third.stdout_text(), "compiled");
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
            .ini_default(probe)
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

/// `fatal_error_backtraces` is `On` in php's table (`Z/Zend/zend.c:263`)
/// and the CLI leaves it, so `php -n` over
/// `<?php\nvar_dump(ini_get('fatal_error_backtraces'));` prints
/// `string(1) "1"`; the recipe's `Off` is `run-tests.php`'s, which is
/// why the witness beside `Request::fatal_error_backtraces` reads `""`.
#[test]
fn fatal_error_backtraces_is_on_by_default() {
    let engine = Engine::cli_builder().build();
    let (body, _) = served(
        &engine,
        "<?php\nvar_dump(ini_get('fatal_error_backtraces'));",
    );
    assert_eq!(body, "string(1) \"1\"\n");
}

/// The compiler's `E_COMPILE_ERROR` goes through `zend_error_impl`, so
/// under php's default it fetches its trace, `#0 {main}` with no
/// frame standing, and the parser's `E_PARSE` is a `ParseError`
/// `zend_exception_error` renders with none. What the pinned oracle
/// writes under `php -n` for `<?php\n(unset)$x;` is the fatal, then
/// `Stack trace:` and `#0 {main}`, in the plain form and, under
/// `-d html_errors=1`, in the second html format, the trace between
/// `<br />` and the newline; and for `<?php\necho 1 +;` the parse
/// error alone, under both.
#[test]
fn a_compile_error_carries_php_s_trace_under_the_default_ini() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(&engine, "<?php\n(unset)$x;");
    assert_eq!(
        body,
        "\nFatal error: The (unset) cast is no longer supported in witness.php on line 2\nStack trace:\n#0 {main}\n"
    );
    assert_eq!(
        observables.fatal.as_deref(),
        Some(
            "PHP Fatal error:  The (unset) cast is no longer supported in witness.php on line 2\nStack trace:\n#0 {main}"
        )
    );
    let (body, observables) = served(&engine, "<?php\necho 1 +;");
    assert_eq!(
        (body.as_str(), observables.exit_code),
        (
            "\nParse error: syntax error, unexpected token \";\" in witness.php on line 2\n",
            255
        )
    );

    let html = Engine::cli_builder().ini("html_errors", "1").build();
    let (body, _) = served(&html, "<?php\n(unset)$x;");
    assert_eq!(
        body,
        "<br />\n<b>Fatal error</b>:  The (unset) cast is no longer supported in <b>witness.php</b> on line <b>2</b><br />\nStack trace:\n#0 {main}\n"
    );
    let (body, _) = served(&html, "<?php\necho 1 +;");
    assert_eq!(
        body,
        "<br />\n<b>Parse error</b>:  syntax error, unexpected token &quot;;&quot; in <b>witness.php</b> on line <b>2</b><br />\n"
    );
}

/// A class the compiler bound with no opline (`zend_compile_class_decl`
/// under `toplevel`) is declared into the request's table before the
/// main op array runs, so the source the doc-comment witness of
/// `php_syntax` declares runs (#257). A name the table already holds
/// is no early bind: its `DECLARE_CLASS` raises
/// `zend_class_redeclaration_error` where it stands, after what ran
/// before it, in the wording for an internal class, or naming the
/// earlier declaration's site for one of the file's. Both are
/// `E_COMPILE_ERROR` at the class's line, over `#0 {main}` under
/// php's default `fatal_error_backtraces`, which is what the pinned
/// oracle writes for `<?php\necho 1;\nclass Exception {}` under
/// `php -n`.
#[test]
fn a_unit_s_classes_are_declared_before_its_main_op_array_runs() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(
        &engine,
        "<?php\nclass M { /** P */ public $x, $y; /** K */ const A = 1, B = 2; }\necho 1;",
    );
    assert_eq!((body.as_str(), observables.exit_code), ("1", 0));
    let (body, observables) = served(&engine, "<?php\necho 1;\nclass Exception {}");
    assert_eq!(
        (body.as_str(), observables.exit_code),
        (
            "1\nFatal error: Cannot redeclare class Exception in witness.php on line 3\nStack trace:\n#0 {main}\n",
            255
        )
    );
    let (body, observables) = served(&engine, "<?php\necho 1;\nclass M {}\nclass m {}");
    assert_eq!(
        (body.as_str(), observables.exit_code),
        (
            "1\nFatal error: Cannot redeclare class M (previously declared in witness.php:3) in witness.php on line 4\nStack trace:\n#0 {main}\n",
            255
        ),
        "php emits a DECLARE_CLASS for the second and fails it at run time"
    );
}

#[test]
fn an_engine_with_no_extensions_serves_what_uses_zend_alone() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(&engine, "<?php echo 1;");
    assert_eq!(body, "1");
    assert_eq!(observables.exit_code, 0);
    assert_eq!(observables.fatal, None);
}

/// A construct php accepts and this compiler cannot read yet is
/// refused in the compiler's own sentence, which says which construct
/// it could not read; a construct php refuses is php's `syntax
/// error`, and the witness beside `serve` holds that to the oracle.
/// The frame around either is php's.
#[test]
fn a_source_the_compiler_refuses_names_the_construct_it_could_not_read() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(&engine, "<?php\ndeclare(ticks=0): enddeclare;");
    assert_eq!(
        body,
        "\nParse error: a `declare` in the alternative syntax this compiler cannot read yet in witness.php on line 2\n"
    );
    assert_eq!(observables.exit_code, 255);
    assert_eq!(
        observables.fatal, None,
        "a parse error is not the fatal `serve` renders a second time"
    );
}

/// A parse error is `error_reporting`'s and `log_errors`' to render,
/// as every other diagnostic is; the status is php's either way.
#[test]
fn a_parse_error_is_logged_and_masked_the_way_php_masks_one() {
    let mut logged = Kept::default();
    let observables = Engine::cli_builder()
        .ini("display_errors", "0")
        .ini("log_errors", "1")
        .build()
        .serve_into(
            description("<?php\ndeclare(ticks=0): enddeclare;"),
            &mut logged,
        );
    assert!(logged.body().is_empty());
    assert_eq!(
        logged.logged(),
        "PHP Parse error:  a `declare` in the alternative syntax this compiler cannot read yet in witness.php on line 2\n"
    );
    assert_eq!(observables.exit_code, 255);

    let mut masked = Kept::default();
    let observables = Engine::cli_builder()
        .ini("error_reporting", "0")
        .build()
        .serve_into(
            description("<?php\ndeclare(ticks=0): enddeclare;"),
            &mut masked,
        );
    assert!(masked.body().is_empty(), "E_PARSE is masked out");
    assert_eq!(observables.exit_code, 255, "the status is php's regardless");
}

/// What php's compiler refuses after parsing is `E_COMPILE_ERROR`,
/// rendered as a fatal and ending the request as one, with the log
/// form beside it: the compiler's stack limit (design §5.6), which
/// `zend.max_allowed_stack_size` bounds from where the request
/// started, on the thread the request runs on. A chain of 200,000
/// links is a tree that deep, and the frame that trips the limit
/// is refused in php's sentence, with the number php prints, and
/// nothing of the tree left to a native overflow, on the way down
/// or on the way out. The witness beside `php_compile::StackLimit`
/// holds the sentence to the oracle; what is held here is the
/// frame the engine puts around it, on a thread of the test
/// harness's own size.
#[test]
fn what_the_compiler_refuses_past_its_stack_limit_is_a_fatal() {
    let source = format!("<?php\n$y = 1{};\necho 'never';", "+1".repeat(200_000));
    let engine = Engine::cli_builder()
        .ini("zend.max_allowed_stack_size", "512K")
        .build();
    let (body, observables) = served(&engine, &source);
    // 512K less the least `zend.reserved_stack_size`: 491520 is the
    // oracle's number under `php -n` on macOS, where php reserves
    // 32K, and 475136 is php's where `ZEND_ALLOCA_MAX_SIZE` is
    // compiled and it reserves 48K (`php_compile::stack::RESERVED_MIN`).
    let number = if cfg!(target_vendor = "apple") {
        491_520
    } else {
        475_136
    };
    let sentence = format!(
        "Maximum call stack size of {number} bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached during compilation. Try splitting expression in witness.php on line 2\nStack trace:\n#0 {{main}}"
    );
    assert_eq!(body, format!("\nFatal error: {sentence}\n"));
    assert_eq!(observables.exit_code, 255);
    assert_eq!(
        observables.fatal.as_deref(),
        Some(format!("PHP Fatal error:  {sentence}").as_str()),
        "the log form the host is handed, as for every fatal, the trace with it"
    );

    let mut masked = Kept::default();
    let observables = Engine::cli_builder()
        .ini("zend.max_allowed_stack_size", "512K")
        .ini("error_reporting", "0")
        .build()
        .serve_into(description(&source), &mut masked);
    assert!(masked.body().is_empty(), "E_COMPILE_ERROR is masked out");
    assert_eq!(observables.exit_code, 255, "the status is php's regardless");

    let (body, observables) = served(
        &Engine::cli_builder()
            .ini("zend.max_allowed_stack_size", "-1")
            .build(),
        "<?php echo 1;",
    );
    assert_eq!(
        (body.as_str(), observables.exit_code),
        ("1", 0),
        "-1 checks nothing"
    );
}

/// `php_error_cb`'s two renderings of one diagnostic, which the two
/// ini entries decide, each naming the line the opline stands on.
/// The oracle writes the same bytes for `<?php echo NAN;` at
/// `display_errors=1, log_errors=0` and at the two swapped.
#[test]
fn a_diagnostic_is_displayed_or_logged_as_the_ini_entries_say() {
    let mut displayed = Kept::default();
    declaring_nan(Engine::cli_builder())
        .build()
        .serve_into(description("<?php echo NAN;"), &mut displayed);
    assert_eq!(
        displayed.body(),
        "\nWarning: unexpected NAN value was coerced to string in witness.php on line 1\nNAN"
    );
    assert!(
        displayed.log().is_empty(),
        "log_errors is off, so nothing is logged"
    );

    let mut logged = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("display_errors", "0")
        .ini("log_errors", "1")
        .build()
        .serve_into(description("<?php echo NAN;"), &mut logged);
    assert_eq!(logged.body(), "NAN");
    assert_eq!(
        logged.logged(),
        "PHP Warning:  unexpected NAN value was coerced to string in witness.php on line 1\n"
    );
}

/// `builder` with `INF` and `NAN` among its persistent constants, as
/// ext/standard declares them, which the bundle of these tests has
/// not: what the diagnostics tests below echo to raise php's warning.
fn declaring_nan(builder: EngineBuilder) -> EngineBuilder {
    let constant = |name: &str, value: f64| php_unit::decl::ConstDecl {
        name: Name::from(name),
        value: php_unit::decl::ConstExpr::Double(value),
        attributes: Vec::new(),
    };
    builder.additional_extension(
        crate::api::abi::ExtensionBuilder::default()
            .constants(vec![
                constant("INF", f64::INFINITY),
                constant("NAN", f64::NAN),
            ])
            .build("standard"),
    )
}

/// The warning php writes for `echo NAN;` on line 2 of the witness
/// regime's file, which the display-branch tests below stand on.
const NAN_WARNING: &str = "unexpected NAN value was coerced to string";

/// `error_prepend_string` and `error_append_string` stand around
/// php's plain display form and around nothing else: the log form
/// carries neither. What the pinned oracle writes for
/// `-d error_prepend_string='<PRE>' -d error_append_string='</PRE>'`
/// over `<?php\necho NAN;` is
/// `<PRE>\nWarning: … on line 2\n</PRE>NAN`.
#[test]
fn the_two_error_strings_stand_around_the_display_form() {
    let mut around = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("error_prepend_string", "<PRE>")
        .ini("error_append_string", "</PRE>")
        .ini("log_errors", "1")
        .build()
        .serve_into(description("<?php\necho NAN;"), &mut around);
    assert_eq!(
        around.body(),
        format!("<PRE>\nWarning: {NAN_WARNING} in witness.php on line 2\n</PRE>NAN")
    );
    assert_eq!(
        around.logged(),
        format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
        "php's log form is the one rendering neither string reaches"
    );
}

/// `html_errors` is php's other display form, and the two strings
/// stand around it as they stand around the plain one. What the
/// pinned oracle writes for `-d html_errors=1` over
/// `<?php\necho NAN;` is
/// `<br />\n<b>Warning</b>:  … in <b>%s</b> on line <b>2</b><br />\n`.
#[test]
fn html_errors_is_phps_other_display_form() {
    let mut html = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("html_errors", "1")
        .build()
        .serve_into(description("<?php\necho NAN;"), &mut html);
    assert_eq!(
        html.body(),
        format!(
            "<br />\n<b>Warning</b>:  {NAN_WARNING} in <b>witness.php</b> on line <b>2</b><br />\nNAN"
        )
    );

    let mut around = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("html_errors", "1")
        .ini("error_prepend_string", "<PRE>")
        .ini("error_append_string", "</PRE>")
        .build()
        .serve_into(description("<?php\necho NAN;"), &mut around);
    assert_eq!(
        around.body(),
        format!(
            "<PRE><br />\n<b>Warning</b>:  {NAN_WARNING} in <b>witness.php</b> on line <b>2</b><br />\n</PRE>NAN"
        )
    );
}

/// `ignore_repeated_errors` drops a repeat of the last diagnostic,
/// and `ignore_repeated_source` says whether a repeat at another
/// line is one. What the pinned oracle writes for
/// `<?php\necho NAN, NAN;\necho NAN;` is three warnings with both
/// entries off, two with `ignore_repeated_errors=1` — the second on
/// line 2 dropped, the one on line 3 kept — and one with
/// `ignore_repeated_source=1` beside it.
#[test]
fn a_repeat_is_dropped_the_way_php_drops_one() {
    let source = "<?php\necho NAN, NAN;\necho NAN;";
    let warning = |line: u32| format!("\nWarning: {NAN_WARNING} in witness.php on line {line}\n");

    let mut every = Kept::default();
    declaring_nan(Engine::cli_builder())
        .build()
        .serve_into(description(source), &mut every);
    assert_eq!(
        every.body(),
        format!("{}NAN{}NAN{}NAN", warning(2), warning(2), warning(3))
    );

    let mut once_a_site = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("ignore_repeated_errors", "1")
        .build()
        .serve_into(description(source), &mut once_a_site);
    assert_eq!(
        once_a_site.body(),
        format!("{}NANNAN{}NAN", warning(2), warning(3)),
        "the repeat on line 2 is dropped and the one on line 3 stands"
    );

    let mut once = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("ignore_repeated_errors", "1")
        .ini("ignore_repeated_source", "1")
        .build()
        .serve_into(description(source), &mut once);
    assert_eq!(
        once.body(),
        format!("{}NANNANNAN", warning(2)),
        "the site no longer parts them, so one warning stands for all three"
    );
}

/// `xmlrpc_errors` is php's first display form, ahead of both the
/// plain and the html one, and the two strings stand around neither
/// it nor the log form. What the pinned oracle writes for
/// `-d xmlrpc_errors=1 -d xmlrpc_error_number=42
/// -d error_prepend_string='<PRE>'` over `<?php\necho NAN;` is one
/// `methodResponse` fault, the code `42`, the message after
/// `Warning:` with no space, and `NAN` straight after it.
#[test]
fn xmlrpc_errors_is_phps_first_display_form() {
    let mut fault = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("xmlrpc_errors", "1")
        .ini("xmlrpc_error_number", "42")
        .ini("error_prepend_string", "<PRE>")
        .ini("log_errors", "1")
        .build()
        .serve_into(description("<?php\necho NAN;"), &mut fault);
    assert_eq!(
        fault.body(),
        format!(
            "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>42</int></value></member><member><name>faultString</name><value><string>Warning:{NAN_WARNING} in witness.php on line 2</string></value></member></struct></value></fault></methodResponse>NAN"
        )
    );
    assert_eq!(
        fault.logged(),
        format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
        "the log form is the same whichever display form is on"
    );
}

/// `display_errors=stderr` is the error stream under the hosts php
/// names at `Z/main/main.c:1484`, `cli`, `cgi` and `phpdbg`, and
/// the plain form on the body under every other. What the pinned
/// oracle writes for `-d display_errors=stderr` over `<?php\necho
/// NAN;` is `NAN` on stdout and `Warning: … on line 2` on stderr,
/// with no leading newline; under `fpm-fcgi` php's test on
/// `sapi_module.name` fails and the `else` is the plain form.
#[test]
fn stderr_is_the_error_stream_only_under_the_hosts_php_names() {
    let engine = declaring_nan(Engine::cli_builder())
        .ini("display_errors", "stderr")
        .build();
    let under = |host: &str| {
        let mut kept = Kept::default();
        engine.serve_into(
            RequestDescription {
                sapi_name: String::from(host),
                ..description("<?php\necho NAN;")
            },
            &mut kept,
        );
        (kept.body(), kept.stderr())
    };
    for host in ["cli", "cgi", "phpdbg"] {
        assert_eq!(
            under(host),
            (
                String::from("NAN"),
                format!("Warning: {NAN_WARNING} in witness.php on line 2\n")
            ),
            "{host}"
        );
    }
    assert_eq!(
        under("fpm-fcgi"),
        (
            format!("\nWarning: {NAN_WARNING} in witness.php on line 2\nNAN"),
            String::new()
        ),
        "another host falls to the plain form on the body"
    );
}

/// `display_errors` is read as `php_get_display_errors_mode`
/// (`Z/main/main.c:492-517`) reads it: a number is `ZEND_ATOL` of the
/// value narrowed to php's `uint8_t`, where `2` is the error stream,
/// `0` is off and any other is the body, php's `STDOUT` fallback.
/// Measured under the pin, `-d display_errors=2` over `<?php\necho
/// NAN;` writes `NAN` alone to stdout and the warning to stderr, as
/// ` 2` and `2x` do, `atoll` skipping the space and stopping at the
/// `x`; `7` and `-1` write the plain form on the body; `256` narrows
/// to `0` and writes `NAN` alone, and `258` narrows to `2`; a word
/// that is none of php's five, `abc`, is `ZEND_ATOL`'s zero.
#[test]
fn a_numeric_display_errors_is_read_as_phps_uint8_mode() {
    let under = |value: &str| {
        let engine = declaring_nan(Engine::cli_builder())
            .ini("display_errors", value)
            .build();
        let mut kept = Kept::default();
        engine.serve_into(description("<?php\necho NAN;"), &mut kept);
        (kept.body(), kept.stderr())
    };
    let on_stderr = (
        String::from("NAN"),
        format!("Warning: {NAN_WARNING} in witness.php on line 2\n"),
    );
    let on_body = (
        format!("\nWarning: {NAN_WARNING} in witness.php on line 2\nNAN"),
        String::new(),
    );
    let off = (String::from("NAN"), String::new());
    for value in ["2", " 2", "2x", "258", "-254"] {
        assert_eq!(under(value), on_stderr, "{value:?} is the error stream");
    }
    for value in ["7", "-1", "1", "stdout", "on", "yes", "true"] {
        assert_eq!(under(value), on_body, "{value:?} is the body");
    }
    for value in ["0", "256", "abc", "", "off", "no", "false"] {
        assert_eq!(under(value), off, "{value:?} is off");
    }
}

/// The error stream and the log are two channels on the sink, so a
/// host with an `error_log` file keeps the stderr form of a
/// diagnostic off the file, and one diagnostic at
/// `display_errors=stderr` and `log_errors=1` is two calls in
/// php's order: `php_error_cb` logs before it displays
/// (`Z/main/main.c:1459`, then `:1484`), and the stderr form ends in
/// `\n` where the log entry ends in nothing. Under the CLI the two
/// are one fd, and what it reads is
/// `PHP Warning:  … line 2\nWarning: … line 2\n`, which is what the
/// pinned oracle writes for `-d display_errors=stderr -d
/// log_errors=1` over `<?php\necho NAN;`.
#[test]
fn the_stderr_form_and_the_log_entry_are_two_calls_in_phps_order() {
    let mut kept = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("display_errors", "stderr")
        .ini("log_errors", "1")
        .build()
        .serve_into(description("<?php\necho NAN;"), &mut kept);
    assert_eq!(kept.body(), "NAN");
    assert_eq!(
        kept.aside(),
        vec![
            Aside::Log(
                format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2").into_bytes()
            ),
            Aside::Stderr(
                format!("Warning: {NAN_WARNING} in witness.php on line 2\n").into_bytes()
            ),
        ]
    );
}

/// A dropped repeat is dropped whole, php's `if (display)` around
/// the store as well as the two renderings: it is not logged, and
/// the diagnostic it repeats stays the one `error_get_last()`
/// answers.
#[test]
fn a_dropped_repeat_is_neither_logged_nor_stored() {
    let mut logged = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("ignore_repeated_errors", "1")
        .ini("ignore_repeated_source", "1")
        .ini("display_errors", "0")
        .ini("log_errors", "1")
        .build()
        .serve_into(description("<?php\necho NAN, NAN;"), &mut logged);
    assert_eq!(
        logged.logged(),
        format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
        "one log line for the two warnings"
    );
}

/// The same request through the `Capture` sink, which is the shape
/// every extension's own test reads (design §4.3 seam 4, §4.10).
#[test]
fn the_capture_sink_holds_what_a_zend_only_request_wrote() {
    let served = Engine::cli_builder()
        .build()
        .serve_capture("<?php echo 1;", crate::test::Capture::default());
    served.assert_clean("1");
    let served = Engine::cli_builder().build().serve_capture(
        "<?php var_dump(extension_loaded('Core'));",
        crate::test::Capture::default(),
    );
    assert_eq!(served.stdout_text(), "bool(true)\n");
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
        if let Some(value) = value.as_str()
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

use crate::test::Files;

/// The realpath cache is the engine's, as php's is a php-fpm child's
/// (#2941): the second request resolves its program's path, and an
/// include's, from what the first entered, asking the file system
/// nothing while the entries' time to live holds; once the cache is
/// cleaned, as `clearstatcache(true)` cleans it, the next request asks
/// again.
#[test]
fn a_second_request_resolves_its_paths_from_the_engine_s_realpath_cache() {
    let files = Files::new(
        "realpath-cache",
        &[
            ("b.php", "<?php\n"),
            ("main.php", "<?php\ninclude 'b.php';\necho \"ok\\n\";\n"),
        ],
    );
    let engine = Engine::cli_builder().build();
    let mut asked = Vec::new();
    let mut serve = || {
        let before = engine.shared.realpath_cache.looked_up();
        files.serve(&engine, "main.php").assert_clean("ok\n");
        asked.push(engine.shared.realpath_cache.looked_up() - before);
    };
    serve();
    serve();
    let entered = engine
        .shared
        .realpath_cache
        .listed(|bucket| bucket.path.clone());
    assert!(
        entered.iter().any(|path| path.ends_with(b"/b.php")),
        "the include's path stands in the cache"
    );
    engine.shared.realpath_cache.clean();
    serve();
    assert!(asked[0] > 0, "the first request asks the file system");
    assert_eq!(asked[1], 0, "the second asks nothing: {asked:?}");
    assert!(asked[2] > 0, "once the cache is cleaned it asks again");
}

/// What turns php's cache off or past its entries still asks the
/// file system on every request: an expired entry, and an
/// `open_basedir`, which turns the cache off.
#[test]
fn an_expired_entry_and_an_open_basedir_ask_the_file_system_again() {
    let files = Files::new("realpath-ttl", &[("main.php", "<?php\n")]);
    for (name, value) in [("realpath_cache_ttl", "-1"), ("open_basedir", "/")] {
        let engine = Engine::cli_builder().ini_default(name, value).build();
        for request in 0..2 {
            let before = engine.shared.realpath_cache.looked_up();
            files.serve(&engine, "main.php").assert_clean("");
            let asked = engine.shared.realpath_cache.looked_up() - before;
            assert!(asked > 0, "{name}={value}: request {request} asked nothing");
        }
    }
}

/// A `realpath_cache_ttl` of zero leaves the time unread, `*t` at
/// php's `-1` (`Z/Zend/zend_virtual_cwd.c:1140`), which each bucket
/// is entered as expiring at, as the oracle's
/// `php -n -d realpath_cache_ttl=0` lists it in `realpath_cache_get()`;
/// no entry expires.
#[test]
fn a_time_to_live_of_zero_enters_each_bucket_expiring_at_minus_one() {
    let files = Files::new("realpath-ttl0", &[("main.php", "<?php\n")]);
    let engine = Engine::cli_builder()
        .ini_default("realpath_cache_ttl", "0")
        .build();
    for _ in 0..2 {
        files.serve(&engine, "main.php").assert_clean("");
    }
    let expires = engine.shared.realpath_cache.listed(|bucket| bucket.expires);
    assert!(!expires.is_empty(), "the program's path was entered");
    assert!(expires.iter().all(|&at| at == -1), "{expires:?}");
}

/// The host's `tsrm_realpath`, which php-fpm's `init_request_info`
/// asks of the program's path before each request, reads and enters
/// the engine's realpath cache: the first asks the file system, the
/// next asks nothing, and the request after it resolves its program
/// from what the host entered. A path that is not there resolves to
/// nothing.
#[test]
fn the_host_s_realpath_reads_and_enters_the_engine_s_realpath_cache() {
    let files = Files::new("realpath-host", &[("main.php", "<?php\necho \"ok\\n\";\n")]);
    let engine = Engine::cli_builder().build();
    let program = files
        .path()
        .join("main.php")
        .into_os_string()
        .into_encoded_bytes();
    let mut asked = Vec::new();
    let mut resolved = Vec::new();
    for _ in 0..2 {
        let before = engine.shared.realpath_cache.looked_up();
        resolved.push(engine.realpath(&program));
        asked.push(engine.shared.realpath_cache.looked_up() - before);
    }
    let before = engine.shared.realpath_cache.looked_up();
    files.serve(&engine, "main.php").assert_clean("ok\n");
    asked.push(engine.shared.realpath_cache.looked_up() - before);
    assert!(resolved[0].is_some(), "the program resolves");
    assert_eq!(resolved[0], resolved[1]);
    assert!(asked[0] > 0, "the first resolution asks the file system");
    assert_eq!(
        &asked[1..],
        [0, 0],
        "the second and the request ask nothing"
    );
    let mut missing = files.path().as_os_str().as_encoded_bytes().to_vec();
    missing.extend_from_slice(b"/missing.php");
    assert_eq!(engine.realpath(&missing), None);
}

/// The once forms over a second file (#396): the file is served the
/// first time and answers what it returned, and `true` every time
/// after, from either once form, as the oracle prints for these
/// files.
#[test]
fn a_once_form_serves_a_file_once_and_answers_true_after() {
    let files = Files::new(
        "once",
        &[
            ("b.php", "<?php\necho \"b\\n\"; return 7;\n"),
            (
                "main.php",
                "<?php\nvar_dump(include_once \"b.php\", include_once \"b.php\", require_once \"b.php\");\n",
            ),
        ],
    );
    files
        .serve(&Engine::cli_builder().build(), "main.php")
        .assert_clean("b\nint(7)\nbool(true)\nbool(true)\n");
}

/// An include inside a function reads and writes the function's
/// variables (#396): `zend_rebuild_symbol_table` gives the frame a
/// table, the included file attaches to it, and what it wrote to
/// `$r` is in the function's CV when the include returns, as the
/// oracle prints for these files.
#[test]
fn an_include_inside_a_function_shares_the_function_s_variables() {
    let files = Files::new(
        "func",
        &[
            (
                "fb.php",
                "<?php\necho \"p=$p\\n\"; $r = 3; return \"ok\";\n",
            ),
            (
                "main.php",
                "<?php\nfunction f($p) { $q = include \"fb.php\"; return \"$p|$q|$r\"; }\nvar_dump(f(\"z\"));\n",
            ),
        ],
    );
    files
        .serve(&Engine::cli_builder().build(), "main.php")
        .assert_clean("p=z\nstring(6) \"z|ok|3\"\n");
}

/// A variable an included file writes at the top level is the
/// includer's, and stands in `EG(symbol_table)` after every name the
/// main op array attached at its start (#396), so
/// `shutdown_destructors`' reverse walk serves it first, then the
/// main file's own in reverse: `d2`, `d3`, `d1`, as the oracle
/// prints for these files.
#[test]
fn a_variable_an_included_file_wrote_is_destructed_in_php_s_order() {
    let files = Files::new(
        "dtor",
        &[
            ("db.php", "<?php\n$b = new A(2);\n"),
            (
                "main.php",
                "<?php\nclass A { public $n; function __construct($n) { $this->n = $n; } function __destruct() { echo \"d{$this->n}\\n\"; } }\n$a = new A(1);\ninclude \"db.php\";\n$c = new A(3);\necho \"end\\n\";\n",
            ),
        ],
    );
    files
        .serve(&Engine::cli_builder().build(), "main.php")
        .assert_clean("end\nd2\nd3\nd1\n");
}

/// A program served as php's phpdbg serves one
/// ([`Primary::Bare`]): the throwable it leaves uncaught is kept
/// for the host, its class, site and `__toString`, and nothing is
/// rendered; the handler `set_exception_handler` set has it first,
/// and what it takes is not kept; the file is no included file.
/// The native a host's own extension declares, for the test below.
#[php_macros::php_function]
fn host_module_answer() -> i64 {
    42
}

/// An extension a host built itself starts with the bundle: its natives
/// are callable and its name is a loaded extension's, the last one
/// `get_loaded_extensions()` lists, as php lists phpdbg's.
#[test]
fn an_extension_the_host_built_starts_after_the_bundle() {
    let built = crate::api::abi::ExtensionBuilder::default()
        .functions(vec![host_module_answer::declared()])
        .build("phpdbg");
    let engine = Engine::cli_builder().additional_extension(built).build();
    let (body, _) = served(
        &engine,
        "<?php echo host_module_answer(), ' ', extension_loaded('phpdbg') ? 'loaded' : 'absent', ' '; foreach (get_loaded_extensions() as $last) {} echo $last;",
    );
    assert_eq!(body, "42 loaded phpdbg");
}

#[test]
fn a_program_served_for_a_host_that_renders_the_uncaught_keeps_it() {
    let files = Files::new(
        "primary",
        &[
            (
                "thrown.php",
                "<?php\necho get_included_files() === [] ? \"none\\n\" : \"some\\n\";\nthrow new ErrorException(\"no\");\n",
            ),
            (
                "handled.php",
                "<?php\nset_exception_handler(function ($e) { echo \"took \", $e->getMessage(), \"\\n\"; });\nthrow new Exception(\"x\");\n",
            ),
        ],
    );
    let serve = |name: &str| {
        let mut sink = Kept::default();
        let observables = Engine::cli_builder().build().serve_into(
            RequestDescription {
                sapi_name: String::from("phpdbg"),
                script_name: Some(
                    files
                        .path()
                        .join(name)
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                ),
                cwd: files.path().to_path_buf(),
                primary: Primary::Bare,
                ..RequestDescription::default()
            },
            &mut sink,
        );
        (sink.body(), observables)
    };
    let (body, observables) = serve("thrown.php");
    assert_eq!(body, "none\n");
    let uncaught = observables
        .uncaught
        .expect("the uncaught throwable observed");
    assert_eq!(uncaught.class, b"ErrorException");
    assert_eq!(uncaught.line, 3);
    assert!(uncaught.file.ends_with(b"thrown.php"));
    assert!(
        uncaught.text.starts_with(b"ErrorException: no in "),
        "{uncaught:?}"
    );
    assert_eq!(observables.exit_code, 255);
    let (body, observables) = serve("handled.php");
    assert_eq!(body, "took x\n");
    assert_eq!(observables.uncaught, None);
}

/// A file the parser refuses is the `ParseError` the includer may
/// catch, at the file's own line (#396); a plain `include` whose
/// compile was refused is not in the included-files table, so a
/// once form after it compiles the file again and the uncaught
/// `ParseError` is rendered at its level, as the oracle prints for
/// these files.
#[test]
fn a_parse_error_in_an_included_file_is_a_parse_error_the_includer_may_catch() {
    let files = Files::new(
        "parse",
        &[
            ("bad.php", "<?php\n$x = ;\n"),
            (
                "main.php",
                "<?php\ntry { include \"bad.php\"; } catch (ParseError $e) { echo $e->getMessage(), \" at line \", $e->getLine(), \"\\n\"; }\necho \"after\\n\";\nvar_dump(include_once \"bad.php\");\n",
            ),
        ],
    );
    let served = files.serve(&Engine::cli_builder().build(), "main.php");
    let bad = files.path().join("bad.php");
    assert_eq!(
        served.stdout_text(),
        format!(
            "syntax error, unexpected token \";\" at line 2\nafter\n\nParse error: syntax error, unexpected token \";\" in {} on line 2\n",
            bad.canonicalize().unwrap_or(bad).display()
        )
    );
    assert_eq!(served.exit_code, 255);
}

/// What compiling a file raised is raised on every request that
/// serves it, the cache hit as the miss, in the unit's own file and
/// at the compiler's line, before the op array writes anything: what
/// the oracle does with no cache, and what opcache does on a hit
/// with `opcache.record_warnings` on. `error_reporting` set from
/// the ini masks it as it masks a runtime warning, since the path
/// is one.
#[test]
fn a_cached_unit_raises_what_compiling_it_raised_on_every_request() {
    let engine = declaring_nan(Engine::cli_builder()).build();
    let files = Files::new("nan", &[("nan.php", "<?php\necho 'a';\necho NAN . 'z';")]);
    let path = files.path().join("nan.php");
    let name = path.to_string_lossy().as_bytes().to_vec();
    let described = || RequestDescription {
        sapi_name: String::from("cli"),
        script_name: Some(name.clone()),
        ..RequestDescription::default()
    };
    // `php_execute_script` expands the primary file's path with its
    // links resolved, which a temporary directory behind a link shows.
    let expanded = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    for _ in 0..2 {
        let mut sink = Kept::default();
        engine.serve_into(described(), &mut sink);
        assert_eq!(
            sink.body(),
            format!(
                "\nWarning: unexpected NAN value was coerced to string in {} on line 3\naNANz",
                expanded.to_string_lossy()
            )
        );
    }
    let mut masked = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("error_reporting", "0")
        .build()
        .serve_into(described(), &mut masked);
    assert_eq!(masked.body(), "aNANz");
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
        let compiled = req.compiling(|strings, _, _, _| {
            compile(
                source,
                Origin {
                    filename: b"witness.php",
                    cwd: Path::new(""),
                    kind: SourceKind::File,
                    keys_before: 0,
                    request_classes: RequestClasses::default(),
                    multibyte: None,
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
                    arena: &mut php_compile::syntax::Arena::new(),
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

#[test]
fn the_call_sequence_reaches_a_native_and_writes_what_it_returned() {
    let engine = Engine::cli_builder().build();
    let (body, _) = served(&engine, "<?php var_dump(extension_loaded('Core'));");
    assert_eq!(body, "bool(true)\n");
    let (body, _) = served(&engine, "<?php var_dump(extension_loaded('nope'));");
    assert_eq!(body, "bool(false)\n");
}

/// The four arithmetic opcodes end to end, each snippet the one its
/// kernel's witness pins, so that what the fences claim is what this
/// engine writes and not only what the oracle wrote.
#[test]
fn the_arithmetic_opcodes_write_what_their_witnesses_pin() {
    let engine = Engine::cli_builder().build();
    for (source, pinned) in [
        (
            "<?php echo 1 + (2 + extension_loaded('Core')), \"|\", 1.5 + (2.5 + extension_loaded('Core')), \"|\", \"1\" + (1 + extension_loaded('Core')), \"|\", 9223372036854775807 + (1 + extension_loaded('Core'));",
            "4|5|3|9.2233720368548E+18",
        ),
        (
            "<?php echo 1 - (2 - extension_loaded('Core')), \"|\", 1.5 - (2.5 - extension_loaded('Core')), \"|\", \"3\" - (1 + extension_loaded('Core')), \"|\", -9223372036854775807 - (1 + extension_loaded('Core'));",
            "0|0|1|-9.2233720368548E+18",
        ),
        (
            "<?php echo 2 * (3 * extension_loaded('Core')), \"|\", 1.5 * (2.5 * extension_loaded('Core')), \"|\", \"3\" * (1 + extension_loaded('Core')), \"|\", 4611686018427387904 * (1 + extension_loaded('Core'));",
            "6|3.75|6|9.2233720368548E+18",
        ),
        (
            "<?php echo 6 / (1 + extension_loaded('Core')), \"|\", 7.5 / (1.5 * extension_loaded('Core')), \"|\", \"7\" / (1 + extension_loaded('Core')), \"|\", (-9223372036854775807 - extension_loaded('Core')) / (0 - extension_loaded('Core'));",
            "3|5|3.5|9.2233720368548E+18",
        ),
    ] {
        let (body, observables) = served(&engine, source);
        assert_eq!(body, pinned, "{source}");
        assert_eq!(observables.fatal, None, "{source}");
    }
}

/// `div_function`: the divisor of zero php refuses to fold, so the
/// opcode throws `DivisionByZeroError` where it stands, on line 1 of
/// a source whose `<?php` and whose `echo` share it. The witness on
/// [`crate::ops::div`] pins the oracle's rendering of the display branch;
/// what this holds is the log branch's spelling of the same fatal,
/// the status, and the double divisor the witness does not reach.
#[test]
fn a_division_by_zero_throws_php_s_error_where_the_opcode_stands() {
    let engine = Engine::cli_builder().build();
    let (_, observables) = served(&engine, "<?php echo 1/0;");
    assert_eq!(
        observables.fatal.as_deref(),
        Some(
            "PHP Fatal error:  Uncaught DivisionByZeroError: Division by zero \
             in witness.php:1\nStack trace:\n#0 {main}\n  thrown in witness.php on line 1"
        )
    );
    assert_eq!(observables.exit_code, 255);
    let (_, observables) = served(&engine, "<?php echo 1/0.0;");
    assert_eq!(
        observables
            .fatal
            .as_deref()
            .map(|text| text.contains("Uncaught DivisionByZeroError: Division by zero")),
        Some(true)
    );
}

/// `zend_binop_error`: an operand php will not read as a number is a
/// `TypeError` naming php's types around the operator as written, and
/// one with a number in front of it is the warning and the number.
#[test]
fn an_operand_php_refuses_to_read_as_a_number_is_php_s_type_error() {
    let engine = Engine::cli_builder().build();
    let (_, observables) = served(&engine, "<?php echo \"abc\" + extension_loaded('Core');");
    assert_eq!(
        observables
            .fatal
            .as_deref()
            .map(|text| text
                .contains("Uncaught TypeError: Unsupported operand types: string + bool")),
        Some(true),
        "{:?}",
        observables.fatal
    );
    let (body, observables) = served(
        &engine,
        "<?php echo \"1abc\" + (1 + extension_loaded('Core'));",
    );
    assert_eq!(
        body,
        "\nWarning: A non-numeric value encountered in witness.php on line 1\n3"
    );
    assert_eq!(observables.fatal, None);
}

#[test]
fn echo_writes_each_part_as_php_converts_it() {
    let engine = Engine::cli_builder().build();
    let (body, _) = served(
        &engine,
        "<?php echo 1, \"|\", true, \"|\", null, \"|\", 2.5;",
    );
    assert_eq!(body, "1|1||2.5");
}

/// `zend_exception_error` through `php_error_cb`: the four lines the
/// witness beside [`Request::uncaught`] pins against the oracle, with
/// the throw site in php's two spellings.
#[test]
fn a_call_of_a_name_nothing_declares_ends_the_request_as_php_names_it() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(&engine, "<?php\nnope();");
    assert_eq!(
        body,
        "\nFatal error: Uncaught Error: Call to undefined function nope() \
         in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2\n"
    );
    assert_eq!(
        observables.fatal.as_deref(),
        Some(
            "PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
             in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
        )
    );
    assert_eq!(observables.exit_code, 255);
}

/// A throwable a native left pending names the native's call as its
/// site, and the native's own frame stands above `{main}`.
#[test]
fn a_throwable_a_native_left_pending_names_the_natives_call() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(&engine, "<?php\nvar_dump();");
    assert_eq!(
        body,
        "\nFatal error: Uncaught ArgumentCountError: var_dump() expects at least 1 argument, \
         0 given in witness.php:2\nStack trace:\n#0 witness.php(2): var_dump()\n#1 {main}\n  \
         thrown in witness.php on line 2\n"
    );
    assert_eq!(observables.exit_code, 255);
}

/// The fatal is `php_error_cb`'s to render, once, on the streams the
/// two ini entries name; `serve` reports its log form and writes
/// nothing of its own.
#[test]
fn a_fatal_is_displayed_or_logged_as_the_ini_entries_say() {
    let mut logged = Kept::default();
    let observables = Engine::cli_builder()
        .ini("display_errors", "0")
        .ini("log_errors", "1")
        .build()
        .serve_into(description("<?php\nnope();"), &mut logged);
    assert!(logged.body().is_empty(), "display_errors is off");
    assert_eq!(
        logged.log().first().cloned(),
        observables.fatal,
        "what the log holds is the fatal, once"
    );
    assert_eq!(logged.log().len(), 1, "the four lines are one entry");

    let mut masked = Kept::default();
    Engine::cli_builder()
        .ini("error_reporting", "0")
        .build()
        .serve_into(description("<?php\nnope();"), &mut masked);
    assert!(masked.body().is_empty(), "E_ERROR is masked out");
    assert!(masked.log().is_empty(), "log_errors is off");
}

/// `php_error_cb` builds one diagnostic with one `spprintf` and gives
/// it to `php_log_err` once, the backtrace inside it, and
/// `php_log_err_with_severity` stamps one date on it and writes it
/// as one message (`Z/main/main.c:1459`). So the sink sees one call
/// for the four-line fatal, and one call per warning for two
/// warnings, with no terminator on any of them.
#[test]
fn the_sink_is_given_one_call_per_diagnostic_whatever_its_lines() {
    let mut fatal = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("display_errors", "0")
        .ini("log_errors", "1")
        .build()
        .serve_into(description("<?php\nnope();"), &mut fatal);
    assert_eq!(
        fatal.log(),
        vec![
            "PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
             in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
        ],
        "four lines, one call"
    );

    let mut warned = Kept::default();
    declaring_nan(Engine::cli_builder())
        .ini("display_errors", "0")
        .ini("log_errors", "1")
        .build()
        .serve_into(description("<?php\necho NAN;\necho NAN;"), &mut warned);
    assert_eq!(
        warned.log(),
        vec![
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2"),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 3"),
        ],
        "two lines, two calls"
    );
}

#[test]
fn an_opcode_with_no_handler_ends_the_request_naming_it() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), description(""));
    let unit = Unit {
        main: php_unit::OpArray {
            oplines: vec![php_unit::Opline {
                opcode: php_unit::Opcode::UserOpcode,
                ..php_unit::Opline::default()
            }],
            ..php_unit::OpArray::default()
        },
        ..Unit::default()
    };
    let frame = req.pushed(unit.published(None));
    assert_eq!(
        crate::tier::execute::<Nop, Untold>(&mut req, frame),
        Err(Fatal::error(
            "InternalError: ZEND_USER_OPCODE has no handler in this engine yet"
        ))
    );
}

/// Design §4.3 seam 8: `Site::Entry` is raised for the entry frame of
/// an `execute` whatever the caller is, and `Site::Return` for no
/// entry frame, which the stack's entry slot says.
#[test]
fn the_loop_asks_the_tier_at_the_entry_and_at_no_entry_frames_return() {
    let engine = Engine::cli_builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), description(""));
    let unit = engine
        .unit(b"<?php echo 1;", b"witness.php")
        .map_or_else(Unit::default, |unit| Unit::clone(&unit));
    let frame = req.pushed(unit.published(None));
    assert_eq!(
        crate::tier::execute::<Probe, Untold>(&mut req, frame),
        Ok(())
    );
    assert_eq!(
        req.state::<Asked>().sites,
        vec![(Site::Entry, frame)],
        "the entry frame is activated, and its own return raises no site"
    );
}

/// Design §5.6, §8: a user frame is activated after its `RECV*`
/// prologue, so `Probe` sees `(Entry, callee)` once per call and
/// `Return` once per callee frame; a call whose `RECV` throws is
/// reported `Threw` before the activation, so no `Entry` is raised
/// for it (#309).
#[test]
fn a_call_activates_its_frame_once_and_a_recv_that_throws_never_does() {
    let engine = Engine::cli_builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), description(""));
    let unit = engine
        .unit(
            b"<?php function f($a, $b = 1) { return $a; } f(1); f(2, 3); f();",
            b"witness.php",
        )
        .unwrap_or_else(|| Unit::default().published(None));
    req.declare_functions(&unit);
    let frame = req.pushed(unit);
    let outcome = crate::tier::execute::<Probe, Untold>(&mut req, frame);
    assert!(
        outcome.is_ok() && req.is_pending(),
        "the third call's RECV throws and nothing catches it: it stays pending for the caller"
    );
    let callee = FrameRef(frame.0 + 1);
    assert_eq!(
        req.state::<Asked>().sites,
        vec![
            (Site::Entry, frame),
            (Site::Entry, callee),
            (Site::Return(crate::tier::SafepointIdx(1)), callee),
            (Site::Entry, callee),
            (Site::Return(crate::tier::SafepointIdx(1)), callee),
        ],
        "two calls activated and returned, the third never activated"
    );
}

/// `zend_ini_parse_quantity` (`Z/Zend/zend_ini.c:613-841`), which is
/// how `opcache.opt_debug_level=0x10000` is a bit and not a zero: a
/// base prefix is read as `strtoul` with base zero reads it, a
/// leading `0` before a digit is octal, a `K`, `M` or `G` multiplies,
/// and every shape php interprets as zero for backward compatibility
/// is zero.
#[test]
fn a_whole_number_ini_value_is_read_as_php_s_quantity_reads_it() {
    let read = |value: &str| ini_quantity(value.as_bytes());
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

/// Design §6.1: what php prints on the second stream before the
/// program runs is the op arrays as the compiler left them, and it
/// prints them only where `opcache.opt_debug_level` carries the
/// `0x10000` bit. The bytes are held to the oracle's by
/// `xtask compile-identity`, which runs both binaries over one file;
/// what is held here is the switch.
#[test]
fn the_dump_before_the_optimizer_is_the_op_arrays_under_the_ini_bit_and_nothing_without_it() {
    // The entry is opcache's, and php has none without it.
    let opcache = || {
        let entry = php_unit::decl::IniDecl {
            name: Name(b"opcache.opt_debug_level"[..].into()),
            default: Some(b"0"[..].into()),
            user: false,
            per_dir: false,
            system: true,
        };
        let extension = crate::api::abi::ExtensionBuilder::default()
            .ini(vec![entry])
            .build("t");
        Engine::cli_builder().extensions(&[extension])
    };
    let described = description("<?php echo 1;");
    assert_eq!(
        Engine::cli_builder()
            .ini("opcache.opt_debug_level", "0x10000")
            .build()
            .dump_before_optimizer(&described),
        None,
        "no opcache, and so no entry"
    );
    assert_eq!(
        opcache().build().dump_before_optimizer(&described),
        None,
        "no opt_debug_level at all"
    );
    assert_eq!(
        opcache()
            .ini("opcache.opt_debug_level", "0")
            .build()
            .dump_before_optimizer(&described),
        None,
        "the entry at its default"
    );
    let dumped = opcache()
        .ini("opcache.opt_debug_level", "0x10000")
        .build()
        .dump_before_optimizer(&described)
        .expect("the dump before the optimizer");
    assert_eq!(
        String::from_utf8_lossy(&dumped),
        "\n$_main:\n     ; (lines=2, args=0, vars=0, tmps=0)\n     ; (before optimizer)\n     ; witness.php:1-1\n     ; return  [] RANGE[0..0]\n0000 ECHO int(1)\n0001 RETURN int(1)\n",
        "the one op array, in php's format"
    );
    assert_eq!(
        opcache()
            .ini("opcache.opt_debug_level", "0x10000")
            .build()
            .dump_before_optimizer(&description("<?php echo 1 +;")),
        None,
        "a source the compiler refused is no dump, as php's empty second stream is"
    );
}
