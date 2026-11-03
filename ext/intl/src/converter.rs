//! `converter/`: `UConverter` over ICU's `ucnv_*` (`converter.c`). A
//! subclass's `toUCallback` and `fromUCallback` are served from inside
//! ICU's conversions, resets, clones and closes, as php installs
//! `php_converter_to_u_callback` and `php_converter_from_u_callback` on
//! the converters of every object whose class is not `UConverter` itself.

use php_ext::abi::{CloneObj, ObjectHandlers, OrFalse, Ref, This, Union, Untyped};
use php_ext::attr::php_class;
use php_ext::call::{call_method, clone_with, lookup_class};
use php_ext::raising::{
    Cause, Thrown, arg_value_error, clear, leave_pending, pending, throw_error, while_pending,
};
use php_ext::request::Request;
use php_ext::values::{
    FreshString, Layout, Owner, PhpArray, PhpObject, PhpRef, PhpString, Value, ValueRef, walk,
};

use crate::error::{self, Carrier, IntlError, Outside};
use crate::ffi::converter::{
    self as ucnv, Callbacks, Converter, ConverterId, Live, Reason, Target,
};
use crate::ffi::heap::Block;
use crate::ffi::{Code, c_string};
use crate::utf8;

type Answer<T> = Result<T, Thrown>;

/// What a callback answers: `string|int|array|null`.
type Substitute = Option<Union<Owner<PhpString>, Union<i64, Owner<PhpArray>>>>;

/// `php_converter_object` past its `zend_object`: the two converters and
/// the error. php's two `zend_fcall_info_cache`s are the callbacks' names,
/// looked up on the object's class as each is served.
#[derive(Default)]
pub(crate) struct ConverterObject {
    src: Option<Converter>,
    dest: Option<Converter>,
    error: IntlError,
}

impl Carrier for ConverterObject {
    fn error(&mut self) -> &mut IntlError {
        &mut self.error
    }
}

/// Which of an object's two converters.
#[derive(Clone, Copy)]
enum Side {
    Source,
    Destination,
}

impl ConverterObject {
    fn side(&mut self, side: Side) -> &mut Option<Converter> {
        match side {
            Side::Source => &mut self.src,
            Side::Destination => &mut self.dest,
        }
    }
}

/// The names of the object's source and destination converters, which a
/// call that lends their callbacks to PHP code holds them by.
fn converters(req: &mut Request, object: PhpObject) -> (Option<ConverterId>, Option<ConverterId>) {
    object
        .native_mut::<ConverterObject>(req)
        .map_or((None, None), |o| {
            (
                o.src.as_ref().map(Converter::id),
                o.dest.as_ref().map(Converter::id),
            )
        })
}

/// What PHP code and failures left pending across one native's steps,
/// php's `EG(exception)`: a later throwable chained onto the earlier, as
/// `zend_throw_exception_internal` chains it.
#[derive(Default)]
struct Pending(Option<Thrown>);

impl Pending {
    fn is_set(&self) -> bool {
        self.0.is_some()
    }

    /// `later` raised with this pending. A throw that the handler
    /// `set_exception_handler()` set took, with no frame standing, leaves
    /// php's `EG(exception)` empty, [`Cause::Taken`], and nothing is kept
    /// for it.
    fn keep(&mut self, req: &mut Request, later: Thrown) {
        if later.cause(req) == Cause::Taken {
            return;
        }
        self.0 = Some(match self.0.take() {
            Some(earlier) => earlier.chain(req, later),
            None => later,
        });
    }

    /// `php_converter_throw_failure` on `object`, or on the request
    /// alone, with this pending: its warning raised while this stands,
    /// and its `IntlException` chained over this, as php's C runs on past
    /// `EG(exception)`.
    fn fail(&mut self, req: &mut Request, object: Option<PhpObject>, code: Code, message: &str) {
        let mut outside = match object {
            Some(object) => Outside::of::<ConverterObject>(object),
            None => Outside::Request,
        };
        if let Err(thrown) = outside.set_over(req, self.0.take(), code, message) {
            self.keep(req, thrown);
        }
    }

    /// `THROW_UFAILURE(obj, error)` with this pending, as [`Pending::fail`].
    fn failure(&mut self, req: &mut Request, object: Option<PhpObject>, code: Code) {
        let message = format!("returned error {}: {}", code.0, code.name());
        self.fail(req, object, code, &message);
    }

    /// What a step that answers nothing left standing on the request, a
    /// destructor's throwable, taken as what is pending: where it was
    /// served while this stood, it holds this as its `previous` already,
    /// and this owner of it is given up.
    fn stood(&mut self, req: &mut Request) {
        let Err(over) = pending(req) else {
            return;
        };
        match self.0.take() {
            Some(earlier) if earlier.exception().is_some() => {
                self.0 = Some(over);
                if let Err(left) = clear(req, earlier) {
                    self.keep(req, left);
                }
            }
            earlier => {
                self.0 = earlier;
                self.keep(req, over);
            }
        }
    }

    /// What a raise that may throw came to.
    fn raised(&mut self, req: &mut Request, raised: Answer<()>) {
        if let Err(later) = raised {
            self.keep(req, later);
        }
    }

    /// The native's answer where nothing is pending, and the pending
    /// throwable otherwise, as php's return value is dropped with
    /// `EG(exception)` set. `T` holds no owner.
    fn answer<T>(self, req: &mut Request, answer: Answer<T>) -> Answer<T> {
        match (self.0, answer) {
            (None, answer) => answer,
            (Some(earlier), Err(later)) => Err(earlier.chain(req, later)),
            (Some(earlier), Ok(_)) => Err(earlier),
        }
    }

    /// What is pending left in php's `EG(exception)` where the step that
    /// served it answers nothing, a free, for the release that freed the
    /// object to raise; a fatal or an `exit` is the request's already.
    fn leave(self, req: &mut Request) {
        if let Some(thrown) = self.0 {
            leave_pending(req, thrown);
        }
    }
}

/// `php_converter_object_handlers`: the converters cloned and closed.
pub(crate) static HANDLERS: Handlers = Handlers;

/// The handlers of `UConverter` and its subclasses.
pub(crate) struct Handlers;

impl ObjectHandlers for Handlers {
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_converter)
    }

    /// `php_converter_free_object`: each converter closed, a subclass's
    /// callbacks told (`UCNV_CLOSE`) while the object still holds it and
    /// its members, which are freed after; what a callback threw is left
    /// pending, for the release that freed the object to raise.
    ///
    /// ```php
    /// class C extends UConverter {
    ///   public $tag = 'x';
    ///   function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null { echo UConverter::reasonText($reason), " ", $this->tag ?? '?', "\n"; return null; }
    /// }
    /// $c = new C('ascii', 'utf-8');
    /// unset($c);
    /// ```
    /// ```expect
    /// REASON_CLOSE x
    /// REASON_CLOSE x
    /// ```
    ///
    /// ```php
    /// class T extends UConverter {
    ///   function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null { if ($reason == UConverter::REASON_CLOSE) throw new Exception("close"); return null; }
    /// }
    /// $c = new T('ascii', 'utf-8');
    /// try { $c = null; echo "after\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// echo "end\n";
    /// ```
    /// ```expect
    /// caught close
    /// end
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let (src, dest) = converters(req, object);
        let mut pending = Pending::default();
        for converter in [&src, &dest] {
            if converter.is_some() {
                told(req, object, &mut pending, Reason::Close);
            }
        }
        let given_up = object
            .native_mut::<ConverterObject>(req)
            .map(|o| [o.src.take(), o.dest.take()]);
        for converter in given_up.into_iter().flatten().flatten() {
            converter.close(req);
        }
        error::free::<ConverterObject>(req, object);
        pending.leave(req);
    }
}

/// `class UConverter`: a source and a destination converter of ICU's,
/// and the callbacks a subclass overrides to substitute what one cannot
/// convert.
///
/// ```php
/// $c = new UConverter('utf-8', 'latin1');
/// var_dump(bin2hex($c->convert("Espa\xF1ol")), bin2hex($c->convert("\xC3\xB1", true)), $c->getSourceEncoding(), $c->getDestinationEncoding());
/// var_dump($c->getSourceType() === UConverter::LATIN_1, $c->getDestinationType() === UConverter::UTF8, bin2hex($c->getSubstChars()));
/// var_dump($c->setSourceEncoding('ascii'), $c->getSourceEncoding(), $c->setDestinationEncoding('nope'), $c->getErrorCode(), $c->getErrorMessage(), $c->getDestinationEncoding());
/// var_dump($c->setSubstChars('!'), $c->getSubstChars(), $c->setSubstChars('<x>'), $c->getErrorMessage(), bin2hex($c->convert("a\xFFb")), $c->getErrorCode());
/// $d = clone $c; var_dump($d->getSourceEncoding(), bin2hex($d->convert("\xFF")));
/// try { new UConverter('nope'); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
/// $u = (new ReflectionClass('UConverter'))->newInstanceWithoutConstructor();
/// var_dump($u->getSourceEncoding(), $u->getDestinationType(), $u->getSubstChars(), $u->convert('x'), $u->getErrorMessage(), $u->setSubstChars('?'), $u->getErrorMessage());
/// try { clone $u; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(16) "45737061c3b16f6c"
/// string(2) "f1"
/// string(10) "ISO-8859-1"
/// string(5) "UTF-8"
/// bool(true)
/// bool(true)
/// string(2) "1a"
/// bool(true)
/// string(8) "US-ASCII"
/// bool(false)
/// int(4)
/// string(96) "UConverter::setDestinationEncoding(): returned error 4: U_FILE_ACCESS_ERROR: U_FILE_ACCESS_ERROR"
/// string(5) "UTF-8"
/// bool(true)
/// string(1) "!"
/// bool(false)
/// string(97) "UConverter::setSubstChars(): returned error 1: U_ILLEGAL_ARGUMENT_ERROR: U_ILLEGAL_ARGUMENT_ERROR"
/// string(10) "61efbfbd62"
/// int(0)
/// string(8) "US-ASCII"
/// string(6) "efbfbd"
/// UConverter::__construct(): returned error 4: U_FILE_ACCESS_ERROR
/// NULL
/// NULL
/// NULL
/// bool(false)
/// string(81) "UConverter::convert(): Internal converters not initialized: U_INVALID_STATE_ERROR"
/// bool(false)
/// string(102) "UConverter::setSubstChars(): Destination Converter has not been initialized yet: U_INVALID_STATE_ERROR"
/// Failed to clone UConverter
/// ```
///
/// A subclass's callbacks are told of each reset, character in error and
/// close, and what they answer is written in the character's place.
///
/// ```php
/// class Loud extends UConverter {
///     public function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null {
///         echo 'to ', UConverter::reasonText($reason), ' ', bin2hex($source), ' ', bin2hex($codeUnits), ' ', $error, "\n";
///         if ($reason == UConverter::REASON_ILLEGAL) { $error = U_ZERO_ERROR; return ['<', 0x1F600, '>']; }
///         return parent::toUCallback($reason, $source, $codeUnits, $error);
///     }
///     public function fromUCallback($reason, $source, $codePoint, &$error): string|int|array|null {
///         echo 'from ', UConverter::reasonText($reason), ' ', json_encode($source), ' ', $codePoint, ' ', $error, "\n";
///         return parent::fromUCallback($reason, $source, $codePoint, $error);
///     }
/// }
/// $c = new Loud('ascii', 'utf-8');
/// var_dump(bin2hex($c->convert("a\xFFb\xC3\xA9")));
/// $c = null;
/// $u = (new ReflectionClass('UConverter'))->newInstanceWithoutConstructor();
/// var_dump($u->toUCallback(UConverter::REASON_ILLEGAL, '', '', $error), $error, $u->fromUCallback(UConverter::REASON_RESET, [], 0, $error));
/// ```
/// ```expect
/// to REASON_RESET   0
/// to REASON_ILLEGAL 62c3a9 ff 12
/// to REASON_RESET   0
/// to REASON_ILLEGAL 62c3a9 ff 12
/// from REASON_RESET [] 0 0
/// from REASON_UNASSIGNED [128512] 128512 10
/// from REASON_UNASSIGNED [233] 233 10
/// from REASON_RESET [] 0 0
/// from REASON_UNASSIGNED [128512] 128512 10
/// from REASON_UNASSIGNED [233] 233 10
/// string(20) "613cefbfbd3e62efbfbd"
/// to REASON_CLOSE   0
/// from REASON_CLOSE [] 0 0
/// to REASON_CLOSE   0
/// from REASON_CLOSE [] 0 0
/// string(1) ""
/// int(27)
/// NULL
/// ```
#[php_class(not_serializable, size = 168, payload = ConverterObject, handlers = crate::converter::HANDLERS)]
pub(crate) struct UConverter;

#[php_class]
#[allow(non_snake_case, non_upper_case_globals)]
impl UConverter {
    const REASON_UNASSIGNED: i64 = 0;
    const REASON_ILLEGAL: i64 = 1;
    const REASON_IRREGULAR: i64 = 2;
    const REASON_RESET: i64 = 3;
    const REASON_CLOSE: i64 = 4;
    const REASON_CLONE: i64 = 5;
    const UNSUPPORTED_CONVERTER: i64 = -1;
    const SBCS: i64 = 0;
    const DBCS: i64 = 1;
    const MBCS: i64 = 2;
    const LATIN_1: i64 = 3;
    const UTF8: i64 = 4;
    const UTF16_BigEndian: i64 = 5;
    const UTF16_LittleEndian: i64 = 6;
    const UTF32_BigEndian: i64 = 7;
    const UTF32_LittleEndian: i64 = 8;
    const EBCDIC_STATEFUL: i64 = 9;
    const ISO_2022: i64 = 10;
    const LMBCS_1: i64 = 11;
    const LMBCS_2: i64 = 12;
    const LMBCS_3: i64 = 13;
    const LMBCS_4: i64 = 14;
    const LMBCS_5: i64 = 15;
    const LMBCS_6: i64 = 16;
    const LMBCS_8: i64 = 17;
    const LMBCS_11: i64 = 18;
    const LMBCS_16: i64 = 19;
    const LMBCS_17: i64 = 20;
    const LMBCS_18: i64 = 21;
    const LMBCS_19: i64 = 22;
    const LMBCS_LAST: i64 = 22;
    const HZ: i64 = 23;
    const SCSU: i64 = 24;
    const ISCII: i64 = 25;
    const US_ASCII: i64 = 26;
    const UTF7: i64 = 27;
    const BOCU1: i64 = 28;
    const UTF16: i64 = 29;
    const UTF32: i64 = 30;
    const CESU8: i64 = 31;
    const IMAP_MAILBOX: i64 = 32;

    #[php(before_args = error::reset)]
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] destination_encoding: Option<PhpString>,
        #[php(default = null)] source_encoding: Option<PhpString>,
    ) -> Answer<()> {
        let object = this.object();
        let utf8 = b"utf-8".as_slice();
        let src = source_encoding.as_ref().map_or(utf8, PhpString::as_bytes);
        let dest = destination_encoding
            .as_ref()
            .map_or(utf8, PhpString::as_bytes);
        error::constructing(req, |req| {
            // An ambiguous alias's warning throws and php goes on.
            let mut pending = Pending::default();
            if set_encoding(req, object, Side::Source, src, &mut pending) {
                set_encoding(req, object, Side::Destination, dest, &mut pending);
            }
            pending.answer(req, Ok(()))
        })
    }

    /// Since 8.5.12 the UTF-16 between the two encodings and the string
    /// are asked for fallibly, so a conversion past the memory limit is
    /// refused there and the limit's fatal is raised as the call returns:
    ///
    /// ```php
    /// ini_set('memory_limit', '8M');
    /// $c = new UConverter("UTF-32BE", "ISO-8859-1");
    /// var_dump($c->convert(str_repeat("A", 2 << 20)), "unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 4210688 bytes) in %s on line 4
    /// ```
    #[php(tentative)]
    fn convert(
        req: &mut Request,
        this: This<Self>,
        str: PhpString,
        #[php(default = false)] reverse: bool,
    ) -> Answer<OrFalse<Owner<PhpString>>> {
        let object = this.object();
        error::errors_reset::<ConverterObject>(req, object);
        let (src, dest) = converters(req, object);
        let (src, dest) = if reverse { (dest, src) } else { (src, dest) };
        match convert(req, Some(object), dest, src, str.as_bytes())? {
            Some(made) => Ok(OrFalse::Value(made)),
            None => Ok(OrFalse::False),
        }
    }

    #[php(tentative)]
    fn fromUCallback(
        req: &mut Request,
        this: This<Self>,
        reason: i64,
        source: PhpArray,
        codePoint: i64,
        error: Ref<Untyped<Value>>,
    ) -> Answer<Substitute> {
        let _ = (source, codePoint);
        default_callback(req, this.object(), reason, &error)
    }

    /// ```php
    /// var_dump(UConverter::getAliases('latin1')[0], in_array('ISO-8859-1', UConverter::getAliases('latin1')), UConverter::getAliases('nope'), intl_get_error_message());
    /// ```
    /// ```expect
    /// string(10) "ISO-8859-1"
    /// bool(true)
    /// array(0) {
    /// }
    /// string(12) "U_ZERO_ERROR"
    /// ```
    #[php(tentative)]
    fn getAliases(req: &mut Request, name: PhpString) -> Answer<Option<OrFalse<Owner<PhpArray>>>> {
        error::reset(req);
        let name = c_string(name.as_bytes());
        let count = match ucnv::count_aliases(&name) {
            Ok(count) => count,
            Err(code) => {
                failure(req, None, code)?;
                return Ok(Some(OrFalse::False));
            }
        };
        let mut aliases = PhpArray::new(req)?;
        for n in 0..count {
            match ucnv::alias(&name, n) {
                Ok(alias) => {
                    let alias = PhpString::new(req, &alias)?;
                    aliases.append(req, alias.into_value())?;
                }
                Err(code) => {
                    aliases.give_up(req);
                    failure(req, None, code)?;
                    return Ok(None);
                }
            }
        }
        Ok(Some(OrFalse::Value(aliases)))
    }

    /// ```php
    /// $all = UConverter::getAvailable();
    /// var_dump(count($all) > 100, $all[0], in_array('UTF-7', $all), array_is_list($all));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(5) "UTF-8"
    /// bool(true)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn getAvailable(req: &mut Request) -> Answer<Owner<PhpArray>> {
        error::reset(req);
        let mut names = PhpArray::new(req)?;
        for name in ucnv::available() {
            let name = PhpString::new(req, &name)?;
            names.append(req, name.into_value())?;
        }
        Ok(names)
    }

    #[php(tentative)]
    fn getDestinationEncoding(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<Option<OrFalse<Owner<PhpString>>>> {
        encoding(req, this.object(), Side::Destination)
    }

    #[php(tentative)]
    fn getDestinationType(req: &mut Request, this: This<Self>) -> Option<OrFalse<i64>> {
        kind(req, this.object(), Side::Destination)
    }

    #[php(tentative)]
    fn getErrorCode(req: &mut Request, this: This<Self>) -> i64 {
        i64::from(error::object_code::<ConverterObject>(req, this.object()).0)
    }

    #[php(tentative)]
    fn getErrorMessage(req: &mut Request, this: This<Self>) -> Answer<Option<Owner<PhpString>>> {
        let message = error::object_message::<ConverterObject>(req, this.object());
        PhpString::new(req, &message).map(Some)
    }

    #[php(tentative)]
    fn getSourceEncoding(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<Option<OrFalse<Owner<PhpString>>>> {
        encoding(req, this.object(), Side::Source)
    }

    #[php(tentative)]
    fn getSourceType(req: &mut Request, this: This<Self>) -> Option<OrFalse<i64>> {
        kind(req, this.object(), Side::Source)
    }

    /// ```php
    /// var_dump(UConverter::getStandards());
    /// ```
    /// ```expect
    /// array(7) {
    ///   [0]=>
    ///   string(5) "UTR22"
    ///   [1]=>
    ///   string(3) "IBM"
    ///   [2]=>
    ///   string(7) "WINDOWS"
    ///   [3]=>
    ///   string(4) "JAVA"
    ///   [4]=>
    ///   string(4) "IANA"
    ///   [5]=>
    ///   string(4) "MIME"
    ///   [6]=>
    ///   string(0) ""
    /// }
    /// ```
    #[php(tentative)]
    fn getStandards(req: &mut Request) -> Answer<Option<Owner<PhpArray>>> {
        error::reset(req);
        let mut standards = PhpArray::new(req)?;
        for n in 0..ucnv::count_standards() {
            match ucnv::standard(n) {
                Ok(name) => {
                    let name = PhpString::new(req, &name)?;
                    standards.append(req, name.into_value())?;
                }
                Err(code) => {
                    standards.give_up(req);
                    failure(req, None, code)?;
                    return Ok(None);
                }
            }
        }
        Ok(Some(standards))
    }

    #[php(tentative)]
    fn getSubstChars(
        req: &mut Request,
        this: This<Self>,
    ) -> Answer<Option<OrFalse<Owner<PhpString>>>> {
        let object = this.object();
        error::errors_reset::<ConverterObject>(req, object);
        let Some(src) = this.payload_mut(req)?.src.as_ref().map(Converter::id) else {
            return Ok(None);
        };
        let Some(chars) = src.get(req).map(|src| src.subst_chars()) else {
            return Ok(None);
        };
        match chars {
            Ok(chars) => PhpString::new(req, &chars).map(|s| Some(OrFalse::Value(s))),
            Err(code) => {
                failure(req, Some(object), code)?;
                Ok(Some(OrFalse::False))
            }
        }
    }

    /// ```php
    /// var_dump(UConverter::reasonText(UConverter::REASON_UNASSIGNED), UConverter::reasonText(UConverter::REASON_CLONE));
    /// try { UConverter::reasonText(6); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(17) "REASON_UNASSIGNED"
    /// string(12) "REASON_CLONE"
    /// UConverter::reasonText(): Argument #1 ($reason) must be a UConverter::REASON_* constant
    /// ```
    #[php(tentative)]
    fn reasonText(req: &mut Request, reason: i64) -> Answer<Owner<PhpString>> {
        error::reset(req);
        let Some(reason) = Reason::of(reason) else {
            let tail = "be a UConverter::REASON_* constant";
            return Err(arg_value_error(req, 1, tail));
        };
        PhpString::new(req, reason.name().as_bytes())
    }

    #[php(tentative)]
    fn setDestinationEncoding(
        req: &mut Request,
        this: This<Self>,
        encoding: PhpString,
    ) -> Answer<bool> {
        let object = this.object();
        error::errors_reset::<ConverterObject>(req, object);
        let mut pending = Pending::default();
        let set = set_encoding(
            req,
            object,
            Side::Destination,
            encoding.as_bytes(),
            &mut pending,
        );
        pending.answer(req, Ok(set))
    }

    /// A converter replaced while ICU is converting with it is closed
    /// only as the conversion ends (ADR-0013), so the conversion goes on
    /// with it and the next one uses the new one, which `convert`'s
    /// witness holds.
    #[php(tentative)]
    fn setSourceEncoding(req: &mut Request, this: This<Self>, encoding: PhpString) -> Answer<bool> {
        let object = this.object();
        error::errors_reset::<ConverterObject>(req, object);
        let mut pending = Pending::default();
        let set = set_encoding(req, object, Side::Source, encoding.as_bytes(), &mut pending);
        pending.answer(req, Ok(set))
    }

    #[php(tentative)]
    fn setSubstChars(req: &mut Request, this: This<Self>, chars: PhpString) -> Answer<bool> {
        let object = this.object();
        error::errors_reset::<ConverterObject>(req, object);
        let (src, dest) = converters(req, object);
        let (mut set, mut pending) = (true, Pending::default());
        for (converter, which) in [(src, "Source"), (dest, "Destination")] {
            let code =
                converter.and_then(|c| c.get(req).map(|c| c.set_subst_chars(chars.as_bytes())));
            let raised = if let Some(code) = code {
                if !code.is_failure() {
                    continue;
                }
                failure(req, Some(object), code)
            } else {
                let message = format!("{which} Converter has not been initialized yet");
                error::errors_set::<ConverterObject>(req, object, Code::INVALID_STATE, &message)
            };
            set = false;
            // php sets the destination's although the source's threw.
            pending.raised(req, raised);
        }
        pending.answer(req, Ok(set))
    }

    #[php(tentative)]
    fn toUCallback(
        req: &mut Request,
        this: This<Self>,
        reason: i64,
        source: PhpString,
        codeUnits: PhpString,
        error: Ref<Untyped<Value>>,
    ) -> Answer<Substitute> {
        let _ = (source, codeUnits);
        default_callback(req, this.object(), reason, &error)
    }

    /// ```php
    /// var_dump(bin2hex(UConverter::transcode("\xE4", 'utf-8', 'koi8-r')), UConverter::transcode("Snow\xE2\x98\x83", 'ascii', 'utf-8', ['to_subst' => '?']));
    /// var_dump(UConverter::transcode('x', 'ascii', 'utf-8', ['to_subst' => '??']), intl_get_error_message());
    /// var_dump(UConverter::transcode('x', 'nope', 'utf-8'), intl_get_error_message());
    /// var_dump(UConverter::transcode('x', 'utf-8', 'cp932'), intl_get_error_message());
    /// ini_set('intl.use_exceptions', 1);
    /// try { UConverter::transcode('x', 'utf-8', 'nope'); } catch (IntlException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(4) "d094"
    /// string(5) "Snow?"
    /// bool(false)
    /// string(93) "UConverter::transcode(): returned error 1: U_ILLEGAL_ARGUMENT_ERROR: U_ILLEGAL_ARGUMENT_ERROR"
    /// bool(false)
    /// string(93) "UConverter::transcode(): Error setting encoding: 4 - U_FILE_ACCESS_ERROR: U_FILE_ACCESS_ERROR"
    /// string(1) "x"
    /// string(105) "UConverter::transcode(): Ambiguous encoding specified, using ibm-943_P15A-2003: U_AMBIGUOUS_ALIAS_WARNING"
    /// UConverter::transcode(): Error setting encoding: 4 - U_FILE_ACCESS_ERROR
    /// ```
    ///
    /// Since 8.5.12 the UTF-16 between the two encodings and the string
    /// are asked for fallibly, so a conversion past the memory limit is
    /// refused there and the limit's fatal is raised as the call returns:
    ///
    /// ```php
    /// ini_set('memory_limit', '8M');
    /// var_dump(UConverter::transcode(str_repeat("A", 2 << 20), "UTF-32BE", "ISO-8859-1"), "unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 4210688 bytes) in %s on line 3
    /// ```
    #[php(tentative)]
    fn transcode(
        req: &mut Request,
        str: PhpString,
        toEncoding: PhpString,
        fromEncoding: PhpString,
        #[php(default = null)] options: Option<PhpArray>,
    ) -> Answer<OrFalse<Owner<PhpString>>> {
        error::reset(req);
        let mut pending = Pending::default();
        let src = open(req, None, fromEncoding.as_bytes(), &mut pending);
        let dest = match src {
            Some(_) => open(req, None, toEncoding.as_bytes(), &mut pending),
            None => None,
        };
        let (src, dest) = match (src, dest) {
            (Some(src), Some(dest)) => (src, dest),
            (src, dest) => {
                for converter in [src, dest].into_iter().flatten() {
                    converter.close(req);
                }
                return pending.answer(req, Ok(OrFalse::False));
            }
        };
        let mut code = Code::ZERO;
        for (key, converter) in [(&b"from_subst"[..], src.id()), (b"to_subst", dest.id())] {
            let found = options
                .as_ref()
                .and_then(|o| o.find(key))
                .map(|v| v.deref());
            if let Some(ValueRef::Str(chars)) = found.as_ref().map(Value::as_ref)
                && !code.is_failure()
            {
                let chars = chars.as_bytes();
                let chars = &chars[..chars.len() & 0x7F];
                code = converter
                    .get(req)
                    .map_or(Code::INVALID_STATE, |c| c.set_subst_chars(chars));
            }
        }
        let answer = if code.is_failure() {
            failure(req, None, code).map(|()| None)
        } else {
            // php leaves `return_value` null where the conversion fails;
            // with ICU's substituting callbacks on both it does not.
            convert(req, None, Some(dest.id()), Some(src.id()), str.as_bytes())
        };
        src.close(req);
        dest.close(req);
        let answer = match answer {
            Ok(Some(made)) if pending.is_set() => {
                made.give_up(req);
                Ok(None)
            }
            answer => answer,
        };
        match pending.answer(req, answer)? {
            Some(made) => Ok(OrFalse::Value(made)),
            None => Ok(OrFalse::False),
        }
    }
}

/// `THROW_UFAILURE(obj, error)`: `returned error <n>: <name>`, set on the
/// object and the request, or the request alone.
fn failure(req: &mut Request, object: Option<PhpObject>, code: Code) -> Answer<()> {
    let message = format!("returned error {}: {}", code.0, code.name());
    fail(req, object, code, &message)
}

/// `php_converter_throw_failure`: `intl_errors_set(obj ? &obj->error : NULL, ..)`.
fn fail(req: &mut Request, object: Option<PhpObject>, code: Code, message: &str) -> Answer<()> {
    match object {
        Some(object) => error::errors_set::<ConverterObject>(req, object, code, message),
        None => error::set(req, code, message),
    }
}

/// `ucnv_open` as `php_converter_set_encoding` opens: an ambiguous alias
/// warned of on the request and its converter kept, a failure set on the
/// object or on the request alone and no converter; what either threw
/// left in `pending`, as php leaves it pending and goes on.
fn open(
    req: &mut Request,
    object: Option<PhpObject>,
    name: &[u8],
    pending: &mut Pending,
) -> Option<Converter> {
    let (converter, code) = Converter::open(req, &c_string(name));
    if code == Code::AMBIGUOUS_ALIAS {
        let id = converter.as_ref().map(Converter::id);
        let actual = id.and_then(|id| id.get(req)).map(|c| c.name());
        let actual = match &actual {
            Some(Ok(actual)) => String::from_utf8_lossy(actual),
            _ => "(unknown)".into(),
        };
        let message = format!("Ambiguous encoding specified, using {actual}");
        let raised = error::set(req, code, &message);
        pending.raised(req, raised);
    } else if code.is_failure() {
        let raised = if object.is_some() {
            failure(req, object, code)
        } else {
            let message = format!("Error setting encoding: {} - {}", code.0, code.name());
            error::set(req, code, &message)
        };
        pending.raised(req, raised);
        if let Some(converter) = converter {
            converter.close(req);
        }
        return None;
    }
    converter
}

/// `php_converter_set_encoding` over one of the object's converters: the
/// new one opened, the old one closed with the object's callbacks told
/// while the object still holds it, and `false` where the name opens
/// none; what a warning or a callback threw left in `pending`.
fn set_encoding(
    req: &mut Request,
    object: PhpObject,
    side: Side,
    name: &[u8],
    pending: &mut Pending,
) -> bool {
    let Some(converter) = open(req, Some(object), name, pending) else {
        return false;
    };
    let held = object
        .native_mut::<ConverterObject>(req)
        .is_some_and(|o| o.side(side).is_some());
    if held {
        told(req, object, pending, Reason::Close);
    }
    let old = match object.native_mut::<ConverterObject>(req) {
        Some(o) => o.side(side).replace(converter),
        None => Some(converter),
    };
    if let Some(old) = old {
        old.close(req);
    }
    true
}

/// `php_converter_do_get_encoding`: the converter's name, null where it
/// has none.
fn encoding(
    req: &mut Request,
    object: PhpObject,
    side: Side,
) -> Answer<Option<OrFalse<Owner<PhpString>>>> {
    error::errors_reset::<ConverterObject>(req, object);
    let id = object
        .native_mut::<ConverterObject>(req)
        .and_then(|o| o.side(side).as_ref().map(Converter::id));
    let Some(name) = id.and_then(|id| id.get(req)).map(|c| c.name()) else {
        return Ok(None);
    };
    match name {
        Ok(name) => PhpString::new(req, &name).map(|s| Some(OrFalse::Value(s))),
        Err(code) => {
            failure(req, Some(object), code)?;
            Ok(Some(OrFalse::False))
        }
    }
}

/// `php_converter_do_get_type`: `ucnv_getType`, null where there is no
/// converter.
fn kind(req: &mut Request, object: PhpObject, side: Side) -> Option<OrFalse<i64>> {
    error::errors_reset::<ConverterObject>(req, object);
    let id = object
        .native_mut::<ConverterObject>(req)?
        .side(side)
        .as_ref()?
        .id();
    id.get(req).map(|c| OrFalse::Value(i64::from(c.kind())))
}

/// `php_converter_default_callback`, what `parent::toUCallback()` and
/// `parent::fromUCallback()` answer: for a character in error, the source
/// converter's substitute and `$error` cleared; nothing otherwise.
fn default_callback(
    req: &mut Request,
    object: PhpObject,
    reason: i64,
    error: &Ref<Untyped<Value>>,
) -> Answer<Substitute> {
    if !Reason::of(reason).is_some_and(Reason::is_error) {
        return Ok(None);
    }
    let (src, _) = converters(req, object);
    let mut pending = Pending::default();
    let (chars, code) = match src.and_then(|src| src.get(req).map(|src| src.subst_chars())) {
        None => {
            let message = "Source Converter has not been initialized yet";
            let raised = fail(req, Some(object), Code::INVALID_STATE, message);
            pending.raised(req, raised);
            (vec![0x1A], Code::INVALID_STATE)
        }
        Some(Ok(chars)) => (chars, Code::ZERO),
        Some(Err(code)) => {
            let raised = failure(req, Some(object), code);
            pending.raised(req, raised);
            (vec![0x1A], code)
        }
    };
    // php assigns `$error` and answers the characters with what it threw
    // pending.
    let assigned = error.set(req, i64::from(code.0));
    pending.raised(req, assigned);
    pending.answer(req, Ok(()))?;
    PhpString::new(req, &chars).map(|s| Some(Union::A(s)))
}

/// `php_converter_do_convert`: the source converted to UTF-16 and on to
/// the destination, each with ICU's preflight first as php sizes its
/// buffers, a subclass's callbacks served throughout; `None` with the
/// error set where ICU fails. A throwable a callback raised is the
/// answer, once the conversion has gone on to its end as php's does with
/// one pending.
///
/// Each converter is held for the conversion (ADR-0013), so a callback
/// that replaces the one ICU is converting with leaves its close to the
/// conversion's end: the conversion goes on with the old converter and
/// the next uses the new. `trace:intl-converter-replaced-in-callback` is
/// its divergence row, so this fence is held to the engine alone:
///
/// ```php
/// class Swap extends UConverter {
///     public $swapped = 0;
///     public function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null {
///         echo 'to ', UConverter::reasonText($reason), "\n";
///         if ($reason == UConverter::REASON_ILLEGAL && !$this->swapped++) {
///             var_dump($this->setSourceEncoding('latin1'), $this->getSourceEncoding());
///         }
///         $error = U_ZERO_ERROR;
///         return '?';
///     }
/// }
/// $c = new Swap('utf-8', 'ascii');
/// var_dump($c->convert("a\x80b\x81c"));
/// var_dump($c->getSourceEncoding(), $c->convert("\xE4"));
/// $c = null;
/// echo "done\n";
/// ```
/// ```expect
/// to REASON_RESET
/// to REASON_ILLEGAL
/// to REASON_CLOSE
/// bool(true)
/// string(10) "ISO-8859-1"
/// to REASON_ILLEGAL
/// to REASON_RESET
/// to REASON_ILLEGAL
/// to REASON_ILLEGAL
/// string(5) "a?b?c"
/// to REASON_RESET
/// to REASON_RESET
/// string(10) "ISO-8859-1"
/// string(2) "ä"
/// to REASON_CLOSE
/// to REASON_CLOSE
/// done
/// ```
fn convert(
    req: &mut Request,
    object: Option<PhpObject>,
    dest: Option<ConverterId>,
    src: Option<ConverterId>,
    text: &[u8],
) -> Answer<Option<Owner<PhpString>>> {
    let mut pending = Pending::default();
    // Each converter held for the whole conversion, so a callback that
    // replaces or frees it leaves its close to the conversion's end.
    let answer = match (src, dest) {
        (Some(src), Some(dest)) => src
            .held(req, |req, src| {
                dest.held(req, |req, dest| {
                    convert_with(req, object, &mut pending, dest, src, text)
                })
            })
            .flatten(),
        _ => None,
    };
    let Some(answer) = answer else {
        let message = "Internal converters not initialized";
        fail(req, object, Code::INVALID_STATE, message)?;
        return Ok(None);
    };
    let answer = match answer {
        Ok(Some(made)) if pending.is_set() => {
            made.give_up(req);
            Ok(None)
        }
        answer => answer,
    };
    pending.answer(req, answer)
}

/// [`convert`]'s body, what a callback threw left in `pending`: the
/// intermediate UTF-16 in a block of the request's heap and the string
/// made while it is held, both asked for fallibly since 8.5.12
/// (`safe_femalloc` and `zend_string_falloc`); `None` where either was
/// refused, as php's `NULL`, the delayed OOM recorded.
fn convert_with(
    req: &mut Request,
    object: Option<PhpObject>,
    pending: &mut Pending,
    dest: &Live<'_>,
    src: &Live<'_>,
    text: &[u8],
) -> Answer<Option<Owner<PhpString>>> {
    let (needed, code) = lend(req, object, pending, |cb| src.decode(&mut [], text, cb));
    if code.is_failure() && code != Code::BUFFER_OVERFLOW {
        pending.failure(req, object, code);
        return Ok(None);
    }
    let capacity = usize::try_from(needed).unwrap_or(0) + 1;
    let Some(temp) = Block::<u16>::ask(req, capacity + 1, true)? else {
        return Ok(None);
    };
    encoded(req, object, pending, (dest, src), temp, capacity, text)
}

/// The UTF-16 of `text` decoded into `temp`, `capacity` units of it,
/// then encoded into the string, which is asked for while `temp` is held.
/// The string is laid at the preflight's length before the pass that
/// writes it, as `zend_string_falloc(ret_len, 0, 1)` lays it ahead of the
/// second `ucnv_fromUChars` (`Z/ext/intl/converter/converter.c:668-676`),
/// so a `fromUCallback` served in that pass reads it in
/// `memory_get_usage()`. ICU writes `ret_len + 1` bytes beside it, the
/// room php's pass has with the NUL's, copied in after: a shorter length
/// is `ZSTR_LEN` set where the allocation stands, and the one byte more
/// a callback can add in the pass grows the string, where php's NUL
/// holds it. Each conversion is a converter of its own, and a plain
/// `UConverter` clears intl's error before the second baseline:
///
/// ```php
/// class F extends UConverter {
///   function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null {
///     if ($reason != UConverter::REASON_CLOSE) echo "to ", UConverter::reasonText($reason), " ", memory_get_usage() - $GLOBALS['m'], "\n";
///     $error = U_ZERO_ERROR;
///     return null;
///   }
///   function fromUCallback($reason, $source, $codePoint, &$error): string|int|array|null {
///     if ($reason != UConverter::REASON_CLOSE) echo "from ", UConverter::reasonText($reason), " ", memory_get_usage() - $GLOBALS['m'], "\n";
///     $error = U_ZERO_ERROR;
///     return null;
///   }
/// }
/// $a = new F('ASCII', 'UTF-8');
/// $b = new F('ASCII', 'UTF-8');
/// $m = memory_get_usage();
/// $a->convert("ab\u{100}");
/// (new UConverter('ASCII', 'UTF-8'))->convert("ab");
/// $m = memory_get_usage();
/// $b->convert("ab\xff", true);
/// ```
/// ```expect
/// to REASON_RESET 64
/// to REASON_RESET 80
/// from REASON_RESET 296
/// from REASON_UNASSIGNED 296
/// from REASON_RESET 328
/// from REASON_UNASSIGNED 328
/// to REASON_RESET 32
/// to REASON_ILLEGAL 96
/// to REASON_RESET 48
/// to REASON_ILLEGAL 112
/// from REASON_RESET 264
/// from REASON_RESET 296
/// ```
///
/// `temp` is given back where php frees it: after the failure of a
/// decode or of the preflight is raised, and before the final pass's is,
/// as `efree(temp)` precedes that `THROW_UFAILURE`
/// (`Z/ext/intl/converter/converter.c:677-679`). A warning handler served
/// by a final-pass failure reads the string alone, then, and a string it
/// lays larger than the UTF-16 moves the peak by the string alone. Two
/// lengths are read and their difference printed, so the message intl
/// keeps falls out of it:
///
/// ```php
/// @ini_set('intl.error_level', E_WARNING);
/// class F extends UConverter {
///   public int $calls = 0;
///   function fromUCallback($reason, $source, $codePoint, &$error): string|int|array|null {
///     if ($reason === self::REASON_UNASSIGNED) {
///       $error = ++$this->calls === 1 ? U_ZERO_ERROR : U_INVALID_CHAR_FOUND;
///       return '?';
///     }
///     return null;
///   }
/// }
/// $n = $live = 0;
/// set_error_handler(function () use (&$n, &$live) {
///   $live = memory_get_usage();
///   $pad = str_repeat('x', 4 * $n);
///   return true;
/// });
/// $at = [];
/// foreach ([100000, 200000] as $n) {
///   $c = new F('ASCII', 'UTF-8');
///   $s = str_repeat('a', $n) . "\u{100}";
///   memory_reset_peak_usage();
///   $m = memory_get_usage();
///   $r = $c->convert($s);
///   $at[] = [$live - $m, memory_get_peak_usage() - $m];
///   echo $c->calls, ' ', gettype($r), "\n";
/// }
/// echo $at[1][0] - $at[0][0], ' ', $at[1][1] - $at[0][1], "\n";
/// ```
/// ```expect
/// 2 boolean
/// 2 boolean
/// 98304 499712
/// ```
fn encoded(
    req: &mut Request,
    object: Option<PhpObject>,
    pending: &mut Pending,
    (dest, src): (&Live<'_>, &Live<'_>),
    mut temp: Block<u16>,
    capacity: usize,
    text: &[u8],
) -> Answer<Option<Owner<PhpString>>> {
    let room = &mut temp.as_mut_slice()[..capacity];
    let (len, code) = lend(req, object, pending, |cb| src.decode(room, text, cb));
    if code.is_failure() {
        pending.failure(req, object, code);
        temp.give_back(req);
        return Ok(None);
    }
    let units = &temp.as_slice()[..usize::try_from(len).unwrap_or(0).min(capacity)];
    let (needed, code) = lend(req, object, pending, |cb| dest.encode(&mut [], units, cb));
    if code.is_failure() && code != Code::BUFFER_OVERFLOW {
        pending.failure(req, object, code);
        temp.give_back(req);
        return Ok(None);
    }
    let needed = usize::try_from(needed).unwrap_or(0);
    let mut made = match req
        .memory_mut()
        .fallible(|req| FreshString::new(req, needed))
    {
        Ok(Some(made)) => made,
        refused => {
            temp.give_back(req);
            return refused.map(|_| None);
        }
    };
    let mut out = vec![0u8; needed + 1];
    let (len, code) = lend(req, object, pending, |cb| dest.encode(&mut out, units, cb));
    temp.give_back(req);
    if code.is_failure() {
        pending.failure(req, object, code);
        made.free(req);
        return Ok(None);
    }
    let len = usize::try_from(len).unwrap_or(0).min(out.len());
    if len > needed {
        made = made.extend(req, len)?;
    }
    made.set_len(len);
    made.bytes_mut().copy_from_slice(&out[..len]);
    Ok(Some(made.finish()))
}

/// `op` with the object's callbacks lent where its class is a subclass of
/// `UConverter`, as `php_converter_set_callbacks` installs them; a
/// throwable a callback raised left in `pending`, after which no more PHP
/// code is served, as `zend_call_function` serves none with one pending.
fn lend<R>(
    req: &mut Request,
    object: Option<PhpObject>,
    pending: &mut Pending,
    op: impl FnOnce(Option<&mut dyn Callbacks>) -> R,
) -> R {
    let base = lookup_class(req, b"UConverter").ok().flatten();
    let hooked = |o: &PhpObject| base.is_some_and(|base| o.class() != base);
    let Some(object) = object.filter(hooked) else {
        return op(None);
    };
    let mut served = Served {
        req,
        object,
        pending,
    };
    op(Some(&mut served))
}

/// What ICU tells a subclass's callbacks as it closes or clones one of the
/// object's converters ([`Callbacks::told`]).
fn told(req: &mut Request, object: PhpObject, pending: &mut Pending, reason: Reason) {
    lend(req, Some(object), pending, |callbacks| {
        if let Some(callbacks) = callbacks {
            callbacks.told(reason);
        }
    });
}

/// The callbacks of one object of a subclass, served on the request.
struct Served<'a> {
    req: &'a mut Request,
    object: PhpObject,
    pending: &'a mut Pending,
}

impl Served<'_> {
    /// `zend_call_known_fcc` with four arguments, the last `$error` by
    /// reference, and what the method answered appended by `append`:
    /// nothing is appended where it threw or one is pending, and ICU's
    /// status is what the reference holds after.
    ///
    /// The answer is appended first, and the answer and the arguments
    /// are given up after it with what the callback threw, or the append
    /// raised, pending, as php's `php_converter_to_u_callback` and
    /// `php_converter_from_u_callback` free `retval` and then `zargs`
    /// (`Z/ext/intl/converter/converter.c:235-249`, `:312-326`). So a
    /// destructor that throws over what the callback wrote into `&$error`
    /// chains what is pending as its `previous`. Where the callback threw:
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', 1);
    /// class D { function __destruct() { throw new Exception("destructed"); } }
    /// class C extends UConverter {
    ///   function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null {
    ///     if ($reason > UConverter::REASON_IRREGULAR) return null;
    ///     $error = new D; throw new Exception("to");
    ///   }
    ///   function fromUCallback($reason, $source, $codePoint, &$error): string|int|array|null {
    ///     if ($reason > UConverter::REASON_IRREGULAR) return null;
    ///     $error = new D; throw new Exception("from");
    ///   }
    /// }
    /// try { (new C('utf-8', 'ascii'))->convert("\x80"); }
    /// catch (Exception $e) { echo $e->getMessage(), " after ", $e->getPrevious()?->getMessage(), "\n"; }
    /// try { (new C('ascii', 'utf-8'))->convert("\u{e9}"); }
    /// catch (Exception $e) { echo $e->getMessage(), " after ", $e->getPrevious()?->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// destructed after to
    /// destructed after from
    /// ```
    ///
    /// Where the answer cannot be appended, its `IntlException` under
    /// `intl.use_exceptions`, or its warning under `intl.error_level`,
    /// comes before the destructor is served. What the destructor throws
    /// is the conversion's pending throwable from there on: the
    /// conversion's own failure chains over it, or raises its warning to
    /// no handler, and no further callback is served:
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', 1);
    /// class D { function __destruct() { echo "dtor\n"; throw new Exception("destructed"); } }
    /// class C extends UConverter {
    ///   function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null {
    ///     if ($reason > UConverter::REASON_IRREGULAR) return null;
    ///     $error = new D; return -1;
    ///   }
    ///   function fromUCallback($reason, $source, $codePoint, &$error): string|int|array|null {
    ///     if ($reason > UConverter::REASON_IRREGULAR) return null;
    ///     $error = new D; return [1.5];
    ///   }
    /// }
    /// function chain($e) { for ($s = []; $e; $e = $e->getPrevious()) $s[] = $e->getMessage(); return implode(" <- ", $s); }
    /// set_error_handler(function ($n, $m) { if ($n == E_WARNING) echo "warning: $m\n"; return true; });
    /// foreach ([1, 0] as $ex) {
    ///   ini_set('intl.use_exceptions', $ex);
    ///   ini_set('intl.error_level', $ex ? 0 : E_WARNING);
    ///   try { (new C('utf-8', 'ascii'))->convert("\x80"); } catch (Exception $e) { echo chain($e), "\n"; }
    ///   try { (new C('ascii', 'utf-8'))->convert("\u{e9}"); } catch (Exception $e) { echo chain($e), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// dtor
    /// UConverter::convert(): returned error 12: U_ILLEGAL_CHAR_FOUND <- destructed <- UConverter::convert(): Invalid codepoint U+ffffffffffffffff
    /// dtor
    /// UConverter::convert(): returned error 10: U_INVALID_CHAR_FOUND <- destructed <- UConverter::convert(): fromUCallback() specified illegal type for substitution character
    /// warning: UConverter::convert(): Invalid codepoint U+ffffffffffffffff
    /// dtor
    /// destructed
    /// warning: UConverter::convert(): fromUCallback() specified illegal type for substitution character
    /// dtor
    /// destructed
    /// ```
    ///
    /// A callback that answers for every reason is served at the close as
    /// well; the first destructor's throwable stops the callbacks after
    /// it, and is what is left uncaught:
    ///
    /// ```php
    /// class D { function __destruct() { echo "dtor\n"; throw new Exception("destructed"); } }
    /// class C extends UConverter {
    ///   function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null { $error = new D; return -1; }
    /// }
    /// $c = new C('utf-8', 'ascii');
    /// try { $c->convert("\x80"); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// dtor
    /// destructed
    /// end
    /// dtor
    ///
    /// Fatal error: Uncaught Exception: destructed in %s:2
    /// Stack trace:
    /// #0 [internal function]: D->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    fn call(
        &mut self,
        method: &[u8],
        reason: Reason,
        middle: [Owner<Value>; 2],
        status: &mut Code,
        append: impl FnOnce(&mut Self, Value),
    ) {
        if self.pending.is_set() {
            for v in middle {
                v.give_up(self.req);
            }
            return;
        }
        let req = &mut *self.req;
        let error = match PhpRef::new(req, Owner::int(i64::from(status.0))) {
            Ok(error) => error,
            Err(thrown) => {
                for v in middle {
                    v.give_up(req);
                }
                self.pending.keep(req, thrown);
                return;
            }
        };
        let [one, two] = middle;
        let arguments = [Value::int(reason.number()), *one, *two, error.as_value()];
        let answer = call_method(req, &self.object, method, &arguments);
        if let ValueRef::Int(n) = error.get().as_ref() {
            *status = crate::code_of(n);
        }
        let answer = match answer {
            Ok(answer) => {
                append(self, *answer);
                Some(answer)
            }
            Err(thrown) => {
                self.pending.keep(self.req, thrown);
                None
            }
        };
        let given = answer.into_iter().chain([one, two, error.into_value()]);
        self.give_up_past(given);
    }

    /// `zval_ptr_dtor` over each value in order, with what is pending in
    /// php's `EG(exception)` standing there while each is given up.
    fn give_up_past(&mut self, given: impl IntoIterator<Item = Owner<Value>>) {
        let req = &mut *self.req;
        let give_up = |req: &mut Request| {
            for value in given {
                value.give_up(req);
            }
        };
        match self.pending.0.as_ref() {
            Some(thrown) if thrown.exception().is_some() => {
                while_pending(req, thrown, give_up);
            }
            _ => give_up(req),
        }
        self.pending.stood(self.req);
    }

    /// A failure a callback's answer met, `php_converter_throw_failure`
    /// on the object; a throwable it raised stops the PHP code after it.
    fn fail(&mut self, code: Code, message: &str) {
        self.pending
            .fail(self.req, Some(self.object), code, message);
    }

    /// `TARGET_CHECK`: whether `needed` units fit, the overrun set where not.
    fn fits(&mut self, available: usize, needed: usize) -> bool {
        if available < needed {
            let message = format!("Buffer overrun {needed} bytes needed, {available} available");
            self.fail(Code::BUFFER_OVERFLOW, &message);
            return false;
        }
        true
    }

    /// `php_converter_append_toUnicode_target`.
    fn append_to_unicode(&mut self, value: Value, target: &mut Target<'_, u16>) {
        match value.as_ref() {
            ValueRef::Null => {}
            ValueRef::Int(c) => {
                if !(0..=0x0010_FFFF).contains(&c) {
                    self.fail(
                        Code::ILLEGAL_ARGUMENT,
                        &format!("Invalid codepoint U+{c:04x}"),
                    );
                } else if let Ok(unit) = u16::try_from(c) {
                    if self.fits(target.available(), 1) {
                        target.write(&[unit]);
                    }
                } else if self.fits(target.available(), 2) {
                    let c = c - 0x10000;
                    let pair = [(c >> 10) | 0xD800, (c & 0x3FF) | 0xDC00];
                    target.write(&pair.map(|u| u16::try_from(u).unwrap_or(0)));
                }
            }
            ValueRef::Str(s) => {
                let bytes = s.as_bytes();
                let mut i = 0;
                while i != bytes.len() && self.fits(target.available(), 1) {
                    let c = utf8::next(bytes, &mut i).to_le_bytes();
                    target.write(&[u16::from_le_bytes([c[0], c[1]])]);
                }
            }
            ValueRef::Array(array) => {
                // Read first: the appends write the converter, which
                // holds the request the rows walk borrows; no code of
                // the program runs in them.
                let values: Vec<Value> = walk::rows(self.req, array).map(|(_, v)| v).collect();
                for value in values {
                    self.append_to_unicode(value, target);
                }
            }
            _ => self.fail(
                Code::ILLEGAL_ARGUMENT,
                "toUCallback() specified illegal type for substitution character",
            ),
        }
    }

    /// `php_converter_append_fromUnicode_target`.
    fn append_from_unicode(&mut self, value: Value, target: &mut Target<'_, u8>) {
        match value.as_ref() {
            ValueRef::Null => {}
            ValueRef::Int(n) => {
                if self.fits(target.available(), 1) {
                    target.write(&[n.to_le_bytes()[0]]);
                }
            }
            ValueRef::Str(s) => {
                if self.fits(target.available(), s.len()) {
                    target.write(s.as_bytes());
                }
            }
            ValueRef::Array(array) => {
                // Read first, as `append_to_unicode` reads them.
                let values: Vec<Value> = walk::rows(self.req, array).map(|(_, v)| v).collect();
                for value in values {
                    self.append_from_unicode(value, target);
                }
            }
            _ => self.fail(
                Code::ILLEGAL_ARGUMENT,
                "fromUCallback() specified illegal type for substitution character",
            ),
        }
    }

    /// The bytes as a string argument, `ZVAL_STRINGL`'s string of the heap
    /// at every length, or `ZVAL_EMPTY_STRING`'s interned one where ICU
    /// handed no bytes; nothing where it cannot be made.
    fn string(&mut self, bytes: Option<&[u8]>) -> Option<Owner<Value>> {
        let made = match bytes {
            Some(bytes) => PhpString::filled(self.req, bytes.len(), |b| b.copy_from_slice(bytes)),
            None => PhpString::new(self.req, b""),
        };
        match made {
            Ok(s) => Some(s.into_value()),
            Err(thrown) => {
                self.pending.keep(self.req, thrown);
                None
            }
        }
    }
}

impl Callbacks for Served<'_> {
    /// `php_converter_to_u_callback`: `$source` and `$codeUnits` each
    /// `ZVAL_STRINGL`, `zend_string_init`, which lays a string of the
    /// heap at every length, the empty string and one byte among them,
    /// and `ZVAL_EMPTY_STRING`, the interned one, only where ICU hands no
    /// pointer, as it does at a reset
    /// (`Z/ext/intl/converter/converter.c:218-227`). So one illegal byte
    /// with nothing after it is two strings of 32 bytes. `fromUCallback`'s
    /// `$source` is `array_init_size` with `zend_hash_real_init_packed`
    /// (`:296-297`), its packed data laid even where it holds no code
    /// point, as at a reset. Each conversion is a converter of its own,
    /// and the failure the first leaves on intl's own error is cleared by
    /// a plain `UConverter` before the second measures, so neither
    /// failure's message moves what the second reads:
    ///
    /// ```php
    /// class F extends UConverter {
    ///   function toUCallback($reason, $source, $codeUnits, &$error): string|int|array|null {
    ///     if ($reason != UConverter::REASON_CLOSE) echo "to ", UConverter::reasonText($reason), " ", memory_get_usage() - $GLOBALS['m'], "\n";
    ///     return null;
    ///   }
    ///   function fromUCallback($reason, $source, $codePoint, &$error): string|int|array|null {
    ///     if ($reason != UConverter::REASON_CLOSE) echo "from ", UConverter::reasonText($reason), " ", memory_get_usage() - $GLOBALS['m'], "\n";
    ///     return null;
    ///   }
    /// }
    /// $a = new F('ASCII', 'UTF-8');
    /// $b = new F('ASCII', 'UTF-8');
    /// $m = memory_get_usage();
    /// $a->convert("ab\u{100}");
    /// (new UConverter('ASCII', 'UTF-8'))->convert("ab");
    /// $m = memory_get_usage();
    /// $b->convert("ab\xff", true);
    /// ```
    /// ```expect
    /// to REASON_RESET 64
    /// to REASON_RESET 80
    /// from REASON_RESET 296
    /// from REASON_UNASSIGNED 296
    /// to REASON_RESET 32
    /// to REASON_ILLEGAL 96
    /// ```
    fn decoding(
        &mut self,
        reason: Reason,
        source: Option<&[u8]>,
        units: Option<&[u8]>,
        status: &mut Code,
        target: &mut Target<'_, u16>,
    ) {
        let Some(source) = self.string(source) else {
            return;
        };
        let Some(units) = self.string(units) else {
            source.give_up(self.req);
            return;
        };
        self.call(
            b"toUCallback",
            reason,
            [source, units],
            status,
            |served, answer| {
                served.append_to_unicode(answer, target);
            },
        );
    }

    /// `php_converter_from_u_callback`: `$source` laid as
    /// `array_init_size` and `zend_hash_real_init_packed` lay it, for as
    /// many elements as there are code units and with its packed data
    /// even where there are none, then the code units as code points,
    /// `U16_NEXT` over them. The fence on `decoding` pins its size.
    fn encoding(
        &mut self,
        reason: Reason,
        units: &[u16],
        code_point: i32,
        status: &mut Code,
        target: &mut Target<'_, u8>,
    ) {
        let size = u32::try_from(units.len()).unwrap_or(u32::MAX);
        let mut source = match PhpArray::sized(self.req, size, Layout::Packed) {
            Ok(array) => array,
            Err(thrown) => {
                self.pending.keep(self.req, thrown);
                return;
            }
        };
        for c in char::decode_utf16(units.iter().copied()) {
            let c = c.map_or_else(
                |e| i64::from(e.unpaired_surrogate()),
                |c| i64::from(u32::from(c)),
            );
            if let Err(thrown) = source.append(self.req, Owner::int(c)) {
                self.pending.keep(self.req, thrown);
            }
        }
        let middle = [source.into_value(), Owner::int(i64::from(code_point))];
        self.call(
            b"fromUCallback",
            reason,
            middle,
            status,
            |served, answer| {
                served.append_from_unicode(answer, target);
            },
        );
    }
}

/// `php_converter_clone_object`: each converter cloned, a subclass's
/// callbacks told of each after (`UCNV_CLONE`), and the clone's error its
/// own.
fn clone_converter(req: &mut Request, object: PhpObject) -> Answer<Owner<PhpObject>> {
    let ce = crate::throwables::Throwables::of(req)?;
    let (src, dest) = converters(req, object);
    let mut pending = Pending::default();
    let mut cloned = |req: &mut Request, converter: Option<ConverterId>| {
        let copy = converter.ok_or(Code::ILLEGAL_ARGUMENT)?.try_clone(req)?;
        told(req, object, &mut pending, Reason::Clone);
        Ok::<_, Code>(copy)
    };
    let src = cloned(req, src).ok();
    let dest = match src {
        Some(_) => cloned(req, dest).ok(),
        None => None,
    };
    let (src, dest) = match (src, dest) {
        (Some(src), Some(dest)) => (src, dest),
        (src, dest) => {
            for copy in [src, dest].into_iter().flatten() {
                copy.close(req);
            }
            let thrown = throw_error(req, ce.error, "Failed to clone UConverter");
            return pending.answer(req, Err(thrown));
        }
    };
    if let Err(thrown) = pending.answer(req, Ok(())) {
        src.close(req);
        dest.close(req);
        return Err(thrown);
    }
    let payload = ConverterObject {
        src: Some(src),
        dest: Some(dest),
        error: IntlError::default(),
    };
    clone_with(req, &object, payload)
}
