//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

#[test]
fn the_extension_is_loaded() {
    let engine = Engine::builder()
        .extensions(&[php_ext_iconv::extension()])
        .build();
    let sink = Capture::default();
    let served = engine.serve_capture("<?php var_dump(extension_loaded('iconv'));", sink);
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

/// `ICONV_IMPL` names the `iconv(3)` linked as php's `configure` does,
/// and `ICONV_VERSION` is that library's own `major.minor`: the hold on a
/// host with no oracle for the witness, such as glibc's, where the pair
/// is `glibc` and `gnu_get_libc_version()`. `--ri`'s two rows read the
/// same `ffi::linked`, which the constants' witness holds on macOS.
#[test]
fn the_implementation_is_the_library_linked() {
    let linked = if cfg!(target_vendor = "apple") {
        "libiconv"
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        "glibc"
    } else {
        "unknown"
    };
    let engine = Engine::builder()
        .extensions(&[php_ext_iconv::extension()])
        .build();
    let served = engine.serve_capture(
        "<?php echo ICONV_IMPL, \"\\n\", ICONV_VERSION;",
        Capture::default(),
    );
    let out = served.stdout_text();
    let (implementation, version) = out.split_once('\n').unwrap_or_default();
    assert_eq!(implementation, linked, "{out}");
    let numbered = version.split('.').count() >= 2
        && version
            .split('.')
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    assert!(
        numbered || (linked == "unknown" && version == "unknown"),
        "{out}"
    );
}
