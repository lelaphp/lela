//! The raises hold over sources written here, as
//! `crates/php_engine/src/tests/typed_errors.rs` held the workspace before
//! the scan moved into each crate's `api-check` reading.

use super::*;

/// A source outside php's sites, as one extension's file.
const ELSEWHERE: &str = "ext/pcre/src/lib.rs";

#[test]
fn a_raise_names_its_class_by_a_handle_never_by_a_name() {
    let named = "throw_exception_ex(req, lookup_class(req, b\"Exception\")?, \"m\");\n";
    assert_eq!(
        of_file(ELSEWHERE, named),
        ["ext/pcre/src/lib.rs: a raise names its class as lookup_class(req, b\"Exception\")?"]
    );
    let held = "throw_exception_ex(req, ce.exception, \"m\");\n";
    assert_eq!(of_file(ELSEWHERE, held), Vec::<String>::new());
}

#[test]
fn a_typed_class_is_thrown_through_its_typed_function() {
    let source = "throw_error(req, ClassId::TYPE_ERROR, \"m\");\n";
    assert_eq!(
        of_file(ELSEWHERE, source),
        ["ext/pcre/src/lib.rs: 1 typed throw_error calls, php's sites 0"]
    );
    assert_eq!(
        of_file("ext/dom/src/token_list.rs", source),
        Vec::<String>::new()
    );
    assert_eq!(
        of_file("ext/dom/src/token_list.rs", ""),
        ["ext/dom/src/token_list.rs: 0 typed throw_error calls, php's sites 1"]
    );
}

#[test]
fn an_error_class_is_thrown_through_zend_throw_exception_at_phps_sites_alone() {
    let source = "throw(req, ce.value_error, \"m\");\n";
    assert_eq!(
        of_file(ELSEWHERE, source),
        ["ext/pcre/src/lib.rs: 1 error-class throws, php's sites 0"]
    );
    assert_eq!(of_file("ext/ftp/src/lib.rs", source), Vec::<String>::new());
}

#[test]
fn the_class_argument_is_read_past_nested_calls_and_strings() {
    let source = r#"
        throw_error(req, ClassId::TYPE_ERROR, &format!("a, {b}", c(d, e)));
        // throw_error(req, ClassId::VALUE_ERROR, "a comment");
        php_ext::raising::throw_error_chained(req, prev, ce.argument_count_error, "x, y");
        my_throw_error(req, ce.value_error, "not this one");
        throw(req, "Exception", "by its name");
        throw_exception_ex(req, lookup_class(req, b"Exception")?.unwrap_or(ce.exception), "m");
        throw_error(req, "a crate's own helper of two arguments");
    "#;
    let classes = classes_of(source, &RAISERS);
    assert_eq!(
        classes,
        [
            "\"Exception\"",
            "ClassId::TYPE_ERROR",
            "lookup_class(req, b\"Exception\")?.unwrap_or(ce.exception)",
            "ce.argument_count_error",
        ]
    );
    assert_eq!(
        classes
            .iter()
            .map(|class| handle(class))
            .collect::<Vec<_>>(),
        [
            "\"exception\"",
            "type_error",
            "exception)",
            "argument_count_error"
        ]
    );
    assert_eq!(
        classes
            .iter()
            .map(|class| by_name(class))
            .collect::<Vec<_>>(),
        [true, false, true, false]
    );
}

/// `php_engine`'s reading names each raise of [`RAISERS`] it no longer
/// defines, so a rename there fails where the list must follow it; a
/// stand-in that defines none holds nothing.
#[test]
fn php_engine_fails_for_a_raise_it_no_longer_defines() {
    let root = crate::scratch::Tree::new("raises-engine").unwrap();
    // A crate's sources are the files git lists, so the tree is a
    // repository, as every scratch workspace is.
    crate::git::git(&root, &["init", "-q"]).unwrap();
    let dir = root.join("crates/php_engine");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let engine = Present {
        name: ENGINE.to_owned(),
        dir: dir.clone(),
        root: root.to_path_buf(),
        package: ENGINE.to_owned(),
        deps: Vec::new(),
        external: std::collections::BTreeSet::new(),
        inherits_lints: true,
    };
    std::fs::write(dir.join("src/lib.rs"), "pub fn execute() {}\n").unwrap();
    assert_eq!(hold(&root, &engine).unwrap().problems, Vec::<String>::new());
    let raising = RAISERS[1..]
        .iter()
        .map(|(call, _, _)| format!("pub fn {call}) {{}}\n"));
    std::fs::write(dir.join("src/lib.rs"), raising.collect::<Vec<_>>().concat()).unwrap();
    assert_eq!(
        hold(&root, &engine).unwrap().problems,
        ["php_engine defines no raise `throw`"]
    );
}
