//! The `xml` extension: `php-src/ext/xml` as one crate under `ext/`
//! (ADR-0004), `xml.c` and `compat.c` over libxml2's SAX2 push parser,
//! which `ext/libxml` binds and hands out as [`PushParser`] (design §4.10,
//! §7.2), with no `src/ffi/` of its own. The phpt subset is
//! `php-src/ext/xml/tests`, through `xtask phpt --subset`.
//!
//! Every `XMLParser` carries one payload, [`Parser`], php's `xml_parser`
//! with the expat-side half of `compat.c`'s `XML_Parser_Struct` beside it:
//! which `h_*` handlers were set ([`Registered`]), the program's callables
//! ([`Callbacks`]), the options, and what `xml_parse_into_struct` builds.
//! A parse is served by [`compat::Compat`].
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod compat;
mod natives;

use php_ext::abi::{
    Callable, Dependency, DependencyKind, Extension, ExtensionBuilder, GcTable, ObjectHandlers,
    Refusals, Std,
};
use php_ext::call::known_method;
use php_ext::coerce::{callable_checked, coerce_param};
use php_ext::decl::{Name, Type as DeclType};
use php_ext::raising::{Level, Thrown, arg_type_error, clear, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{Position, PushParser};

use compat::Collected;

/// The extension value (design §7.1).
///
/// ```php
/// var_dump(extension_loaded('xml'));
/// print_r((new ReflectionExtension('xml'))->getDependencies());
/// (new ReflectionExtension('xml'))->info();
/// var_dump(XML_SAX_IMPL, XML_OPTION_PARSE_HUGE, XML_ERROR_EXTERNAL_ENTITY_HANDLING);
/// ```
/// ```expect
/// bool(true)
/// Array
/// (
///     [libxml] => Required
/// )
///
/// xml
///
/// XML Support => active
/// XML Namespace Support => active
/// libxml2 Version => 2.9.13
/// string(6) "libxml"
/// int(5)
/// int(21)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            natives::xml_parser_create::declared(),
            natives::xml_parser_create_ns::declared(),
            natives::xml_set_object::declared(),
            natives::xml_set_element_handler::declared(),
            natives::xml_set_character_data_handler::declared(),
            natives::xml_set_processing_instruction_handler::declared(),
            natives::xml_set_default_handler::declared(),
            natives::xml_set_unparsed_entity_decl_handler::declared(),
            natives::xml_set_notation_decl_handler::declared(),
            natives::xml_set_external_entity_ref_handler::declared(),
            natives::xml_set_start_namespace_decl_handler::declared(),
            natives::xml_set_end_namespace_decl_handler::declared(),
            natives::xml_parse::declared(),
            natives::xml_parse_into_struct::declared(),
            natives::xml_get_error_code::declared(),
            natives::xml_error_string::declared(),
            natives::xml_get_current_line_number::declared(),
            natives::xml_get_current_column_number::declared(),
            natives::xml_get_current_byte_index::declared(),
            natives::xml_parser_free::declared(),
            natives::xml_parser_set_option::declared(),
            natives::xml_parser_get_option::declared(),
        ])
        .constants(natives::constants::declared())
        .dependencies(vec![Dependency {
            name: String::from("libxml"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .class::<XMLParser>()
        .info(xml_info)
        .build("xml")
}

/// `PHP_MINFO_FUNCTION(xml)`, `LIBXML_DOTTED_VERSION` read off libxml's
/// constant.
fn xml_info(req: &mut Request) {
    let version = match php_ext::call::constant(req, b"LIBXML_DOTTED_VERSION") {
        Ok(Some(v)) => v
            .as_php_string()
            .map(|s| String::from_utf8_lossy(s.as_bytes()).into_owned()),
        _ => None,
    }
    .unwrap_or_default();
    req.extensions_mut().info_table(&[
        &["XML Support", "active"],
        &["XML Namespace Support", "active"],
        &["libxml2 Version", &version],
    ]);
}

// ---- the object

/// The handlers `xml.c` keeps a callable for, in the order `xml_set_object`
/// walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hook {
    StartElement,
    EndElement,
    CharacterData,
    ProcessingInstruction,
    Default,
    UnparsedEntityDecl,
    NotationDecl,
    ExternalEntityRef,
    StartNamespaceDecl,
    EndNamespaceDecl,
}

impl Hook {
    const ALL: [Hook; 10] = [
        Hook::StartElement,
        Hook::EndElement,
        Hook::CharacterData,
        Hook::ProcessingInstruction,
        Hook::Default,
        Hook::UnparsedEntityDecl,
        Hook::NotationDecl,
        Hook::ExternalEntityRef,
        Hook::StartNamespaceDecl,
        Hook::EndNamespaceDecl,
    ];

    /// The setter a program called, which `xml_set_object`'s refusal names.
    fn setter(self) -> &'static str {
        match self {
            Hook::StartElement | Hook::EndElement => "xml_set_element_handler()",
            Hook::CharacterData => "xml_set_character_data_handler()",
            Hook::ProcessingInstruction => "xml_set_processing_instruction_handler()",
            Hook::Default => "xml_set_default_handler()",
            Hook::UnparsedEntityDecl => "xml_set_unparsed_entity_decl_handler()",
            Hook::NotationDecl => "xml_set_notation_decl_handler()",
            Hook::ExternalEntityRef => "xml_set_external_entity_ref_handler()",
            Hook::StartNamespaceDecl => "xml_set_start_namespace_decl_handler()",
            Hook::EndNamespaceDecl => "xml_set_end_namespace_decl_handler()",
        }
    }
}

/// `compat.c`'s `h_*` pointers: which `XML_Set*Handler` was called, which
/// is what routes a callback to `xml.c` rather than to the default
/// handler's text, whether or not a callable stands behind it. The element
/// pair is set together, under [`Hook::StartElement`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Registered(u16);

impl Registered {
    fn bit(hook: Hook) -> u16 {
        1 << (hook as u16)
    }

    pub(crate) fn set(&mut self, hook: Hook) {
        self.0 |= Registered::bit(hook);
    }

    pub(crate) fn has(self, hook: Hook) -> bool {
        self.0 & Registered::bit(hook) != 0
    }
}

/// One callable a handler slot holds: `zend_fcall_info_cache` kept as
/// `zend_fcc_dup` keeps it, and for one `xml_set_object`'s object
/// resolved by name, the name, which a later `xml_set_object` resolves
/// again.
pub(crate) struct Handler {
    pub(crate) callable: Owner<Callable>,
    by_name: Option<Vec<u8>>,
}

/// The program's callables, one slot per [`Hook`].
#[derive(Default)]
pub(crate) struct Callbacks([Option<Handler>; 10]);

impl Callbacks {
    pub(crate) fn get(&self, hook: Hook) -> Option<&Handler> {
        self.0[hook as usize].as_ref()
    }

    fn slot(&mut self, hook: Hook) -> &mut Option<Handler> {
        &mut self.0[hook as usize]
    }
}

/// `xml_encoding`: a target encoding and how UTF-8 is decoded into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Target {
    Iso88591,
    UsAscii,
    #[default]
    Utf8,
}

impl Target {
    /// `xml_get_encoding`, by name without case.
    fn named(name: &[u8]) -> Option<Target> {
        [Target::Iso88591, Target::UsAscii, Target::Utf8]
            .into_iter()
            .find(|t| t.name().as_bytes().eq_ignore_ascii_case(name))
    }

    fn name(self) -> &'static str {
        match self {
            Target::Iso88591 => "ISO-8859-1",
            Target::UsAscii => "US-ASCII",
            Target::Utf8 => "UTF-8",
        }
    }

    /// `xml_utf8_decode`: UTF-8 as it is, or each character as its byte in
    /// the target, `?` for one it lacks and for each ill-formed sequence.
    pub(crate) fn decode(self, s: &[u8]) -> Vec<u8> {
        let top = match self {
            Target::Utf8 => return s.to_vec(),
            Target::Iso88591 => 0xff,
            Target::UsAscii => 0x7f,
        };
        let mut out = Vec::with_capacity(s.len());
        let mut pos = 0;
        while pos < s.len() {
            let (c, next) = php_ext_standard::php_next_utf8_char(s, pos);
            pos = next;
            out.push(
                match c.map(|c| if c > 0xff { u32::from(b'?') } else { c }) {
                    Some(c) if c <= top => u8::try_from(c).unwrap_or(b'?'),
                    _ => b'?',
                },
            );
        }
        out
    }
}

/// The parser's libxml half as it stands: the `xmlParserCtxt`, or while a parse
/// holds it (`isparsing`), where it stood when a handler was last served,
/// what the position getters read.
#[derive(Default)]
pub(crate) enum Engine {
    /// Before `xml_parser_create` laid one down.
    #[default]
    Unmade,
    Idle(PushParser),
    Parsing(Position),
}

impl Engine {
    pub(crate) fn note(&mut self, at: Position) {
        if let Engine::Parsing(p) = self {
            *p = at;
        }
    }

    fn position(&self) -> Position {
        match self {
            Engine::Unmade => Position::default(),
            Engine::Idle(p) => p.position(),
            Engine::Parsing(p) => *p,
        }
    }

    fn parsing(&self) -> bool {
        matches!(self, Engine::Parsing(_))
    }
}

/// `xml_parser`, the payload of an `XMLParser`.
#[derive(Default)]
pub(crate) struct Parser {
    pub(crate) engine: Engine,
    /// `_ns_separator`: `None` for a parser without namespaces.
    pub(crate) separator: Option<Vec<u8>>,
    pub(crate) registered: Registered,
    pub(crate) handlers: Box<Callbacks>,
    /// `xml_set_object`'s object, an owner of it.
    object: Option<Owner<PhpObject>>,
    /// `parser->data` and `parser->info`, once `xml_parse_into_struct` set them.
    pub(crate) into: Option<Collected>,
    pub(crate) level: i32,
    pub(crate) curtag: i32,
    pub(crate) ctag_index: i64,
    /// `ltags`, `XML_MAXLEVEL` slots once `xml_parse_into_struct` made them.
    pub(crate) ltags: Vec<Option<Vec<u8>>>,
    pub(crate) lastwasopen: bool,
    pub(crate) options: Options,
}

/// The five `XML_OPTION_*` a program sets.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Options {
    pub(crate) case_folding: bool,
    pub(crate) target: Target,
    pub(crate) skip_tagstart: usize,
    pub(crate) skip_white: bool,
    pub(crate) parse_huge: bool,
}

pub(crate) fn parser_mut(req: &mut Request, object: PhpObject) -> Option<&mut Parser> {
    object.native_mut::<Parser>(req)
}

/// `xml_parser_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: construction, cloning and comparison.
const REFUSALS: Refusals = Refusals::NONE
    .construction(Some("xml_parser_create() or xml_parser_create_ns()"))
    .cloning()
    .comparison();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `xml_parser_get_gc`: the object, each callable's owner and the two
    /// references `xml_parse_into_struct` keeps.
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(p) = object.native::<Parser>(req) else {
            return GcTable::Properties;
        };
        if let Some(o) = &p.object {
            visit(o.as_value());
        }
        for h in p.handlers.0.iter().flatten() {
            h.callable.visit(visit);
        }
        if let Some(into) = &p.into {
            into.visit(visit);
        }
        GcTable::Properties
    }

    /// `xml_parser_free_obj`.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let taken = parser_mut(req, object).map(std::mem::take);
        if let Some(p) = taken {
            if let Some(into) = p.into {
                into.release(req);
            }
            for h in p.handlers.0.into_iter().flatten() {
                h.callable.give_up(req);
            }
            if let Some(o) = p.object {
                o.give_up(req);
            }
        }
        Std.free_obj(req, object);
    }
}

// ---- what the natives share

/// `php_xml_parser_create_impl`.
fn create(
    req: &mut Request,
    encoding: Option<PhpString>,
    separator: Option<Vec<u8>>,
) -> Result<Owner<PhpObject>, Thrown> {
    let target = match encoding {
        None => Target::Utf8,
        Some(e) if e.is_empty() => Target::Utf8,
        Some(e) => {
            let Some(t) = Target::named(e.as_bytes()) else {
                let tail = "is not a supported source encoding";
                return Err(php_ext::raising::argument_error(req, "ValueError", 1, tail));
            };
            t
        }
    };
    let class = parser_class(req)?;
    let object = php_ext::call::new_object(req, class)?;
    let Some(push) = PushParser::new(separator.is_some()) else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: libxml made no parser",
        ));
    };
    if let Some(p) = parser_mut(req, *object) {
        p.engine = Engine::Idle(push);
        p.separator = separator;
        p.options.target = target;
        p.options.case_folding = true;
    }
    Ok(object)
}

fn parser_class(req: &mut Request) -> Result<php_ext::values::ClassId, Thrown> {
    match php_ext::call::lookup_class(req, b"XMLParser")? {
        Some(class) => Ok(class),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: XMLParser is not linked",
        )),
    }
}

/// `xml_parse_helper`: one chunk through the parser, the `xmlParserCtxt` out of
/// the object while libxml holds it; `1` where it parsed with nothing
/// above a warning.
fn parse(req: &mut Request, object: PhpObject, data: &[u8], last: bool) -> Result<i64, Thrown> {
    let Some(p) = parser_mut(req, object) else {
        return Ok(0);
    };
    let at = p.engine.position();
    let Engine::Idle(mut push) = std::mem::replace(&mut p.engine, Engine::Parsing(at)) else {
        return Ok(0);
    };
    let huge = p.options.parse_huge;
    let mut compat = compat::Compat { object };
    let (ok, raised, thrown) = push.parse(req, data, last, huge, &mut compat);
    if let Some(p) = parser_mut(req, object) {
        p.engine = Engine::Idle(push);
    }
    php_ext_libxml::Errors::settle(req, raised, thrown)?;
    Ok(i64::from(ok))
}

/// What one `callable|string|null` argument is, as `zend_parse_parameters`
/// tries it quietly: a callable or `null` (`F!`), a string (`S`, under the
/// caller's `strict_types`), or neither.
enum Given {
    Callable(Callable),
    Null,
    Name(Vec<u8>),
    Neither,
}

impl Given {
    fn of(req: &mut Request, value: &Value, position: u32) -> Result<Given, Thrown> {
        if matches!(value.as_ref(), ValueRef::Null) {
            return Ok(Given::Null);
        }
        if let Ok(callable) = callable_checked(req, value) {
            return Ok(Given::Callable(callable));
        }
        match coerce_param(req, value, &DeclType::String, position) {
            // The slot holds the string the coercion made; no owner is
            // taken.
            Ok(s) => Ok(Given::Name(
                s.as_php_string()
                    .map(|s| s.as_bytes().to_vec())
                    .unwrap_or_default(),
            )),
            Err(thrown) => clear(req, thrown).map(|()| Given::Neither),
        }
    }

    fn is_name(&self) -> bool {
        matches!(self, Given::Name(_))
    }
}

/// `php_xml_check_string_method_arg`: the handler a method name names on
/// `xml_set_object`'s object; `None` for the empty name, which unsets it.
fn by_name(
    req: &mut Request,
    parser: PhpObject,
    name: &[u8],
    position: u32,
) -> Result<Option<Handler>, Option<Thrown>> {
    let object = parser_mut(req, parser).and_then(|p| p.object.as_deref().copied());
    method_on(req, object, name, position)
}

/// [`by_name`] against `object`, which `xml_set_object` asks of the
/// object it is about to set. With `position` zero, `xml_set_object`'s
/// own check, a failure raises nothing.
fn method_on(
    req: &mut Request,
    object: Option<PhpObject>,
    name: &[u8],
    position: u32,
) -> Result<Option<Handler>, Option<Thrown>> {
    if name.is_empty() {
        return Ok(None);
    }
    let Some(object) = object else {
        let tail = "an object must be set via xml_set_object() to be able to lookup method";
        return Err(Some(php_ext::raising::argument_error(
            req,
            "ValueError",
            position,
            tail,
        )));
    };
    let class = object.class();
    match known_method(req, class, &Name(name.into()), Some(object)) {
        Some(callable) => Ok(Some(Handler {
            callable: Owner::take(req, callable),
            by_name: Some(name.to_vec()),
        })),
        None if position == 0 => Err(None),
        None => {
            let class_name = req
                .symbols()
                .class_decl(class)
                .map(|d| String::from_utf8_lossy(&d.name.0).into_owned())
                .unwrap_or_default();
            let tail = format!(
                "method {class_name}::{}() does not exist",
                String::from_utf8_lossy(name)
            );
            Err(Some(php_ext::raising::argument_error(
                req,
                "ValueError",
                position,
                &tail,
            )))
        }
    }
}

/// The handler an argument of a setter gives, the deprecation of a name
/// raised first: `Some(None)` unsets the slot.
fn handler_of(
    req: &mut Request,
    parser: PhpObject,
    given: Given,
    position: u32,
) -> Result<Option<Handler>, Thrown> {
    match given {
        Given::Null => Ok(None),
        Given::Callable(callable) => Ok(Some(Handler {
            callable: Owner::take(req, callable),
            by_name: None,
        })),
        Given::Name(name) => by_name(req, parser, &name, position).map_err(|e| {
            e.unwrap_or_else(|| {
                php_ext::raising::fatal(req, "InternalError: unreachable handler check")
            })
        }),
        Given::Neither => Err(arg_type_error(
            req,
            position,
            "be of type callable|string|null",
        )),
    }
}

/// `Passing non-callable strings is deprecated since 8.4`.
fn deprecate_name(req: &mut Request) -> Result<(), Thrown> {
    docref(
        req,
        Level::Deprecated,
        "Passing non-callable strings is deprecated since 8.4",
    );
    php_ext::raising::pending(req)
}

/// `xml_set_handler` over each slot, the old callable's owner released,
/// and `XML_Set*Handler`'s registration.
fn install(req: &mut Request, parser: PhpObject, slots: Vec<(Hook, Option<Handler>)>) {
    let mut released = Vec::new();
    if let Some(p) = parser_mut(req, parser) {
        for (hook, handler) in slots {
            if let Some(old) = std::mem::replace(p.handlers.slot(hook), handler) {
                released.push(old.callable);
            }
            p.registered.set(hook);
        }
    }
    for old in released {
        old.give_up(req);
    }
}

/// One of the eight single-handler setters.
fn set_handler(
    req: &mut Request,
    parser: PhpObject,
    hook: Hook,
    given: &Value,
) -> Result<(), Thrown> {
    let given = Given::of(req, given, 2)?;
    if given.is_name() {
        deprecate_name(req)?;
    }
    let handler = handler_of(req, parser, given, 2)?;
    install(req, parser, vec![(hook, handler)]);
    Ok(())
}

/// `xml_set_element_handler`'s four tries, then its refusal.
fn set_element_handler(
    req: &mut Request,
    parser: PhpObject,
    start: &Value,
    end: &Value,
) -> Result<(), Thrown> {
    let start_raw = *start;
    let start = Given::of(req, start, 2)?;
    let end = Given::of(req, end, 3)?;
    let names = start.is_name() || end.is_name();
    if matches!(start, Given::Neither) || matches!(end, Given::Neither) {
        // `Ozz`: the start is refused unless it is `null` or a string.
        let position = if matches!(start_raw.as_ref(), ValueRef::Null | ValueRef::Str(_)) {
            3
        } else {
            2
        };
        return Err(arg_type_error(
            req,
            position,
            "be of type callable|string|null",
        ));
    }
    if names {
        deprecate_name(req)?;
    }
    let start = handler_of(req, parser, start, 2)?;
    let end = match handler_of(req, parser, end, 3) {
        Ok(end) => end,
        Err(thrown) => {
            if let Some(h) = start {
                h.callable.give_up(req);
            }
            return Err(thrown);
        }
    };
    install(
        req,
        parser,
        vec![(Hook::StartElement, start), (Hook::EndElement, end)],
    );
    Ok(())
}

/// `xml_set_object`: each handler resolved by name against the old object
/// resolved again against `new`, then `new` kept.
fn set_object(req: &mut Request, parser: PhpObject, new: PhpObject) -> Result<(), Thrown> {
    if parser_mut(req, parser).is_some_and(|p| p.object.is_some()) {
        for hook in Hook::ALL {
            let Some(name) = parser_mut(req, parser)
                .and_then(|p| p.handlers.get(hook))
                .and_then(|h| h.by_name.clone())
            else {
                continue;
            };
            let old = parser_mut(req, parser).and_then(|p| p.handlers.slot(hook).take());
            if let Some(old) = old {
                old.callable.give_up(req);
            }
            let Ok(handler) = method_on(req, Some(new), &name, 0) else {
                let class_name = req
                    .symbols()
                    .class_decl(new.class())
                    .map(|d| String::from_utf8_lossy(&d.name.0).into_owned())
                    .unwrap_or_default();
                let tail = format!(
                    "cannot safely swap to object of class {class_name} as method \"{}\" does not exist, which was set via {}",
                    String::from_utf8_lossy(&name),
                    hook.setter()
                );
                return Err(php_ext::raising::argument_error(
                    req,
                    "ValueError",
                    2,
                    &tail,
                ));
            };
            if let Some(p) = parser_mut(req, parser) {
                *p.handlers.slot(hook) = handler;
            }
        }
    }
    let new = Owner::take(req, new);
    let old = match parser_mut(req, parser) {
        Some(p) => p.object.replace(new),
        None => Some(new),
    };
    if let Some(old) = old {
        old.give_up(req);
    }
    Ok(())
}

/// `class XMLParser`: `final`, `@strict-properties`, `@not-serializable`,
/// uncloneable and not comparable, made by `xml_parser_create` alone. The
/// object is `sizeof(xml_parser)`, and `XML_ParserCreate_MM` allocates
/// `compat.c`'s `XML_Parser_Struct` beside it.
///
/// ```php
/// try { new XMLParser; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $a = memory_get_usage(); $p = xml_parser_create(); var_dump(memory_get_usage() - $a);
/// try { clone $p; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $p->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { serialize($p); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump($p == xml_parser_create());
/// ```
/// ```expect
/// Cannot directly construct XMLParser, use xml_parser_create() or xml_parser_create_ns() instead
/// int(768)
/// Trying to clone an uncloneable object of class XMLParser
/// Cannot create dynamic property XMLParser::$x
/// Serialization of 'XMLParser' is not allowed
/// bool(false)
/// ```
#[php_ext::attr::php_class(
    final,
    not_serializable,
    size = 576, beside(120), payload = Parser,
    handlers = HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct XMLParser;

#[php_ext::attr::php_class]
impl XMLParser {}
