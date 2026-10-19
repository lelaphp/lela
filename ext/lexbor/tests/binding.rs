//! The binding to lexbor held on its own, before ext/dom reads it: a
//! document parsed and walked, its parse errors and observations, a
//! fragment, the encodings and the selector parser. Every layout the
//! binding mirrors is read by one of these.

use php_ext::abi::ExtensionBuilder;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::test::{Capture, Engine};
use php_ext_lexbor::ffi::css::{Combinator, Kind, List, Match, Modifier, PseudoClassFunction};
use php_ext_lexbor::ffi::encoding::{self, Decoder, Encoder, Encoding, Status};
use php_ext_lexbor::ffi::html::{
    Document, FragmentParser, Namespace, Node, NodeKind, Quirks, TokenizerErrorId, TreeErrorId,
};
use php_ext_lexbor::ffi::url::{ErrorKind, Host, Parser, ipv4_text, ipv6_text};
use php_ext_lexbor::ffi::{Lent, Owned, lent};

/// `body` served under a lend of a request's heap ([`in_request`]), as
/// every caller of the binding serves a lexbor call that allocates.
macro_rules! lent_by_a_request {
    ($body:expr) => {
        in_request(|req| {
            lent(req, |lent| {
                let body: fn(&Lent) = $body;
                body(lent);
            })
        })
    };
}

fn parse(lent: &Lent, source: &[u8]) -> Document {
    let mut doc = Document::new(lent).expect("a document");
    assert_eq!(doc.parse_chunk_begin(lent), Status::Ok);
    assert_eq!(doc.parse_chunk(lent, source), Status::Ok);
    assert_eq!(doc.parse_chunk_end(lent), Status::Ok);
    doc
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut at = node.first_child();
    while let Some(n) = at {
        out.push(n);
        at = n.next();
    }
    out
}

fn names<'a>(nodes: &[Node<'a>]) -> Vec<&'a [u8]> {
    nodes.iter().map(|n| n.qualified_name()).collect()
}

#[test]
fn a_document_is_parsed_and_walked() {
    lent_by_a_request!(|lent| {
        let doc = parse(
            lent,
            b"<p class=a id='b'>x</p><!--c--><template><b>t</b></template>",
        );
        walk(&doc);
        doc.give_back(lent);
    });
}

fn walk(doc: &Document) {
    let root = doc.node();
    assert_eq!(root.kind(), NodeKind::Document);
    let html = children(root);
    assert_eq!(names(&html), [b"html".as_slice()]);
    assert_eq!(html[0].namespace(), Namespace::Html);
    let parts = children(html[0]);
    assert_eq!(names(&parts), [b"head".as_slice(), b"body"]);
    let body = children(parts[1]);
    assert_eq!(body.len(), 3);
    let p = body[0];
    assert_eq!(p.qualified_name(), b"p");
    assert_eq!(p.line(), 0, "zero-based, as php's getLineNo() shows it");
    let attrs: Vec<_> = p
        .attributes()
        .map(|a| (a.qualified_name(), a.value(), a.namespace(), a.has_prefix()))
        .collect();
    assert_eq!(
        attrs,
        [
            (b"class".as_slice(), b"a".as_slice(), Namespace::Html, false),
            (b"id", b"b", Namespace::Html, false)
        ]
    );
    let text = children(p);
    assert_eq!(text[0].kind(), NodeKind::Text);
    assert_eq!(text[0].data(), b"x");
    assert_eq!(body[1].kind(), NodeKind::Comment);
    assert_eq!(body[1].data(), b"c");
    let template = body[2];
    assert!(template.is_template());
    assert!(
        template.first_child().is_none(),
        "a template's children are its content's"
    );
    let content = template.template_content().expect("content");
    assert_eq!(content.kind(), NodeKind::DocumentFragment);
    let b = content.last_child().expect("b");
    assert_eq!(b.qualified_name(), b"b");
    assert_eq!(b.first_child().map(Node::data), Some(b"t".as_slice()));
    assert_eq!(b.parent(), Some(content));
    assert_eq!(b.prev(), None);
    assert!(!p.is_template());
}

#[test]
fn doctypes_foreign_content_and_prefixes() {
    lent_by_a_request!(|lent| {
        let doc = parse(
            lent,
            b"<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"http://www.w3.org/TR/html4/strict.dtd\">\
          <svg><circle xlink:href=\"u\"/></svg><math></math>",
        );
        foreign(&doc);
        doc.give_back(lent);
    });
}

fn foreign(doc: &Document) {
    let top = children(doc.node());
    assert_eq!(top[0].kind(), NodeKind::DocumentType);
    assert_eq!(top[0].doctype_name(), b"html");
    assert_eq!(top[0].doctype_public_id(), b"-//W3C//DTD HTML 4.01//EN");
    assert_eq!(
        top[0].doctype_system_id(),
        b"http://www.w3.org/TR/html4/strict.dtd"
    );
    let body = children(children(top[1])[1]);
    assert_eq!(body[0].namespace(), Namespace::Svg);
    assert_eq!(body[1].namespace(), Namespace::MathMl);
    let circle = body[0].first_child().expect("circle");
    let href = circle.attributes().next().expect("xlink:href");
    assert_eq!(href.qualified_name(), b"xlink:href");
    assert!(href.has_prefix());
    assert_eq!(href.namespace(), Namespace::XLink);
    assert_eq!(doc.quirks(), Quirks::NoQuirks);
}

#[test]
fn parse_errors_and_observations() {
    lent_by_a_request!(errors_and_observations);
}

fn errors_and_observations(lent: &Lent) {
    let source = b"<!DOCTYPE html><p a=1 a=2>x&unknown;</p>";
    let mut doc = Document::new(lent).expect("a document");
    assert_eq!(doc.parse_chunk_begin(lent), Status::Ok);
    assert_eq!(doc.parse_chunk(lent, source), Status::Ok);
    let tokenizer = doc.tokenizer_errors(0, source);
    assert_eq!(tokenizer.len(), 1);
    assert_eq!(
        tokenizer[0].id,
        TokenizerErrorId::UnknownNamedCharacterReference
    );
    // php reports it at column 36 of line 1: the `;` at offset 35.
    assert_eq!(tokenizer[0].offset, 35);
    assert!(doc.tokenizer_errors(1, source).is_empty());
    assert_eq!(doc.parse_chunk_end(lent), Status::Ok);
    assert!(doc.tree_errors(0).is_empty());
    let seen = doc.observations();
    assert!(
        !seen.has_explicit_html_tag && !seen.has_explicit_head_tag && !seen.has_explicit_body_tag
    );
    assert_eq!(seen.quirks, Quirks::NoQuirks);
    doc.give_back(lent);

    // php: "tree error unexpected-token-in-initial-mode in Entity, line:
    // 1, column: 2", from zero here.
    let doc = parse(lent, b"<p class=a>x</p>");
    let tree = doc.tree_errors(0);
    assert_eq!(tree[0].id, TreeErrorId::UnexpectedTokenInInitialMode);
    assert_eq!((tree[0].line, tree[0].column), (0, 1));
    assert!(tree[0].length <= 1);
    assert_eq!(doc.quirks(), Quirks::Quirks);
    doc.give_back(lent);

    let doc = parse(lent, b"<html><head></head><body></body></html>");
    let seen = doc.observations();
    assert!(seen.has_explicit_html_tag && seen.has_explicit_head_tag && seen.has_explicit_body_tag);
    doc.give_back(lent);
}

#[test]
fn a_fragment_is_parsed_in_its_context() {
    lent_by_a_request!(fragments);
}

fn fragments(lent: &Lent) {
    let mut parser = FragmentParser::begin(
        lent,
        b"tr",
        b"http://www.w3.org/1999/xhtml",
        Quirks::NoQuirks,
    )
    .expect("a parser");
    assert_eq!(parser.chunk(lent, b"<td>a</td><td>"), Status::Ok);
    assert_eq!(parser.chunk(lent, b"b</td>"), Status::Ok);
    let fragment = parser.end(lent).expect("a fragment");
    let cells = children(fragment.root());
    assert_eq!(names(&cells), [b"td".as_slice(), b"td"]);
    assert_eq!(
        cells[1].first_child().map(Node::data),
        Some(b"b".as_slice())
    );
    fragment.give_back(lent);

    let mut parser =
        FragmentParser::begin(lent, b"svg", b"http://www.w3.org/2000/svg", Quirks::Quirks)
            .expect("a parser");
    assert_eq!(parser.chunk(lent, b"<g/>"), Status::Ok);
    let fragment = parser.end(lent).expect("a fragment");
    let g = fragment.root().first_child().expect("g");
    assert_eq!(g.namespace(), Namespace::Svg);
    fragment.give_back(lent);
}

#[test]
fn encodings_by_name_and_prescan() {
    let utf8 = Encoding::utf_8().expect("UTF-8");
    assert_eq!(utf8.name(), b"UTF-8");
    assert!(utf8.is_utf_8());
    let latin = Encoding::by_name(b"latin1").expect("a label");
    assert_eq!(latin.name(), b"windows-1252");
    assert!(!latin.is_utf_8());
    assert!(Encoding::by_name(b"nope").is_none());
    assert_eq!(
        Encoding::utf_16le().map(Encoding::name),
        Some(b"UTF-16LE".as_slice())
    );
    assert_eq!(
        Encoding::utf_16be().map(Encoding::name),
        Some(b"UTF-16BE".as_slice())
    );
    assert_eq!(Encoding::by_pre_name(b"utf-8"), Some(utf8));
    lent_by_a_request!(|lent| {
        let found = Encoding::prescan(lent, b"<html><head><meta charset=\"koi8-r\"></head>");
        assert_eq!(found.map(Encoding::name), Some(b"KOI8-R".as_slice()));
        let found = Encoding::prescan(
            lent,
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=ISO-8859-2\">",
        );
        assert_eq!(found.map(Encoding::name), Some(b"ISO-8859-2".as_slice()));
        assert!(Encoding::prescan(lent, b"<p>nothing</p>").is_none());
    });
}

#[test]
fn decoding_and_encoding_stream() {
    let latin = Encoding::by_name(b"windows-1252").expect("windows-1252");
    let utf8 = Encoding::utf_8().expect("UTF-8");
    let mut decoder = Decoder::new(latin, 2);
    let mut input: &[u8] = b"caf\xe9!";
    let mut codepoints = Vec::new();
    loop {
        let status = decoder.decode(&mut input);
        codepoints.extend_from_slice(decoder.codepoints());
        decoder.clear();
        if status != Status::SmallBuffer {
            break;
        }
    }
    assert!(input.is_empty());
    assert_eq!(codepoints, [0x63, 0x61, 0x66, 0xe9, 0x21]);
    assert_eq!(decoder.finish(), Status::Ok);

    let mut encoder = Encoder::new(utf8, 3, encoding::REPLACEMENT_BYTES);
    let mut rest: &[u32] = &codepoints;
    let mut out = Vec::new();
    loop {
        let status = encoder.encode(&mut rest);
        out.extend_from_slice(encoder.bytes());
        encoder.clear();
        if status != Status::SmallBuffer {
            break;
        }
    }
    assert_eq!(encoder.finish(), Status::Ok);
    assert_eq!(out, "café!".as_bytes());

    // A code point windows-1252 lacks becomes the replacement given.
    let mut encoder = Encoder::new(latin, 16, b"?");
    let mut rest: &[u32] = &[0x41, 0x4e2d];
    encoder.encode(&mut rest);
    assert_eq!(encoder.bytes(), b"A?");

    // The UTF-8 single steps of php's fast paths.
    let mut single = Decoder::single(utf8);
    let mut input: &[u8] = b"\xc3\xa9\xff\xe2\x82";
    assert_eq!(single.utf_8_single(&mut input), 0xe9);
    assert!(single.utf_8_single(&mut input) > encoding::MAX_CODEPOINT);
    assert_eq!(single.utf_8_single(&mut input), encoding::DECODE_CONTINUE);
    assert!(input.is_empty());
    assert_ne!(single.utf_8_need(), 0);
    single.set_status(Status::Continue);
    assert_eq!(single.status(), Status::Continue);
    assert_eq!(Encoder::utf_8_single(0x20ac), Some("€".as_bytes().to_vec()));
    assert_eq!(Encoder::utf_8_single(0x0011_0000), None);
}

#[test]
fn selectors_are_parsed_into_an_owned_tree() {
    lent_by_a_request!(selectors);
}

fn selectors(lent: &Lent) {
    let list = List::parse(
        lent,
        b"div > p.a[title^=x i], :nth-child(2n+1 of .b), :not(#c) ~ *",
    )
    .expect("a list");
    assert_eq!(list.len(), 3);
    let first = &list[0].selectors;
    assert_eq!(first.len(), 4);
    assert_eq!(
        (first[0].kind.clone(), first[0].name.as_slice()),
        (Kind::Element, b"div".as_slice())
    );
    assert_eq!(first[1].combinator, Combinator::Child);
    assert_eq!(first[1].name, b"p");
    assert_eq!(first[2].combinator, Combinator::Close);
    assert_eq!(first[2].kind, Kind::Class);
    assert_eq!(first[2].name, b"a");
    let Kind::Attribute(Some(m)) = &first[3].kind else {
        panic!("an attribute matcher: {:?}", first[3].kind);
    };
    assert_eq!(
        (m.matcher, m.modifier, m.value.as_slice()),
        (Match::Prefix, Modifier::I, b"x".as_slice())
    );
    assert_eq!(first[3].name, b"title");
    assert!(list[0].specificity > 0);

    let Kind::PseudoClassFunction(PseudoClassFunction::NthChild(anb)) = &list[1].selectors[0].kind
    else {
        panic!("nth-child: {:?}", list[1].selectors[0].kind);
    };
    assert_eq!((anb.a, anb.b), (2, 1));
    let of = anb.of.as_ref().expect("of");
    assert_eq!(of[0].selectors[0].kind, Kind::Class);

    let third = &list[2].selectors;
    let Kind::PseudoClassFunction(PseudoClassFunction::Not(inner)) = &third[0].kind else {
        panic!("not: {:?}", third[0].kind);
    };
    assert_eq!(inner[0].selectors[0].kind, Kind::Id);
    assert_eq!(inner[0].selectors[0].name, b"c");
    assert_eq!(third[1].combinator, Combinator::Following);
    assert_eq!(third[1].kind, Kind::Any);

    let has = List::parse(lent, b"a:has(> b)").expect("a list");
    let Kind::PseudoClassFunction(PseudoClassFunction::Has(rel)) = &has[0].selectors[1].kind else {
        panic!("has: {:?}", has[0].selectors[1].kind);
    };
    assert_eq!(rel[0].selectors[0].combinator, Combinator::Child);

    let presence = List::parse(lent, b"[data-x]").expect("a list");
    assert_eq!(presence[0].selectors[0].kind, Kind::Attribute(None));
}

#[test]
fn a_selector_is_refused_with_lexbor_message() {
    lent_by_a_request!(|lent| {
        let refused = List::parse(lent, b"div >").expect_err("refused");
        let message = refused.message.expect("a message");
        assert!(!message.is_empty(), "{}", String::from_utf8_lossy(&message));
        assert!(List::parse(lent, b"").is_err());
    });
}

/// `body` served as a request's `rinit`, with the heap lent to lexbor
/// as every caller of the binding lends it: lexbor allocates on the
/// request's heap, and the request leak sensor, armed, holds that what
/// `body` made is given back before the request ends.
fn in_request(body: fn(&mut Request) -> Result<(), Thrown>) {
    let probe = ExtensionBuilder::default().rinit(body).build("binding");
    let engine = Engine::builder()
        .extensions(&[php_ext_lexbor::extension(), probe])
        .build();
    let served = engine.serve_capture("<?php echo 'served';", Capture::default());
    assert_eq!(served.fatal, None);
    assert_eq!(served.stdout, b"served");
}

/// `body` served in a request with the heap lent and a fresh URL
/// parser, destroyed after it.
macro_rules! with_parser {
    ($body:expr) => {
        in_request(|req| {
            lent(req, |lent| {
                let mut parser = Parser::new(lent).expect("a parser");
                let body: fn(&Lent, &mut Parser) = $body;
                body(lent, &mut parser);
                parser.give_back(lent);
            })
        })
    };
}

#[test]
fn a_url_is_parsed_read_and_serialised() {
    with_parser!(|lent, parser| {
        let parsed = parser.parse(
            lent,
            b"HTTPS://us%65r:pa ss@EXAMPLE.com:8080/a/../b?q=1#frag",
            None,
        );
        assert!(parsed.errors.is_empty() || parsed.url.is_some());
        let url = parsed.url.expect("a URL");
        assert_eq!(url.scheme(), b"https");
        assert_eq!(url.username(), b"us%65r");
        assert_eq!(url.password(), b"pa%20ss");
        assert_eq!(url.host(), Some(Host::Domain(b"example.com")));
        assert_eq!(url.port(), Some(8080));
        assert_eq!(url.path(), b"/b");
        assert_eq!(url.query(), Some(b"q=1".as_slice()));
        assert_eq!(url.fragment(), Some(b"frag".as_slice()));
        assert_eq!(
            url.serialize(true),
            b"https://us%65r:pa%20ss@example.com:8080/b?q=1"
        );
        let copy = parser.clone_url(lent, &url).expect("a clone");
        parser.free(lent, url);
        assert_eq!(
            copy.serialize(false),
            b"https://us%65r:pa%20ss@example.com:8080/b?q=1#frag"
        );
        parser.free(lent, copy);
    });
}

#[test]
fn a_url_logs_its_validation_errors_last_first() {
    with_parser!(|lent, parser| {
        let parsed = parser.parse(lent, b"https://example.com:99999", None);
        assert!(parsed.url.is_none());
        assert_eq!(parsed.errors.len(), 1);
        assert_eq!(parsed.errors[0].kind, ErrorKind::PortOutOfRange);
        assert_eq!(
            parsed.errors[0].data, b"",
            "the position past the port, as php shows it"
        );
        let base = parser.parse(lent, b"https://x/", None).url.expect("a base");
        let parsed = parser.parse(lent, b" \\\\example.com\\a\tb", Some(&base));
        let url = parsed.url.expect("a URL");
        assert_eq!(url.serialize(false), b"https://example.com/ab");
        let kinds: Vec<ErrorKind> = parsed.errors.iter().map(|e| e.kind).collect();
        assert_eq!(kinds.first(), Some(&ErrorKind::InvalidReverseSolidus));
        assert_eq!(kinds.last(), Some(&ErrorKind::InvalidUrlUnit));
    });
}

#[test]
fn a_url_is_changed_by_its_setters() {
    with_parser!(|lent, parser| {
        let mut url = parser
            .parse(lent, b"https://example.com/", None)
            .url
            .expect("a URL");
        let url = &mut url;
        parser.set_protocol(lent, url, b"http").expect("a scheme");
        parser.set_username(lent, url, b"u").expect("a username");
        parser.set_password(lent, url, b"p").expect("a password");
        parser.set_port(lent, url, b"81").expect("a port");
        parser.set_pathname(lent, url, b"x y").expect("a path");
        parser.set_search(lent, url, b"a=b").expect("a query");
        parser.set_hash(lent, url, b"h").expect("a fragment");
        assert_eq!(
            url.serialize(false),
            b"http://u:p@example.com:81/x%20y?a=b#h"
        );
        let refused = parser
            .set_hostname(lent, url, b"exa[mple")
            .expect_err("a malformed host");
        assert_eq!(
            refused.0.first().map(|e| e.kind),
            Some(ErrorKind::DomainInvalidCodePoint)
        );
        parser
            .set_hostname(lent, url, b"192.168.0.1")
            .expect("an IPv4 host");
        let Some(Host::Ipv4(v4)) = url.host() else {
            panic!("an IPv4 host")
        };
        assert_eq!(ipv4_text(v4), b"192.168.0.1");
        parser
            .set_hostname(lent, url, b"[2001:db8::1]")
            .expect("an IPv6 host");
        let Some(Host::Ipv6(v6)) = url.host() else {
            panic!("an IPv6 host")
        };
        assert_eq!(ipv6_text(&v6), b"2001:db8::1");
        parser
            .set_hostname(lent, url, b"xn--mnchen-3ya.de")
            .expect("a domain");
        assert_eq!(parser.host_unicode(lent, url), "münchen.de".as_bytes());
        assert_eq!(
            parser.serialize_idna(lent, url, true),
            "http://u:p@münchen.de:81/x%20y?a=b".as_bytes()
        );
        parser.set_port(lent, url, b"").expect("no port");
        assert_eq!(url.port(), None);
    });
}
