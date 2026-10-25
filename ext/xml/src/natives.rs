//! `xml.stub.php`'s constants and functions.

use php_ext::abi::{Instance, OrFalse, Raw, Ref, True, Union, Untyped};
use php_ext::attr::{php_const, php_function};
use php_ext::coerce::{to_long, truthy, type_name, with_string};
use php_ext::raising::{Level, Thrown, arg_value_error, argument_error, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use crate::compat::Collected;
use crate::{
    Engine, Hook, Target, XMLParser, create, parse, parser_mut, set_element_handler, set_handler,
    set_object,
};

/// `register_xml_symbols`: `XML_ERROR_*` as `expat_compat.h` numbers them,
/// the options, and `XML_SAX_IMPL`.
#[php_const]
pub(crate) mod constants {
    pub(crate) const XML_ERROR_NONE: i64 = 0;
    pub(crate) const XML_ERROR_NO_MEMORY: i64 = 1;
    pub(crate) const XML_ERROR_SYNTAX: i64 = 2;
    pub(crate) const XML_ERROR_NO_ELEMENTS: i64 = 3;
    pub(crate) const XML_ERROR_INVALID_TOKEN: i64 = 4;
    pub(crate) const XML_ERROR_UNCLOSED_TOKEN: i64 = 5;
    pub(crate) const XML_ERROR_PARTIAL_CHAR: i64 = 6;
    pub(crate) const XML_ERROR_TAG_MISMATCH: i64 = 7;
    pub(crate) const XML_ERROR_DUPLICATE_ATTRIBUTE: i64 = 8;
    pub(crate) const XML_ERROR_JUNK_AFTER_DOC_ELEMENT: i64 = 9;
    pub(crate) const XML_ERROR_PARAM_ENTITY_REF: i64 = 10;
    pub(crate) const XML_ERROR_UNDEFINED_ENTITY: i64 = 11;
    pub(crate) const XML_ERROR_RECURSIVE_ENTITY_REF: i64 = 12;
    pub(crate) const XML_ERROR_ASYNC_ENTITY: i64 = 13;
    pub(crate) const XML_ERROR_BAD_CHAR_REF: i64 = 14;
    pub(crate) const XML_ERROR_BINARY_ENTITY_REF: i64 = 15;
    pub(crate) const XML_ERROR_ATTRIBUTE_EXTERNAL_ENTITY_REF: i64 = 16;
    pub(crate) const XML_ERROR_MISPLACED_XML_PI: i64 = 17;
    pub(crate) const XML_ERROR_UNKNOWN_ENCODING: i64 = 18;
    pub(crate) const XML_ERROR_INCORRECT_ENCODING: i64 = 19;
    pub(crate) const XML_ERROR_UNCLOSED_CDATA_SECTION: i64 = 20;
    pub(crate) const XML_ERROR_EXTERNAL_ENTITY_HANDLING: i64 = 21;
    pub(crate) const XML_OPTION_CASE_FOLDING: i64 = 1;
    pub(crate) const XML_OPTION_TARGET_ENCODING: i64 = 2;
    pub(crate) const XML_OPTION_SKIP_TAGSTART: i64 = 3;
    pub(crate) const XML_OPTION_SKIP_WHITE: i64 = 4;
    pub(crate) const XML_OPTION_PARSE_HUGE: i64 = 5;
    pub(crate) const XML_SAX_IMPL: &str = "libxml";
}

/// One `XML_OPTION_*`, decoded once from the number a program passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setting {
    CaseFolding,
    TargetEncoding,
    SkipTagstart,
    SkipWhite,
    ParseHuge,
}

impl Setting {
    fn of(n: i64) -> Option<Setting> {
        match n {
            constants::XML_OPTION_CASE_FOLDING => Some(Setting::CaseFolding),
            constants::XML_OPTION_TARGET_ENCODING => Some(Setting::TargetEncoding),
            constants::XML_OPTION_SKIP_TAGSTART => Some(Setting::SkipTagstart),
            constants::XML_OPTION_SKIP_WHITE => Some(Setting::SkipWhite),
            constants::XML_OPTION_PARSE_HUGE => Some(Setting::ParseHuge),
            _ => None,
        }
    }
}

/// `error_mapping`: `XML_ErrorString`'s table, indexed by libxml's
/// `xmlParserErrors` code.
const ERRORS: [&str; 102] = [
    "No error",
    "No memory",
    "Invalid document start",
    "Empty document",
    "Not well-formed (invalid token)",
    "Invalid document end",
    "Invalid hexadecimal character reference",
    "Invalid decimal character reference",
    "Invalid character reference",
    "Invalid character",
    "XML_ERR_CHARREF_AT_EOF",
    "XML_ERR_CHARREF_IN_PROLOG",
    "XML_ERR_CHARREF_IN_EPILOG",
    "XML_ERR_CHARREF_IN_DTD",
    "XML_ERR_ENTITYREF_AT_EOF",
    "XML_ERR_ENTITYREF_IN_PROLOG",
    "XML_ERR_ENTITYREF_IN_EPILOG",
    "XML_ERR_ENTITYREF_IN_DTD",
    "PEReference at end of document",
    "PEReference in prolog",
    "PEReference in epilog",
    "PEReference: forbidden within markup decl in internal subset",
    "XML_ERR_ENTITYREF_NO_NAME",
    "EntityRef: expecting ';'",
    "PEReference: no name",
    "PEReference: expecting ';'",
    "Undeclared entity error",
    "Undeclared entity warning",
    "Unparsed Entity",
    "XML_ERR_ENTITY_IS_EXTERNAL",
    "XML_ERR_ENTITY_IS_PARAMETER",
    "Unknown encoding",
    "Unsupported encoding",
    "String not started expecting ' or \"",
    "String not closed expecting \" or '",
    "Namespace declaration error",
    "EntityValue: \" or ' expected",
    "EntityValue: \" or ' expected",
    "< in attribute",
    "Attribute not started",
    "Attribute not finished",
    "Attribute without value",
    "Attribute redefined",
    "SystemLiteral \" or ' expected",
    "SystemLiteral \" or ' expected",
    "Comment not finished",
    "Processing Instruction not started",
    "Processing Instruction not finished",
    "NOTATION: Name expected here",
    "'>' required to close NOTATION declaration",
    "'(' required to start ATTLIST enumeration",
    "'(' required to start ATTLIST enumeration",
    "MixedContentDecl : '|' or ')*' expected",
    "XML_ERR_MIXED_NOT_FINISHED",
    "ELEMENT in DTD not started",
    "ELEMENT in DTD not finished",
    "XML declaration not started",
    "XML declaration not finished",
    "XML_ERR_CONDSEC_NOT_STARTED",
    "XML conditional section not closed",
    "Content error in the external subset",
    "DOCTYPE not finished",
    "Sequence ']]>' not allowed in content",
    "CDATA not finished",
    "Reserved XML Name",
    "Space required",
    "XML_ERR_SEPARATOR_REQUIRED",
    "NmToken expected in ATTLIST enumeration",
    "XML_ERR_NAME_REQUIRED",
    "MixedContentDecl : '#PCDATA' expected",
    "SYSTEM or PUBLIC, the URI is missing",
    "PUBLIC, the Public Identifier is missing",
    "< required",
    "> required",
    "</ required",
    "= required",
    "Mismatched tag",
    "Tag not finished",
    "standalone accepts only 'yes' or 'no'",
    "Invalid XML encoding name",
    "Comment must not contain '--' (double-hyphen)",
    "Invalid encoding",
    "external parsed entities cannot be standalone",
    "XML conditional section '[' expected",
    "Entity value required",
    "chunk is not well balanced",
    "extra content at the end of well balanced chunk",
    "XML_ERR_ENTITY_CHAR_ERROR",
    "PEReferences forbidden in internal subset",
    "Detected an entity reference loop",
    "XML_ERR_ENTITY_BOUNDARY",
    "Invalid URI",
    "Fragment not allowed",
    "XML_WAR_CATALOG_PI",
    "XML_ERR_NO_DTD",
    "conditional section INCLUDE or IGNORE keyword expected",
    "Version in XML Declaration missing",
    "XML_WAR_UNKNOWN_VERSION",
    "XML_WAR_LANG_VALUE",
    "XML_WAR_NS_URI",
    "XML_WAR_NS_URI_RELATIVE",
    "Missing encoding in text declaration",
];

/// `xml_parser_create(?string $encoding = null): XMLParser`.
///
/// ```php
/// $p = xml_parser_create(); var_dump(get_class($p), xml_parser_get_option($p, XML_OPTION_TARGET_ENCODING), xml_parser_get_option($p, XML_OPTION_CASE_FOLDING));
/// var_dump(xml_parser_get_option(xml_parser_create('iso-8859-1'), XML_OPTION_TARGET_ENCODING), xml_parser_get_option(xml_parser_create(''), XML_OPTION_TARGET_ENCODING));
/// try { xml_parser_create('koi8-r'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(9) "XMLParser"
/// string(5) "UTF-8"
/// bool(true)
/// string(10) "ISO-8859-1"
/// string(5) "UTF-8"
/// xml_parser_create(): Argument #1 ($encoding) is not a supported source encoding
/// ```
#[php_function]
fn xml_parser_create(
    req: &mut Request,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<Owner<Instance<XMLParser>>, Thrown> {
    create(req, encoding, None).map(Owner::into_instance)
}

/// `xml_parser_create_ns(?string $encoding = null, string $separator = ":"): XMLParser`.
///
/// ```php
/// $p = xml_parser_create_ns(null, '#');
/// xml_set_element_handler($p, function ($p, $n) { echo $n, "\n"; }, null);
/// xml_parse($p, '<a xmlns="urn:x"><b/></a>', true);
/// ```
/// ```expect
/// URN:X#A
/// URN:X#B
/// ```
#[php_function]
fn xml_parser_create_ns(
    req: &mut Request,
    #[php(default = null)] encoding: Option<PhpString>,
    #[php(default = ":")] separator: PhpString,
) -> Result<Owner<Instance<XMLParser>>, Thrown> {
    let separator = separator.as_bytes().to_vec();
    create(req, encoding, Some(separator)).map(Owner::into_instance)
}

/// `xml_set_object(XMLParser $parser, object $object): true`, deprecated
/// since 8.4: the object handlers named by a method name are looked up on.
///
/// ```php
/// class H { function s($p, $n) { echo "start $n\n"; } }
/// $p = xml_parser_create(); xml_set_object($p, new H); xml_set_element_handler($p, 's', '');
/// xml_parse($p, '<a/>', true);
/// ```
/// ```expect
///
/// Deprecated: Function xml_set_object() is deprecated since 8.4, provide a proper method callable to xml_set_*_handler() functions in %s on line 3
///
/// Deprecated: xml_set_element_handler(): Passing non-callable strings is deprecated since 8.4 in %s on line 3
/// start A
/// ```
#[php_function(deprecated(
    since = "8.4",
    message = "provide a proper method callable to xml_set_*_handler() functions"
))]
fn xml_set_object(
    req: &mut Request,
    parser: Instance<XMLParser>,
    object: php_ext::values::PhpObject,
) -> Result<True, Thrown> {
    set_object(req, parser.object(), object).map(|()| True)
}

/// `xml_set_element_handler(XMLParser $parser, callable|string|null $start_handler, callable|string|null $end_handler): true`.
///
/// ```php
/// $p = xml_parser_create();
/// var_dump(xml_set_element_handler($p, fn($p, $n, $a) => print("<$n " . json_encode($a) . ">\n"), fn($p, $n) => print("</$n>\n")));
/// xml_parse($p, '<a x="1"><b/></a>', true);
/// try { xml_set_element_handler($p, fn() => 1, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { xml_set_element_handler($p, 10, 'end'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// <A {"X":"1"}>
/// <B []>
/// </B>
/// </A>
/// xml_set_element_handler(): Argument #2 ($start_handler) must be of type callable|string|null
///
/// Deprecated: xml_set_element_handler(): Passing non-callable strings is deprecated since 8.4 in %s on line 6
/// xml_set_element_handler(): Argument #2 ($start_handler) an object must be set via xml_set_object() to be able to lookup method
/// ```
///
/// No handler is changed while the parser is parsing, so a handler cannot
/// free the callable being served; the one that threw stops the handlers
/// after it from being served:
///
/// ```php
/// $p = xml_parser_create();
/// xml_set_element_handler($p, function ($p, $n) {
///     try { xml_set_element_handler($p, null, null); } catch (Error $e) { echo $e->getMessage(), "\n"; }
///     try { xml_set_default_handler($p, null); } catch (Error $e) { echo $e->getMessage(), "\n"; }
///     echo "<$n>\n";
///     if ($n === 'B') throw new Exception('stop');
/// }, fn($p, $n) => print("</$n>\n"));
/// try { xml_parse($p, '<a><b/>t<c/></a>', true); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump(xml_set_element_handler($p, null, null));
/// ```
/// ```expect
/// Cannot change handlers while parsing
/// Cannot change handlers while parsing
/// <A>
/// Cannot change handlers while parsing
/// Cannot change handlers while parsing
/// <B>
/// stop
/// bool(true)
/// ```
///
/// A handler is kept as `zend_fcc_dup` keeps it, so a `Class::method`
/// string set with `$this` in scope keeps that object until the parser
/// gives the handler up:
///
/// ```php
/// class A { public $n = 'kept'; function __destruct(){ echo "dtor\n"; } function s($p, $name, $a){ echo $this->n, " $name\n"; }
///   function set($p){ xml_set_element_handler($p, 'A::s', null); } }
/// $p = xml_parser_create();
/// (new A)->set($p);
/// echo "set\n";
/// xml_parse($p, '<r/>', true);
/// unset($p);
/// echo "end\n";
/// ```
/// ```expect
/// set
/// kept R
/// dtor
/// end
/// ```
#[php_function]
fn xml_set_element_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    start_handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
    end_handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    let (start, end) = (start_handler.value(), end_handler.value());
    set_element_handler(req, parser.object(), &start, &end).map(|()| True)
}

/// `xml_set_character_data_handler(XMLParser $parser, callable|string|null $handler): true`.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_set_character_data_handler($p, fn($p, $d) => print("[$d]")));
/// xml_parse($p, '<a>x&amp;y<![CDATA[z]]></a>', true); echo "\n";
/// try { xml_set_character_data_handler($p, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// [x][&][y][z]
/// xml_set_character_data_handler(): Argument #2 ($handler) must be of type callable|string|null
/// ```
#[php_function]
fn xml_set_character_data_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    set_handler(req, parser.object(), Hook::CharacterData, &handler.value()).map(|()| True)
}

/// `xml_set_processing_instruction_handler(XMLParser $parser, callable|string|null $handler): true`.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_set_processing_instruction_handler($p, fn($p, $t, $d) => print("$t: $d\n")));
/// xml_parse($p, '<a><?php echo 1; ?></a>', true);
/// ```
/// ```expect
/// bool(true)
/// php: echo 1;
/// ```
#[php_function]
fn xml_set_processing_instruction_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    let value = handler.value();
    set_handler(req, parser.object(), Hook::ProcessingInstruction, &value).map(|()| True)
}

/// `xml_set_default_handler(XMLParser $parser, callable|string|null $handler): true`: the
/// text of what no other handler is set for.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_set_default_handler($p, fn($p, $d) => print("{" . $d . "}")));
/// xml_parse($p, '<a x="1"><!--c--><?t d?>t&amp;<b/></a>', true); echo "\n";
/// ```
/// ```expect
/// bool(true)
/// {<a x="1">}{<!--c-->}{<?t d?>}{t}{&amp;}{<b>}{</b>}{</a>}
/// ```
#[php_function]
fn xml_set_default_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    set_handler(req, parser.object(), Hook::Default, &handler.value()).map(|()| True)
}

/// `xml_set_unparsed_entity_decl_handler(XMLParser $parser, callable|string|null $handler): true`.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_set_unparsed_entity_decl_handler($p, function (...$a) { array_shift($a); var_dump($a); }));
/// xml_parse($p, '<!DOCTYPE r [<!NOTATION gif SYSTEM "g"><!ENTITY i SYSTEM "i.gif" NDATA gif>]><r/>', true);
/// ```
/// ```expect
/// bool(true)
/// array(5) {
///   [0]=>
///   string(1) "i"
///   [1]=>
///   bool(false)
///   [2]=>
///   string(5) "i.gif"
///   [3]=>
///   bool(false)
///   [4]=>
///   string(3) "gif"
/// }
/// ```
#[php_function]
fn xml_set_unparsed_entity_decl_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    let value = handler.value();
    set_handler(req, parser.object(), Hook::UnparsedEntityDecl, &value).map(|()| True)
}

/// `xml_set_notation_decl_handler(XMLParser $parser, callable|string|null $handler): true`.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_set_notation_decl_handler($p, function (...$a) { array_shift($a); var_dump($a); }));
/// xml_parse($p, '<!DOCTYPE r [<!NOTATION gif PUBLIC "-//G" "g.exe">]><r/>', true);
/// ```
/// ```expect
/// bool(true)
/// array(4) {
///   [0]=>
///   string(3) "gif"
///   [1]=>
///   bool(false)
///   [2]=>
///   string(5) "g.exe"
///   [3]=>
///   string(4) "-//G"
/// }
/// ```
#[php_function]
fn xml_set_notation_decl_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    let value = handler.value();
    set_handler(req, parser.object(), Hook::NotationDecl, &value).map(|()| True)
}

/// `xml_set_external_entity_ref_handler(XMLParser $parser, callable|string|null $handler): true`:
/// a falsy answer stops the parser with `XML_ERROR_EXTERNAL_ENTITY_HANDLING`.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_set_external_entity_ref_handler($p, function ($p, $n, $b, $s, $i) { var_dump($n, $b, $s, $i); return false; }));
/// var_dump(xml_parse($p, '<!DOCTYPE r [<!ENTITY e SYSTEM "e.xml">]><r>&e;</r>', true), xml_get_error_code($p));
/// ```
/// ```expect
/// bool(true)
/// string(1) "e"
/// string(0) ""
/// string(5) "e.xml"
/// bool(false)
/// int(0)
/// int(21)
/// ```
#[php_function]
fn xml_set_external_entity_ref_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    let value = handler.value();
    set_handler(req, parser.object(), Hook::ExternalEntityRef, &value).map(|()| True)
}

/// `xml_set_start_namespace_decl_handler(XMLParser $parser, callable|string|null $handler): true`.
///
/// ```php
/// $p = xml_parser_create_ns(); var_dump(xml_set_start_namespace_decl_handler($p, fn($p, $pre, $uri) => var_dump($pre, $uri)));
/// xml_parse($p, '<a xmlns="urn:d" xmlns:x="urn:x"/>', true);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(5) "urn:d"
/// string(1) "x"
/// string(5) "urn:x"
/// ```
#[php_function]
fn xml_set_start_namespace_decl_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    let value = handler.value();
    set_handler(req, parser.object(), Hook::StartNamespaceDecl, &value).map(|()| True)
}

/// `xml_set_end_namespace_decl_handler(XMLParser $parser, callable|string|null $handler): true`:
/// kept, and never served by libxml's parser.
///
/// ```php
/// $p = xml_parser_create_ns(); var_dump(xml_set_end_namespace_decl_handler($p, fn($p, $pre) => var_dump($pre)));
/// xml_parse($p, '<a xmlns:x="urn:x"/>', true);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn xml_set_end_namespace_decl_handler(
    req: &mut Request,
    parser: Instance<XMLParser>,
    handler: Raw<Option<Union<php_ext::abi::Callable, PhpString>>>,
) -> Result<True, Thrown> {
    let value = handler.value();
    set_handler(req, parser.object(), Hook::EndNamespaceDecl, &value).map(|()| True)
}

/// `xml_parse(XMLParser $parser, string $data, bool $is_final = false): int`.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_parse($p, '<a>'), xml_parse($p, '</a>', true), xml_parse(xml_parser_create(), '<a></b>', true));
/// $q = xml_parser_create(); xml_set_character_data_handler($q, function ($p) { xml_parse($p, 'x'); });
/// try { xml_parse($q, '<a>t</a>', true); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(1)
/// int(0)
/// Parser must not be called recursively
/// ```
#[php_function]
fn xml_parse(
    req: &mut Request,
    parser: Instance<XMLParser>,
    data: PhpString,
    #[php(default = false)] is_final: bool,
) -> Result<i64, Thrown> {
    let object = parser.object();
    if parser_mut(req, object).is_some_and(|p| p.engine.parsing()) {
        return Err(throw_error(
            req,
            "Error",
            "Parser must not be called recursively",
        ));
    }
    parse(req, object, data.as_bytes(), is_final)
}

/// `xml_parse_into_struct(XMLParser $parser, string $data, &$values, &$index = null): int|false`.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_parse_into_struct($p, "<a x='1'>t<b>u</b>v</a>", $v, $i));
/// echo json_encode($v), "\n", json_encode($i), "\n";
/// ```
/// ```expect
/// int(1)
/// [{"tag":"A","type":"open","level":1,"attributes":{"X":"1"},"value":"t"},{"tag":"B","type":"complete","level":2,"value":"u"},{"tag":"A","value":"v","type":"cdata","level":1},{"tag":"A","type":"close","level":1}]
/// {"A":[0,2,3],"B":[1]}
/// ```
#[php_function]
fn xml_parse_into_struct(
    req: &mut Request,
    parser: Instance<XMLParser>,
    data: PhpString,
    values: Ref<Untyped<Value>>,
    #[php(default = null)] index: Ref<Untyped<Value>>,
) -> Result<OrFalse<i64>, Thrown> {
    let object = parser.object();
    if parser_mut(req, object).is_some_and(|p| p.engine.parsing()) {
        docref(req, Level::Warning, "Parser must not be called recursively");
        return Ok(OrFalse::False);
    }
    if index.is_given() {
        let empty = PhpArray::new(req)?;
        index.set(req, Untyped(empty.into_value()))?;
    }
    let empty = PhpArray::new(req)?;
    values.set(req, Untyped(empty.into_value()))?;
    let (Some(data_ref), info_ref) = (values.reference(), index.reference()) else {
        return Ok(OrFalse::False);
    };
    let values_ref = Owner::take(req, data_ref);
    let index_ref = info_ref.map(|info| Owner::take(req, info));
    // An `$index` not given keeps the one an earlier call set.
    let old = match parser_mut(req, object) {
        Some(p) => {
            let mut old = p.into.take();
            let info = index_ref.or_else(|| old.as_mut().and_then(|o| o.info.take()));
            p.into = Some(Collected {
                data: values_ref,
                info,
            });
            old
        }
        None => Some(Collected {
            data: values_ref,
            info: index_ref,
        }),
    };
    if let Some(old) = old {
        old.release(req);
    }
    if let Some(p) = parser_mut(req, object) {
        p.level = 0;
        p.ltags = vec![None; 255];
        p.registered.set(Hook::StartElement);
        p.registered.set(Hook::CharacterData);
    }
    parse(req, object, data.as_bytes(), true).map(OrFalse::Value)
}

/// The parser's position, `XML_GetCurrent*`.
fn position(req: &mut Request, parser: Instance<XMLParser>) -> php_ext_libxml::ffi::Position {
    parser_mut(req, parser.object()).map_or_else(Default::default, |p| match &p.engine {
        Engine::Idle(push) => push.position(),
        Engine::Parsing(at) => *at,
        Engine::Unmade => php_ext_libxml::ffi::Position::default(),
    })
}

/// `xml_get_error_code(XMLParser $parser): int`.
///
/// ```php
/// $p = xml_parser_create(); xml_parse($p, '<a></b>', true); var_dump(xml_get_error_code($p), xml_get_error_code(xml_parser_create()));
/// ```
/// ```expect
/// int(76)
/// int(0)
/// ```
#[php_function]
fn xml_get_error_code(req: &mut Request, parser: Instance<XMLParser>) -> i64 {
    i64::from(position(req, parser).error_code)
}

/// `xml_error_string(int $error_code): ?string`.
///
/// ```php
/// var_dump(xml_error_string(0), xml_error_string(76), xml_error_string(101), xml_error_string(102), xml_error_string(-1));
/// ```
/// ```expect
/// string(8) "No error"
/// string(14) "Mismatched tag"
/// string(36) "Missing encoding in text declaration"
/// string(7) "Unknown"
/// string(7) "Unknown"
/// ```
#[php_function]
fn xml_error_string(
    req: &mut Request,
    error_code: i64,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    // `XML_ErrorString((int) code)`.
    #[allow(clippy::cast_possible_truncation)]
    let code = error_code as i32;
    let text = usize::try_from(code)
        .ok()
        .and_then(|at| ERRORS.get(at))
        .copied()
        .unwrap_or("Unknown");
    PhpString::new(req, text.as_bytes()).map(Some)
}

/// `xml_get_current_line_number(XMLParser $parser): int`.
///
/// ```php
/// $p = xml_parser_create(); xml_set_element_handler($p, fn($p) => print(xml_get_current_line_number($p) . ':' . xml_get_current_column_number($p) . ':' . xml_get_current_byte_index($p) . "\n"), null);
/// xml_parse($p, "<a>\n <b/></a>", true); var_dump(xml_get_current_line_number($p));
/// ```
/// ```expect
/// 1:3:2
/// 2:4:7
/// int(2)
/// ```
#[php_function]
fn xml_get_current_line_number(req: &mut Request, parser: Instance<XMLParser>) -> i64 {
    i64::from(position(req, parser).line)
}

/// `xml_get_current_column_number(XMLParser $parser): int`.
///
/// ```php
/// $p = xml_parser_create(); xml_parse($p, "<a>\n <b>", false); var_dump(xml_get_current_column_number($p));
/// ```
/// ```expect
/// int(5)
/// ```
#[php_function]
fn xml_get_current_column_number(req: &mut Request, parser: Instance<XMLParser>) -> i64 {
    i64::from(position(req, parser).column)
}

/// `xml_get_current_byte_index(XMLParser $parser): int`.
///
/// ```php
/// $p = xml_parser_create(); xml_parse($p, "<a>\n <b>", false); var_dump(xml_get_current_byte_index($p));
/// ```
/// ```expect
/// int(8)
/// ```
#[php_function]
fn xml_get_current_byte_index(req: &mut Request, parser: Instance<XMLParser>) -> i64 {
    position(req, parser).byte_index
}

/// `xml_parser_free(XMLParser $parser): bool`, deprecated since 8.5: no
/// effect, beyond refusing a parser that is parsing.
///
/// ```php
/// $p = xml_parser_create(); var_dump(xml_parser_free($p));
/// ```
/// ```expect
///
/// Deprecated: Function xml_parser_free() is deprecated since 8.5, as it has no effect since PHP 8.0 in %s on line 2
/// bool(true)
/// ```
#[php_function(deprecated(since = "8.5", message = "as it has no effect since PHP 8.0"))]
fn xml_parser_free(req: &mut Request, parser: Instance<XMLParser>) -> bool {
    if parser_mut(req, parser.object()).is_some_and(|p| p.engine.parsing()) {
        docref(
            req,
            Level::Warning,
            "Parser cannot be freed while it is parsing",
        );
        return false;
    }
    true
}

/// `xml_parser_set_option(XMLParser $parser, int $option, $value): bool`.
///
/// ```php
/// $p = xml_parser_create();
/// var_dump(xml_parser_set_option($p, XML_OPTION_CASE_FOLDING, 0), xml_parser_set_option($p, XML_OPTION_SKIP_TAGSTART, 2), xml_parser_set_option($p, XML_OPTION_TARGET_ENCODING, 'us-ascii'));
/// var_dump(xml_parser_set_option($p, XML_OPTION_SKIP_TAGSTART, -1), xml_parser_set_option($p, XML_OPTION_SKIP_WHITE, []));
/// try { xml_parser_set_option($p, 99, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { xml_parser_set_option($p, XML_OPTION_TARGET_ENCODING, 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
///
/// Warning: xml_parser_set_option(): Argument #3 ($value) must be between 0 and 2147483647 for option XML_OPTION_SKIP_TAGSTART in %s on line 4
///
/// Warning: xml_parser_set_option(): Argument #3 ($value) must be of type string|int|bool, array given in %s on line 4
/// bool(false)
/// bool(true)
/// xml_parser_set_option(): Argument #2 ($option) must be a XML_OPTION_* constant
/// xml_parser_set_option(): Argument #3 ($value) is not a supported target encoding
/// ```
#[php_function]
fn xml_parser_set_option(
    req: &mut Request,
    parser: Instance<XMLParser>,
    option: i64,
    value: Untyped<Value>,
) -> Result<bool, Thrown> {
    let value = value.0;
    if !matches!(
        value.as_ref(),
        ValueRef::Bool(_) | ValueRef::Int(_) | ValueRef::Str(_)
    ) {
        let given = type_name(req, &value);
        let message =
            format!("Argument #3 ($value) must be of type string|int|bool, {given} given");
        docref(req, Level::Warning, &message);
    }
    let object = parser.object();
    let Some(option) = Setting::of(option) else {
        return Err(arg_value_error(req, 2, "be a XML_OPTION_* constant"));
    };
    match option {
        Setting::CaseFolding => {
            let on = truthy(req, &value)?;
            if let Some(p) = parser_mut(req, object) {
                p.options.case_folding = on;
            }
        }
        Setting::SkipWhite => {
            let on = truthy(req, &value)?;
            if let Some(p) = parser_mut(req, object) {
                p.options.skip_white = on;
            }
        }
        Setting::ParseHuge => {
            if parser_mut(req, object).is_some_and(|p| p.engine.parsing()) {
                return Err(throw_error(
                    req,
                    "Error",
                    "Cannot change option XML_OPTION_PARSE_HUGE while parsing",
                ));
            }
            let on = truthy(req, &value)?;
            if let Some(p) = parser_mut(req, object) {
                p.options.parse_huge = on;
            }
        }
        Setting::SkipTagstart => {
            let n = to_long(req, &value)?;
            let Some(offset) = usize::try_from(n).ok().filter(|_| i32::try_from(n).is_ok()) else {
                let message = "Argument #3 ($value) must be between 0 and 2147483647 for option XML_OPTION_SKIP_TAGSTART";
                docref(req, Level::Warning, message);
                return Ok(false);
            };
            if let Some(p) = parser_mut(req, object) {
                p.options.skip_tagstart = offset;
            }
        }
        Setting::TargetEncoding => {
            let Some(target) = with_string(req, &value, Target::named)? else {
                return Err(argument_error(
                    req,
                    "ValueError",
                    3,
                    "is not a supported target encoding",
                ));
            };
            if let Some(p) = parser_mut(req, object) {
                p.options.target = target;
            }
        }
    }
    Ok(true)
}

/// `xml_parser_get_option(XMLParser $parser, int $option): string|int|bool`.
///
/// ```php
/// $p = xml_parser_create();
/// foreach ([XML_OPTION_CASE_FOLDING, XML_OPTION_TARGET_ENCODING, XML_OPTION_SKIP_TAGSTART, XML_OPTION_SKIP_WHITE, XML_OPTION_PARSE_HUGE] as $o) var_dump(xml_parser_get_option($p, $o));
/// try { xml_parser_get_option($p, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(5) "UTF-8"
/// int(0)
/// bool(false)
/// bool(false)
/// xml_parser_get_option(): Argument #2 ($option) must be a XML_OPTION_* constant
/// ```
#[php_function]
fn xml_parser_get_option(
    req: &mut Request,
    parser: Instance<XMLParser>,
    option: i64,
) -> Result<Union<Owner<PhpString>, Union<i64, bool>>, Thrown> {
    let Some(option) = Setting::of(option) else {
        return Err(arg_value_error(req, 2, "be a XML_OPTION_* constant"));
    };
    let Some(p) = parser_mut(req, parser.object()) else {
        return Ok(Union::B(Union::B(false)));
    };
    Ok(match option {
        Setting::CaseFolding => Union::B(Union::B(p.options.case_folding)),
        Setting::SkipTagstart => Union::B(Union::A(
            i64::try_from(p.options.skip_tagstart).unwrap_or(0),
        )),
        Setting::SkipWhite => Union::B(Union::B(p.options.skip_white)),
        Setting::ParseHuge => Union::B(Union::B(p.options.parse_huge)),
        Setting::TargetEncoding => {
            let name = p.options.target.name();
            Union::A(PhpString::new(req, name.as_bytes())?)
        }
    })
}
