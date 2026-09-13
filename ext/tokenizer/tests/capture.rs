//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle. What is here is
//! what a witness cannot carry: the bytes a fence has no spelling for,
//! and php's error text held without the oracle.

use php_ext::test::{Capture, Engine};

fn served(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_tokenizer::extension()])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

#[test]
fn the_extension_is_loaded() {
    assert_eq!(
        served("<?php var_dump(extension_loaded('tokenizer'));"),
        "bool(true)\n"
    );
}

/// php's `<ST_IN_SCRIPTING>{ANY_CHAR}` returns one `T_BAD_CHARACTER`
/// per byte; the lexer's one `ERROR` token over a stretch of them is cut
/// back into that. What the pinned php printed for the same call, read
/// once: two rows of `411` and the byte, both on line 1.
#[test]
fn a_bad_byte_is_one_token_each() {
    let out = served("<?php var_dump(token_get_all(\"<?php \\v\\f\"));");
    let expected = "array(3) {\n  [0]=>\n  array(3) {\n    [0]=>\n    int(394)\n    [1]=>\n    string(6) \"<?php \"\n    [2]=>\n    int(1)\n  }\n  [1]=>\n  array(3) {\n    [0]=>\n    int(411)\n    [1]=>\n    string(1) \"\u{b}\"\n    [2]=>\n    int(1)\n  }\n  [2]=>\n  array(3) {\n    [0]=>\n    int(411)\n    [1]=>\n    string(1) \"\u{c}\"\n    [2]=>\n    int(1)\n  }\n}\n";
    assert_eq!(out, expected);
}

/// `HANDLE_NEWLINES`: `\r\n` is one line and a lone `\r` is one too.
/// The pinned php gives `"\r\n<?php\r\n$x\r\n"` the lines 1, 2, 3, 3 and
/// `"<?php /*\r*/ $x"` the variable line 2.
#[test]
fn a_line_is_counted_as_handle_newlines_counts() {
    let out = served("<?php var_dump(token_get_all(\"\\r\\n<?php\\r\\n\\$x\\r\\n\"));");
    let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("    int(")).collect();
    assert_eq!(
        lines,
        [
            "    int(267)",
            "    int(1)",
            "    int(394)",
            "    int(2)",
            "    int(266)",
            "    int(3)",
            "    int(397)",
            "    int(3)"
        ]
    );
    let out = served("<?php var_dump(token_get_all(\"<?php /*\\r*/ \\$x\"));");
    let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("    int(")).collect();
    assert_eq!(
        lines,
        [
            "    int(394)",
            "    int(1)",
            "    int(392)",
            "    int(1)",
            "    int(397)",
            "    int(2)",
            "    int(266)",
            "    int(2)"
        ]
    );
}

/// `ZEND_PARSE_PARAMETERS_START(1, 2)` and `(1, 1)`: php's
/// `ArgumentCountError` text for a call outside either arity.
#[test]
fn the_arity_is_php_s() {
    let out = served("<?php token_get_all();");
    assert!(
        out.contains("token_get_all() expects at least 1 argument, 0 given"),
        "{out}"
    );
    let out = served("<?php token_get_all('a', 1, 2);");
    assert!(
        out.contains("token_get_all() expects at most 2 arguments, 3 given"),
        "{out}"
    );
    let out = served("<?php token_name();");
    assert!(
        out.contains("token_name() expects exactly 1 argument, 0 given"),
        "{out}"
    );
}
