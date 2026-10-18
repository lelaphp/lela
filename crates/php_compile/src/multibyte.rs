//! `zend.multibyte` in the scanner (design §4.9, §6.1): php's
//! `Z/Zend/zend_multibyte.c` as the scanner calls it, and the multibyte
//! half of `Z/Zend/zend_language_scanner.l` and of the parser's
//! `declare` rule. A file is read in its own encoding and handed to
//! the lexer in one it can read: the file's encoding is found
//! ([`open`]: a byte order mark, NULs, `zend.script_encoding`), the
//! filters are chosen by which of the file's and the internal
//! encodings the lexer can read ([`Filter::choose`]), the source filter
//! converts the whole file before it is scanned and the token filter
//! each name and string as it is handed to the parser
//! ([`php_syntax::filter_tokens`]). A `declare(encoding=...)`
//! re-reads the rest of the file in the encoding it names, as the
//! parser's mid-rule action does (`zend_handle_encoding_declaration`),
//! and `__COMPILER_HALT_OFFSET__` is the offset in the file as read
//! (`zend_get_scanned_file_offset`).
//!
//! The encodings are the functions an extension hands the engine
//! ([`Encodings`], php's `zend_multibyte_functions`), which the engine
//! binds for each compile; with none, php's own dummies answer
//! ([`Dummy`]), under which no encoding is fetched and no conversion
//! succeeds.

use std::borrow::Cow;

use php_syntax::tree::{self, DeclareStmt, TokenId};
use php_syntax::{Arena, Holder, Parsed, Span, SyntaxKind, TokenValue};

use crate::{CompileError, Level, Options};

/// One encoding, as the functions that fetched it name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Encoding(pub u32);

/// The five encodings Zend fetches by name as the functions are set
/// (`zend_multibyte_set_functions`): the byte order marks' and the
/// intermediate one the filters pass through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unicode {
    Utf32Be,
    Utf32Le,
    Utf16Be,
    Utf16Le,
    Utf8,
}

/// php's `zend_multibyte_functions` as the scanner calls them, bound to
/// the request the source compiles for.
pub trait Encodings {
    /// `encoding_fetcher`: the encoding a name names.
    fn fetch(&self, name: &[u8]) -> Option<Encoding>;
    /// `zend_multibyte_encoding_utf8` and its four kin.
    fn unicode(&self, which: Unicode) -> Encoding;
    /// `encoding_name_getter`.
    fn name(&self, encoding: Encoding) -> Vec<u8>;
    /// `lexer_compatibility_checker`: whether the lexer reads the
    /// encoding's bytes as its own.
    fn lexer_compatible(&self, encoding: Encoding) -> bool;
    /// `encoding_detector` over `list`, the source encodings named.
    fn detect(&self, text: &[u8], list: &[Encoding]) -> Option<Encoding>;
    /// `encoding_converter`: `None` is its `(size_t)-1`.
    fn convert(&self, text: &[u8], to: Encoding, from: Encoding) -> Option<Vec<u8>>;
    /// `internal_encoding_getter`.
    fn internal(&self) -> Option<Encoding>;
    /// `encoding_list_parser` over a comma-separated list, what
    /// `zend.script_encoding` holds: the encodings in order, or the first
    /// name that names none.
    ///
    /// # Errors
    ///
    /// The name the functions could not fetch.
    fn list(&self, names: &[u8]) -> Result<Vec<Encoding>, Vec<u8>>;
}

/// Zend's dummy functions (`Z/Zend/zend_multibyte.c:25-80`), what the
/// scanner calls where no extension set any: nothing fetched, nothing
/// detected or converted, no internal encoding, and the five Unicode
/// encodings their own names, the UTF-16LE one spelled `UTF-32LE` as
/// php spells it.
#[derive(Debug, Clone, Copy)]
pub struct Dummy;

impl Encodings for Dummy {
    fn fetch(&self, _: &[u8]) -> Option<Encoding> {
        None
    }

    fn unicode(&self, which: Unicode) -> Encoding {
        Encoding(which as u32)
    }

    fn name(&self, encoding: Encoding) -> Vec<u8> {
        let name: &[u8] = match encoding.0 {
            0 => b"UTF-32BE",
            1 | 3 => b"UTF-32LE",
            2 => b"UTF-16BE",
            _ => b"UTF-8",
        };
        name.to_vec()
    }

    fn lexer_compatible(&self, _: Encoding) -> bool {
        false
    }

    fn detect(&self, _: &[u8], _: &[Encoding]) -> Option<Encoding> {
        None
    }

    fn convert(&self, _: &[u8], _: Encoding, _: Encoding) -> Option<Vec<u8>> {
        None
    }

    fn internal(&self) -> Option<Encoding> {
        None
    }

    fn list(&self, _: &[u8]) -> Result<Vec<Encoding>, Vec<u8>> {
        Ok(Vec::new())
    }
}

/// One of php's four encoding filters (`encoding_filter_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    SourceToInternal,
    SourceToIntermediate,
    IntermediateToSource,
    IntermediateToInternal,
}

/// `LANG_SCNG(script_encoding)` with its source and token filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filter {
    pub encoding: Encoding,
    pub source: Option<Step>,
    pub tokens: Option<Step>,
}

impl Filter {
    /// `zend_multibyte_set_filter` once the file's encoding is known:
    /// no filter where it is the internal one (or there is none) and the
    /// lexer reads it, a round trip through UTF-8 where it does not; the
    /// whole file converted where the lexer reads the internal
    /// encoding, each value where it reads the file's alone, and both
    /// through UTF-8 where it reads neither.
    #[must_use]
    pub fn choose(encodings: &dyn Encodings, script: Encoding) -> Filter {
        let compatible = |e| encodings.lexer_compatible(e);
        let (input, output) = match encodings.internal() {
            None => Self::alone(compatible(script)),
            Some(internal) if internal == script => Self::alone(compatible(script)),
            Some(internal) if compatible(internal) => (Some(Step::SourceToInternal), None),
            Some(_) if compatible(script) => (None, Some(Step::SourceToInternal)),
            Some(_) => (
                Some(Step::SourceToIntermediate),
                Some(Step::IntermediateToInternal),
            ),
        };
        Filter {
            encoding: script,
            source: input,
            tokens: output,
        }
    }

    /// The filters of a file read as the internal encoding.
    fn alone(compatible: bool) -> (Option<Step>, Option<Step>) {
        if compatible {
            (None, None)
        } else {
            (
                Some(Step::SourceToIntermediate),
                Some(Step::IntermediateToSource),
            )
        }
    }

    /// One filter over `text`.
    #[must_use]
    pub fn convert(&self, encodings: &dyn Encodings, step: Step, text: &[u8]) -> Option<Vec<u8>> {
        let utf8 = encodings.unicode(Unicode::Utf8);
        let (to, from) = match step {
            Step::SourceToInternal => (encodings.internal()?, self.encoding),
            Step::SourceToIntermediate => (utf8, self.encoding),
            Step::IntermediateToSource => (self.encoding, utf8),
            Step::IntermediateToInternal => (encodings.internal()?, utf8),
        };
        encodings.convert(text, to, from)
    }
}

/// What the engine hands the compiler where `zend.multibyte` is on.
#[derive(Clone, Copy)]
pub struct Multibyte<'a> {
    /// The functions, bound to the request.
    pub encodings: &'a dyn Encodings,
    /// `zend.detect_unicode`, `CG(detect_unicode)`.
    pub detect_unicode: bool,
    /// `CG(script_encoding_list)`, what `zend.script_encoding` named.
    pub listed: &'a [Encoding],
}

impl std::fmt::Debug for Multibyte<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Multibyte")
            .field("detect_unicode", &self.detect_unicode)
            .field("listed", &self.listed)
            .finish_non_exhaustive()
    }
}

/// What the scanner holds of a file [`open`] read: `SCNG(script_org)`,
/// the file as read with its byte order mark taken off, which a re-read
/// converts again, and the filters chosen, none where no encoding was
/// found.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scanning<'p> {
    pub(crate) original: &'p [u8],
    pub(crate) filter: Option<Filter>,
}

/// A file [`open`] could not convert: php's `Could not convert the
/// file from the detected encoding "%s" to a compatible encoding`,
/// which `open_file_for_scanning` raises before anything compiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unconverted(pub Vec<u8>);

impl Unconverted {
    /// php's sentence.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "Could not convert the script from the detected encoding \"{}\" to a compatible encoding",
            String::from_utf8_lossy(&self.0)
        )
    }
}

/// What [`open`] made of a file: where its text starts past a byte
/// order mark, the filters, and the text the lexer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub bom: usize,
    pub filter: Option<Filter>,
    pub text: Vec<u8>,
}

/// Where the file's encoding comes from: a file's is found as
/// `zend_multibyte_find_script_encoding` finds it, and an `eval`'s
/// string is the internal encoding's (`zend_prepare_string_for_scanning`).
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    File {
        /// `zend.detect_unicode`.
        detect_unicode: bool,
        /// `CG(script_encoding_list)`, what `zend.script_encoding` named.
        listed: &'a [Encoding],
    },
    Eval,
}

/// `open_file_for_scanning` and `zend_prepare_string_for_scanning` under
/// `CG(multibyte)`: the file's encoding found, the filters chosen, and
/// the source filter passed over the file.
///
/// # Errors
///
/// [`Unconverted`] where the source filter fails.
pub fn open(
    encodings: &dyn Encodings,
    script: &[u8],
    source: Source<'_>,
) -> Result<Opened, Unconverted> {
    let (found, bom) = match source {
        Source::Eval => match encodings.internal() {
            Some(internal) => (Some(internal), 0),
            None => find(encodings, script, true, &[]),
        },
        Source::File {
            detect_unicode,
            listed,
        } => find(encodings, script, detect_unicode, listed),
    };
    let original = script.get(bom..).unwrap_or_default();
    let filter = found.map(|script| Filter::choose(encodings, script));
    let text = match filter {
        Some(f) => match f.source {
            Some(step) => f
                .convert(encodings, step, original)
                .ok_or_else(|| Unconverted(encodings.name(f.encoding)))?,
            None => original.to_vec(),
        },
        None => original.to_vec(),
    };
    Ok(Opened { bom, filter, text })
}

/// `zend_multibyte_find_script_encoding`: a byte order mark or NULs
/// under `zend.detect_unicode` first, then the one encoding listed, or
/// the one the detector picks of several; with the byte order mark's
/// length.
fn find(
    encodings: &dyn Encodings,
    script: &[u8],
    detect_unicode: bool,
    listed: &[Encoding],
) -> (Option<Encoding>, usize) {
    if detect_unicode && let Some(found) = detect_unicode_encoding(encodings, script) {
        return (Some(found.0), found.1);
    }
    let found = match listed {
        [] => None,
        [one] => Some(*one),
        several => encodings.detect(script, several),
    };
    (found, 0)
}

/// `zend_multibyte_detect_unicode`: a byte order mark, or, where the
/// file holds a NUL that is not after `__HALT_COMPILER();`, the
/// UTF-16 or UTF-32 its NULs suggest.
fn detect_unicode_encoding(encodings: &dyn Encodings, script: &[u8]) -> Option<(Encoding, usize)> {
    if script.len() < 4 {
        return None;
    }
    let boms: [(&[u8], Unicode); 5] = [
        (b"\x00\x00\xfe\xff", Unicode::Utf32Be),
        (b"\xff\xfe\x00\x00", Unicode::Utf32Le),
        (b"\xfe\xff", Unicode::Utf16Be),
        (b"\xff\xfe", Unicode::Utf16Le),
        (b"\xef\xbb\xbf", Unicode::Utf8),
    ];
    if let Some((bom, which)) = boms.iter().find(|(bom, _)| script.starts_with(bom)) {
        return Some((encodings.unicode(*which), bom.len()));
    }
    let nul = script.iter().position(|&b| b == 0)?;
    if halted_before(script, nul) {
        return None;
    }
    Some((encodings.unicode(wide_encoding(script)), 0))
}

/// Whether `__HALT_COMPILER();` stands before the first NUL, as the
/// search in `zend_multibyte_detect_unicode` finds it: a `_` then
/// `_HALT_COMPILER` in any case, then `(`, `)` and `;` with blanks
/// between.
fn halted_before(script: &[u8], nul: usize) -> bool {
    const NAME: &[u8] = b"_HALT_COMPILER";
    let at = |i: usize| script.get(i).copied().unwrap_or(0);
    let blanks = |mut i: usize| {
        while matches!(at(i), b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        i
    };
    let mut pos = 0;
    while nul.saturating_sub(pos) >= b"__HALT_COMPILER();".len() {
        let Some(underscore) = script[pos..nul].iter().position(|&b| b == b'_') else {
            break;
        };
        pos += underscore + 1;
        let named = script
            .get(pos..pos + NAME.len())
            .is_some_and(|word| word.eq_ignore_ascii_case(NAME));
        if !named {
            continue;
        }
        let open = blanks(pos + NAME.len());
        if at(open) != b'(' {
            continue;
        }
        let close = blanks(open + 1);
        if at(close) != b')' {
            continue;
        }
        if at(blanks(close + 1)) == b';' {
            return true;
        }
    }
    false
}

/// `zend_multibyte_detect_utf_encoding`: UTF-32 where two NULs follow a
/// NUL in the first four bytes of a unit, UTF-16 otherwise; big-endian
/// where the first unit that tells leads with its NUL. Past the end the
/// file reads as NULs, as php's padded buffer does.
fn wide_encoding(script: &[u8]) -> Unicode {
    let at = |i: usize| script.get(i).copied().unwrap_or(0);
    let mut wide = 2;
    let mut p = 0;
    while p < script.len() {
        let end = script.len().saturating_sub(2).max(p);
        let Some(found) = script[p..end].iter().position(|&b| b == 0) else {
            break;
        };
        p += found;
        if at(p + 1) == 0 && at(p + 2) == 0 {
            wide = 4;
            break;
        }
        p += 4;
    }
    let mut little = false;
    let mut p = 0;
    while p < script.len() {
        if at(p) == 0 && at(p + wide - 1) != 0 {
            break;
        }
        if at(p) != 0 && at(p + wide - 1) == 0 {
            little = true;
            break;
        }
        p += wide;
    }
    match (wide, little) {
        (2, true) => Unicode::Utf16Le,
        (2, false) => Unicode::Utf16Be,
        (_, true) => Unicode::Utf32Le,
        (_, false) => Unicode::Utf32Be,
    }
}

/// A fatal the parse-time pass raised, with where it stands among the
/// parse's diagnostics: the index of the first one it comes before.
pub(crate) struct Refusal {
    pub(crate) before: usize,
    pub(crate) level: Level,
    pub(crate) message: String,
    pub(crate) line: u32,
    pub(crate) thrown: bool,
}

impl Refusal {
    /// The fatal, with what was raised before it.
    pub(crate) fn error(&self, raised: Vec<php_unit::Diagnostic>) -> CompileError {
        CompileError {
            level: self.level,
            message: self.message.clone().into(),
            line: self.line,
            raised,
            thrown: self.thrown,
            stage: crate::Stage::Compiling,
        }
    }
}

/// What the parse-time pass made of a parse: the parse the compiler
/// walks, re-read and filtered where an encoding says so, its warnings
/// among its diagnostics; the fatal it stopped at, if any; and what
/// maps an offset in the text read back to the file.
pub(crate) struct Scanned<'p> {
    pub(crate) parsed: Cow<'p, Parsed<'p>>,
    pub(crate) refused: Option<Refusal>,
    halt: Option<Halt<'p>>,
}

/// The file as read and the source filter it was read through, which
/// `zend_get_scanned_file_offset` maps an offset back over.
#[derive(Clone, Copy)]
struct Halt<'p> {
    encodings: &'p dyn Encodings,
    original: &'p [u8],
    filter: Filter,
    step: Step,
}

impl Scanned<'_> {
    /// `zend_get_scanned_file_offset`: the offset in the file as read
    /// whose prefix the source filter converts to exactly `offset` bytes,
    /// found a byte at a time from `offset` itself; `offset` where no
    /// source filter stands.
    pub(crate) fn original_offset(&self, offset: i64) -> i64 {
        let Some(halt) = self.halt else {
            return offset;
        };
        let Ok(target) = usize::try_from(offset) else {
            return offset;
        };
        let mut at = target;
        for _ in 0..=halt.original.len().saturating_add(target) {
            let prefix = halt.original.get(..at).unwrap_or(halt.original);
            let Some(length) = halt
                .filter
                .convert(halt.encodings, halt.step, prefix)
                .map(|t| t.len())
            else {
                return offset;
            };
            match length.cmp(&target) {
                std::cmp::Ordering::Equal => return i64::try_from(at).unwrap_or(offset),
                std::cmp::Ordering::Greater => at = at.saturating_sub(1),
                std::cmp::Ordering::Less => at += 1,
            }
        }
        offset
    }
}

/// One `encoding` directive the parser reduced, in the order it did.
struct Declared {
    /// Where the `)` closing its `declare (...)` ends, which is where
    /// the scanner stood when the parser's action ran.
    close: u32,
    line: u32,
    value: tree::Expr,
}

/// `zend_handle_encoding_declaration`, served for each `declare` the
/// parser reduced before it stopped, in source order: the value held to
/// a written value (`Encoding must be a literal`, thrown); under
/// `zend.multibyte` the encoding it names fetched (`Unsupported
/// encoding [%s]`) and, where the filters it chooses read the script
/// differently, the rest of the file read again from where the
/// scanner stands (`zend_multibyte_yyinput_again`); with it off, php's
/// warning that the declaration is ignored. Then the token filter the
/// last one chose, over the parse.
pub(crate) fn scan<'p>(
    arena: &'p Arena,
    parsed: &'p Parsed<'p>,
    options: &Options<'p>,
    scanning: Option<Scanning<'p>>,
) -> Scanned<'p> {
    // No `declare` to read and no filter the file was opened through:
    // the parse stands as it is, and nothing is walked.
    if scanning.is_none_or(|s| s.filter.is_none()) && parsed.tree.nodes::<DeclareStmt>().is_empty()
    {
        return Scanned {
            parsed: Cow::Borrowed(parsed),
            refused: None,
            halt: None,
        };
    }
    scan_declared(arena, parsed, options, scanning)
}

/// [`scan`] over a parse with a `declare` to read or a filter the file
/// was opened through.
fn scan_declared<'p>(
    arena: &'p Arena,
    parsed: &'p Parsed<'p>,
    options: &Options<'p>,
    scanning: Option<Scanning<'p>>,
) -> Scanned<'p> {
    let scanning = scanning.unwrap_or(Scanning {
        original: parsed.source,
        filter: None,
    });
    let multibyte = options.multibyte;
    let mut current = Cow::Borrowed(parsed);
    let mut filter = scanning.filter;
    let mut warnings: Vec<(u32, php_syntax::Diagnostic)> = Vec::new();
    let mut read_to = 0;
    let mut refused = None;
    'reparse: loop {
        let parsed_to = current
            .diagnostics
            .iter()
            .find(|d| d.level == php_syntax::Level::Parse)
            .map_or(u32::MAX, |d| d.span.start);
        for declared in declared(&current) {
            if declared.close <= read_to || declared.close > parsed_to {
                continue;
            }
            read_to = declared.close;
            match read(&current, &declared, options, scanning.original, &mut filter) {
                Read::Kept => {}
                Read::Warned(message) => warnings.push((
                    declared.close,
                    php_syntax::Diagnostic::of(
                        php_syntax::Level::CompileWarning,
                        Span {
                            file: php_syntax::FileId::default(),
                            start: declared.close,
                            len: 0,
                        },
                        declared.line,
                        message,
                    ),
                )),
                Read::Refused { message, thrown } => {
                    refused = Some((declared.close, message, thrown));
                    break 'reparse;
                }
                Read::Again(reread) => {
                    let close = declared.close as usize;
                    let text = arena.concat(&[
                        current.source.get(..close).unwrap_or_default(),
                        reread.get(close..).unwrap_or_default(),
                    ]);
                    let syntax = crate::syntax_options(options);
                    current = Cow::Owned(php_syntax::parse(arena, text, &syntax));
                    continue 'reparse;
                }
            }
        }
        break;
    }
    let refused = refused.map(|(close, message, thrown)| {
        let line = current.lines.line_number(close.saturating_sub(1));
        let before = warnings.len() + position(&current.diagnostics, close);
        Refusal {
            before,
            level: Level::CompileError,
            message,
            line,
            thrown,
        }
    });
    if !warnings.is_empty() {
        let parsed = current.to_mut();
        for (close, warning) in warnings.into_iter().rev() {
            let at = position(&parsed.diagnostics, close);
            parsed.diagnostics.insert(at, warning);
        }
    }
    let halt = multibyte.zip(filter).and_then(|(multibyte, filter)| {
        filter.source.map(|step| Halt {
            encodings: multibyte.encodings,
            original: scanning.original,
            filter,
            step,
        })
    });
    if let Some((multibyte, filter)) = multibyte.zip(filter)
        && let Some(step) = filter.tokens
        && refused.is_none()
    {
        php_syntax::filter_tokens(current.to_mut(), &mut |text| {
            filter
                .convert(multibyte.encodings, step, text)
                .unwrap_or_else(|| text.to_vec())
        });
    }
    Scanned {
        parsed: current,
        refused,
        halt,
    }
}

/// What the parser's action did at one `encoding` directive.
enum Read {
    /// Nothing the scanner sees: the encoding was already the file's.
    Kept,
    Warned(String),
    Refused {
        message: String,
        thrown: bool,
    },
    /// The file as read, through the filter the directive chose, for
    /// the scanner to go on in from where it stands.
    Again(Vec<u8>),
}

/// `zend_handle_encoding_declaration` for one directive, the filter it
/// chose left in `filter`.
fn read(
    parsed: &Parsed,
    declared: &Declared,
    options: &Options<'_>,
    original: &[u8],
    filter: &mut Option<Filter>,
) -> Read {
    if crate::emit::written(parsed, declared.value).is_none() {
        return Read::Refused {
            message: "Encoding must be a literal".to_owned(),
            thrown: true,
        };
    }
    let Some(multibyte) = options.multibyte else {
        return Read::Warned(
            "declare(encoding=...) ignored because Zend multibyte feature is turned off by settings"
                .to_owned(),
        );
    };
    let name = string_of(parsed, declared.value, options.precision);
    let Some(named) = multibyte.encodings.fetch(&name) else {
        return Read::Warned(format!(
            "Unsupported encoding [{}]",
            String::from_utf8_lossy(&name)
        ));
    };
    let new = Filter::choose(multibyte.encodings, named);
    let old = filter.replace(new);
    let old_input = old.and_then(|f| f.source);
    let again =
        old_input != new.source || (old_input.is_some() && old.map(|f| f.encoding) != Some(named));
    if !again {
        return Read::Kept;
    }
    let reread = match new.source {
        None => Some(original.to_vec()),
        Some(step) => new.convert(multibyte.encodings, step, original),
    };
    match reread {
        Some(reread) => Read::Again(reread),
        None => Read::Refused {
            message: Unconverted(multibyte.encodings.name(named)).message(),
            thrown: false,
        },
    }
}

/// Where a diagnostic raised as the scanner stood at `close` goes among
/// the parse's: after every one raised at a token before it, and before
/// the compile-phase ones.
fn position(diagnostics: &[php_syntax::Diagnostic], close: u32) -> usize {
    diagnostics
        .iter()
        .position(|d| d.level == php_syntax::Level::CompileError || d.span.start >= close)
        .unwrap_or(diagnostics.len())
}

/// Every `encoding` directive of every `declare` in the parse, by where
/// its `)` ends.
fn declared(parsed: &Parsed) -> Vec<Declared> {
    let mut all = Vec::new();
    for node in parsed.tree.nodes::<DeclareStmt>() {
        let directives = parsed.tree.list(node.declare_directives);
        let Some(last) = directives.last() else {
            continue;
        };
        let after = parsed.tree[*last].range.end();
        let Some(close) = parsed
            .tokens
            .iter()
            .find(|t| t.offset >= after && t.kind == SyntaxKind::R_PAREN)
        else {
            continue;
        };
        let end = close.offset + close.len;
        let line = parsed.lines.line_number(close.offset);
        for directive in directives {
            let directive = &parsed.tree[*directive];
            if php_syntax::token_text(parsed, directive.name).eq_ignore_ascii_case(b"encoding")
                && let Some(value) = directive.value
            {
                all.push(Declared {
                    close: end,
                    line,
                    value,
                });
            }
        }
    }
    all.sort_by_key(|d| d.close);
    all
}

/// `zval_get_string` of a written value the parser made: a long in decimal,
/// a double at `precision`, a string's bytes.
fn string_of(parsed: &Parsed, value: tree::Expr, precision: i32) -> Vec<u8> {
    let tree = &parsed.tree;
    let parts = |holder: Holder<'_>, tokens: &mut dyn Iterator<Item = TokenId>| {
        let mut out = Vec::new();
        for token in tokens {
            if let Some(TokenValue::Bytes(bytes)) = TokenValue::of(parsed, holder, token) {
                out.extend(bytes);
            }
        }
        out
    };
    let literal_parts = |parts: &[tree::StringPart]| -> Vec<TokenId> {
        parts
            .iter()
            .filter_map(|part| match part {
                tree::StringPart::LiteralStringPart(id) => tree[*id].text,
                _ => None,
            })
            .collect()
    };
    match crate::emit::peel(parsed, value) {
        tree::Expr::LiteralExpr(id) => {
            let node = &tree[id];
            match node
                .value
                .and_then(|t| TokenValue::of(parsed, Holder::Scalar(node), t))
            {
                Some(TokenValue::Int(n)) => n.to_string().into_bytes(),
                Some(TokenValue::Float(f)) => {
                    php_heap::double_to_string(f, precision, php_heap::Exponent::Upper).into_bytes()
                }
                Some(TokenValue::Bytes(bytes)) => bytes,
                None => Vec::new(),
            }
        }
        tree::Expr::InterpolatedStringExpr(id) => parts(
            Holder::DoubleQuoted(&tree[id]),
            &mut literal_parts(tree.list(tree[id].parts)).into_iter(),
        ),
        tree::Expr::HeredocExpr(id) => parts(
            Holder::Heredoc(&tree[id]),
            &mut literal_parts(tree.list(tree[id].parts)).into_iter(),
        ),
        tree::Expr::NowdocExpr(id) => parts(
            Holder::Nowdoc(&tree[id]),
            &mut tree.list(tree[id].text).iter().copied(),
        ),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Functions over five Unicode encodings (0 to 4, [`Unicode`]'s
    /// order), ISO-8859-1 (5), and ISO-8859-1's bytes named as one the
    /// lexer cannot read (6), with the internal encoding given.
    struct Fake(Option<Encoding>);

    impl Encodings for Fake {
        fn fetch(&self, name: &[u8]) -> Option<Encoding> {
            match name.to_ascii_uppercase().as_slice() {
                b"UTF-8" => Some(Encoding(4)),
                b"ISO-8859-1" => Some(Encoding(5)),
                b"X-GL" => Some(Encoding(6)),
                _ => None,
            }
        }
        fn unicode(&self, which: Unicode) -> Encoding {
            Encoding(which as u32)
        }
        fn name(&self, e: Encoding) -> Vec<u8> {
            format!("E{}", e.0).into_bytes()
        }
        fn lexer_compatible(&self, e: Encoding) -> bool {
            matches!(e.0, 4 | 5)
        }
        fn detect(&self, _: &[u8], list: &[Encoding]) -> Option<Encoding> {
            list.last().copied()
        }
        fn convert(&self, text: &[u8], to: Encoding, from: Encoding) -> Option<Vec<u8>> {
            let chars: Vec<char> = match from.0 {
                4 => String::from_utf8_lossy(text).chars().collect(),
                5 | 6 => text.iter().map(|&b| char::from(b)).collect(),
                3 => text.chunks(2).map(|c| char::from(c[0])).collect(),
                _ => return None,
            };
            Some(match to.0 {
                4 => chars.into_iter().collect::<String>().into_bytes(),
                5 | 6 => chars
                    .into_iter()
                    .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                    .collect(),
                _ => return None,
            })
        }
        fn internal(&self) -> Option<Encoding> {
            self.0
        }
        fn list(&self, names: &[u8]) -> Result<Vec<Encoding>, Vec<u8>> {
            names
                .split(|&b| b == b',')
                .map(|name| self.fetch(name.trim_ascii()).ok_or_else(|| name.to_vec()))
                .collect()
        }
    }

    const LATIN: Option<Encoding> = Some(Encoding(5));
    const UNSAFE: Option<Encoding> = Some(Encoding(6));

    /// The file opened as the engine opens it, then compiled.
    fn compiled(
        encodings: &dyn Encodings,
        script: &[u8],
        listed: &[Encoding],
    ) -> Result<(crate::Unit, php_heap::Strings), CompileError> {
        let options = Options {
            multibyte: Some(Multibyte {
                encodings,
                detect_unicode: true,
                listed,
            }),
            ..Options::default()
        };
        let mut strings = php_heap::Strings::new();
        crate::compile_source(
            &mut php_syntax::Arena::new(),
            script,
            &options,
            &mut strings,
        )
        .map(|unit| (unit, strings))
    }

    /// The dump, read while the unit's strings stand.
    fn dump(unit: &Result<(crate::Unit, php_heap::Strings), CompileError>) -> String {
        unit.as_ref()
            .map(|(u, _)| String::from_utf8_lossy(&u.dump(&[])).into_owned())
            .unwrap_or_default()
    }

    /// A byte order mark names the encoding and is taken off; UTF-16's
    /// NULs name it without one, except after `__halt_compiler();`.
    #[test]
    fn a_byte_order_mark_or_nuls_name_the_script_encoding() {
        let fake = Fake(LATIN);
        let file = Source::File {
            detect_unicode: true,
            listed: &[],
        };
        let opened = open(&fake, b"\xef\xbb\xbf<?php echo '\xc3\xa9';", file);
        assert_eq!(
            opened.map(|o| (o.bom, o.text)),
            Ok((3, b"<?php echo '\xe9';".to_vec()))
        );
        let wide = b"<\0?\0p\0h\0p\0";
        let opened = open(&fake, wide, file).map(|o| o.filter.map(|f| f.encoding));
        assert_eq!(opened, Ok(Some(Encoding(Unicode::Utf16Le as u32))));
        let halted = b"<?php __halt_compiler (); \0\0";
        assert_eq!(open(&fake, halted, file).map(|o| o.filter), Ok(None));
        let off = Source::File {
            detect_unicode: false,
            listed: &[],
        };
        assert_eq!(open(&fake, wide, off).map(|o| o.filter), Ok(None));
        // php's dummies, where no extension set the functions: nothing
        // converts, and UTF-16LE is named as php names it.
        let refused = open(&Dummy, b"\xff\xfe<\0?\0", file);
        assert_eq!(refused, Err(Unconverted(b"UTF-32LE".to_vec())));
    }

    /// The four arrangements `zend_multibyte_set_filter` chooses between.
    #[test]
    fn the_filters_are_chosen_by_what_the_lexer_reads() {
        let choose = |internal, script| {
            let f = Filter::choose(&Fake(internal), Encoding(script));
            (f.source, f.tokens)
        };
        assert_eq!(choose(LATIN, 5), (None, None));
        assert_eq!(
            choose(UNSAFE, 6),
            (
                Some(Step::SourceToIntermediate),
                Some(Step::IntermediateToSource)
            )
        );
        assert_eq!(choose(LATIN, 4), (Some(Step::SourceToInternal), None));
        assert_eq!(choose(UNSAFE, 4), (None, Some(Step::SourceToInternal)));
        assert_eq!(
            choose(Some(Encoding(3)), 6),
            (
                Some(Step::SourceToIntermediate),
                Some(Step::IntermediateToInternal)
            )
        );
    }

    /// The listed encoding converts the file before it is scanned,
    /// and `__COMPILER_HALT_OFFSET__` is the offset in the file as read.
    #[test]
    fn the_input_filter_converts_the_script_and_the_halt_offset_maps_back() {
        let script = "<?php echo 'äé', __COMPILER_HALT_OFFSET__; __halt_compiler();x".as_bytes();
        let unit = compiled(&Fake(LATIN), script, &[Encoding(4)]);
        assert!(
            dump(&unit).contains(r#"string("\xE4\xE9")"#),
            "{}",
            dump(&unit)
        );
        let offset = i64::try_from(script.len() - 1).unwrap_or_default();
        assert_eq!(unit.map(|(u, _)| u.halt_offset), Ok(Some(offset)));
    }

    /// Where the lexer reads the file's encoding and not the internal
    /// one, names and strings are converted as the parser is handed them.
    #[test]
    fn the_output_filter_converts_names_and_strings() {
        let script = "<?php function é() {} echo 'é', \"é\\x41\";".as_bytes();
        let unit = compiled(&Fake(UNSAFE), script, &[Encoding(4)]);
        let names: Vec<Vec<u8>> = unit
            .as_ref()
            .map(|(u, _)| u.functions.iter().map(|(f, _)| f.name.0.to_vec()).collect())
            .unwrap_or_default();
        assert_eq!(names, vec![b"\xe9".to_vec()]);
        assert!(
            dump(&unit).contains(r#"string("\xE9A")"#),
            "{}",
            dump(&unit)
        );
    }

    /// `declare(encoding)` re-reads the rest of the file from its `)`,
    /// warns where the name fetches nothing, and is refused where its
    /// value is no written value, each where the parser stood.
    #[test]
    fn a_declared_encoding_reads_the_rest_of_the_script_again() {
        let script = b"<?php declare(encoding='ISO-8859-1'); echo '\xe9';";
        let unit = compiled(&Fake(Some(Encoding(4))), script, &[]);
        assert!(
            dump(&unit).contains(r#"string("\xC3\xA9")"#),
            "{}",
            dump(&unit)
        );
        let warned = |encodings: &dyn Encodings, script: &[u8]| {
            compiled(encodings, script, &[]).map(|(u, _)| {
                u.diagnostics
                    .iter()
                    .map(|d| (d.line, d.message.to_string()))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            warned(
                &Dummy,
                b"<?php declare(encoding=1);\ndeclare(encoding=2.5); echo \"\\777\";"
            ),
            Ok(vec![
                (1, "Unsupported encoding [1]".to_owned()),
                (2, "Unsupported encoding [2.5]".to_owned()),
                (
                    2,
                    "Octal escape sequence overflow \\777 is greater than \\377".to_owned()
                ),
            ])
        );
        let refused = compiled(
            &Dummy,
            b"<?php declare(encoding=1);\ndeclare(encoding=M_PI);",
            &[],
        );
        assert_eq!(
            refused
                .err()
                .map(|e| (e.message.into_owned(), e.line, e.thrown, e.raised.len())),
            Some(("Encoding must be a literal".to_owned(), 2, true, 1))
        );
    }
}
