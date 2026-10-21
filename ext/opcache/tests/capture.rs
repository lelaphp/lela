//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine, Files};

/// php registers the extension as `Zend OPcache`, and the crate's name is
/// no name php answers to.
#[test]
fn the_extension_is_loaded_under_php_s_name() {
    let engine = Engine::builder()
        .extensions(&[php_ext_opcache::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture(
        "<?php var_dump(extension_loaded('Zend OPcache'), extension_loaded('opcache'));",
        sink,
    );
    assert_eq!(served.stdout_text(), "bool(true)\nbool(false)\n");
}

/// `opcache.enable_file_override`: `file_exists`, `is_file` and
/// `is_readable` answer `true` for a file the cache holds though it was
/// deleted since, as php's `accel_file_exists` answers it from the
/// cache's hash, and the call of a file the cache does not hold reaches
/// the native; without the directive, every call does.
#[test]
fn a_cached_file_deleted_since_exists_under_the_file_override() {
    let files = Files::new("opcache-override", &[]);
    let dir = files.path();
    let probe = r"<?php
$f = __DIR__ . '/held.inc';
file_put_contents($f, '<?php return 1;');
include $f;
unlink($f);
var_dump(file_exists($f), is_file($f), is_readable($f), file_exists($f . 'x'));
";
    let served = |overriding: &str| {
        let engine = Engine::builder()
            .extensions(&[
                php_ext_lexbor::extension(),
                php_ext_uri::extension(),
                php_ext_standard::extension(),
                php_ext_opcache::extension(),
            ])
            .ini("opcache.enable_cli", "1")
            .ini("opcache.file_update_protection", "0")
            .ini("opcache.enable_file_override", overriding)
            .build();
        let served = engine.serve_capture_in(dir, b"override.php", probe, Capture::default());
        served.stdout_text()
    };
    let on = served("1");
    let off = served("0");
    assert_eq!(on, "bool(true)\nbool(true)\nbool(true)\nbool(false)\n");
    assert_eq!(off, "bool(false)\nbool(false)\nbool(false)\nbool(false)\n");
}

/// An engine of ext/standard and this crate under `ini`.
fn engine(ini: &[(&str, &str)]) -> Engine {
    let mut builder = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_opcache::extension(),
        ])
        .ini("html_errors", "0");
    for (name, value) in ini {
        builder = builder.ini(name, value);
    }
    builder.build()
}

/// `opcache.preload`: the file is served once, before the first request,
/// what it declared stands in every request, a class a file it only
/// compiled could not link is php's warning, and
/// `opcache_get_status()` reports the preload.
#[test]
fn the_preload_s_declarations_stand_in_every_request() {
    let files = Files::new(
        "opcache-preload",
        &[
            (
                "compiled.inc",
                "<?php\nclass Kept {}\nclass Unlinked extends Missing {}\nfunction compiled() { return 'compiled'; }\n",
            ),
            (
                "preload.inc",
                "<?php\necho \"preloading\\n\";\nopcache_compile_file(__DIR__ . '/compiled.inc');\nfunction preloaded() { return 'preloaded'; }\n",
            ),
        ],
    );
    let dir = files.path();
    let script = dir.join("preload.inc").to_string_lossy().into_owned();
    let engine = engine(&[("opcache.enable_cli", "1"), ("opcache.preload", &script)]);
    let program = r"<?php
echo preloaded(), ' ', compiled(), ' ', class_exists('Kept') ? 'kept' : '-', ' ', class_exists('Unlinked', false) ? '-' : 'unlinked', PHP_EOL;
$s = opcache_get_status()['preload_statistics'];
echo implode(' ', $s['functions']), ' | ', implode(' ', $s['classes']), ' | ', count($s['scripts']), PHP_EOL;
";
    let first = engine.serve_capture_in(dir, b"first.php", program, Capture::default());
    let second = engine.serve_capture_in(dir, b"second.php", program, Capture::default());
    let compiled = dir
        .join("compiled.inc")
        .canonicalize()
        .expect("the compiled file resolves");
    let listed = "compiled preloaded | Kept Unlinked | 2\n";
    assert_eq!(
        first.stdout_text(),
        format!(
            "preloading\n\nWarning: Can't preload unlinked class Unlinked: Unknown parent Missing in {} on line 3\npreloaded compiled kept unlinked\n{listed}",
            compiled.display()
        )
    );
    assert_eq!(
        second.stdout_text(),
        format!("preloaded compiled kept unlinked\n{listed}")
    );
}

/// `zend_accel_error` at `ACCEL_LOG_FATAL` ends the process before
/// anything is served whatever `opcache.log_verbosity_level` says, and
/// writes its line where the level admits it; at `ACCEL_LOG_INFO` the
/// cache logs each file it holds.
#[test]
fn a_fatal_startup_serves_nothing_and_the_cache_logs_its_own_events() {
    let silent = engine(&[
        ("opcache.enable_cli", "1"),
        ("opcache.file_cache_only", "1"),
        ("opcache.log_verbosity_level", "-1"),
    ])
    .serve_capture("<?php echo 'served';", Capture::default());
    assert_eq!(
        (
            silent.stdout_text().as_str(),
            &*String::from_utf8_lossy(&silent.stderr),
            silent.exit_code
        ),
        ("", "", -2)
    );
    let logged = engine(&[
        ("opcache.enable_cli", "1"),
        ("opcache.interned_strings_buffer", "500"),
    ])
    .serve_capture("<?php echo 'served';", Capture::default());
    let stderr = String::from_utf8_lossy(&logged.stderr);
    assert!(logged.stdout.is_empty(), "{stderr}");
    assert!(
        stderr.ends_with(
            "Fatal Error Insufficient shared memory for interned strings buffer! (tried to allocate 524288216 bytes)\n"
        ),
        "{stderr}"
    );
    let files = Files::new("opcache-logging", &[("held.inc", "<?php return 1;")]);
    let dir = files.path();
    let events = engine(&[
        ("opcache.enable_cli", "1"),
        ("opcache.file_update_protection", "0"),
        ("opcache.log_verbosity_level", "4"),
    ])
    .serve_capture_in(
        dir,
        b"events.php",
        "<?php include __DIR__ . '/held.inc'; opcache_reset();",
        Capture::default(),
    );
    let held = dir
        .join("held.inc")
        .canonicalize()
        .expect("the held file resolves");
    let stderr = String::from_utf8_lossy(&events.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .filter_map(|line| line.split_once("): ").map(|(_, logged)| logged))
        .collect();
    assert_eq!(
        lines,
        [
            format!("Message Cached script '{}'", held.display()).as_str(),
            "Debug Restart Scheduled! Reason: user",
        ]
    );
}
