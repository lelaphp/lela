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
    assert_eq!(served.stdout, b"bool(true)\n");
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
        .ini("session.use_trans_sid", "1")
        .build();
    let served = engine.serve_capture(
        "<?php var_dump(session_save_path(\"/tmp\"), session_save_path());",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "\nDeprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated in Unknown on line 0\nstring(0) \"\"\nstring(4) \"/tmp\"\n"
    );
}
