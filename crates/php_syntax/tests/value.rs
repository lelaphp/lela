//! `TokenValue::of` on the two forms no witness in `value.rs` can say:
//! the double a binary token accumulates to, which php prints rounded
//! and this holds as the bit pattern; and a lone `\r` before a closing
//! label, which a rustdoc fence trims off the line. Each is held to
//! what the pinned php printed for the same token under
//! `serialize_precision=-1` or `printf("%.17g")` (probed 8.5.9 on
//! 2026-09-21); the witnesses in `value.rs` hold the rest through the
//! engine, the `` `…` `` mark beside `unescape` since the backtick
//! operator compiles and runs.

mod support;

use php_syntax::tree::{HeredocExpr, LiteralExpr, StringPart};
use php_syntax::{Holder, TokenValue};

/// The value of every `LiteralExpr` in `src`, in source order.
fn values(src: &str) -> Vec<TokenValue> {
    let parsed = support::parse_clean(src);
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

/// The bytes of every text part of the one heredoc in `src`, in source
/// order.
fn parts(src: &str) -> Vec<Vec<u8>> {
    let parsed = support::parse_clean(src);
    let bytes = |v: Option<TokenValue>| match v {
        Some(TokenValue::Bytes(b)) => b,
        other => panic!("not bytes: {other:?}"),
    };
    // The text parts; an interpolation between them is not one.
    let text = |part: &StringPart| match part {
        StringPart::LiteralStringPart(id) => parsed.tree[*id].text,
        _ => None,
    };
    if let Some(node) = parsed.tree.nodes::<HeredocExpr>().first() {
        return parsed
            .tree
            .list(node.parts)
            .iter()
            .filter_map(|p| text(p).map(|t| TokenValue::of(&parsed, Holder::Heredoc(node), t)))
            .map(bytes)
            .collect();
    }
    panic!("no heredoc in {src}");
}

#[test]
fn the_binary_retry_lands_an_ulp_below_two_to_the_sixty_third() {
    use TokenValue::{Float, Int};
    // %.17g: 9.2233720368547758e+18 for the decimal, hex and octal
    // retries, and 9.2233720368547748e+18 for the binary one, php's
    // `value * 2 + c - '0'` falling short of 2^63 by an ulp.
    assert_eq!(
        values(
            "<?php 0b111111111111111111111111111111111111111111111111111111111111111; 0b1000000000000000000000000000000000000000000000000000000000000000;"
        ),
        vec![Int(i64::MAX), Float(2f64.powi(63) - 1024.0)]
    );
}

#[test]
fn a_lone_carriage_return_before_the_closing_label_is_the_markers() {
    assert_eq!(parts("<?php <<<E\na\rE;"), vec![b"a".to_vec()]);
}
