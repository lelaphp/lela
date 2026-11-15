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
    served.stdout_text()
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

/// The token names a source comes back with, whitespace dropped, a
/// single character as itself: the ids `var_dump` printed, named by
/// `token_name` through the same engine.
fn names(source: &str, flags: i64) -> String {
    let escaped = source.replace('\\', "\\\\").replace('\'', "\\'");
    let out = served(&format!(
        "<?php var_dump(token_get_all('{escaped}', {flags}));"
    ));
    let mut ids = Vec::new();
    let mut characters = Vec::new();
    let lines: Vec<&str> = out.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("  [") {
            if lines[i + 1].starts_with("  array(3)") {
                ids.push(lines[i + 3].trim().to_owned());
                characters.push(None);
                i += 4;
            } else {
                let text = lines[i + 1].trim();
                characters.push(Some(
                    text[text.find('"').unwrap_or(0) + 1..text.len() - 1].to_owned(),
                ));
                i += 2;
            }
        } else {
            i += 1;
        }
    }
    let named = served(&format!(
        "<?php var_dump({});",
        ids.iter()
            .map(|id| format!(
                "token_name({})",
                id.trim_start_matches("int(").trim_end_matches(')')
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let mut named = named.lines().map(|l| {
        l.trim_start_matches(|c: char| c != '"')
            .trim_matches('"')
            .to_owned()
    });
    characters
        .into_iter()
        .map(|character| character.unwrap_or_else(|| named.next().unwrap_or_default()))
        .filter(|name| name != "T_WHITESPACE")
        .collect::<Vec<_>>()
        .join(" ")
}

/// The rest of the sites php's grammar takes a keyword as a name at,
/// beside the two the witness pins: `static` fed back after `::` and as
/// a method's or a constant's name but not as a class reference; the
/// trait adaptations; a magic constant as a member and as a named
/// argument; an enum case, a namespace's name, `readonly` as a
/// function's name and as a call; and `callable` and `array` kept in
/// type position. Each line is what the pinned php printed for the same
/// source under `TOKEN_PARSE`, read once.
#[test]
fn the_parser_s_names_are_php_s() {
    let held = [
        (
            "<?php new static; static::x; A::static; class C { function static(){} const static = 1; }",
            "T_OPEN_TAG T_NEW T_STATIC ; T_STATIC T_DOUBLE_COLON T_STRING ; T_STRING T_DOUBLE_COLON T_STRING ; T_CLASS T_STRING { T_FUNCTION T_STRING ( ) { } T_CONST T_STRING = T_LNUMBER ; }",
        ),
        (
            "<?php class C { use T { list as foo; T::list insteadof U; list as public bar; } }",
            "T_OPEN_TAG T_CLASS T_STRING { T_USE T_STRING { T_STRING T_AS T_STRING ; T_STRING T_DOUBLE_COLON T_STRING T_INSTEADOF T_STRING ; T_STRING T_AS T_PUBLIC T_STRING ; } }",
        ),
        (
            "<?php class C { function __CLASS__(){} const __LINE__ = 1; } A::__CLASS__; f(__LINE__: 1);",
            "T_OPEN_TAG T_CLASS T_STRING { T_FUNCTION T_STRING ( ) { } T_CONST T_STRING = T_LNUMBER ; } T_STRING T_DOUBLE_COLON T_STRING ; T_STRING ( T_STRING : T_LNUMBER ) ;",
        ),
        (
            "<?php enum E { case list; } namespace fn; function readonly(){} readonly();",
            "T_OPEN_TAG T_ENUM T_STRING { T_CASE T_STRING ; } T_NAMESPACE T_STRING ; T_FUNCTION T_STRING ( ) { } T_STRING ( ) ;",
        ),
        (
            "<?php function f(): callable { return fn(callable $c): array => [$c]; } $x instanceof A;",
            "T_OPEN_TAG T_FUNCTION T_STRING ( ) : T_CALLABLE { T_RETURN T_FN ( T_CALLABLE T_VARIABLE ) : T_ARRAY T_DOUBLE_ARROW [ T_VARIABLE ] ; } T_VARIABLE T_INSTANCEOF T_STRING ;",
        ),
    ];
    for (source, expected) in held {
        assert_eq!(names(source, 1), expected, "{source}");
    }
}

/// Without `TOKEN_PARSE` the same sources keep the scanner's names,
/// which is the difference `TOKEN_PARSE` makes.
#[test]
fn without_the_flag_the_scanner_s_names_stand() {
    assert_eq!(
        names("<?php A::static; class C { const list = 1; } A::class;", 0),
        "T_OPEN_TAG T_STRING T_DOUBLE_COLON T_STATIC ; T_CLASS T_STRING { T_CONST T_LIST = T_LNUMBER ; } T_STRING T_DOUBLE_COLON T_CLASS ;"
    );
}

/// `PhpToken` through `serve` on an engine of this crate alone:
/// `tokenize` called on the class and `getTokenName` on an instance,
/// each a native method the engine reached through
/// `ExtensionBuilder::classes`. What the oracle printed for the
/// same call is beside each method in `src/lib.rs`; this holds the
/// calls on the crate's own engine, where the bundle is not linked.
#[test]
fn php_token_s_class_and_instance_methods_are_served() {
    assert_eq!(
        served("<?php var_dump(class_exists('PhpToken'), PhpToken::tokenize('1'));"),
        "bool(true)\narray(1) {\n  [0]=>\n  object(PhpToken)#1 (4) {\n    [\"id\"]=>\n    int(267)\n    [\"text\"]=>\n    string(1) \"1\"\n    [\"line\"]=>\n    int(1)\n    [\"pos\"]=>\n    int(0)\n  }\n}\n"
    );
    assert_eq!(
        served(
            "<?php $t = new PhpToken(T_ECHO, 'echo'); var_dump($t->getTokenName(), get_class($t), $t instanceof PhpToken, $t instanceof Stringable);"
        ),
        "string(6) \"T_ECHO\"\nstring(8) \"PhpToken\"\nbool(true)\nbool(true)\n"
    );
}
