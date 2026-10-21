//! Under the `C.UTF-8` a request starts in, a byte past 0x7F is classed as
//! the C library classes it: macOS's counts `\xE9` a letter, `\xA0`
//! printable and a space, and `\xC9` upper case, as the macOS oracle does,
//! where glibc's classes no such byte. A fence would pin one platform's
//! reading, so the macOS one is held here (#2816, as #2796 held
//! `is_space`'s in `php_process::locale`'s test).

#[cfg(target_os = "macos")]
#[test]
fn macos_c_utf8_classes_bytes_past_0x7f() {
    use php_ext::test::{Capture, Engine};

    let engine = Engine::builder()
        .extensions(&[php_ext_ctype::extension()])
        .build();
    let served = engine.serve_capture(
        r#"<?php var_dump(ctype_alpha("\xE9"), ctype_print("\xA0"), ctype_upper("\xC9"), ctype_space("\xA0"));"#,
        Capture::default(),
    );
    assert_eq!(
        served.stdout_text(),
        "bool(true)\nbool(true)\nbool(true)\nbool(true)\n"
    );
}
