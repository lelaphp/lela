//! Diagnostics: how a warning, a parse error and a fatal are displayed,
//! logged and handed to the sink under the ini entries.

use super::*;

use crate::api::request::Aside;

/// The warning php writes for `echo NAN;` on line 2 of the witness
/// regime's file, which the display-branch tests below stand on.
const NAN_WARNING: &str = "unexpected NAN value was coerced to string";

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
        observables.fatal(),
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
        observables.fatal(),
        None,
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
        observables.fatal(),
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
        observables.fatal().map(str::to_owned),
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
