//! What a host hands the engine and what the engine hands back: the
//! request description, the host's ini and modules, its sinks.

use super::*;

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
