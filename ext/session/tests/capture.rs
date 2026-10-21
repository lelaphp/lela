//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_session::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('session'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// A deprecation `OnUpdateTransSid` raises over a startup value
/// (`-d session.use_trans_sid=1`) is displayed as `MINIT` displays it,
/// through `php_output_direct` before the `php_output` layer is active, so it
/// sends no header: `SG(headers_sent)` is still unset when the program
/// runs, and `session_save_path()` takes the path, as the oracle does
/// under the same `-d`. A witness sets no startup ini, so the fence
/// cannot hold this.
#[test]
fn a_startup_deprecation_sends_no_header() {
    let engine = Engine::builder()
        .extensions(&[php_ext_session::extension()])
        .ini("html_errors", "0")
        .ini("session.use_trans_sid", "1")
        .build();
    let served = engine.serve_capture(
        "<?php var_dump(session_save_path(\"/tmp\"), session_save_path());",
        Capture::default(),
    );
    assert_eq!(
        served.stdout_text(),
        "\nDeprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated in Unknown on line 0\nstring(0) \"\"\nstring(4) \"/tmp\"\n"
    );
}

/// `zend_register_ini_entries_ex` meets the host's values in the order
/// session.c's `PHP_INI_BEGIN()` declares the entries, not the order the
/// host set them: `use_only_cookies` before `use_trans_sid` before
/// `sid_length`, as the oracle raises them under the same three `-d`
/// given the other way round.
#[test]
fn the_startup_rules_meet_the_values_in_declaration_order() {
    let engine = Engine::builder()
        .extensions(&[php_ext_session::extension()])
        .ini("html_errors", "0")
        .ini("session.sid_length", "40")
        .ini("session.use_trans_sid", "1")
        .ini("session.use_only_cookies", "0")
        .build();
    let served = engine.serve_capture("<?php echo \"ok\\n\";", Capture::default());
    assert_eq!(
        served.stdout_text(),
        "\nDeprecated: PHP Startup: Disabling session.use_only_cookies INI setting is deprecated in Unknown on line 0\n\nDeprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated in Unknown on line 0\n\nDeprecated: PHP Startup: session.sid_length INI setting is deprecated in Unknown on line 0\nok\n"
    );
}

/// `OnUpdateRfc1867Freq` over a `ZEND_INI_PERDIR` entry's startup value:
/// php's `PHP Startup` warning and the default kept, as the oracle
/// answers under `-d session.upload_progress.freq=-1` and `=200%`. No
/// regime of the pin sets it, so the fence cannot hold this.
#[test]
fn a_startup_upload_progress_freq_out_of_range_keeps_the_default() {
    for (value, bound) in [
        ("-1", "greater than or equal to 0"),
        ("200%", "less than or equal to 100%"),
    ] {
        let engine = Engine::builder()
            .extensions(&[php_ext_session::extension()])
            .ini("html_errors", "0")
            .ini("session.upload_progress.freq", value)
            .build();
        let served = engine.serve_capture(
            "<?php var_dump(ini_get(\"session.upload_progress.freq\"));",
            Capture::default(),
        );
        assert_eq!(
            served.stdout_text(),
            format!(
                "\nWarning: PHP Startup: session.upload_progress.freq must be {bound} in Unknown on line 0\nstring(2) \"1%\"\n"
            )
        );
    }
}
