//! `uri_parser_whatwg.c` and the `Uri\WhatWg` classes of `php_uri.c`:
//! the WHATWG URL standard over ext/lexbor's parser
//! ([`php_ext_lexbor::ffi::url`]), and the validation errors it reports
//! as `Uri\WhatWg\UrlValidationError` objects.

use php_ext::abi::{Instance, IntoReturn, Passed, Ref, Static, This, Untyped};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object, write_prop};
use php_ext::raising::{Thrown, arg_value_error, fatal, new_throwable, throw_value};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value, walk};
use php_ext_lexbor::ffi::url::{self as lxb, ErrorKind, Host, Url as LxbUrl, ValidationError};
use php_ext_lexbor::ffi::{self as lexbor, Lent, Owned};

use crate::{
    Handlers, InvalidUriException, Parsed, Read, ReadAs, Recomposition, Slot, UriComparisonMode,
    UriParser,
};

/// A URL as lexbor holds it.
pub(crate) struct WhatWg(LxbUrl);

/// `fill_errors`' table: the enum case a lexbor error is reported as, and
/// whether the standard calls it a failure.
fn reported(kind: ErrorKind) -> (UrlValidationErrorType, bool) {
    use UrlValidationErrorType as T;
    match kind {
        ErrorKind::DomainToAscii => (T::DomainToAscii, true),
        ErrorKind::DomainToUnicode => (T::DomainToUnicode, false),
        ErrorKind::DomainInvalidCodePoint => (T::DomainInvalidCodePoint, true),
        ErrorKind::HostInvalidCodePoint => (T::HostInvalidCodePoint, true),
        ErrorKind::Ipv4EmptyPart => (T::Ipv4EmptyPart, false),
        ErrorKind::Ipv4TooManyParts => (T::Ipv4TooManyParts, true),
        ErrorKind::Ipv4NonNumericPart => (T::Ipv4NonNumericPart, true),
        ErrorKind::Ipv4NonDecimalPart => (T::Ipv4NonDecimalPart, false),
        ErrorKind::Ipv4OutOfRangePart => (T::Ipv4OutOfRangePart, true),
        ErrorKind::Ipv6Unclosed => (T::Ipv6Unclosed, true),
        ErrorKind::Ipv6InvalidCompression => (T::Ipv6InvalidCompression, true),
        ErrorKind::Ipv6TooManyPieces => (T::Ipv6TooManyPieces, true),
        ErrorKind::Ipv6MultipleCompression => (T::Ipv6MultipleCompression, true),
        ErrorKind::Ipv6InvalidCodePoint => (T::Ipv6InvalidCodePoint, true),
        ErrorKind::Ipv6TooFewPieces => (T::Ipv6TooFewPieces, true),
        ErrorKind::Ipv4InIpv6TooManyPieces => (T::Ipv4InIpv6TooManyPieces, true),
        ErrorKind::Ipv4InIpv6InvalidCodePoint => (T::Ipv4InIpv6InvalidCodePoint, true),
        ErrorKind::Ipv4InIpv6OutOfRangePart => (T::Ipv4InIpv6OutOfRangePart, true),
        ErrorKind::Ipv4InIpv6TooFewParts => (T::Ipv4InIpv6TooFewParts, true),
        ErrorKind::InvalidUrlUnit => (T::InvalidUrlUnit, false),
        ErrorKind::SpecialSchemeMissingFollowingSolidus => {
            (T::SpecialSchemeMissingFollowingSolidus, false)
        }
        ErrorKind::MissingSchemeNonRelativeUrl => (T::MissingSchemeNonRelativeUrl, true),
        ErrorKind::InvalidReverseSolidus => (T::InvalidReverseSoldius, false),
        ErrorKind::InvalidCredentials => (T::InvalidCredentials, false),
        ErrorKind::HostMissing => (T::HostMissing, true),
        ErrorKind::PortOutOfRange => (T::PortOutOfRange, true),
        ErrorKind::PortInvalid => (T::PortInvalid, true),
        ErrorKind::FileInvalidWindowsDriveLetter => (T::FileInvalidWindowsDriveLetter, false),
        ErrorKind::FileInvalidWindowsDriveLetterHost => {
            (T::FileInvalidWindowsDriveLetterHost, false)
        }
    }
}

/// `fill_errors`: one `UrlValidationError` per logged error, in the order
/// php pops them, and the reason a message names, the case of the last
/// failure popped.
fn fill_errors(
    req: &mut Request,
    errors: &[ValidationError],
) -> Result<(Owner<PhpArray>, Option<&'static str>), Thrown> {
    if errors.is_empty() {
        return Ok((Owner::take(req, PhpArray::empty()), None));
    }
    let mut out = PhpArray::new(req)?;
    let mut reason = None;
    let Some(class) = lookup_class(req, b"Uri\\WhatWg\\UrlValidationError")? else {
        out.give_up(req);
        return Err(fatal(
            req,
            "InternalError: Uri\\WhatWg\\UrlValidationError is not linked",
        ));
    };
    for error in errors {
        let (kind, failure) = reported(error.kind);
        let object = new_object(req, class)?;
        let context = PhpString::new(req, &error.data)?;
        write_prop(req, &object, b"context", context.into_value())?;
        let kind_value = case_owner(req, kind)?;
        write_prop(req, &object, b"type", kind_value)?;
        write_prop(req, &object, b"failure", Owner::bool(failure))?;
        if failure {
            reason = Some(kind.case_name());
        }
        out.append(req, object.into_value())?;
    }
    Ok((out, reason))
}

/// `Uri\WhatWg\InvalidUrlException` with `message`, the reason in
/// parentheses where there is one, and `errors`, made pending.
fn throw_invalid(
    req: &mut Request,
    message: &str,
    reason: Option<&str>,
    errors: PhpArray,
) -> Thrown {
    let message = match reason {
        Some(reason) => format!("{message} ({reason})"),
        None => message.to_owned(),
    };
    let throwable = match new_throwable(req, "Uri\\WhatWg\\InvalidUrlException", &message) {
        Ok(throwable) => throwable,
        Err(thrown) => return thrown,
    };
    if let Some(object) = throwable.as_object() {
        let errors = Owner::take(req, Value::array(errors));
        let written = write_prop(req, &object, b"errors", errors);
        if let Err(thrown) = written {
            throwable.give_up(req);
            return thrown;
        }
    }
    let thrown = throw_value(req, &throwable);
    throwable.give_up(req);
    thrown
}

/// `throw_invalid_url_exception_during_write`.
fn refused_write(req: &mut Request, component: &str, errors: &[ValidationError]) -> Thrown {
    match fill_errors(req, errors) {
        Ok((array, reason)) => {
            let thrown = throw_invalid(
                req,
                &format!("The specified {component} is malformed"),
                reason,
                *array,
            );
            array.give_up(req);
            thrown
        }
        Err(thrown) => thrown,
    }
}

/// The extension's request state: the WHATWG parser php's
/// `PHP_RINIT_FUNCTION(uri_parser_whatwg)` lays, its pool, parser and
/// IDNA converter, which every `Uri\WhatWg\Url` of the request is
/// allocated in and served by; `None` before `rinit` and after
/// `post_deactivate`.
#[derive(Default)]
pub(crate) struct State {
    parser: Option<lxb::Parser>,
}

/// `PHP_RINIT_FUNCTION(uri_parser_whatwg)` (`Z/ext/uri/uri_parser_whatwg.c:533`):
/// the request's pool, parser and IDNA converter laid on its heap. Where
/// lexbor refuses one, php's `RINIT` answers `FAILURE`; here that is the
/// `InternalError`, since only an allocation lexbor could not make
/// refuses it, and the memory limit's fatal comes first.
///
/// What it lays, 73,952 bytes (#2923), is the last item of a request's
/// memory at its first line (design §5.13, #2863), so an empty program's
/// `memory_get_usage()` reads php's under opcache's regime. The strings
/// that move with the directory and the environment are taken out, the
/// working directory's two copies, the file's path and each variable, so
/// it reads the same from any directory:
///
/// ```php
/// $m = memory_get_usage();
/// $bin = function ($n) { foreach ([8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896, 1024, 1280, 1536, 1792, 2048, 2560, 3072] as $b) { if ($n <= $b) { return $b; } } return ($n + 4095) & ~4095; };
/// $str = fn ($s) => strlen($s) < 2 ? 0 : $bin(24 + strlen($s) + 1);
/// $moves = $bin(strlen(getcwd()) + 1) + $str(getcwd()) + $str(__FILE__);
/// foreach (getenv() as $k => $v) { $moves += $str($k) + $str($v); }
/// echo $m - $moves, "\n";
/// ```
/// ```expect regime=jit-off capture
/// 423208
/// ```
pub(crate) fn rinit(req: &mut Request) -> Result<(), Thrown> {
    match lexbor::lent(req, lxb::Parser::new)? {
        Some(parser) => {
            req.state::<State>().parser = Some(parser);
            Ok(())
        }
        None => Err(fatal(
            req,
            "InternalError: lexbor refused the request's URL parser",
        )),
    }
}

/// `ZEND_MODULE_POST_ZEND_DEACTIVATE_D(uri_parser_whatwg)`: the
/// converter, the parser and the pool destroyed under a lend of the
/// request, after its objects, and the URLs they held, are freed.
pub(crate) fn post_deactivate(req: &mut Request) {
    if let Some(parser) = req.state::<State>().parser.take() {
        // The limit cannot refuse a free, and nothing is left to raise
        // into at the request's end.
        lexbor::lent(req, |lent| parser.give_back(lent)).ok();
    }
}

/// `call` served with the request's parser and the heap lent to lexbor;
/// the `Err` is the memory limit's fatal, or the `InternalError` of a
/// request whose `rinit` laid none.
fn with_parser<R>(
    req: &mut Request,
    call: impl FnOnce(&mut lxb::Parser, &Lent) -> R,
) -> Result<R, Thrown> {
    let Some(mut parser) = req.state::<State>().parser.take() else {
        return Err(fatal(
            req,
            "InternalError: a URL served before the request laid its parser",
        ));
    };
    let answered = lexbor::lent(req, |lent| call(&mut parser, lent));
    req.state::<State>().parser = Some(parser);
    answered
}

impl WhatWg {
    fn includes_credentials(&self) -> bool {
        !self.0.username().is_empty() || !self.0.password().is_empty()
    }

    /// A setter over the URL, under the request's parser, a refusal
    /// thrown naming `component`.
    fn set(
        &mut self,
        req: &mut Request,
        component: &str,
        value: &[u8],
        set: fn(&mut lxb::Parser, &Lent, &mut LxbUrl, &[u8]) -> Result<(), lxb::Refused>,
    ) -> Result<(), Thrown> {
        with_parser(req, |parser, lent| set(parser, lent, &mut self.0, value))?
            .map_err(|refused| refused_write(req, component, &refused.0))
    }

    /// `php_uri_parser_whatwg_username_write`.
    fn set_username(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        let value = value.unwrap_or_default();
        self.set(req, "username", value, lxb::Parser::set_username)
    }

    /// `php_uri_parser_whatwg_password_write`.
    fn set_password(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        let value = value.unwrap_or_default();
        self.set(req, "password", value, lxb::Parser::set_password)
    }
}

impl UriParser for WhatWg {
    const CLASS: &'static str = "Uri\\WhatWg\\Url";

    /// `php_uri_parser_whatwg_parse_ex`: the errors array made where the
    /// caller wants it or a throw carries it.
    fn parse(
        req: &mut Request,
        text: &[u8],
        base: Option<&Self>,
        want_errors: bool,
        silent: bool,
    ) -> Result<Parsed<Self>, Thrown> {
        let parsed = with_parser(req, |parser, lent| {
            parser.parse(lent, text, base.map(|base| &base.0))
        })?;
        let fails = parsed.url.is_none() && !silent;
        let mut errors = None;
        if fails || want_errors {
            let (array, reason) = fill_errors(req, &parsed.errors)?;
            if fails {
                let thrown = throw_invalid(req, "The specified URI is malformed", reason, *array);
                array.give_up(req);
                return Err(thrown);
            }
            errors = Some(array);
        }
        Ok(Parsed {
            uri: parsed.url.map(WhatWg),
            errors,
        })
    }

    /// `php_uri_parser_whatwg_clone`: a copy in the request's pool.
    fn duplicate(&self, req: &mut Request) -> Result<Option<Self>, Thrown> {
        let copy = with_parser(req, |parser, lent| parser.clone_url(lent, &self.0))?;
        Ok(copy.map(WhatWg))
    }

    /// `php_uri_parser_whatwg_destroy`: the URL given back to the
    /// request's pool.
    fn free(self, req: &mut Request) {
        // A request whose parser is gone has destroyed the pool, and the
        // URL with it, so there is nothing to give back and nothing to
        // raise; the limit cannot refuse a free.
        let Some(mut parser) = req.state::<State>().parser.take() else {
            return;
        };
        lexbor::lent(req, |lent| parser.free(lent, self.0)).ok();
        req.state::<State>().parser = Some(parser);
    }

    /// `php_uri_parser_whatwg_to_string`: the Unicode forms through the
    /// request's IDNA converter.
    fn recompose(
        &mut self,
        req: &mut Request,
        mode: Recomposition,
        exclude_fragment: bool,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        Ok(Some(match mode {
            Recomposition::RawUnicode => with_parser(req, |parser, lent| {
                parser.serialize_idna(lent, &self.0, exclude_fragment)
            })?,
            Recomposition::RawAscii | Recomposition::NormalizedAscii => {
                self.0.serialize(exclude_fragment)
            }
        }))
    }

    fn scheme(&mut self, _: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(Some(self.0.scheme().to_vec()))
    }

    /// `php_uri_parser_whatwg_username_read`: `null` unless the URL
    /// includes credentials.
    fn username(&mut self, _: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(self
            .includes_credentials()
            .then(|| self.0.username().to_vec()))
    }

    fn password(&mut self, _: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(self
            .includes_credentials()
            .then(|| self.0.password().to_vec()))
    }

    /// `php_uri_parser_whatwg_host_read`: an IPv6 address in brackets, a
    /// domain in Unicode for the Unicode read, through the request's
    /// IDNA converter.
    fn host(&mut self, req: &mut Request, mode: ReadAs) -> Result<Read<Option<Vec<u8>>>, Thrown> {
        let Some(host) = self.0.host() else {
            return Ok(Ok(None));
        };
        Ok(Ok(Some(match host {
            Host::Ipv4(address) => lxb::ipv4_text(address),
            Host::Ipv6(pieces) => {
                let mut text = b"[".to_vec();
                text.extend_from_slice(&lxb::ipv6_text(&pieces));
                text.push(b']');
                text
            }
            Host::Empty => Vec::new(),
            Host::Domain(name) | Host::Opaque(name) => match mode {
                ReadAs::NormalizedUnicode => {
                    with_parser(req, |parser, lent| parser.host_unicode(lent, &self.0))?
                }
                ReadAs::NormalizedAscii | ReadAs::Raw => name.to_vec(),
            },
        })))
    }

    fn port(&mut self, _: ReadAs) -> Read<Option<i64>> {
        Ok(self.0.port().map(i64::from))
    }

    fn path(&mut self, _: ReadAs) -> Read<Vec<u8>> {
        Ok(self.0.path().to_vec())
    }

    fn query(&mut self, _: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(self.0.query().map(<[u8]>::to_vec))
    }

    fn fragment(&mut self, _: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(self.0.fragment().map(<[u8]>::to_vec))
    }

    fn set_scheme(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        let value = value.unwrap_or_default();
        self.set(req, "scheme", value, lxb::Parser::set_protocol)
    }

    fn set_host(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        let value = value.unwrap_or_default();
        self.set(req, "host", value, lxb::Parser::set_hostname)
    }

    /// `php_uri_parser_whatwg_port_write`: the number as its decimal text,
    /// `null` as the empty string.
    fn set_port(&mut self, req: &mut Request, value: Option<i64>) -> Result<(), Thrown> {
        let text = value.map(|port| port.to_string()).unwrap_or_default();
        self.set(req, "port", text.as_bytes(), lxb::Parser::set_port)
    }

    fn set_path(&mut self, req: &mut Request, value: &[u8]) -> Result<(), Thrown> {
        self.set(req, "path", value, lxb::Parser::set_pathname)
    }

    fn set_query(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        let value = value.unwrap_or_default();
        self.set(req, "query string", value, lxb::Parser::set_search)
    }

    fn set_fragment(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        let value = value.unwrap_or_default();
        self.set(req, "fragment", value, lxb::Parser::set_hash)
    }
}

/// The handlers of `Uri\WhatWg\Url`.
pub(crate) static HANDLERS: Handlers<WhatWg> = Handlers(std::marker::PhantomData);

type W = WhatWg;

/// Core's `Throwable`, `$previous`'s type.
#[php_class(extern)]
pub(crate) struct Throwable;

/// `class Uri\WhatWg\InvalidUrlException extends Uri\InvalidUriException`,
/// `@strict-properties`, with the validation errors that made the URL
/// invalid.
#[php_class(
    namespace(Uri::WhatWg),
    extends(InvalidUriException),
    dynamic_properties = forbidden
)]
pub(crate) struct InvalidUrlException;

#[php_class]
impl InvalidUrlException {
    #[php(property, readonly)]
    const errors: PhpArray;

    /// `__construct(string $message = "", array $errors = [],`
    /// `int $code = 0, ?Throwable $previous = null)`:
    /// `zend_update_exception_properties` (the message where given, the
    /// code where not zero, the previous where not null), then `errors`,
    /// a list of `UrlValidationError`s.
    ///
    /// ```php
    /// $e = new Uri\WhatWg\InvalidUrlException("m", [new Uri\WhatWg\UrlValidationError("x", Uri\WhatWg\UrlValidationErrorType::PortInvalid, true)], 3);
    /// var_dump($e->getMessage(), $e->getCode(), count($e->errors), (new Uri\WhatWg\InvalidUrlException)->errors);
    /// try { new Uri\WhatWg\InvalidUrlException("m", [1]); } catch (ValueError $v) { echo $v->getMessage(), "\n"; }
    /// try { $e->__construct("n"); } catch (Error $v) { echo $v->getMessage(), "\n"; }
    /// var_dump($e->getMessage());
    /// ```
    /// ```expect
    /// string(1) "m"
    /// int(3)
    /// int(1)
    /// array(0) {
    /// }
    /// Uri\WhatWg\InvalidUrlException::__construct(): Argument #2 ($errors) must be a list of Uri\WhatWg\UrlValidationError
    /// Cannot modify readonly property Uri\WhatWg\InvalidUrlException::$errors
    /// string(1) "n"
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "")] message: Passed<PhpString>,
        #[php(default = [])] errors: Passed<PhpArray>,
        #[php(default = 0)] code: i64,
        #[php(default = null)] previous: Option<Instance<Throwable>>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        if let Passed::Given(message) = message {
            let message = Owner::take(req, Value::string(message));
            write_prop(req, &this, b"message", message)?;
        }
        if code != 0 {
            let code = Owner::int(code);
            write_prop(req, &this, b"code", code)?;
        }
        if let Some(previous) = previous {
            let previous = Owner::take(req, previous.object().as_value());
            write_prop(req, &this, b"previous", previous)?;
        }
        let errors = match errors {
            Passed::Given(errors) => {
                if !is_list_of_errors(req, errors)? {
                    return Err(arg_value_error(
                        req,
                        2,
                        "be a list of Uri\\WhatWg\\UrlValidationError",
                    ));
                }
                Owner::take(req, Value::array(errors))
            }
            Passed::Omitted => Owner::take(req, Value::array(PhpArray::empty())),
        };
        if this.prop(req, b"errors").is_some() {
            errors.give_up(req);
            return Err(readonly_property(
                req,
                "Uri\\WhatWg\\InvalidUrlException",
                "errors",
            ));
        }
        write_prop(req, &this, b"errors", errors)
    }
}

/// The case object of `kind` as an owner: `IntoReturn` answers a case
/// with the one owner taken for the caller (`enum_case`).
fn case_owner(req: &mut Request, kind: UrlValidationErrorType) -> Result<Owner<Value>, Thrown> {
    kind.into_return(req)
}

/// `zend_readonly_property_modification_error`.
fn readonly_property(req: &mut Request, class: &str, name: &str) -> Thrown {
    php_ext::raising::throw_error(
        req,
        "Error",
        &format!("Cannot modify readonly property {class}::${name}"),
    )
}

/// `is_list_of_whatwg_validation_errors`: a list whose every element is a
/// `UrlValidationError`, references refused.
fn is_list_of_errors(req: &mut Request, array: PhpArray) -> Result<bool, Thrown> {
    if !array.is_list() {
        return Ok(false);
    }
    let Some(class) = lookup_class(req, b"Uri\\WhatWg\\UrlValidationError")? else {
        return Ok(false);
    };
    Ok(walk::rows(req, array).all(|(_, value)| {
        value
            .as_object()
            .is_some_and(|object| object.instance_of(req, class))
    }))
}

/// `enum Uri\WhatWg\UrlValidationErrorType`: the validation errors of the
/// URL standard, `InvalidReverseSoldius` spelt as php spells it.
///
/// ```php
/// var_dump(count(Uri\WhatWg\UrlValidationErrorType::cases()), Uri\WhatWg\UrlValidationErrorType::cases()[22]);
/// ```
/// ```expect
/// int(29)
/// enum(Uri\WhatWg\UrlValidationErrorType::InvalidReverseSoldius)
/// ```
#[php_class(namespace(Uri::WhatWg))]
pub(crate) enum UrlValidationErrorType {
    DomainToAscii,
    DomainToUnicode,
    DomainInvalidCodePoint,
    HostInvalidCodePoint,
    Ipv4EmptyPart,
    Ipv4TooManyParts,
    Ipv4NonNumericPart,
    Ipv4NonDecimalPart,
    Ipv4OutOfRangePart,
    Ipv6Unclosed,
    Ipv6InvalidCompression,
    Ipv6TooManyPieces,
    Ipv6MultipleCompression,
    Ipv6InvalidCodePoint,
    Ipv6TooFewPieces,
    Ipv4InIpv6TooManyPieces,
    Ipv4InIpv6InvalidCodePoint,
    Ipv4InIpv6OutOfRangePart,
    Ipv4InIpv6TooFewParts,
    InvalidUrlUnit,
    SpecialSchemeMissingFollowingSolidus,
    MissingSchemeNonRelativeUrl,
    InvalidReverseSoldius,
    InvalidCredentials,
    HostMissing,
    PortOutOfRange,
    PortInvalid,
    FileInvalidWindowsDriveLetter,
    FileInvalidWindowsDriveLetterHost,
}

#[php_class]
impl UrlValidationErrorType {}

impl UrlValidationErrorType {
    /// The case's php name, what a message's reason reads.
    fn case_name(self) -> &'static str {
        use php_ext::abi::Cases;
        Self::CASES
            .iter()
            .find(|(_, case)| *case == self)
            .and_then(|(name, _)| std::str::from_utf8(name).ok())
            .unwrap_or_default()
    }
}

/// `final readonly class Uri\WhatWg\UrlValidationError`,
/// `@strict-properties`: one validation error, where it was met, its
/// kind, and whether it failed the parse.
#[php_class(
    final,
    readonly,
    namespace(Uri::WhatWg),
    dynamic_properties = forbidden
)]
pub(crate) struct UrlValidationError;

#[php_class]
impl UrlValidationError {
    #[php(property, readonly)]
    const context: PhpString;
    #[php(property, readonly)]
    const r#type: Instance<UrlValidationErrorType>;
    #[php(property, readonly)]
    const failure: bool;

    /// `__construct(string $context, Uri\WhatWg\UrlValidationErrorType
    /// $type, bool $failure)`: the three written once.
    ///
    /// ```php
    /// $e = new Uri\WhatWg\UrlValidationError("ctx", Uri\WhatWg\UrlValidationErrorType::HostMissing, true);
    /// var_dump($e);
    /// try { $e->__construct("x", Uri\WhatWg\UrlValidationErrorType::HostMissing, false); } catch (Error $v) { echo $v->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(Uri\WhatWg\UrlValidationError)#1 (3) {
    ///   ["context"]=>
    ///   string(3) "ctx"
    ///   ["type"]=>
    ///   enum(Uri\WhatWg\UrlValidationErrorType::HostMissing)
    ///   ["failure"]=>
    ///   bool(true)
    /// }
    /// Cannot modify readonly property Uri\WhatWg\UrlValidationError::$context
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        context: PhpString,
        r#type: UrlValidationErrorType,
        failure: bool,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let class = "Uri\\WhatWg\\UrlValidationError";
        if this.prop(req, b"context").is_some() {
            return Err(readonly_property(req, class, "context"));
        }
        let context = Owner::take(req, Value::string(context));
        write_prop(req, &this, b"context", context)?;
        if this.prop(req, b"type").is_some() {
            return Err(readonly_property(req, class, "type"));
        }
        let kind = case_owner(req, r#type)?;
        write_prop(req, &this, b"type", kind)?;
        if this.prop(req, b"failure").is_some() {
            return Err(readonly_property(req, class, "failure"));
        }
        let failure = Owner::bool(failure);
        write_prop(req, &this, b"failure", failure)
    }
}

/// `final readonly class Uri\WhatWg\Url`, `@strict-properties`: one URL
/// under the WHATWG standard, immutable, each `with*()` a changed copy.
///
/// `php_uri_object_create` allocates a `php_uri_object`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// new Uri\WhatWg\Url('http://x/');
/// $m = memory_get_usage();
/// $u = new Uri\WhatWg\Url('http://x/');
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 80
/// ```
#[php_class(
    final,
    readonly,
    namespace(Uri::WhatWg),
    dynamic_properties = forbidden,
    size = 72, payload = Slot<WhatWg>,
    handlers = crate::whatwg::HANDLERS
)]
pub(crate) struct Url;

/// The by-reference errors parameter, `&$errors = null`.
type Errors = Ref<Untyped<Value>>;

#[php_class]
impl Url {
    /// `Uri\WhatWg\Url::parse(string $uri, ?Uri\WhatWg\Url $baseUrl =
    /// null, &$errors = null): ?static`: the URL or `null`, the
    /// validation errors in `$errors`.
    ///
    /// ```php
    /// var_dump(Uri\WhatWg\Url::parse("https://example.com:99999", null, $errors), $errors[0]->type);
    /// var_dump(Uri\WhatWg\Url::parse("c", Uri\WhatWg\Url::parse("https://example.com/a/b"))->toAsciiString());
    /// ```
    /// ```expect
    /// NULL
    /// enum(Uri\WhatWg\UrlValidationErrorType::PortOutOfRange)
    /// string(23) "https://example.com/a/c"
    /// ```
    fn parse(
        req: &mut Request,
        uri: PhpString,
        #[php(default = null)] baseUrl: Option<Instance<Url>>,
        #[php(default = null)] errors: Errors,
    ) -> Result<Option<Owner<Static>>, Thrown> {
        crate::parse_static::<W>(req, uri, baseUrl.map(|b| b.object()), Some(&errors))
    }

    /// `__construct(string $uri, ?Uri\WhatWg\Url $baseUrl = null,
    /// &$softErrors = null)`: the URL, or `InvalidUrlException` naming
    /// the first failure; the errors that did not fail it in
    /// `$softErrors`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url(" https://EXAMPLE.com/\\a\tb ", null, $soft);
    /// var_dump($u->toAsciiString(), count($soft), $soft[0]->type);
    /// try { new Uri\WhatWg\Url("https://exa mple.com"); } catch (Uri\WhatWg\InvalidUrlException $e) { echo $e->getMessage(), "\n", count($e->errors), "\n"; }
    /// ```
    /// ```expect
    /// string(23) "https://example.com//ab"
    /// int(4)
    /// enum(Uri\WhatWg\UrlValidationErrorType::InvalidReverseSoldius)
    /// The specified URI is malformed (DomainInvalidCodePoint)
    /// 1
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        uri: PhpString,
        #[php(default = null)] baseUrl: Option<Instance<Url>>,
        #[php(default = null)] softErrors: Errors,
    ) -> Result<(), Thrown> {
        let base = baseUrl.map(|b| b.object());
        crate::construct::<W>(req, this.object(), uri, base, Some(&softErrors))
    }

    /// `getScheme(): string`.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("HTTPS://example.com"))->getScheme());
    /// ```
    /// ```expect
    /// string(5) "https"
    /// ```
    fn getScheme(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let scheme = crate::component_string::<W>(
            req,
            this.object(),
            "scheme",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.scheme(ReadAs::NormalizedAscii)),
        )?;
        match scheme {
            Some(scheme) => Ok(scheme),
            None => PhpString::new(req, b""),
        }
    }

    /// `withScheme(string $scheme): static`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url("https://example.com/");
    /// var_dump($u->withScheme("http")->toAsciiString(), $u->withScheme("foo")->toAsciiString());
    /// try { $u->withScheme("1"); } catch (Uri\WhatWg\InvalidUrlException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(19) "http://example.com/"
    /// string(20) "https://example.com/"
    /// The specified scheme is malformed
    /// ```
    fn withScheme(
        req: &mut Request,
        this: This<Self>,
        scheme: PhpString,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<W>(req, this.object(), Some(scheme), W::set_scheme)
    }

    /// `getUsername(): ?string`: `null` without credentials.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://u:p@example.com"))->getUsername(), (new Uri\WhatWg\Url("https://:p@example.com"))->getUsername(), (new Uri\WhatWg\Url("https://example.com"))->getUsername());
    /// ```
    /// ```expect
    /// string(1) "u"
    /// string(0) ""
    /// NULL
    /// ```
    fn getUsername(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<W>(
            req,
            this.object(),
            "username",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.username(ReadAs::NormalizedAscii)),
        )
    }

    /// `withUsername(?string $username): static`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url("https://example.com/");
    /// var_dump($u->withUsername("a b")->toAsciiString(), $u->withUsername("a")->withUsername(null)->toAsciiString());
    /// ```
    /// ```expect
    /// string(26) "https://a%20b@example.com/"
    /// string(20) "https://example.com/"
    /// ```
    fn withUsername(
        req: &mut Request,
        this: This<Self>,
        username: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<W>(req, this.object(), username, W::set_username)
    }

    /// `getPassword(): ?string`: `null` without credentials.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://u:p@example.com"))->getPassword(), (new Uri\WhatWg\Url("https://u@example.com"))->getPassword());
    /// ```
    /// ```expect
    /// string(1) "p"
    /// string(0) ""
    /// ```
    fn getPassword(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<W>(
            req,
            this.object(),
            "password",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.password(ReadAs::NormalizedAscii)),
        )
    }

    /// `withPassword(#[\SensitiveParameter] ?string $password): static`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url("https://u@example.com/");
    /// var_dump($u->withPassword("p w")->toAsciiString(), $u->withPassword(null)->toAsciiString());
    /// ```
    /// ```expect
    /// string(28) "https://u:p%20w@example.com/"
    /// string(22) "https://u@example.com/"
    /// ```
    fn withPassword(
        req: &mut Request,
        this: This<Self>,
        #[php(sensitive)] password: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<W>(req, this.object(), password, W::set_password)
    }

    /// `getAsciiHost(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://MÜNCHEN.de"))->getAsciiHost(), (new Uri\WhatWg\Url("https://0x7f.1"))->getAsciiHost(), (new Uri\WhatWg\Url("https://[0:0::1]"))->getAsciiHost(), (new Uri\WhatWg\Url("file:///a"))->getAsciiHost(), (new Uri\WhatWg\Url("mailto:a"))->getAsciiHost());
    /// ```
    /// ```expect
    /// string(17) "xn--mnchen-3ya.de"
    /// string(9) "127.0.0.1"
    /// string(5) "[::1]"
    /// string(0) ""
    /// NULL
    /// ```
    fn getAsciiHost(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<W>(
            req,
            this.object(),
            "host",
            ReadAs::NormalizedAscii,
            |req, u| u.host(req, ReadAs::NormalizedAscii),
        )
    }

    /// `getUnicodeHost(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://xn--mnchen-3ya.de"))->getUnicodeHost(), (new Uri\WhatWg\Url("https://[::1]"))->getUnicodeHost());
    /// ```
    /// ```expect
    /// string(11) "münchen.de"
    /// string(5) "[::1]"
    /// ```
    fn getUnicodeHost(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<W>(
            req,
            this.object(),
            "host",
            ReadAs::NormalizedUnicode,
            |req, u| u.host(req, ReadAs::NormalizedUnicode),
        )
    }

    /// `withHost(?string $host): static`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url("https://example.com/");
    /// var_dump($u->withHost("EXAMPLE.org")->toAsciiString());
    /// try { $u->withHost("exa[mple"); } catch (Uri\WhatWg\InvalidUrlException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(20) "https://example.org/"
    /// The specified host is malformed (DomainInvalidCodePoint)
    /// ```
    fn withHost(
        req: &mut Request,
        this: This<Self>,
        host: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<W>(req, this.object(), host, W::set_host)
    }

    /// `getPort(): ?int`: `null` for a special scheme's default.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://example.com:443"))->getPort(), (new Uri\WhatWg\Url("https://example.com:8443"))->getPort());
    /// ```
    /// ```expect
    /// NULL
    /// int(8443)
    /// ```
    fn getPort(req: &mut Request, this: This<Self>) -> Result<Option<i64>, Thrown> {
        crate::component::<W, _>(req, this.object(), "port", ReadAs::Raw, |_, u| {
            Ok(u.port(ReadAs::Raw))
        })
    }

    /// `withPort(?int $port): static`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url("https://example.com/");
    /// var_dump($u->withPort(81)->toAsciiString(), $u->withPort(81)->withPort(null)->toAsciiString());
    /// try { $u->withPort(65536); } catch (Uri\WhatWg\InvalidUrlException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(23) "https://example.com:81/"
    /// string(20) "https://example.com/"
    /// The specified port is malformed (PortOutOfRange)
    /// ```
    fn withPort(
        req: &mut Request,
        this: This<Self>,
        port: Option<i64>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write::<W>(req, this.object(), |req, uri| uri.set_port(req, port))
    }

    /// `getPath(): string`.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://example.com/a/../b c"))->getPath(), (new Uri\WhatWg\Url("mailto:x"))->getPath());
    /// ```
    /// ```expect
    /// string(6) "/b%20c"
    /// string(1) "x"
    /// ```
    fn getPath(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let path = crate::component::<W, _>(
            req,
            this.object(),
            "path",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.path(ReadAs::NormalizedAscii)),
        )?;
        PhpString::new(req, &path)
    }

    /// `withPath(string $path): static`.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://example.com/a"))->withPath("b/c d")->toAsciiString());
    /// ```
    /// ```expect
    /// string(27) "https://example.com/b/c%20d"
    /// ```
    fn withPath(
        req: &mut Request,
        this: This<Self>,
        path: PhpString,
    ) -> Result<Owner<Static>, Thrown> {
        let path = path.as_bytes().to_vec();
        crate::write::<W>(req, this.object(), |req, uri| uri.set_path(req, &path))
    }

    /// `getQuery(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://example.com/?a b"))->getQuery(), (new Uri\WhatWg\Url("https://example.com/?"))->getQuery(), (new Uri\WhatWg\Url("https://example.com/"))->getQuery());
    /// ```
    /// ```expect
    /// string(5) "a%20b"
    /// string(0) ""
    /// NULL
    /// ```
    fn getQuery(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<W>(
            req,
            this.object(),
            "query",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.query(ReadAs::NormalizedAscii)),
        )
    }

    /// `withQuery(?string $query): static`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url("https://example.com/");
    /// var_dump($u->withQuery("?a=b")->toAsciiString(), $u->withQuery("a")->withQuery(null)->toAsciiString());
    /// ```
    /// ```expect
    /// string(24) "https://example.com/?a=b"
    /// string(20) "https://example.com/"
    /// ```
    fn withQuery(
        req: &mut Request,
        this: This<Self>,
        query: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<W>(req, this.object(), query, W::set_query)
    }

    /// `getFragment(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://example.com/#a b"))->getFragment(), (new Uri\WhatWg\Url("https://example.com/"))->getFragment());
    /// ```
    /// ```expect
    /// string(5) "a%20b"
    /// NULL
    /// ```
    fn getFragment(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<W>(
            req,
            this.object(),
            "fragment",
            ReadAs::NormalizedUnicode,
            |_, u| Ok(u.fragment(ReadAs::NormalizedUnicode)),
        )
    }

    /// `withFragment(?string $fragment): static`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url("https://example.com/");
    /// var_dump($u->withFragment("#f")->toAsciiString(), $u->withFragment("f")->withFragment(null)->toAsciiString());
    /// ```
    /// ```expect
    /// string(22) "https://example.com/#f"
    /// string(20) "https://example.com/"
    /// ```
    fn withFragment(
        req: &mut Request,
        this: This<Self>,
        fragment: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<W>(req, this.object(), fragment, W::set_fragment)
    }

    /// `equals(Uri\WhatWg\Url $url, Uri\UriComparisonMode $comparisonMode
    /// = Uri\UriComparisonMode::ExcludeFragment): bool`.
    ///
    /// ```php
    /// $a = new Uri\WhatWg\Url("HTTPS://example.com:443/#x");
    /// var_dump($a->equals(new Uri\WhatWg\Url("https://EXAMPLE.com#y")), $a->equals(new Uri\WhatWg\Url("https://example.com/#y"), Uri\UriComparisonMode::IncludeFragment));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn equals(
        req: &mut Request,
        this: This<Self>,
        url: Instance<Url>,
        #[php(default = Uri::UriComparisonMode::ExcludeFragment)] comparisonMode: UriComparisonMode,
    ) -> Result<bool, Thrown> {
        crate::equals::<W>(req, this.object(), url.object(), comparisonMode)
    }

    /// `toAsciiString(): string`.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://münchen.de/ä?ö#ü"))->toAsciiString());
    /// ```
    /// ```expect
    /// string(46) "https://xn--mnchen-3ya.de/%C3%A4?%C3%B6#%C3%BC"
    /// ```
    fn toAsciiString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        crate::recompose::<W>(req, this.object(), Recomposition::RawAscii)
    }

    /// `toUnicodeString(): string`: the host in Unicode.
    ///
    /// ```php
    /// var_dump((new Uri\WhatWg\Url("https://xn--mnchen-3ya.de/%C3%A4"))->toUnicodeString());
    /// ```
    /// ```expect
    /// string(26) "https://münchen.de/%C3%A4"
    /// ```
    fn toUnicodeString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        crate::recompose::<W>(req, this.object(), Recomposition::RawUnicode)
    }

    /// `resolve(string $uri, &$softErrors = null): static`.
    ///
    /// ```php
    /// $u = new Uri\WhatWg\Url("https://example.com/a/b");
    /// var_dump($u->resolve("c\td", $soft)->toAsciiString(), count($soft));
    /// try { $u->resolve("https://exa mple.com"); } catch (Uri\WhatWg\InvalidUrlException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(24) "https://example.com/a/cd"
    /// int(1)
    /// The specified URI is malformed (DomainInvalidCodePoint)
    /// ```
    fn resolve(
        req: &mut Request,
        this: This<Self>,
        uri: PhpString,
        #[php(default = null)] softErrors: Errors,
    ) -> Result<Owner<Static>, Thrown> {
        crate::resolve::<W>(req, this.object(), uri, Some(&softErrors))
    }

    /// `__serialize(): array`.
    ///
    /// ```php
    /// $s = serialize(new Uri\WhatWg\Url("HTTPS://example.com"));
    /// var_dump($s, unserialize($s)->toAsciiString());
    /// ```
    /// ```expect
    /// string(84) "O:14:"Uri\WhatWg\Url":2:{i:0;a:1:{s:3:"uri";s:20:"https://example.com/";}i:1;a:0:{}}"
    /// string(20) "https://example.com/"
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        crate::serialize::<W>(req, this.object())
    }

    /// `__unserialize(array $data): void`.
    ///
    /// ```php
    /// try { unserialize('O:14:"Uri\WhatWg\Url":2:{i:0;a:1:{s:3:"uri";s:1:"a";}i:1;a:0:{}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Invalid serialization data for Uri\WhatWg\Url object
    /// ```
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        crate::unserialize::<W>(req, this.object(), data)
    }

    /// `__debugInfo(): array`.
    ///
    /// ```php
    /// var_dump(new Uri\WhatWg\Url("https://u@[::1]:8080/a?#"));
    /// ```
    /// ```expect
    /// object(Uri\WhatWg\Url)#1 (8) {
    ///   ["scheme"]=>
    ///   string(5) "https"
    ///   ["username"]=>
    ///   string(1) "u"
    ///   ["password"]=>
    ///   string(0) ""
    ///   ["host"]=>
    ///   string(5) "[::1]"
    ///   ["port"]=>
    ///   int(8080)
    ///   ["path"]=>
    ///   string(2) "/a"
    ///   ["query"]=>
    ///   string(0) ""
    ///   ["fragment"]=>
    ///   string(0) ""
    /// }
    /// ```
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        crate::debug_info::<W>(req, this.object())
    }
}
