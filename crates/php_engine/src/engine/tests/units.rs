//! Units across requests: the map-pointer count, preloading, the file
//! cache, a cached unit's diagnostics and the dump before the
//! optimizer.

use super::*;

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

/// A unit [`Frozen::new`] refuses is held nowhere
/// (`unit_cache::refused`): the asker is answered as php without the
/// cache answers a compile, [`Filed::Compiled`] with no early-bound class
/// published, so it binds every class it declares as its own, and each
/// later ask compiles the file again, one compile for each. The check
/// runs after the unit took its map-pointer offsets of the engine's, so
/// each of those compiles moves the count by as much as the first did.
/// No compile's unit is refused, since a unit's walk passes its
/// closures' cells over (#4684); the refusal is driven here by the
/// `share` the cache is handed.
#[test]
fn a_unit_the_check_refuses_is_held_nowhere_and_every_ask_compiles_it_again() {
    use crate::unit_cache::{Asked, Compiled, Filed, Levels};
    let files = Files::new(
        "refused",
        &[(
            "declares.inc",
            "<?php class Refused { function m() {} } function refused() {}",
        )],
    );
    let path = files.path().join("declares.inc");
    let path = path.to_string_lossy().into_owned().into_bytes();
    let engine = Engine::cli_builder().build();
    let shared = &engine.shared;
    let arena = shared.arena.current();
    let compiles = std::cell::Cell::new(0);
    let count = || shared.map_ptr_last.load(Ordering::Relaxed);
    let mut moved = Vec::new();
    for _ in 0..3 {
        let before = count();
        let held = std::cell::Cell::new(false);
        let asked = Asked {
            path: &path,
            source: None,
            written: None,
            opened: true,
            validation: Validation::Never,
            protection: 0,
            now: crate::unit_cache::now(),
            levels: Levels::Memory,
            optimizer: shared.optimizer(),
            laid: crate::tier::words_laid(shared, None),
            held: &held,
        };
        let (answered, how, early) = shared.units.file(
            &mut (),
            &arena.files,
            asked,
            |(), source, _| {
                compiles.set(compiles.get() + 1);
                let request = ForRequest::none(shared.unbound());
                let mut laid = php_compile::syntax::Arena::default();
                let (cwd, stack) = (Path::new(""), StackLimit::UNCHECKED);
                let heap = CompileHeap::Engine;
                shared.compile_file(
                    source,
                    &path,
                    SourceKind::File,
                    cwd,
                    stack,
                    request,
                    heap,
                    &mut laid,
                )
            },
            |_| None,
            |unit| Err(shared.published_alone(unit)),
            |(), _| Arc::new([]),
        );
        assert_eq!(how, Filed::Compiled);
        assert!(early.is_none(), "no early-bound class published");
        assert!(matches!(answered, Some(Ok(Compiled::Published(_)))));
        moved.push(count() - before);
    }
    assert_eq!(compiles.get(), 3, "one compile for each ask");
    assert!(
        moved.iter().all(|moved| *moved > 0),
        "each took offsets: {moved:?}"
    );
    assert!(
        moved.windows(2).all(|pair| pair[0] == pair[1]),
        "as many each time: {moved:?}"
    );
}

/// The refusal a compile reaches: a class that uses a trait and holds a
/// closure in its composed declaration, which the check refuses where
/// its link would be published (`Request::publish`, #4684), so the class
/// is bound per request. What this holds of its cost: the file is
/// compiled once and held, every later request is served the held unit
/// with no compile, and the arena reserves nothing for the class, where
/// the same class holding no closure is published at its first link;
/// the link each request makes again is read by no count here.
#[test]
fn a_class_the_check_refuses_at_its_link_is_compiled_once_and_reserves_nothing() {
    use crate::api::abi::{Admission, ExtensionBuilder, Lookup};
    fn compiled(req: &mut Request, path: &[u8]) {
        let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
        req.response_mut()
            .write(&[b"compiled ", name, b"\n"].concat());
    }
    let files = Files::new(
        "refused-link",
        &[
            (
                "closures.inc",
                "<?php trait Greets { public function hi(): string { return 'hi'; } }\n\
                 class Greeter { use Greets; const LEN = strlen(...);\n\
                 public $twice = static function (int $n): int { return 2 * $n; }; }",
            ),
            (
                "plain.inc",
                "<?php trait Greets { public function hi(): string { return 'hi'; } }\n\
                 class Greeter { use Greets; const LEN = 4; public $twice = 8; }",
            ),
        ],
    );
    let serve = |file: &str, echo: &str| {
        let extension = ExtensionBuilder::default()
            .admission(|_, _| Admission::Held {
                validation: Validation::Never,
                lookup: Lookup::Resolved,
                protection: 0,
                file_cache: None,
            })
            .optimizer_pass(compiled)
            .build("t");
        let engine = Engine::cli_builder().extensions(&[extension]).build();
        let classes = || engine.shared.arena.current().counts()[0];
        let path = files.path().join(file);
        let program = format!(
            "<?php require '{}'; $g = new Greeter; echo {echo}, $g->hi(), \"\\n\";",
            path.display()
        );
        let before = classes();
        let mut bodies = Vec::new();
        let mut reserved = Vec::new();
        for _ in 0..3 {
            bodies.push(served(&engine, &program).0);
            reserved.push(classes() - before);
        }
        (bodies, reserved)
    };
    let (bodies, refused) = serve("closures.inc", "(Greeter::LEN)('abcd'), ($g->twice)(4)");
    assert_eq!(
        bodies,
        ["compiled closures.inc\n48hi\n", "48hi\n", "48hi\n"],
        "compiled once, and served held after"
    );
    let (bodies, published) = serve("plain.inc", "Greeter::LEN, $g->twice");
    assert_eq!(bodies, ["compiled plain.inc\n48hi\n", "48hi\n", "48hi\n"]);
    assert_eq!(
        published[0],
        refused[0] + 1,
        "the plain class is published at its first link and the refused one never: {published:?} {refused:?}"
    );
    for reserved in [&refused, &published] {
        assert!(
            reserved.windows(2).all(|pair| pair[0] == pair[1]),
            "nothing reserved by a later request: {reserved:?}"
        );
    }
}

/// An extension that names a file to preload as the engine
/// finishes starting up, reading it from an ini entry of its own,
/// and holds every file in the cache, never validated, as opcache
/// holds one with `opcache.validate_timestamps=0`.
fn preloading(script: &Path) -> Extension {
    let entry = php_unit::decl::IniDecl {
        name: b"t.preload",
        default: Some(b""),
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

/// Whether each function's and method's op array of `unit` has its row
/// of words in `epoch`, in the order the unit holds them.
fn words_laid(epoch: &crate::arena::Arena, unit: &PublishedUnit) -> Vec<bool> {
    let functions = unit.functions.iter().map(|(_, code)| code);
    let methods = unit.methods.iter().map(|method| &method.body);
    let laid = |code: &php_unit::OpArray| {
        crate::tier::EngineFuncRef::of(crate::tier::FuncRef::at(code.runtime_cache))
            .is_some_and(|func| epoch.func(func.offset()).is_some())
    };
    functions.chain(methods).map(laid).collect()
}

/// A preload's `eval` is the preload's as a file it preloads is (design
/// §5.14, #4696): the epoch laid after the preload holds a row of words
/// for the op arrays of the function and the method the `eval` declared,
/// as it holds one for the preloaded file's own function, and the
/// request reaches all three.
#[test]
fn a_preload_evals_function_and_method_have_their_words_laid_as_a_preloaded_files_do() {
    let files = Files::new(
        "preload-eval-words",
        &[(
            "preload.inc",
            "<?php\nfunction pre_w() { return 'w'; }\n\
             eval('function eval_w() { return \"e\"; } class EvalW { function m() { return \"m\"; } }');\n",
        )],
    );
    let script = files.path().join("preload.inc");
    let engine = Engine::cli_builder()
        .extensions(&[preloading(&script)])
        .ini("t.preload", script.as_os_str().as_encoded_bytes())
        .build();
    let served = engine.serve_capture(
        "<?php echo pre_w(), eval_w(), (new EvalW)->m();",
        crate::test::Capture::default(),
    );
    assert_eq!(served.stdout_text(), "wem");
    let epoch = engine.shared.arena.current();
    let preloaded = engine
        .shared
        .units
        .preloaded()
        .expect("the preload published");
    let scripts: Vec<bool> = (preloaded.scripts.iter())
        .flat_map(|held| words_laid(&epoch, &held.unit))
        .collect();
    let evals: Vec<bool> = (preloaded.evals.iter())
        .flat_map(|unit| words_laid(&epoch, unit))
        .collect();
    assert_eq!(scripts, [true], "the preloaded file's function");
    assert_eq!(evals, [true, true], "the eval's function and its method");
}

/// Only a preload's `eval` with a declaration the preload keeps outlives
/// the preload (#4696): of an `eval` that declares nothing and one that
/// declares a function, the preload holds the second's unit alone, and
/// the function stands in every request.
#[test]
fn only_a_preload_eval_with_a_kept_declaration_outlives_the_preload() {
    let files = Files::new(
        "preload-eval-kept",
        &[(
            "preload.inc",
            "<?php\neval('$x = 1;');\neval('function eval_k() { return \"k\"; }');\n",
        )],
    );
    let script = files.path().join("preload.inc");
    let engine = Engine::cli_builder()
        .extensions(&[preloading(&script)])
        .ini("t.preload", script.as_os_str().as_encoded_bytes())
        .build();
    let served = engine.serve_capture("<?php echo eval_k();", crate::test::Capture::default());
    assert_eq!(served.stdout_text(), "k");
    let preloaded = engine
        .shared
        .units
        .preloaded()
        .expect("the preload published");
    let kept: Vec<usize> = (preloaded.evals.iter())
        .map(|unit| unit.functions.len())
        .collect();
    assert_eq!(
        kept,
        [1],
        "the eval that declared a function, and not the other"
    );
}

/// An extension that keeps every file in the cache, never validated,
/// and names a file cache from an ini entry of its own.
fn file_caching(dir: &Path) -> Extension {
    let entry = php_unit::decl::IniDecl {
        name: b"t.file_cache",
        default: Some(b""),
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

/// What compiling a file raised is raised on every request that
/// serves it where php serves it without opcache, the CLI's default
/// (`opcache.enable_cli` off, so the unit cache's validation is
/// `EveryHit` and no load is opcache's hit): in the unit's own file and
/// at the compiler's line, before the op array writes anything, the
/// second request as the first. Under opcache with
/// `opcache.record_warnings` off a hit raises none of it, which
/// `note_compiled`'s witness holds (#3123). `error_reporting` set from
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
            name: b"opcache.opt_debug_level",
            default: Some(b"0"),
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
