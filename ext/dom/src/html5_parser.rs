//! `html5_parser.c` and the parse half of `html_document.c`: lexbor's
//! HTML5 parser fed chunk by chunk through the decode and encode steps
//! php runs, its parse errors raised with php's line and column, and the
//! tree it built converted into libxml2 nodes (`lexbor_libxml2_bridge_*`),
//! each element in the namespace the mapper hands out and each template's
//! contents a fragment of its own.

use php_ext::raising::{Thrown, arg_value_error, throw_exception_ex};
use php_ext::request::{OwnedStream as _, Request, Streams as _};
use php_ext::values::{Owner, PhpObject, PhpString, Value};
use php_ext_lexbor::ffi::array::ArrayObj;
use php_ext_lexbor::ffi::encoding::{
    DECODE_CONTINUE, Decoder, Encoder, Encoding, MAX_CODEPOINT, REPLACEMENT_BYTES, Status,
};
use php_ext_lexbor::ffi::html::{
    Document as HtmlDocument, Fragment, FragmentParser, Namespace, Node as HtmlNode, NodeKind,
    Observations, Quirks as HtmlQuirks, TokenizerErrorId, TreeErrorId,
};
use php_ext_lexbor::ffi::{self as lexbor, Lent, Owned as _};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{DocString, Kind, Node, NodeObject, Ns, Quirks};

use crate::dom_throw;
use crate::object::Code;
use crate::private::{HTML_NS, Handed, MATHML_NS, Mapper, Private, SVG_NS, XLINK_NS};

/// `HTML_PARSE_NOERROR`, `XML_PARSE_NOERROR` under another name.
const NOERROR: i64 = 1 << 5;
/// `HTML_PARSE_COMPACT`.
const COMPACT: i64 = 1 << 16;
/// `HTML_PARSE_NOIMPLIED`.
const NOIMPLIED: i64 = 1 << 13;

/// The size of php's decoding and encoding buffers, and of a chunk.
const BUFFER: usize = 4096;

/// `check_options_validity` of `html_document.c`.
fn check_options(req: &mut Request, position: u32, options: i64) -> Result<(), Thrown> {
    let valid = NOERROR | COMPACT | NOIMPLIED | crate::modern_constants::HTML_NO_DEFAULT_NS;
    if options & !valid != 0 {
        return Err(php_ext::raising::argument_error(
            req,
            "ValueError",
            position,
            "contains invalid flags (allowed flags: LIBXML_NOERROR, LIBXML_COMPACT, \
             LIBXML_HTML_NOIMPLIED, Dom\\HTML_NO_DEFAULT_NS)",
        ));
    }
    Ok(())
}

// ---- the error reporters

/// `dom_lexbor_tokenizer_error_code_to_string`.
fn tokenizer_error_name(id: TokenizerErrorId) -> &'static str {
    use TokenizerErrorId as T;
    match id {
        T::AbruptClosingOfEmptyComment => "abrupt-closing-of-empty-comment",
        T::AbruptDoctypePublicIdentifier => "abrupt-doctype-public-identifier",
        T::AbruptDoctypeSystemIdentifier => "abrupt-doctype-system-identifier",
        T::AbsenceOfDigitsInNumericCharacterReference => {
            "absence-of-digits-in-numeric-character-reference"
        }
        T::CdataInHtmlContent => "cdata-in-html-content",
        T::CharacterReferenceOutsideUnicodeRange => "character-reference-outside-unicode-range",
        T::ControlCharacterInInputStream => "control-character-in-input-stream",
        T::ControlCharacterReference => "control-character-reference",
        T::EndTagWithAttributes => "end-tag-with-attributes",
        T::DuplicateAttribute => "duplicate-attribute",
        T::EndTagWithTrailingSolidus => "end-tag-with-trailing-solidus",
        T::EofBeforeTagName => "eof-before-tag-name",
        T::EofInCdata => "eof-in-cdata",
        T::EofInComment => "eof-in-comment",
        T::EofInDoctype => "eof-in-doctype",
        T::EofInScriptHtmlCommentLikeText => "eof-in-script-html-comment-like-text",
        T::EofInTag => "eof-in-tag",
        T::IncorrectlyClosedComment => "incorrectly-closed-comment",
        T::IncorrectlyOpenedComment => "incorrectly-opened-comment",
        T::InvalidCharacterSequenceAfterDoctypeName => {
            "invalid-character-sequence-after-doctype-name"
        }
        T::InvalidFirstCharacterOfTagName => "invalid-first-character-of-tag-name",
        T::MissingAttributeValue => "missing-attribute-value",
        T::MissingDoctypeName => "missing-doctype-name",
        T::MissingDoctypePublicIdentifier => "missing-doctype-public-identifier",
        T::MissingDoctypeSystemIdentifier => "missing-doctype-system-identifier",
        T::MissingEndTagName => "missing-end-tag-name",
        T::MissingQuoteBeforeDoctypePublicIdentifier => {
            "missing-quote-before-doctype-public-identifier"
        }
        T::MissingQuoteBeforeDoctypeSystemIdentifier => {
            "missing-quote-before-doctype-system-identifier"
        }
        T::MissingSemicolonAfterCharacterReference => "missing-semicolon-after-character-reference",
        T::MissingWhitespaceAfterDoctypePublicKeyword => {
            "missing-whitespace-after-doctype-public-keyword"
        }
        T::MissingWhitespaceAfterDoctypeSystemKeyword => {
            "missing-whitespace-after-doctype-system-keyword"
        }
        T::MissingWhitespaceBeforeDoctypeName => "missing-whitespace-before-doctype-name",
        T::MissingWhitespaceBetweenAttributes => "missing-whitespace-between-attributes",
        T::MissingWhitespaceBetweenDoctypePublicAndSystemIdentifiers => {
            "missing-whitespace-between-doctype-public-and-system-identifiers"
        }
        T::NestedComment => "nested-comment",
        T::NoncharacterCharacterReference => "noncharacter-character-reference",
        T::NoncharacterInInputStream => "noncharacter-in-input-stream",
        T::NonVoidHtmlElementStartTagWithTrailingSolidus => {
            "non-void-html-element-start-tag-with-trailing-solidus"
        }
        T::NullCharacterReference => "null-character-reference",
        T::SurrogateCharacterReference => "surrogate-character-reference",
        T::SurrogateInInputStream => "surrogate-in-input-stream",
        T::UnexpectedCharacterAfterDoctypeSystemIdentifier => {
            "unexpected-character-after-doctype-system-identifier"
        }
        T::UnexpectedCharacterInAttributeName => "unexpected-character-in-attribute-name",
        T::UnexpectedCharacterInUnquotedAttributeValue => {
            "unexpected-character-in-unquoted-attribute-value"
        }
        T::UnexpectedEqualsSignBeforeAttributeName => {
            "unexpected-equals-sign-before-attribute-name"
        }
        T::UnexpectedNullCharacter => "unexpected-null-character",
        T::UnexpectedQuestionMarkInsteadOfTagName => "unexpected-question-mark-instead-of-tag-name",
        T::UnexpectedSolidusInTag => "unexpected-solidus-in-tag",
        T::UnknownNamedCharacterReference => "unknown-named-character-reference",
        T::Other(_) => "unknown error",
    }
}

/// `dom_lexbor_tree_error_code_to_string`.
fn tree_error_name(id: TreeErrorId) -> &'static str {
    use TreeErrorId as T;
    match id {
        T::UnexpectedToken => "unexpected-token",
        T::UnexpectedClosedToken => "unexpected-closed-token",
        T::NullCharacter => "null-character",
        T::UnexpectedCharacterToken => "unexpected-character-token",
        T::UnexpectedTokenInInitialMode => "unexpected-token-in-initial-mode",
        T::BadDoctypeTokenInInitialMode => "bad-doctype-token-in-initial-mode",
        T::DoctypeTokenInBeforeHtmlMode => "doctype-token-in-before-html-mode",
        T::UnexpectedClosedTokenInBeforeHtmlMode => "unexpected-closed-token-in-before-html-mode",
        T::DoctypeTokenInBeforeHeadMode => "doctype-token-in-before-head-mode",
        T::UnexpectedClosedTokenInBeforeHeadMode => "unexpected-closed_token-in-before-head-mode",
        T::DoctypeTokenInHeadMode => "doctype-token-in-head-mode",
        T::NonVoidHtmlElementStartTagWithTrailingSolidus => {
            "non-void-html-element-start-tag-with-trailing-solidus"
        }
        T::HeadTokenInHeadMode => "head-token-in-head-mode",
        T::UnexpectedClosedTokenInHeadMode => "unexpected-closed-token-in-head-mode",
        T::TemplateClosedTokenWithoutOpeningInHeadMode => {
            "template-closed-token-without-opening-in-head-mode"
        }
        T::TemplateElementIsNotCurrentInHeadMode => "template-element-is-not-current-in-head-mode",
        T::DoctypeTokenInHeadNoscriptMode => "doctype-token-in-head-noscript-mode",
        T::DoctypeTokenAfterHeadMode => "doctype-token-after-head-mode",
        T::HeadTokenAfterHeadMode => "head-token-after-head-mode",
        T::DoctypeTokenInBodyMode => "doctype-token-in-body-mode",
        T::BadEndingOpenElementsIsWrong => "bad-ending-open-elements-is-wrong",
        T::OpenElementsIsWrong => "open-elements-is-wrong",
        T::UnexpectedElementInOpenElementsStack => "unexpected-element-in-open-elements-stack",
        T::MissingElementInOpenElementsStack => "missing-element-in-open-elements-stack",
        T::NoBodyElementInScope => "no-body-element-in-scope",
        T::MissingElementInScope => "missing-element-in-scope",
        T::UnexpectedElementInScope => "unexpected-element-in-scope",
        T::UnexpectedElementInActiveFormattingStack => {
            "unexpected-element-in-active-formatting-stack"
        }
        T::UnexpectedEndOfFile => "unexpected-end-of-file",
        T::CharactersInTableText => "characters-in-table-text",
        T::DoctypeTokenInTableMode => "doctype-token-in-table-mode",
        T::DoctypeTokenInSelectMode => "doctype-token-in-select-mode",
        T::DoctypeTokenAfterBodyMode => "doctype-token-after-body-mode",
        T::DoctypeTokenInFramesetMode => "doctype-token-in-frameset-mode",
        T::DoctypeTokenAfterFramesetMode => "doctype-token-after-frameset-mode",
        T::DoctypeTokenForeignContentMode => "doctype-token-foreign-content-mode",
        T::Other(_) => "unknown error",
    }
}

/// `dom_line_column_cache`.
struct LineColumn {
    line: usize,
    column: usize,
    offset: usize,
}

/// Where the reporter reads the input it walks for a position: the code
/// points the slow path decoded, or the bytes of the source from a window
/// start on.
enum Walked {
    Codepoints(Vec<u32>),
    Bytes(usize),
}

/// `dom_lexbor_libxml2_bridge_application_data`, with whether errors are
/// reported at all.
struct App<'a> {
    name: &'a [u8],
    source: &'a [u8],
    walked: Walked,
    input_length: usize,
    total_offset: usize,
    cache: LineColumn,
    no_implied: bool,
    report: bool,
    tokenizer_index: usize,
    tree_index: usize,
}

impl App<'_> {
    /// `dom_find_line_and_column_using_cache`.
    fn find(&mut self, offset: usize) {
        let offset = offset
            .wrapping_sub(self.total_offset)
            .min(self.input_length);
        let c = &mut self.cache;
        match &self.walked {
            Walked::Codepoints(cps) => {
                while c.offset < offset {
                    if cps.get(c.offset) == Some(&0x0A) {
                        c.line += 1;
                        c.column = 1;
                    } else {
                        c.column += 1;
                    }
                    c.offset += 1;
                }
            }
            Walked::Bytes(start) => {
                while c.offset < offset {
                    let b = self.source.get(start + c.offset).copied().unwrap_or(0);
                    if b == b'\n' {
                        c.line += 1;
                        c.column = 1;
                    } else if b & 0b1100_0000 != 0b1000_0000 {
                        c.column += 1;
                    }
                    c.offset += 1;
                }
            }
        }
    }
}

/// `dom_process_parse_chunk`: one chunk handed to lexbor under a lend,
/// its errors reported, and the position moved past it; `false` where
/// lexbor refused the chunk, the `Err` the memory limit's fatal.
fn process_chunk(
    req: &mut Request,
    app: &mut App<'_>,
    doc: &mut HtmlDocument,
    chunk: &[u8],
    input_length: usize,
) -> Result<bool, Thrown> {
    app.input_length = input_length;
    if lexbor::lent(req, |lent| doc.parse_chunk(lent, chunk))? != Status::Ok {
        return Ok(false);
    }
    if app.report {
        report_errors(req, app, doc, chunk);
        let end = app.total_offset + input_length;
        app.find(end);
    }
    app.total_offset += input_length;
    app.cache.offset = 0;
    Ok(true)
}

/// `lexbor_libxml2_bridge_report_errors` with php's two reporters.
fn report_errors(req: &mut Request, app: &mut App<'_>, doc: &HtmlDocument, chunk: &[u8]) {
    let tokenizer = doc.tokenizer_errors(app.tokenizer_index, chunk);
    app.tokenizer_index += tokenizer.len();
    for error in tokenizer {
        let offset = error.offset.wrapping_add(app.total_offset);
        app.find(offset);
        let (line, column) = (app.cache.line, app.cache.column);
        let what = tokenizer_error_name(error.id);
        pretend(req, app.name, "tokenizer", what, line, column, None);
    }
    let tree = doc.tree_errors(app.tree_index);
    app.tree_index += tree.len();
    for error in tree {
        let (line, column, len) = (error.line + 1, error.column + 1, error.length);
        if line == 1 && app.no_implied && error.id == TreeErrorId::UnexpectedTokenInInitialMode {
            continue;
        }
        let what = tree_error_name(error.id);
        let last = (len > 1).then(|| column + len - 1);
        pretend(req, app.name, "tree", what, line, column, last);
    }
}

/// The two reporters' `php_libxml_pretend_ctx_error_ex`: `"%s error %s in
/// %s, line: %zu, column: %zu\n"`, the column a range up to `last` where
/// the tree's error spans more than one, and the input's name as its
/// bytes, as php's `%s` prints them.
///
/// ```php
/// set_error_handler(function ($n, $s) { echo $n, " ", bin2hex(strstr($s, " in ")), "\n"; return true; });
/// Dom\HTMLDocument::createFromFile("data:text/html,<!DOCTYPE html><p a=>\xff");
/// Dom\HTMLDocument::createFromFile("data:text/html,<p>x</b>\xff");
/// ```
/// ```expect
/// 2 20696e20646174613a746578742f68746d6c2c3c21444f43545950452068746d6c3e3c7020613d3eff2c206c696e653a20312c20636f6c756d6e3a203231
/// 2 20696e20646174613a746578742f68746d6c2c3c703e783c2f623eff2c206c696e653a20312c20636f6c756d6e3a2032
/// ```
fn pretend(
    req: &mut Request,
    file: &[u8],
    kind: &str,
    what: &str,
    line: usize,
    column: usize,
    last: Option<usize>,
) {
    let columns = last.map_or_else(|| column.to_string(), |last| format!("{column}-{last}"));
    let head = format!("{kind} error {what} in ");
    let tail = format!(", line: {line}, column: {columns}\n");
    let message = [head.as_bytes(), file, tail.as_bytes()].concat();
    let line = i32::try_from(line).unwrap_or(i32::MAX);
    let column = i32::try_from(column).unwrap_or(i32::MAX);
    Errors::pretend(req, file, line, column, &message);
}

// ---- decoding and encoding the input

/// `dom_decoding_encoding_ctx`.
struct Coding {
    fast: bool,
    decoder: Decoder,
    encoder: Encoder,
}

impl Coding {
    /// `dom_decoding_encoding_ctx_init` then
    /// `dom_setup_parser_encoding_manually(encoding)`.
    fn new(encoding: Encoding) -> Option<Coding> {
        let utf_8 = Encoding::utf_8()?;
        Some(Coding {
            fast: encoding.is_utf_8(),
            decoder: Decoder::new(encoding, BUFFER),
            encoder: Encoder::new(utf_8, BUFFER, REPLACEMENT_BYTES),
        })
    }
}

/// `dom_determine_encoding`: a byte order mark, then the `<meta>` pre-scan
/// of the first 1,024 bytes, then UTF-8; and the length of the mark.
fn determine_encoding(lent: &Lent, source: &[u8]) -> Option<(Encoding, usize)> {
    if source.starts_with(b"\xEF\xBB\xBF") {
        return Some((Encoding::utf_8()?, 3));
    }
    if source.starts_with(b"\xFE\xFF") {
        return Some((Encoding::utf_16be()?, 2));
    }
    if source.starts_with(b"\xFF\xFE") {
        return Some((Encoding::utf_16le()?, 2));
    }
    let head = source.get(..source.len().min(1024)).unwrap_or_default();
    match Encoding::prescan(lent, head) {
        Some(found) => Some((found, 0)),
        None => Some((Encoding::utf_8()?, 0)),
    }
}

/// The parser, its decoder and the reporter's state, fed one range of the
/// source at a time. Each step answers `false` where lexbor refused a
/// chunk, and the `Err` is the memory limit's fatal.
struct Feed<'a> {
    app: App<'a>,
    doc: &'a mut HtmlDocument,
    coding: Coding,
}

impl Feed<'_> {
    /// `dom_parse_decode_encode_step` over `source[start..end]`.
    fn step(&mut self, req: &mut Request, start: usize, end: usize) -> Result<bool, Thrown> {
        if self.coding.fast {
            self.fast(req, start, end)
        } else {
            self.slow(req, start, end)
        }
    }

    /// `dom_decode_encode_fast_path`: UTF-8 in, validated UTF-8 out, a bad
    /// sequence as the replacement bytes.
    fn fast(&mut self, req: &mut Request, start: usize, end: usize) -> Result<bool, Thrown> {
        let source = self.app.source;
        let window = source.get(start..end).unwrap_or_default();
        let mut input = window;
        if self.coding.decoder.status() == Status::Continue {
            let cp = self.coding.decoder.utf_8_single(&mut input);
            let bytes = Encoder::utf_8_single(cp).unwrap_or_else(|| REPLACEMENT_BYTES.to_vec());
            self.coding.decoder.set_status(Status::Ok);
            let consumed = window.len() - input.len();
            if !process_chunk(req, &mut self.app, self.doc, &bytes, consumed)? {
                return Ok(false);
            }
        }
        let mut last_output = window.len() - input.len();
        while !input.is_empty() {
            if self.coding.decoder.utf_8_need() == 0 {
                match input.iter().position(|b| *b & 0x80 != 0) {
                    None => {
                        input = &[];
                        break;
                    }
                    Some(at) => input = input.get(at..).unwrap_or_default(),
                }
            }
            let backup = window.len() - input.len();
            let cp = self.coding.decoder.utf_8_single(&mut input);
            if cp > MAX_CODEPOINT {
                let at = window.len() - input.len();
                let chunk = window.get(last_output..backup).unwrap_or_default();
                if !process_chunk(req, &mut self.app, self.doc, chunk, at - last_output)? {
                    return Ok(false);
                }
                if cp == DECODE_CONTINUE {
                    self.coding.decoder.set_status(Status::Continue);
                    return Ok(true);
                }
                if !process_chunk(req, &mut self.app, self.doc, REPLACEMENT_BYTES, 0)? {
                    return Ok(false);
                }
                last_output = at;
            }
        }
        let at = window.len() - input.len();
        if at != last_output {
            let chunk = window.get(last_output..at).unwrap_or_default();
            if !process_chunk(req, &mut self.app, self.doc, chunk, at - last_output)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `dom_decode_encode_slow_path`: another encoding decoded to code
    /// points and encoded to UTF-8, buffer by buffer.
    fn slow(&mut self, req: &mut Request, start: usize, end: usize) -> Result<bool, Thrown> {
        let source = self.app.source;
        let mut input = source.get(start..end).unwrap_or_default();
        loop {
            let decoded = self.coding.decoder.decode(&mut input);
            let cps = self.coding.decoder.codepoints().to_vec();
            let used = cps.len();
            self.app.walked = Walked::Codepoints(cps.clone());
            let mut rest: &[u32] = &cps;
            loop {
                let encoded = self.coding.encoder.encode(&mut rest);
                let bytes = self.coding.encoder.bytes().to_vec();
                if !process_chunk(req, &mut self.app, self.doc, &bytes, used)? {
                    return Ok(false);
                }
                self.coding.encoder.clear();
                if encoded != Status::SmallBuffer {
                    break;
                }
            }
            self.coding.decoder.clear();
            if decoded != Status::SmallBuffer {
                break;
            }
        }
        Ok(true)
    }

    /// `dom_parse_decode_encode_finish`.
    fn finish(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        self.coding.decoder.finish();
        let cps = self.coding.decoder.codepoints().to_vec();
        if !cps.is_empty() {
            let mut rest: &[u32] = &cps;
            self.coding.encoder.encode(&mut rest);
        }
        self.coding.encoder.finish();
        let bytes = self.coding.encoder.bytes().to_vec();
        if !bytes.is_empty() {
            let used = self.coding.decoder.codepoints().len();
            return process_chunk(req, &mut self.app, self.doc, &bytes, used);
        }
        Ok(true)
    }
}

/// `dom_should_register_error_handlers`: errors are reported unless the
/// options silence them.
fn reports_errors(options: i64) -> bool {
    options & NOERROR == 0
}

/// How the source reached the parser, which decides php's chunks and where
/// the reporter's walk of each starts.
enum Input<'a> {
    /// `createFromString`: the string, fed in windows of 4,096 bytes after
    /// any byte order mark, the walk starting where the window would
    /// without the mark.
    String(&'a [u8]),
    /// `createFromFile`: what each read of the stream answered, in order,
    /// the walk starting at each read's first byte.
    File(&'a [u8], Vec<usize>),
}

/// The parse shared by `createFromString` and `createFromFile`: the
/// source fed through the decoder in php's chunks into `doc`, the
/// document ended; the encoding it was read in. Every lexbor call is
/// under a lend of the request (ADR-0014), so what the parse allocates is
/// the request's.
fn parse(
    req: &mut Request,
    doc: &mut HtmlDocument,
    name: &[u8],
    input: &Input<'_>,
    options: i64,
    override_encoding: Option<Encoding>,
) -> Result<Encoding, Thrown> {
    let source = match input {
        Input::String(s) | Input::File(s, _) => *s,
    };
    if lexbor::lent(req, |lent| doc.parse_chunk_begin(lent))? != Status::Ok {
        return Err(dom_throw(req, Code::InvalidState));
    }
    let app = App {
        name,
        source,
        walked: Walked::Bytes(0),
        input_length: 0,
        total_offset: 0,
        cache: LineColumn {
            line: 1,
            column: 1,
            offset: 0,
        },
        no_implied: options & NOIMPLIED != 0,
        report: reports_errors(options),
        tokenizer_index: 0,
        tree_index: 0,
    };
    let fail = |req: &mut Request| dom_throw(req, Code::InvalidState);
    let utf_8 = Encoding::utf_8().ok_or_else(|| fail(req))?;
    // The encoding the decoder is set up with before any read:
    // `dom_decoding_encoding_ctx_init`'s UTF-8, or the override.
    let initial = override_encoding.unwrap_or(utf_8);
    let coding = Coding::new(initial).ok_or_else(|| fail(req))?;
    let mut feed = Feed { app, doc, coding };
    let mut encoding = initial;
    match input {
        Input::String(source) => {
            let mut from = 0;
            if override_encoding.is_none() {
                let found = lexbor::lent(req, |lent| determine_encoding(lent, source))?;
                let (found, bom) = found.ok_or_else(|| fail(req))?;
                encoding = found;
                feed.coding = Coding::new(found).ok_or_else(|| fail(req))?;
                from = bom;
            }
            let mut walk = 0;
            while from < source.len() {
                let end = source.len().min(from + BUFFER);
                if feed.coding.fast {
                    feed.app.walked = Walked::Bytes(walk);
                }
                if !feed.step(req, from, end)? {
                    return Err(fail(req));
                }
                walk += end - from;
                from = end;
            }
        }
        Input::File(_, chunks) => {
            let mut determine = override_encoding.is_none();
            let mut read_start = 0;
            for &len in chunks {
                let end = read_start + len;
                let mut from = read_start;
                if determine {
                    determine = false;
                    let head = source.get(from..end).unwrap_or_default();
                    let found = lexbor::lent(req, |lent| determine_encoding(lent, head))?;
                    let (found, bom) = found.ok_or_else(|| fail(req))?;
                    encoding = found;
                    feed.coding = Coding::new(found).ok_or_else(|| fail(req))?;
                    from += bom;
                }
                if feed.coding.fast {
                    feed.app.walked = Walked::Bytes(read_start);
                }
                if !feed.step(req, from, end)? {
                    return Err(fail(req));
                }
                read_start = end;
            }
        }
    }
    if !feed.finish(req)? || lexbor::lent(req, |lent| feed.doc.parse_chunk_end(lent))? != Status::Ok
    {
        return Err(fail(req));
    }
    Ok(encoding)
}

/// `lxb_html_document_create` under a lend, the document then kept
/// across `call` and destroyed under a lend on every path out, as
/// `html_document.c` destroys it on each.
///
/// Every lexbor call of the parse and the bridge is under a lend
/// (ADR-0014), so what lexbor holds is the request's memory as php's is:
/// a parse grows `memory_get_peak_usage()` by php's number, for a document
/// of one element as for one of twenty thousand, and meets
/// `memory_limit` where php's does, on php's request.
///
/// ```php
/// $peak = function ($n) {
///     $html = str_repeat('<div class=a><p>hello <b>world</b></p></div>', $n);
///     $m = memory_get_usage(); memory_reset_peak_usage();
///     $d = Dom\HTMLDocument::createFromString($html, LIBXML_NOERROR);
///     return memory_get_peak_usage() - $m;
/// };
/// var_dump($peak(1), $peak(20000));
/// ini_set('memory_limit', '8M');
/// Dom\HTMLDocument::createFromString(str_repeat('<div class=a><p>hello <b>world</b></p></div>', 20000), LIBXML_NOERROR);
/// echo "parsed\n";
/// ```
/// ```expect capture exit=255
/// int(1172688)
/// int(31525256)
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %SCRATCH%/witness.php on line 10
/// ```
fn with_document<R>(
    req: &mut Request,
    call: impl FnOnce(&mut Request, &mut HtmlDocument) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let Some(doc) = lexbor::lent(req, HtmlDocument::new)? else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    lexbor::kept(req, doc, call)
}

// ---- the bridge

/// One item of `lexbor_libxml2_bridge_convert`'s work list, php's
/// `work_list_item`, of its size, which the list's growth is counted in.
#[derive(Clone, Copy)]
struct Work<'doc> {
    node: HtmlNode<'doc>,
    active: Option<Namespace>,
    parent: Node,
    ns: Option<Ns>,
}

const _: () = assert!(size_of::<Work<'static>>() == 32, "sizeof(work_list_item)");

/// `WORK_LIST_INIT_SIZE`.
const WORK_LIST_INIT_SIZE: usize = 128;

/// `lexbor_libxml2_bridge_status` where the bridge failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failed {
    /// `LEXBOR_LIBXML2_BRIDGE_STATUS_OOM`: lexbor or libxml2 refused an
    /// allocation.
    Oom,
    /// `LEXBOR_LIBXML2_BRIDGE_STATUS_OVERFLOW`: a text, or an attribute's
    /// local name or value, of `INT_MAX` bytes or more.
    Overflow,
}

impl Failed {
    /// `dom_lexbor_libxml2_bridge_status_code_to_string`.
    fn as_str(self) -> &'static str {
        match self {
            Failed::Oom => "out of memory",
            Failed::Overflow => "string length overflow",
        }
    }

    /// php's `>= INT_MAX` test of a length libxml2's `int` is to hold:
    /// `Overflow` where it cannot.
    fn length(len: usize) -> Result<(), Failed> {
        if i32::try_from(len).is_ok_and(|len| len < i32::MAX) {
            Ok(())
        } else {
            Err(Failed::Overflow)
        }
    }
}

/// What a step of the bridge answers: the bridge's status, or the memory
/// limit's fatal.
type Bridged<T> = Result<Result<T, Failed>, Thrown>;

/// The declarations the bridge makes once for the attributes of a whole
/// conversion: `xlink_ns`, `xml_ns` and `prefixed_xmlns_ns`.
#[derive(Default)]
struct AttrNs {
    xlink: Option<Ns>,
    xml: Option<Ns>,
    prefixed_xmlns: Option<Ns>,
}

/// The bridge's attribute loop: each of `node`'s attributes made on
/// `element` in order, a local name without its prefix, its value one
/// text child, its namespace the mapper's, and an HTML `id` an ID; the
/// bridge's status where a length overflows or libxml refused a node, and
/// the `Err` is the memory limit, met where the mapper makes a declaration.
/// Only a prefixed attribute takes the `XLink`, XML or XMLNS namespace, and
/// any `id` in no namespace is an ID.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString('<!DOCTYPE html><svg id="s" xlink:href="#a" xml:lang="en" xmlns:xlink="urn:x"/><math id="m"></math>', LIBXML_NOERROR);
/// var_dump($d->getElementById('s')?->tagName, $d->getElementById('m')?->tagName);
/// foreach ($d->getElementById('s')->attributes as $a) { echo $a->name, ' => ', var_export($a->namespaceURI, true), "\n"; }
/// $f = Dom\HTMLDocument::createEmpty();
/// $c = $f->appendChild($f->createElementNS('http://www.w3.org/1999/xlink', 'z'));
/// $c->innerHTML = '<z id="q" xlink:href="#a"></z>';
/// echo $c->innerHTML, "\n";
/// var_dump($f->getElementById('q')?->tagName);
/// ```
/// ```expect
/// string(3) "svg"
/// string(4) "math"
/// id => NULL
/// xlink:href => 'http://www.w3.org/1999/xlink'
/// xml:lang => 'http://www.w3.org/XML/1998/namespace'
/// xmlns:xlink => 'http://www.w3.org/2000/xmlns/'
/// <z id="q" xlink:href="#a"></z>
/// string(1) "Z"
/// ```
fn convert_attributes(
    req: &mut Request,
    mapper: &mut Mapper,
    doc: Node,
    node: HtmlNode<'_>,
    element: Node,
    attr_ns: &mut AttrNs,
) -> Bridged<()> {
    let mut last: Option<Node> = None;
    for attr in node.attributes() {
        let mut local = attr.qualified_name();
        if attr.has_prefix()
            && let Some(colon) = local.iter().position(|b| *b == b':')
        {
            local = local.get(colon + 1..).unwrap_or_default();
        }
        let value = attr.value();
        if let Err(failed) = Failed::length(local.len()).and(Failed::length(value.len())) {
            return Ok(Err(failed));
        }
        let Some(lattr) = Node::new_attr(Some(doc), local, None) else {
            return Ok(Err(Failed::Oom));
        };
        let Some(text) = Node::new_text_len(Some(doc), value) else {
            lattr.free();
            return Ok(Err(Failed::Oom));
        };
        lattr.add_child(text);
        lattr.set_parent(Some(element));
        // Only a prefixed attribute is in the XLink, XML or XMLNS namespace,
        // `xmlns` itself aside: an unprefixed one in foreign content is in
        // none (php 8.5.12), whatever namespace lexbor's context gave it.
        let prefixed = attr.has_prefix();
        let ns = match attr.namespace() {
            Namespace::Xmlns if local == b"xmlns" => mapper.prefixless_xmlns(req, Some(doc))?,
            Namespace::Xmlns if prefixed => {
                if attr_ns.prefixed_xmlns.is_none() {
                    attr_ns.prefixed_xmlns = mapper.get_raw(
                        req,
                        Some(doc),
                        Some(b"xmlns"),
                        Some(crate::document::XMLNS_NS),
                    )?;
                }
                attr_ns.prefixed_xmlns
            }
            Namespace::XLink if prefixed => {
                if attr_ns.xlink.is_none() {
                    attr_ns.xlink =
                        mapper.get_raw(req, Some(doc), Some(b"xlink"), Some(XLINK_NS))?;
                }
                attr_ns.xlink
            }
            Namespace::Xml if prefixed => {
                if attr_ns.xml.is_none() {
                    attr_ns.xml = mapper.get_raw(
                        req,
                        Some(doc),
                        Some(b"xml"),
                        Some(crate::document::XML_NS),
                    )?;
                }
                attr_ns.xml
            }
            _ => None,
        };
        lattr.set_ns(ns);
        match last {
            None => element.set_properties(Some(lattr)),
            Some(prev) => {
                prev.set_next(Some(lattr));
                lattr.set_prev(Some(prev));
            }
        }
        last = Some(lattr);
        // Any `id` in no namespace is an ID, an SVG or MathML element's too.
        if local == b"id" && ns.is_none() && !lattr.add_id(doc, value) {
            lattr.set_atype(php_ext_libxml::ffi::XML_ATTRIBUTE_ID);
        }
    }
    Ok(Ok(()))
}

/// `lexbor_libxml2_bridge_convert`: `start` and the siblings before it,
/// and all their descendants, made libxml2 nodes under `root`, the
/// namespaces the mapper of `private`'s, each template's contents a
/// fragment it keeps. The mapper's HTML namespace comes first, then the
/// work list php keeps on lexbor's allocator, which is the request's
/// memory as php's is: each lexbor call is under a lend of its own
/// (ADR-0014), so the mapper reaches the request between them. Answers
/// the template elements whose namespaces `dom_add_element_ns_hook` tags,
/// which the caller tags through the document's object
/// ([`tag_templates`]); the status where the bridge failed, and the
/// `Err` is the memory limit's fatal.
fn bridge(
    req: &mut Request,
    private: &mut Private,
    start: Option<HtmlNode<'_>>,
    doc: Node,
    root: Node,
    create_default_ns: bool,
) -> Bridged<Vec<Node>> {
    let html_ns = private.mapper.html(req, Some(doc))?;
    let Some(work) = lexbor::lent(req, |lent| ArrayObj::new(lent, WORK_LIST_INIT_SIZE))? else {
        return Ok(Err(Failed::Oom));
    };
    let to = Into {
        doc,
        root,
        html_ns,
        create_default_ns,
    };
    lexbor::kept(req, work, |req, work| {
        convert(req, private, work, start, &to)
    })
}

/// What the bridge converts into: the document, the node the top items
/// hang from, the mapper's HTML namespace, and whether elements are put
/// in their namespaces at all.
struct Into {
    doc: Node,
    root: Node,
    html_ns: Option<Ns>,
    create_default_ns: bool,
}

/// `lexbor_libxml2_bridge_work_list_item_push`, under a lend of its own;
/// `Oom` where lexbor could not grow the list, which php's writes through.
fn push<'doc>(req: &mut Request, work: &mut ArrayObj<Work<'doc>>, item: Work<'doc>) -> Bridged<()> {
    let pushed = lexbor::lent(req, |lent| work.push(lent, item))?;
    Ok(if pushed { Ok(()) } else { Err(Failed::Oom) })
}

/// What the walk has made for the whole conversion: the attributes'
/// declarations, and the template elements in the order they were made.
#[derive(Default)]
struct Made {
    attr_ns: AttrNs,
    templates: Vec<Node>,
}

/// The bridge's walk of the work list.
fn convert<'doc>(
    req: &mut Request,
    private: &mut Private,
    work: &mut ArrayObj<Work<'doc>>,
    start: Option<HtmlNode<'doc>>,
    to: &Into,
) -> Bridged<Vec<Node>> {
    let doc = to.doc;
    let mut made = Made::default();
    let mut at = start;
    while let Some(node) = at {
        let item = Work {
            node,
            active: None,
            parent: to.root,
            ns: None,
        };
        if let Err(failed) = push(req, work, item)? {
            return Ok(Err(failed));
        }
        at = node.prev();
    }
    while let Some(item) = work.pop() {
        let node = item.node;
        let converted = match node.kind() {
            NodeKind::Element => convert_element(req, private, work, item, to, &mut made)?,
            NodeKind::Text => Failed::length(node.data().len()).and_then(|()| {
                let text = Node::new_text_len(Some(doc), node.data()).ok_or(Failed::Oom)?;
                item.parent.add_child(text);
                text.set_line(node.line());
                Ok(())
            }),
            NodeKind::DocumentType => {
                let public = node.doctype_public_id();
                let system = node.doctype_system_id();
                Node::create_int_subset(
                    Some(doc),
                    node.doctype_name(),
                    (!public.is_empty()).then_some(public),
                    (!system.is_empty()).then_some(system),
                )
                .map(drop)
                .ok_or(Failed::Oom)
            }
            NodeKind::Comment => {
                let data = php_ext::coerce::c_str(node.data());
                Node::new_comment(Some(doc), data)
                    .map(|comment| {
                        item.parent.add_child(comment);
                        comment.set_line(node.line());
                    })
                    .ok_or(Failed::Oom)
            }
            _ => Ok(()),
        };
        if let Err(failed) = converted {
            return Ok(Err(failed));
        }
    }
    Ok(Ok(made.templates))
}

/// One element of the walk: made under its parent in the namespace it
/// enters, a template's contents a fragment of their own, its children
/// pushed last first, then its attributes.
fn convert_element<'doc>(
    req: &mut Request,
    private: &mut Private,
    work: &mut ArrayObj<Work<'doc>>,
    item: Work<'doc>,
    to: &Into,
    made: &mut Made,
) -> Bridged<()> {
    let (doc, node) = (to.doc, item.node);
    let Some(element) = Node::new_element(Some(doc), None, node.qualified_name(), None) else {
        return Ok(Err(Failed::Oom));
    };
    item.parent.add_child(element);
    element.set_line(node.line());
    let entering = node.namespace();
    let mut current = item.ns;
    if to.create_default_ns && Some(entering) != item.active {
        current = if entering == Namespace::Html {
            to.html_ns
        } else {
            let uri = match entering {
                Namespace::Svg => SVG_NS,
                Namespace::MathMl => MATHML_NS,
                _ => HTML_NS,
            };
            private
                .mapper
                .get_ns(req, Some(doc), Handed::Empty, Handed::Made(uri))?
        };
    }
    element.set_ns(current);
    let mut child_parent = element;
    let mut child = node.last_child();
    if node.is_template() {
        if to.create_default_ns {
            let Some(fragment) = Node::new_fragment(Some(doc)) else {
                return Ok(Err(Failed::Oom));
            };
            fragment.set_parent(Some(element));
            made.templates.push(element);
            if let Err(thrown) = private.add_template(req, element, fragment) {
                fragment.free();
                return Err(thrown);
            }
            child_parent = fragment;
        }
        if let Some(content) = node.template_content() {
            child = content.last_child();
        }
    }
    while let Some(c) = child {
        let item = Work {
            node: c,
            active: Some(entering),
            parent: child_parent,
            ns: current,
        };
        if let Err(failed) = push(req, work, item)? {
            return Ok(Err(failed));
        }
        child = c.prev();
    }
    convert_attributes(
        req,
        &mut private.mapper,
        doc,
        node,
        element,
        &mut made.attr_ns,
    )
}

/// `dom_add_element_ns_hook` for each template element the bridge made,
/// through the document's object, whose reference the tag names.
fn tag_templates(obj: &mut NodeObject, templates: Vec<Node>) {
    for element in templates {
        obj.add_ns_tag(element);
    }
}

/// `dom_search_child`.
fn search_child(parent: Node, name: &[u8]) -> Option<Node> {
    let mut at = parent.children();
    while let Some(n) = at {
        if n.kind() == Kind::Element && n.name() == name {
            return Some(n);
        }
        at = n.next();
    }
    None
}

/// `dom_place_remove_element_and_hoist_children`.
fn hoist(parent: Node, name: &[u8]) {
    let Some(node) = search_child(parent, name) else {
        return;
    };
    node.unlink();
    while let Some(child) = node.children() {
        child.unlink();
        parent.add_child(child);
    }
    node.free();
}

/// `dom_post_process_html5_loading`: `LIBXML_HTML_NOIMPLIED` drops the
/// `head`, `body` and `html` the parser implied.
fn post_process(doc: Node, options: i64, seen: Observations) {
    if options & NOIMPLIED == 0 {
        return;
    }
    if let Some(html) = search_child(doc, b"html") {
        if !seen.has_explicit_head_tag {
            hoist(html, b"head");
        }
        if !seen.has_explicit_body_tag {
            hoist(html, b"body");
        }
    }
    if !seen.has_explicit_html_tag {
        hoist(doc, b"html");
    }
}

/// `dom_translate_quirks_mode`.
fn quirks_of(quirks: HtmlQuirks) -> Quirks {
    match quirks {
        HtmlQuirks::NoQuirks => Quirks::NoQuirks,
        HtmlQuirks::Quirks => Quirks::Quirks,
        HtmlQuirks::LimitedQuirks => Quirks::LimitedQuirks,
    }
}

/// `php_dom_create_html_doc`: an HTML document with no DTD.
fn new_html_doc(req: &mut Request) -> Result<Node, Thrown> {
    match Node::new_html_doc() {
        Some(doc) => Ok(doc),
        None => Err(dom_throw(req, Code::InvalidState)),
    }
}

/// What the bridge made of a parsed document, before its object is: the
/// tree, its private data, the template elements whose namespaces are
/// still to be tagged, and what the parser observed.
struct Converted {
    doc: Node,
    private: Private,
    templates: Vec<Node>,
    seen: Observations,
}

/// `lexbor_libxml2_bridge_convert_document` with the private data
/// `html_document.c` makes just before it: the parsed tree converted into
/// a fresh HTML document, and the parser's observations copied, while
/// lexbor's document still stands. Where the bridge fails, what it made
/// is given back and the status is `php_libxml_ctx_error`'s `"%s in %s"`
/// with the input's `name`, which ends in no newline and so waits in the
/// error buffer for the next error, as php's does; `None` then, which
/// php's caller answers as `false`. The `Err` is the memory limit.
fn convert_document(
    req: &mut Request,
    parsed: &HtmlDocument,
    options: i64,
    name: &[u8],
) -> Result<Option<Converted>, Thrown> {
    let mut private = Private::new(req)?;
    let create_default_ns = options & crate::modern_constants::HTML_NO_DEFAULT_NS == 0;
    let failed = match Node::new_html_doc() {
        Some(doc) => {
            let start = parsed.node().last_child();
            match bridge(req, &mut private, start, doc, doc, create_default_ns) {
                Ok(Ok(templates)) => {
                    return Ok(Some(Converted {
                        doc,
                        private,
                        templates,
                        seen: parsed.observations(),
                    }));
                }
                Ok(Err(failed)) => {
                    doc.free_document();
                    failed
                }
                Err(thrown) => {
                    doc.free_document();
                    private.free(req);
                    return Err(thrown);
                }
            }
        }
        None => Failed::Oom,
    };
    private.free(req);
    let message = [failed.as_str().as_bytes(), b" in ", name].concat();
    php_ext_libxml::Errors::ctx_error(req, &message);
    Ok(None)
}

/// The document object for a converted tree, made once lexbor's document
/// is given back, as `html_document.c` instantiates it last: the parser's
/// observations applied, the encoding and the URL set, then the object,
/// the template elements tagged through it.
///
/// So a document of one element costs the request php's memory, and its
/// parse peaks where php's does:
///
/// ```php
/// $f = fn () => Dom\HTMLDocument::createFromString('<p>x</p>', LIBXML_NOERROR);
/// $d = $f(); unset($d);
/// $m = memory_get_usage(); memory_reset_peak_usage();
/// $d = $f();
/// echo memory_get_usage() - $m, ' ', memory_get_peak_usage() - $m, "\n";
/// unset($d);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 1064 1172688
/// 32
/// ```
fn finish(
    req: &mut Request,
    converted: Converted,
    encoding: Encoding,
    options: i64,
    url: Option<Vec<u8>>,
) -> Result<Owner<Value>, Thrown> {
    let Converted {
        doc,
        private,
        templates,
        seen,
    } = converted;
    post_process(doc, options, seen);
    doc.set_doc_string(DocString::Encoding, Some(encoding.name()));
    if let Some(url) = url {
        doc.set_doc_string(DocString::Url, Some(&url));
    }
    let object = match crate::modern::document_object(req, "Dom\\HTMLDocument", doc, private) {
        Ok(object) => object,
        Err(thrown) => {
            doc.free_document();
            return Err(thrown);
        }
    };
    if let Some(mut obj) = crate::libxml_half(req, *object) {
        tag_templates(&mut obj, templates);
        obj.set_quirks(quirks_of(seen.quirks));
    }
    Ok(object.into_value())
}

/// The override encoding argument, `None` where none was given.
fn override_encoding(
    req: &mut Request,
    position: u32,
    given: Option<PhpString>,
) -> Result<Option<Encoding>, Thrown> {
    let Some(name) = given.map(|n| n.as_bytes().to_vec()) else {
        return Ok(None);
    };
    if name.contains(&0) {
        return Err(arg_value_error(req, position, "not contain any null bytes"));
    }
    match Encoding::by_name(&name) {
        Some(e) => Ok(Some(e)),
        None => Err(arg_value_error(
            req,
            position,
            "be a valid document encoding",
        )),
    }
}

/// `dom_html_fragment_parsing_algorithm`'s parse and bridge: `input`
/// parsed as the children of an element named like `context` in its
/// namespace, in the document's quirks, and converted into a fragment of
/// `context`'s document.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString('<!doctype html><template id=t><b>x</b></template>');
/// $t = $d->getElementById('t');
/// var_dump($t->childNodes->length, $t->innerHTML);
/// $t->insertAdjacentHTML(Dom\AdjacentPosition::BeforeEnd, '<i>y</i>');
/// var_dump($t->innerHTML, $d->saveHtml($t));
/// ```
/// ```expect
/// int(0)
/// string(8) "<b>x</b>"
/// string(8) "<b>x</b>"
/// string(36) "<template id="t"><b>x</b></template>"
/// ```
pub(crate) fn parse_fragment(
    req: &mut Request,
    object: PhpObject,
    context: Node,
    input: &[u8],
) -> Result<Node, Thrown> {
    let quirks = match crate::dom(req, object).map(|d| d.obj.quirks()) {
        Some(Quirks::Quirks) => HtmlQuirks::Quirks,
        Some(Quirks::LimitedQuirks) => HtmlQuirks::LimitedQuirks,
        _ => HtmlQuirks::NoQuirks,
    };
    let ns_uri = context.ns().and_then(Ns::href).unwrap_or_default();
    let name = context.name();
    let parsed = lexbor::lent(req, |lent| {
        parse_fragment_input(lent, &name, &ns_uri, quirks, input)
    })?;
    let Some(fragment) = parsed else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    lexbor::kept(req, fragment, |req, fragment| {
        let Some(doc) = context.doc() else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        let Some(root) = Node::new_fragment(Some(doc)) else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        let start = fragment.root().last_child();
        let mut obj = crate::libxml_half(req, object);
        let bridged = match obj.as_deref_mut().and_then(crate::private::of) {
            Some(private) => bridge(req, private, start, doc, root, true),
            None => Ok(Err(Failed::Oom)),
        };
        match bridged {
            Ok(Ok(templates)) => {
                if let Some(obj) = obj.as_deref_mut() {
                    tag_templates(obj, templates);
                }
                Ok(root)
            }
            Ok(Err(_)) => {
                root.free();
                Err(dom_throw(req, Code::InvalidState))
            }
            Err(thrown) => {
                root.free();
                Err(thrown)
            }
        }
    })
}

/// The fragment parse of `dom_html_fragment_parsing_algorithm`, under one
/// lend: `input` decoded as UTF-8, each bad sequence handed on as the
/// replacement bytes, into a fragment parser under an element named
/// `name` in `ns_uri`; `None` where lexbor refused a step, what it made
/// given back.
fn parse_fragment_input(
    lent: &Lent,
    name: &[u8],
    ns_uri: &[u8],
    quirks: HtmlQuirks,
    input: &[u8],
) -> Option<Fragment> {
    let mut parser = FragmentParser::begin(lent, name, ns_uri, quirks)?;
    let Some(mut decoder) = Encoding::utf_8().map(Decoder::single) else {
        parser.give_back(lent);
        return None;
    };
    let mut rest = input;
    let mut last_output = 0;
    let mut failed = false;
    while !rest.is_empty() {
        let backup = input.len() - rest.len();
        let cp = decoder.utf_8_single(&mut rest);
        if cp > MAX_CODEPOINT {
            let at = input.len() - rest.len();
            let before = input.get(last_output..backup).unwrap_or_default();
            failed |= parser.chunk(lent, before) != Status::Ok;
            failed |= parser.chunk(lent, REPLACEMENT_BYTES) != Status::Ok;
            last_output = at;
        }
    }
    if last_output != input.len() {
        let tail = input.get(last_output..).unwrap_or_default();
        failed |= parser.chunk(lent, tail) != Status::Ok;
    }
    let fragment = parser.end(lent)?;
    if failed {
        fragment.give_back(lent);
        return None;
    }
    Some(fragment)
}

/// `Dom\HTMLDocument::createFromString`'s body.
pub(crate) fn create_from_string(
    req: &mut Request,
    source: &[u8],
    options: i64,
    given_encoding: Option<PhpString>,
) -> Result<Owner<Value>, Thrown> {
    check_options(req, 2, options)?;
    let encoding = override_encoding(req, 3, given_encoding)?;
    let (converted, found) = with_document(req, |req, doc| {
        let found = parse(
            req,
            doc,
            b"Entity",
            &Input::String(source),
            options,
            encoding,
        )?;
        Ok((convert_document(req, doc, options, b"Entity")?, found))
    })?;
    match converted {
        Some(converted) => finish(req, converted, found, options, None),
        None => Ok(Owner::bool(false)),
    }
}

/// `Dom\HTMLDocument::createFromFile`'s body. A file that does not
/// open is php's `Cannot open file '%s'`, the name's bytes as given:
///
/// ```php
/// try { @Dom\HTMLDocument::createFromFile("/nonexistent/\xff.html"); } catch (Exception $e) { echo bin2hex($e->getMessage()), "\n"; }
/// ```
/// ```expect exit=0
/// 43616e6e6f74206f70656e2066696c6520272f6e6f6e6578697374656e742fff2e68746d6c27
/// ```
pub(crate) fn create_from_file(
    req: &mut Request,
    path: &[u8],
    options: i64,
    given_encoding: Option<PhpString>,
) -> Result<Owner<Value>, Thrown> {
    let path = path.to_vec();
    if path.contains(&0) {
        return Err(arg_value_error(req, 1, "not contain any null bytes"));
    }
    if path.windows(3).any(|w| w == b"%00") {
        return Err(arg_value_error(
            req,
            1,
            "not contain percent-encoded NUL bytes",
        ));
    }
    check_options(req, 2, options)?;
    let encoding = override_encoding(req, 3, given_encoding)?;
    let Some(stream) = req.open_stream(&path, b"rb", false)? else {
        // `zend_throw_exception_ex`'s `%s`: the name's bytes as they are.
        let message = [&b"Cannot open file '"[..], &path, b"'"].concat();
        return Err(throw_exception_ex(req, "Exception", &message));
    };
    // The MIME sniff: the charset of the http headers the stream carries,
    // where no encoding was given and lexbor knows the one named.
    let mut encoding = encoding;
    if encoding.is_none() {
        match php_ext_libxml::sniff_charset_from_stream(req, *stream) {
            Ok(Some(charset)) => encoding = Encoding::by_name(&charset),
            Ok(None) => {}
            Err(thrown) => {
                stream.free(req);
                return Err(thrown);
            }
        }
    }
    let mut source = Vec::new();
    let mut chunks = Vec::new();
    let read = loop {
        match stream.read(req, BUFFER) {
            Ok(Some(chunk)) => {
                let len = chunk.len();
                source.extend_from_slice(chunk.as_bytes());
                chunk.give_up(req);
                if len == 0 {
                    break Ok(());
                }
                chunks.push(len);
            }
            Ok(None) => break Ok(()),
            Err(thrown) => break Err(thrown),
        }
    };
    stream.free(req);
    read?;
    let (converted, found) = with_document(req, |req, doc| {
        let input = Input::File(&source, chunks);
        let found = parse(req, doc, &path, &input, options, encoding)?;
        Ok((convert_document(req, doc, options, &path)?, found))
    })?;
    let Some(converted) = converted else {
        return Ok(Owner::bool(false));
    };
    let url = file_url(req, &path);
    finish(req, converted, found, options, Some(url))
}

/// The document's URL: a stream's path as given, a file's absolute path
/// as libxml's `xmlPathToURI` writes it, under `file://`.
fn file_url(req: &mut Request, path: &[u8]) -> Vec<u8> {
    if crate::modern::is_stream_path(path) {
        path.to_vec()
    } else {
        let absolute = req.environment_mut().realpath(path).map_or_else(
            || path.to_vec(),
            |p| p.to_string_lossy().into_owned().into_bytes(),
        );
        let uri = php_ext_libxml::ffi::Library::path_to_uri(&absolute).unwrap_or(absolute);
        if uri.starts_with(b"file:/") {
            uri
        } else {
            [b"file://".as_slice(), &uri].concat()
        }
    }
}

/// `Dom\HTMLDocument::createEmpty`'s body.
pub(crate) fn create_empty(req: &mut Request, encoding: &[u8]) -> Result<Owner<PhpObject>, Thrown> {
    if Encoding::by_name(encoding).is_none() {
        return Err(arg_value_error(req, 1, "be a valid document encoding"));
    }
    let doc = new_html_doc(req)?;
    doc.set_doc_string(DocString::Encoding, Some(encoding));
    let private = crate::private::Private::new(req)?;
    crate::modern::document_object(req, "Dom\\HTMLDocument", doc, private)
}
