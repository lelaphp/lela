//! Includes: the realpath cache, the once forms and what an included
//! file shares with its includer.

use super::*;

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
