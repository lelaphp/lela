//! The lane from inside: the verdict on two renderings, the negative
//! control under `xtask/fixtures/crosscheck/`, and which extensions a run
//! holds and which engine serves it. What a run writes is held from outside, in
//! `tests/crosscheck.rs`, through `run`. Nothing here spawns php.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::common::registry::registry;
use crate::manifest::Crate;

fn fixtures() -> PathBuf {
    crate::package_dir().join("fixtures/crosscheck")
}

#[test]
fn the_fixture_is_the_oracles_own_rendering_and_the_one_byte_apart_is_not_a_pass() {
    let oracle = fs::read(fixtures().join("oracle.re")).unwrap();
    let engine = fs::read(fixtures().join("engine.re")).unwrap();
    assert_eq!(
        oracle.len(),
        engine.len(),
        "the fixture differs by a byte, not a length"
    );
    assert_eq!(
        judge(
            &registry(""),
            &Host::Cli.id("ctype", Column::Re),
            &oracle,
            &Rendering::Bytes(oracle.clone())
        ),
        Judgement::Pass
    );
    let Judgement::Fail(note) = judge(
        &registry(""),
        &Host::Cli.id("ctype", Column::Re),
        &oracle,
        &Rendering::Bytes(engine),
    ) else {
        panic!("one byte apart is a pass")
    };
    assert!(
        note.starts_with(
            "the engine's rendering differs from the oracle's at line 4, 2 lines in all:\n"
        ),
        "{note}"
    );
    assert!(
        note.contains("-    Function [ <internal:ctype> function ctype_alnum ] {")
            && note.contains("+    Function [ <internal:ctype> function ctype_alnun ] {"),
        "{note}"
    );
}

/// The note names the first differing line and the hunk there, not the
/// whole diff: a line the oracle has and the engine lacks is `-` alone,
/// and the count is over every changed line, later hunks included.
#[test]
fn the_note_is_the_first_differing_line_and_the_count_of_all_of_them() {
    let diff = " same\n same\n-oracle only\n same\n-theirs\n+ours\n";
    assert_eq!(
        first_difference(diff),
        "the engine's rendering differs from the oracle's at line 3, 3 lines in all:\n-oracle only"
    );
    let added = " same\n+engine only\n same\n";
    assert_eq!(
        first_difference(added),
        "the engine's rendering differs from the oracle's at line 2, 1 line in all:\n+engine only"
    );
    assert!(first_difference(" same\n").contains("no line shows"));
    let long = " head\n-a\n-b\n-c\n-d\n-e\n-f\n same\n";
    assert_eq!(
        first_difference(long),
        "the engine's rendering differs from the oracle's at line 2, 6 lines in all:\n-a\n-b\n-c\n-d\n… and 2 more lines of this hunk"
    );
}

/// The engine's side is php's own call: `echo` over a `ReflectionExtension`
/// is `__toString()`, what `php --re` prints, and `info()` is
/// `php_info_print_module`, what `php --ri` prints. A name is a PHP
/// string literal whatever it holds.
#[test]
fn the_program_is_echo_over_a_reflection_extension_and_the_name_is_a_literal() {
    assert_eq!(
        Column::Re.program("ctype"),
        "echo new ReflectionExtension('ctype');"
    );
    assert_eq!(
        Column::Ri.program("ctype"),
        "(new ReflectionExtension('ctype'))->info();"
    );
    assert_eq!(
        Column::Re.program("it's\\"),
        "echo new ReflectionExtension('it\\'s\\\\');"
    );
    assert_eq!(
        Column::Ri.program("it's\\"),
        "(new ReflectionExtension('it\\'s\\\\'))->info();"
    );
}

/// An extension php registers under another name than its crate's is
/// asked for by php's name on both sides, the oracle's flag and the
/// engine's reflector, while its reading keeps the crate's scope:
/// `ext/opcache` is `Zend OPcache` (run-tests.php:2908-2916), and the
/// same table lowers `-m`'s listing back to the crate for `--EXTENSIONS--`.
#[test]
fn a_crate_php_names_otherwise_is_rendered_under_the_module_name() {
    assert_eq!(
        Column::Re.program("opcache"),
        "echo new ReflectionExtension('Zend OPcache');"
    );
    assert_eq!(Column::Ri.scope("opcache"), "ext/opcache/ri");
    assert_eq!(crate::oracle::module_name("ctype"), "ctype");
    assert_eq!(crate::oracle::crate_name("Zend OPcache"), "opcache");
    assert_eq!(crate::oracle::crate_name("SPL"), "spl");
}

/// The `--ri` column (#273): php's own `--ri ctype` standing for the
/// engine's is a pass, and the one a byte apart, `Enabled` for `enabled`
/// on line 3, is not; the fixture carries no load number to renumber.
#[test]
fn the_ri_fixture_is_the_oracles_own_rendering_and_the_one_byte_apart_is_not_a_pass() {
    let oracle = fs::read(fixtures().join("oracle.ri")).unwrap();
    let engine = fs::read(fixtures().join("engine.ri")).unwrap();
    assert_eq!(oracle.len(), engine.len(), "one byte, not a length");
    assert_eq!(
        oracle, b"ctype\n\nctype functions => enabled",
        "php_info_print_module in text mode, trimmed"
    );
    assert_eq!(
        judge(
            &registry(""),
            &Host::Cli.id("ctype", Column::Ri),
            &oracle,
            &Rendering::Bytes(oracle.clone())
        ),
        Judgement::Pass
    );
    let Judgement::Fail(note) = judge(
        &registry(""),
        &Host::Cli.id("ctype", Column::Ri),
        &oracle,
        &Rendering::Bytes(engine),
    ) else {
        panic!("one byte apart is a pass")
    };
    assert!(
        note.starts_with(
            "the engine's rendering differs from the oracle's at line 3, 2 lines in all:\n"
        ) && note.contains("-ctype functions => enabled")
            && note.contains("+ctype functions => Enabled"),
        "{note}"
    );
}

/// Each column has its flag, its kept extension, its scope and its
/// divergence key: the `--re` reading keeps the crate's name, the scope
/// `done` holds a default extension to, and the `--ri` one stands beside
/// it under `/ri`; an accepted row for one column accepts nothing for the
/// other.
#[test]
fn the_two_columns_have_their_own_flag_scope_and_key() {
    assert_eq!(Column::ALL, [Column::Re, Column::Ri]);
    assert_eq!(Column::Re.flag(), "--re");
    assert_eq!(Column::Ri.flag(), "--ri");
    assert_eq!(Column::Re.kept(), "re");
    assert_eq!(Column::Ri.kept(), "ri");
    assert_eq!(Column::Re.scope("ctype"), "ext/ctype");
    assert_eq!(Column::Ri.scope("ctype"), "ext/ctype/ri");
    assert_eq!(Host::Cli.id("ctype", Column::Re), "crosscheck:ctype");
    assert_eq!(Host::Cli.id("ctype", Column::Ri), "crosscheck:ctype/ri");
    let oracle = fs::read(fixtures().join("oracle.ri")).unwrap();
    let engine = fs::read(fixtures().join("engine.ri")).unwrap();
    let accepted = registry(
        r#"
[[divergence]]
id = "crosscheck:ctype"
kind = "crosscheck"
reason = "the --re row (fixture)."
status = "accepted"
since = "2026-09-18"
"#,
    );
    assert!(
        matches!(
            judge(
                &accepted,
                &Host::Cli.id("ctype", Column::Ri),
                &oracle,
                &Rendering::Bytes(engine)
            ),
            Judgement::Fail(_)
        ),
        "a row for the --re column accepts nothing for --ri"
    );
}

/// The load number (#317): the rendering with `#10` standing as `#7` is
/// the oracle's own under the pin's `extension_number_token` row, since
/// both streams go through [`oracle::normalise`] before the verdict, and
/// the same bytes against a pin without the row are not.
#[test]
fn the_renumbered_fixture_is_a_pass_under_the_row_and_not_without_it() {
    let oracle = fs::read(fixtures().join("oracle.re")).unwrap();
    let engine = fs::read(fixtures().join("renumbered.re")).unwrap();
    assert!(
        oracle.starts_with(b"Extension [ <persistent> extension #10 ctype")
            && engine.starts_with(b"Extension [ <persistent> extension #7 ctype"),
        "the fixture differs by its load number"
    );
    let with_row = Manifest::load(crate::package_dir().parent().unwrap())
        .unwrap()
        .pin
        .recipe
        .normalise;
    assert!(
        with_row.extension_number_token.is_some(),
        "the pin carries the row"
    );
    let mut without_row = with_row.clone();
    without_row.extension_number_token = None;

    let under = |rules: &oracle::Normalise| {
        judge(
            &registry(""),
            &Host::Cli.id("ctype", Column::Re),
            &oracle::normalise(rules, oracle.clone()),
            &Rendering::Bytes(oracle::normalise(rules, engine.clone())),
        )
    };
    assert_eq!(under(&with_row), Judgement::Pass);
    let Judgement::Fail(note) = under(&without_row) else {
        panic!("the load number is compared without the row")
    };
    assert!(
        note.contains("-Extension [ <persistent> extension #10 ctype")
            && note.contains("+Extension [ <persistent> extension #7 ctype"),
        "{note}"
    );
}

/// The registry's `crosscheck` kind (#318): a difference an accepted row
/// names under `crosscheck:<name>` is `Diverged` with the row's reason and
/// a pass; the same row with status `open`, or a row for another
/// extension, accepts nothing.
#[test]
fn a_difference_an_accepted_row_names_is_diverged_and_a_pass() {
    let oracle = fs::read(fixtures().join("oracle.re")).unwrap();
    let engine = fs::read(fixtures().join("engine.re")).unwrap();
    let row = |status: &str| {
        format!(
            r#"
[[divergence]]
id = "crosscheck:ctype"
kind = "crosscheck"
reason = "ctype_alnun is the fixture's one byte (fixture)."
status = "{status}"
since = "2026-09-18"
"#
        )
    };
    let accepted = registry(&row("accepted"));
    let judged = judge(
        &accepted,
        &Host::Cli.id("ctype", Column::Re),
        &oracle,
        &Rendering::Bytes(engine.clone()),
    );
    assert_eq!(
        judged,
        Judgement::Diverged("ctype_alnun is the fixture's one byte (fixture).".to_owned())
    );
    assert_eq!(judged.status(), Status::Pass);
    assert_eq!(
        judge(
            &accepted,
            &Host::Cli.id("ctype", Column::Re),
            &oracle,
            &Rendering::Bytes(oracle.clone())
        ),
        Judgement::Pass,
        "the same bytes are a pass, row or no row"
    );
    assert!(
        matches!(
            judge(
                &accepted,
                &Host::Cli.id("hash", Column::Re),
                &oracle,
                &Rendering::Bytes(engine.clone())
            ),
            Judgement::Fail(_)
        ),
        "a row for ctype accepts nothing for hash"
    );
    assert!(
        matches!(
            judge(
                &registry(&row("open")),
                &Host::Cli.id("ctype", Column::Re),
                &oracle,
                &Rendering::Bytes(engine)
            ),
            Judgement::Fail(_)
        ),
        "an open row accepts nothing"
    );
}

#[test]
fn an_absent_rendering_fails_saying_the_engine_rendered_nothing() {
    let judged = judge(
        &registry(""),
        &Host::Cli.id("ctype", Column::Re),
        b"x",
        &Rendering::Absent("not built".to_owned()),
    );
    assert_eq!(judged.status(), Status::Fail);
    let Judgement::Fail(note) = judged else {
        panic!("{judged:?}")
    };
    assert_eq!(note, "the engine rendered nothing: not built");
}

/// The set a bare run renders is every `ext/*` row the binary under test
/// carries: the default set and the rows behind a feature it turns on,
/// never one behind a feature it does not (harness §7, revision 63).
#[test]
fn the_default_set_is_every_ext_row_the_binary_carries_and_ext_names_one() {
    let manifest = Manifest::of([
        Crate::named("php_heap"),
        Crate::named("ext/ctype"),
        Crate::named("ext/mbstring").with_feature("mbstring"),
        Crate::named("ext/hash"),
        Crate::named("ext/bcmath").with_feature("bcmath"),
    ]);
    let carried = Carried::turning_on(&["bcmath"]);
    assert_eq!(
        extensions(&manifest, &carried, &Ask::default()),
        ["ctype", "hash", "bcmath"]
    );
    let one = Ask {
        ext: vec!["mbstring".to_owned()],
        ..Ask::default()
    };
    assert_eq!(extensions(&manifest, &carried, &one), ["mbstring"]);
}

#[test]
fn the_oracles_bytes_are_kept_under_the_scope_and_the_digest() {
    let store = Store::at("/store");
    assert_eq!(
        kept(&store, "abc", "ctype", Column::Re),
        PathBuf::from("/store/crosscheck/ext/ctype/abc.re")
    );
    assert_eq!(
        kept(&store, "abc", "ctype", Column::Ri),
        PathBuf::from("/store/crosscheck/ext/ctype/abc.ri")
    );
}

/// The negative control for #1479: the linked engine, built with no host,
/// renders `standard` without the natives the CLI registers under it, so a
/// run holding that rendering to the oracle's `php --re standard` fails,
/// and the lane serves `standard` through the `php` binary instead; an
/// extension no host adds to stays the linked engine's.
#[test]
fn standard_is_the_binarys_since_the_linked_engine_lacks_a_hosts_natives() {
    let recipe = Manifest::load(&crate::built_root()).unwrap().pin.recipe;
    let served = crate::oracle::linked::serve(&recipe, None, &Column::Re.program(HOSTED)).unwrap();
    let rendered = String::from_utf8_lossy(&served.stdout);
    assert!(rendered.contains("function str_repeat ]"), "{rendered}");
    for native in ["dl", "cli_set_process_title", "cli_get_process_title"] {
        assert!(
            !rendered.contains(&format!("function {native} ]")),
            "{native}"
        );
    }
    let manifest = Manifest::of([Crate::named("ext/standard"), Crate::named("ext/ctype")]);
    let carried = Carried::turning_on(&[]);
    assert!(!served_linked(&manifest, &carried, HOSTED));
    assert!(served_linked(&manifest, &carried, "ctype"));
}

/// A host's own module is rendered by that host on both sides (#1496):
/// `cgi-fcgi` by php-cgi and by php-fpm, `cli_server` by the built-in
/// server, every other module by the CLI's flags; each host's readings
/// stand under its row with the module beneath, so php-cgi's and
/// php-fpm's `cgi-fcgi` are two pairs and neither is an `ext/*` scope.
#[test]
fn a_hosts_own_module_is_served_by_that_host_and_scoped_under_its_row() {
    assert_eq!(host::hosts("cgi-fcgi"), [Host::Cgi, Host::Fpm]);
    assert_eq!(host::hosts("cli_server"), [Host::Server]);
    assert_eq!(host::hosts("ctype"), [Host::Cli]);
    assert_eq!(host::hosts("standard"), [Host::Cli]);
    assert_eq!(Host::Cli.scope("ctype", Column::Re), "ext/ctype");
    assert_eq!(
        Host::Cgi.scope("cgi-fcgi", Column::Ri),
        "sapi/cgi/cgi-fcgi/ri"
    );
    assert_eq!(Host::Fpm.scope("cgi-fcgi", Column::Re), "sapi/fpm/cgi-fcgi");
    assert_eq!(
        Host::Server.id("cli_server", Column::Re),
        "crosscheck:sapi/cli/cli_server"
    );
    let store = Store::at("/store");
    assert_eq!(
        kept_at(
            &store,
            "abc",
            &Host::Cgi.scope("cgi-fcgi", Column::Re),
            Column::Ri
        ),
        PathBuf::from("/store/crosscheck/sapi/cgi/cgi-fcgi/abc.ri")
    );
}

/// A CGI-shaped host's head is the host's, and only a head of header
/// lines is let go: a rendering that opens otherwise is held whole.
#[test]
fn the_head_a_host_writes_is_let_go_and_a_body_alone_is_kept() {
    let rules = Manifest::load(&crate::built_root())
        .unwrap()
        .pin
        .recipe
        .normalise;
    let served = b"X-Powered-By: PHP\r\nContent-type: text/html; charset=UTF-8\r\n\r\nExtension [ x ] {\n\n}\n";
    assert_eq!(
        host::without_head(&rules, served.to_vec()),
        b"Extension [ x ] {\n\n}"
    );
    let bare = b"Extension [ x ] {\n\n  - INI {\n}\n";
    assert_eq!(
        host::without_head(&rules, bare.to_vec()),
        b"Extension [ x ] {\n\n  - INI {\n}"
    );
}

/// Both columns of each of `names` pass over the tree as it is on disk.
fn rendered(tree: &Path, manifest: &Manifest, store: &Store, names: &[&str]) {
    for name in names {
        for column in Column::ALL {
            let mut reading = reading_of(
                &crate::key::Snapshot::disk(tree).unwrap(),
                manifest,
                name,
                Host::Cli,
                column,
            )
            .unwrap();
            reading.status = Status::Pass;
            store.record(&[reading]).unwrap();
        }
    }
}

/// Write `text` at `path` under the tree and stage it.
fn staging(tree: &Path, path: &str, text: &str) {
    let at = tree.join(path);
    fs::create_dir_all(at.parent().unwrap()).unwrap();
    fs::write(&at, text).unwrap();
    crate::git::git(tree, &["add", "--", path], None).unwrap();
}

/// `crosscheck --staged` over the closure fixture of #1318, both
/// extensions rendered and passing (#1159): a staged xtask file renders
/// nothing; a staged change to `ext/ctype` alone renders ctype alone, and
/// once its readings are taken over what is staged, nothing; a staged
/// `php_engine` change, in every extension's closure, renders every one.
#[test]
fn a_staged_change_renders_again_the_extensions_whose_rendering_reads_it() {
    let tree = crate::key::fixture("crosscheck-staged");
    let manifest = Manifest::load(&crate::built_root()).unwrap();
    let store = Store::at(tree.join("target/harness"));
    let carried = ["ctype".to_owned(), "reflection".to_owned()];
    rendered(&tree, &manifest, &store, &["ctype", "reflection"]);
    let again = || staged(&tree, &manifest, &store, &carried).unwrap();

    staging(&tree, "xtask/src/lib.rs", "pub fn harness() {}\n");
    assert!(again().is_empty(), "{:?}", again());

    staging(&tree, "ext/ctype/src/lib.rs", "pub fn ctype_digit() {}\n");
    assert_eq!(again(), ["ctype"]);
    rendered(&tree, &manifest, &store, &["ctype"]);
    assert!(again().is_empty(), "{:?}", again());

    staging(
        &tree,
        "crates/php_engine/src/lib.rs",
        "pub fn execute() { }\n",
    );
    assert_eq!(again(), carried);
}

/// A staged change to `ext/reflection`, whose `ReflectionExtension`
/// prints every extension's `--re` and is in no other extension's
/// closure, renders ctype again though ctype's readings were taken over
/// what is staged, since its key does not move with reflection
/// ([`Reads::beyond`]); reflection's own key does, and its readings over
/// what is staged stand.
#[test]
fn a_staged_change_beyond_the_key_renders_again_though_the_reading_stands() {
    let tree = crate::key::fixture("crosscheck-beyond");
    let manifest = Manifest::load(&crate::built_root()).unwrap();
    let store = Store::at(tree.join("target/harness"));
    let carried = ["ctype".to_owned(), "reflection".to_owned()];
    staging(&tree, "ext/reflection/src/lib.rs", "pub fn export() {}\n");
    rendered(&tree, &manifest, &store, &["ctype", "reflection"]);
    let graph = crate::key::Graph::of(&tree).unwrap();
    let reads = Reads::of(
        &graph,
        &manifest,
        &Carried::linked(&tree).unwrap(),
        "ctype",
        Host::Cli,
    );
    assert!(
        reads.beyond.contains(&PathBuf::from("ext/reflection")),
        "{reads:?}"
    );
    assert!(
        !reads.keyed.contains(&PathBuf::from("ext/reflection")),
        "{reads:?}"
    );
    assert_eq!(
        staged(&tree, &manifest, &store, &carried).unwrap(),
        ["ctype"]
    );
}

/// A staged `docs/divergence.toml` with nothing else staged renders every
/// carried extension again, though both pass over what is staged: an
/// accepted `crosscheck:` row turns a rendering's fail into a pass, and
/// the file is under no crate's key ([`sources::Outside::Every`], #1738).
#[test]
fn a_staged_file_every_reading_reads_renders_every_extension_again() {
    let tree = crate::key::fixture("crosscheck-every");
    let manifest = Manifest::load(&crate::built_root()).unwrap();
    let store = Store::at(tree.join("target/harness"));
    let carried = ["ctype".to_owned(), "reflection".to_owned()];
    rendered(&tree, &manifest, &store, &["ctype", "reflection"]);
    let again = || staged(&tree, &manifest, &store, &carried).unwrap();
    assert!(again().is_empty(), "{:?}", again());

    staging(&tree, crate::divergence::FILE, "");
    assert_eq!(again(), carried);
}

/// A staged `Cargo.lock` that bumps a dependency only `ext/ctype`'s
/// closure reaches renders ctype again and leaves reflection, whose
/// closure the lock pins as `HEAD` does, unrendered (#1738).
#[test]
fn a_staged_lock_renders_again_only_the_extensions_whose_closure_it_moves() {
    let tree = crate::key::fixture("crosscheck-lock");
    let manifest = Manifest::load(&crate::built_root()).unwrap();
    let store = Store::at(tree.join("target/harness"));
    let carried = ["ctype".to_owned(), "reflection".to_owned()];
    let lock = "version = 4\n\n\
        [[package]]\nname = \"php_ext_ctype\"\nversion = \"0.0.0\"\ndependencies = [\n \"serde\",\n]\n\n\
        [[package]]\nname = \"php_ext_reflection\"\nversion = \"0.0.0\"\n\n\
        [[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+x\"\nchecksum = \"aa\"\n";
    staging(&tree, crate::lock::FILE, lock);
    crate::scratch::committing(&tree, &["commit", "-q", "-m", "lock"]);
    rendered(&tree, &manifest, &store, &["ctype", "reflection"]);
    let again = || staged(&tree, &manifest, &store, &carried).unwrap();
    assert!(again().is_empty(), "{:?}", again());

    let bumped = lock.replace("checksum = \"aa\"", "checksum = \"bb\"");
    staging(&tree, crate::lock::FILE, &bumped);
    assert_eq!(again(), ["ctype"]);
}
