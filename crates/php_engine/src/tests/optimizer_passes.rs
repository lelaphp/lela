//! An extension's optimizer passes as the unit cache calls them
//! ([`ExtensionBuilder::optimizer_pass`]): where php's
//! `zend_optimize_script` calls `zend_optimizer_call_registered_passes`,
//! over each file an accelerator admitted and the cache compiles and
//! stores, in the order the passes were registered, and over nothing it
//! serves as stored or holds as php without opcache compiles.

use std::path::PathBuf;

use crate::api::abi::{Admission, ExtensionBuilder, FileCache, Lookup, Validation};
use crate::api::request::Request;
use crate::engine::Engine;
use crate::test::{Capture, Files};

/// The included file's name, past the directory, for a pass to print.
fn named(path: &[u8]) -> &[u8] {
    path.rsplit(|b| *b == b'/').next().unwrap_or(path)
}

fn first(req: &mut Request, path: &[u8]) {
    let line = [b"first ", named(path), b"\n"].concat();
    req.response_mut().write(&line);
}

fn second(req: &mut Request, path: &[u8]) {
    let line = [b"second ", named(path), b"\n"].concat();
    req.response_mut().write(&line);
}

fn third(req: &mut Request, path: &[u8]) {
    let line = [b"third ", named(path), b"\n"].concat();
    req.response_mut().write(&line);
}

/// What an accelerator answers for every file: held, never revalidated.
fn admitted(_: &mut Request, _: &[u8]) -> Admission {
    Admission::Held {
        validation: Validation::Never,
        lookup: Lookup::Resolved,
        protection: 0,
        file_cache: None,
    }
}

/// What the cache answers with no accelerator: held, validated on every
/// hit, as php without opcache compiles each file.
fn unadmitted(_: &mut Request, _: &[u8]) -> Admission {
    Admission::Held {
        validation: Validation::EveryHit,
        lookup: Lookup::Resolved,
        protection: 0,
        file_cache: None,
    }
}

/// The file cache [`on_disk`] keeps units in: the directory of
/// `Files::new("optimizer-passes-disk", ..)`, named here alone since a
/// function pointer's admission reads nothing a test hands it.
fn disk_directory() -> PathBuf {
    std::env::temp_dir().join(format!("dax-{}-optimizer-passes-disk", std::process::id()))
}

/// What an accelerator answers under `opcache.file_cache_only`: kept in
/// the file cache alone.
fn on_disk(_: &mut Request, _: &[u8]) -> Admission {
    Admission::OnDisk {
        validation: Validation::Never,
        protection: 0,
        file_cache: FileCache {
            directory: disk_directory(),
            read_only: false,
        },
    }
}

/// An extension whose admission is `admission`, with two passes, and one
/// after it with a third.
fn bundle(admission: fn(&mut Request, &[u8]) -> Admission) -> [crate::api::abi::Extension; 2] {
    let admitting = ExtensionBuilder::default()
        .admission(admission)
        .optimizer_pass(first)
        .optimizer_pass(second)
        .build("admitting");
    let after = ExtensionBuilder::default()
        .optimizer_pass(third)
        .build("after");
    [admitting, after]
}

/// What a program in `files` that includes `inc.php` twice wrote.
fn including(engine: &Engine, files: &Files) -> String {
    files.write("inc.php", "<?php echo \"inc\\n\";");
    let source = "<?php include __DIR__ . '/inc.php'; include __DIR__ . '/inc.php';";
    files.write("main.php", source);
    let main = files.path().join("main.php");
    engine
        .serve_capture_in(
            files.path(),
            main.to_string_lossy().as_bytes(),
            source,
            Capture::default(),
        )
        .stdout_text()
}

/// A file the cache compiles and holds is handed to every pass once,
/// in registration order across the bundle, before it is served; the second
/// include and the next request's are hits, which no pass sees.
#[test]
fn every_pass_runs_once_over_a_file_the_cache_compiles_and_holds() {
    let engine = Engine::cli_builder().extensions(bundle(admitted)).build();
    let files = Files::new("optimizer-passes-held", &[]);
    assert_eq!(
        including(&engine, &files),
        "first inc.php\nsecond inc.php\nthird inc.php\ninc\ninc\n"
    );
    assert_eq!(including(&engine, &files), "inc\ninc\n");
}

/// A file validated on every hit is one no accelerator admitted, held as
/// php without opcache compiles it, so no pass is handed it.
#[test]
fn no_pass_runs_where_no_accelerator_admitted_the_file() {
    let engine = Engine::cli_builder().extensions(bundle(unadmitted)).build();
    let files = Files::new("optimizer-passes-unadmitted", &[]);
    assert_eq!(including(&engine, &files), "inc\ninc\n");
}

/// Under `opcache.file_cache_only` a file compiled and kept in the file
/// cache is handed to every pass, `cache_script_in_file_cache`; one
/// read back from it is not.
#[test]
fn every_pass_runs_over_a_file_kept_in_the_file_cache_alone() {
    let engine = Engine::cli_builder().extensions(bundle(on_disk)).build();
    let files = Files::new("optimizer-passes-kept", &[]);
    let disk = Files::new("optimizer-passes-disk", &[]);
    assert_eq!(disk.path(), disk_directory());
    assert_eq!(
        including(&engine, &files),
        "first inc.php\nsecond inc.php\nthird inc.php\ninc\ninc\n"
    );
    assert_eq!(including(&engine, &files), "inc\ninc\n");
}
