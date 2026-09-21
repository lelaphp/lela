//! `TokenValue::of` on the forms the linked engine cannot print yet, each
//! held to what the pinned php printed for the same token under
//! `serialize_precision=-1` or `printf("%.17g")` (probed 8.5.9 on
//! 2026-09-21); the witnesses in `value.rs` hold the rest through the
//! engine.

use php_syntax::tree::{
    HeredocExpr, InterpolatedStringExpr, LiteralExpr, NowdocExpr, ShellExecExpr, StringPart,
};
use php_syntax::{Holder, Options, TokenValue, parse};

/// The value of every `LiteralExpr` in `src`, in source order.
fn values(src: &str) -> Vec<TokenValue> {
    let parsed = parse(src.as_bytes(), &Options::default());
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let mut nodes: Vec<&LiteralExpr> = parsed.tree.nodes::<LiteralExpr>().iter().collect();
    nodes.sort_by_key(|n| n.range.start);
    nodes
        .iter()
        .map(|node| {
            let token = node.value.unwrap();
            TokenValue::of(&parsed, Holder::Scalar(node), token).unwrap()
        })
        .collect()
}

/// The bytes of every text part of the one string in `src`,
/// whatever holds it, in source order.
fn parts(src: &str) -> Vec<Vec<u8>> {
    let parsed = parse(src.as_bytes(), &Options::default());
    let bytes = |v: Option<TokenValue>| match v {
        Some(TokenValue::Bytes(b)) => b,
        other => panic!("not bytes: {other:?}"),
    };
    // The text parts; an interpolation between them is not one.
    let text = |part: &StringPart| match part {
        StringPart::LiteralStringPart(id) => parsed.tree[*id].text,
        _ => None,
    };
    if let Some(node) = parsed.tree.nodes::<InterpolatedStringExpr>().first() {
        return node
            .parts
            .iter()
            .filter_map(|p| text(p).map(|t| TokenValue::of(&parsed, Holder::DoubleQuoted(node), t)))
            .map(bytes)
            .collect();
    }
    if let Some(node) = parsed.tree.nodes::<ShellExecExpr>().first() {
        return node
            .parts
            .iter()
            .filter_map(|p| text(p).map(|t| TokenValue::of(&parsed, Holder::ShellExec(node), t)))
            .map(bytes)
            .collect();
    }
    if let Some(node) = parsed.tree.nodes::<HeredocExpr>().first() {
        return node
            .parts
            .iter()
            .filter_map(|p| text(p).map(|t| TokenValue::of(&parsed, Holder::Heredoc(node), t)))
            .map(bytes)
            .collect();
    }
    if let Some(node) = parsed.tree.nodes::<NowdocExpr>().first() {
        return node
            .text
            .iter()
            .map(|t| bytes(TokenValue::of(&parsed, Holder::Nowdoc(node), *t)))
            .collect();
    }
    panic!("no string in {src}");
}

#[test]
fn the_prefixed_radices_and_their_retry_to_a_double() {
    use TokenValue::{Float, Int};
    // var_dump: int(31) int(15) int(15) int(5) int(63) int(255)
    assert_eq!(
        values("<?php 0x1F; 0o17; 017; 0b101; 0_7_7; 0xf_f;"),
        vec![Int(31), Int(15), Int(15), Int(5), Int(63), Int(255)]
    );
    // The last long and the first double in each radix: %.17g
    // 9.2233720368547758e+18 for hex, decimal and both octal
    // spellings, and 9.2233720368547748e+18 for binary, php's
    // `value * 2 + c - '0'` falling short of 2^63 by an ulp.
    assert_eq!(
        values(
            "<?php 0x7FFFFFFFFFFFFFFF; 0x8000000000000000; 0777777777777777777777; 01000000000000000000000; 0o777777777777777777777; 0o1000000000000000000000; 9223372036854775807; 9223372036854775808;"
        ),
        vec![
            Int(i64::MAX),
            Float(2f64.powi(63)),
            Int(i64::MAX),
            Float(2f64.powi(63)),
            Int(i64::MAX),
            Float(2f64.powi(63)),
            Int(i64::MAX),
            Float(2f64.powi(63)),
        ]
    );
    assert_eq!(
        values(
            "<?php 0b111111111111111111111111111111111111111111111111111111111111111; 0b1000000000000000000000000000000000000000000000000000000000000000;"
        ),
        vec![Int(i64::MAX), Float(2f64.powi(63) - 1024.0)]
    );
    // Past 53 bits the accumulation is observable: %.17g
    // 3.6893488147419103e+19, 4.7223664828696452e+21,
    // 8.5968058283706963e+22, 1.844674407370955e+19,
    // 7.3786976294838206e+19, 1.6168958788107255e+27, 1e23.
    assert_eq!(
        values(
            "<?php 0x1FFFFFFFFFFFFFFFF; 0xFFFFFFFFFFFFFFFFFF; 0x123456789ABCDEF12345; 0b1111111111111111111111111111111111111111111111111111111111111111; 0o7777777777777777777777; 01234567012345670123456701234567; 99999999999999999999999;"
        ),
        vec![
            Float(3.689_348_814_741_910_3e19),
            Float(4.722_366_482_869_645e21),
            Float(8.596_805_828_370_696e22),
            Float(1.844_674_407_370_955e19),
            Float(7.378_697_629_483_821e19),
            Float(1.616_895_878_810_725_5e27),
            Float(1e23),
        ]
    );
}

#[test]
fn the_b_prefix_and_the_upper_case_hex_escape() {
    // var_dump: string(1) "e", string(2) "JJ"
    assert_eq!(
        values("<?php b'e'; B'e';"),
        vec![TokenValue::Bytes(b"e".to_vec()); 2]
    );
    assert_eq!(parts(r#"<?php "\X4aJ";"#), vec![b"JJ".to_vec()]);
}

#[test]
fn a_codepoint_escape_is_its_utf8() {
    // var_dump: string(11) "Aé€😀\0", string(4) "\u{10FFFF}", string(4) "\u x"
    assert_eq!(
        parts(r#"<?php "\u{41}\u{e9}\u{20ac}\u{1F600}\u{0}";"#),
        vec![b"A\xc3\xa9\xe2\x82\xac\xf0\x9f\x98\x80\0".to_vec()]
    );
    assert_eq!(
        parts(r#"<?php "\u{10FFFF}";"#),
        vec![b"\xf4\x8f\xbf\xbf".to_vec()]
    );
    assert_eq!(parts(r#"<?php "\u x";"#), vec![b"\\u x".to_vec()]);
}

#[test]
fn the_mark_each_string_kind_takes_as_an_escape() {
    // var_dump("\"\`\$"): string(4) "\"\`$" less the one escape: `"\`$`
    assert_eq!(parts(r#"<?php "\"\`\$";"#), vec![b"\"\\`$".to_vec()]);
    // `echo \"a\` \$b\``: the shell is handed `echo \"a` $b``
    assert_eq!(
        parts("<?php `echo \\\"a\\` \\$b\\``;"),
        vec![b"echo \\\"a` $b`".to_vec()]
    );
    // A heredoc takes neither mark and every other escape, and the
    // newline before the closing label is the marker's.
    assert_eq!(
        parts("<?php <<<E\n\\\"\\`\\$\\n\\x41\\101\nE;"),
        vec![b"\\\"\\`$\nAA".to_vec()]
    );
    // A nowdoc takes none.
    assert_eq!(
        parts("<?php <<<'E'\n\\n\\\\$x\r\nE;"),
        vec![b"\\n\\\\$x".to_vec()]
    );
}

#[test]
fn the_newline_before_the_closing_label_is_the_markers() {
    // An interpolation right before it leaves an empty last part;
    // a flexible heredoc's indentation stands between the two.
    assert_eq!(
        parts("<?php <<<E\na\n$v\nE;"),
        vec![b"a\n".to_vec(), Vec::new()]
    );
    assert_eq!(
        parts("<?php <<<E\n  a\n  b\n  E;"),
        vec![b"a\n".to_vec(), b"b".to_vec()]
    );
    assert_eq!(parts("<?php <<<E\na\rE;"), vec![b"a".to_vec()]);
}
