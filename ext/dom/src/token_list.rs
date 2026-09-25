//! `Dom\TokenList` (`token_list.c`): an element's `class` attribute as an
//! ordered set of tokens, `Element::$classList`. The object shares its
//! element's node and document, as `dom_token_list_ctor` counts both, and
//! keeps the set parsed from the attribute value it last saw, parsing
//! again whenever the attribute has changed under it.

use php_ext::abi::{CloneObj, ObjectHandlers, PropertyRead, PropertyTest};
use php_ext::coerce::{numeric_string, to_long, truthy};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::Node;

use crate::object::{Code, Dom, Extra};
use crate::{
    Entry, arity, class_name, dom_error_message, dom_mut, long_arg, nullable_bool_arg, string, this,
};

/// `ascii_whitespace`, infra's.
pub(crate) const ASCII_WHITESPACE: &[u8] = b"\x09\x0A\x0C\x0D\x20";

/// Whether `b` is infra's ASCII whitespace.
fn is_ascii_whitespace(b: u8) -> bool {
    ASCII_WHITESPACE.contains(&b)
}

/// `dom_ordered_set_parser`: the tokens of `input`, first occurrence
/// first, lowercased where asked.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString("<!doctype html><p class=' a\tb\na  C\x0c'>x</p><p class='a b c'>y</p>");
/// $p = $d->body->firstChild;
/// var_dump(count($p->classList), json_encode($p->classList->value), $p->classList[3]);
/// var_dump($d->getElementsByClassName("b\ta")->length, $d->getElementsByClassName('c')->length, $d->getElementsByClassName('C')->length);
/// ```
/// ```expect
/// int(3)
/// string(15) "" a\tb\na  C\f""
/// NULL
/// int(2)
/// int(1)
/// int(1)
/// ```
pub(crate) fn ordered_set(input: &[u8], to_lowercase: bool) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for token in input
        .split(|b| is_ascii_whitespace(*b))
        .filter(|t| !t.is_empty())
    {
        let token = if to_lowercase {
            token.to_ascii_lowercase()
        } else {
            token.to_vec()
        };
        if !out.contains(&token) {
            out.push(token);
        }
    }
    out
}

/// `dom_ordered_set_all_contained`: whether every token of `set` is a
/// token of `value`.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString("<!doctype html><p class=' a\tb\na  C\x0c'>x</p><p class='a b c'>y</p>");
/// $p = $d->body->firstChild;
/// var_dump(count($p->classList), json_encode($p->classList->value), $p->classList[3]);
/// var_dump($d->getElementsByClassName("b\ta")->length, $d->getElementsByClassName('c')->length, $d->getElementsByClassName('C')->length);
/// ```
/// ```expect
/// int(3)
/// string(15) "" a\tb\na  C\f""
/// NULL
/// int(2)
/// int(1)
/// int(1)
/// ```
pub(crate) fn all_contained(set: &[Vec<u8>], value: &[u8], to_lowercase: bool) -> bool {
    let present = ordered_set(value, to_lowercase);
    set.iter().all(|t| present.contains(t))
}

/// `dom_strip_and_collapse_ascii_whitespace`.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString("<!doctype html><title>\t a \n b  </title>");
/// var_dump($d->title);
/// ```
/// ```expect
/// string(3) "a b"
/// ```
pub(crate) fn strip_and_collapse(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for token in input
        .split(|b| is_ascii_whitespace(*b))
        .filter(|t| !t.is_empty())
    {
        if !out.is_empty() {
            out.push(b' ');
        }
        out.extend_from_slice(token);
    }
    out
}

/// `dom_token_list_object`'s own fields: the token set and the value it
/// was parsed from, `cached_string`.
#[derive(Default)]
pub(crate) struct TokenSet {
    tokens: Vec<Vec<u8>>,
    cached: Option<Vec<u8>>,
}

/// The element's `class` value, `None` where it has no such attribute or
/// the attribute has no child.
fn class_value(element: Node) -> Option<Vec<u8>> {
    let attr = element.has_ns_prop(b"class", None)?;
    attr.children()?;
    Some(attr.attr_value())
}

/// A token list's element.
fn element(req: &Request, object: PhpObject) -> Option<Node> {
    crate::node_of(req, object)
}

/// `dom_token_list_ensure_set_up_to_date`: the set, parsed again where the
/// attribute moved on.
fn tokens(req: &mut Request, object: PhpObject) -> Vec<Vec<u8>> {
    let value = element(req, object).and_then(class_value);
    let Some(Dom {
        extra: Extra::TokenList(set),
        ..
    }) = dom_mut(req, object)
    else {
        return Vec::new();
    };
    // `xmlStrEqual` treats null and the empty string alike.
    let now = value.as_deref().unwrap_or_default();
    let then = set.cached.as_deref().unwrap_or_default();
    if now != then {
        set.tokens = value
            .as_deref()
            .map(|v| ordered_set(v, false))
            .unwrap_or_default();
        set.cached = value;
    }
    set.tokens.clone()
}

/// `dom_token_list_update`: the set written back as the attribute's
/// value, unless there is no attribute and no token.
fn update(req: &mut Request, object: PhpObject, tokens: Vec<Vec<u8>>) {
    let Some(elem) = element(req, object) else {
        return;
    };
    let has_attr = elem.has_ns_prop(b"class", None).is_some();
    if let Some(Dom {
        extra: Extra::TokenList(set),
        ..
    }) = dom_mut(req, object)
    {
        if !has_attr && tokens.is_empty() {
            set.tokens = tokens;
            return;
        }
        let value = tokens.join(&b' ');
        elem.set_ns_prop(None, b"class", &value);
        set.cached = Some(value);
        set.tokens = tokens;
    }
}

/// `dom_token_list_ctor`: a token list over `element_object`'s element.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $e = $d->createElement('p');
/// var_dump(array_keys(get_object_vars($e)));
/// $e->classList; $e->children;
/// var_dump(array_keys(get_object_vars($e)));
/// $c = clone $e;
/// var_dump(array_keys(get_object_vars($c)), $c->classList === $e->classList);
/// $f = $d->createDocumentFragment(); $f->children;
/// var_dump(array_keys(get_object_vars(clone $f)));
/// ```
/// ```expect
/// array(0) {
/// }
/// array(2) {
///   [0]=>
///   string(8) "children"
///   [1]=>
///   string(9) "classList"
/// }
/// array(0) {
/// }
/// bool(false)
/// array(0) {
/// }
/// ```
pub(crate) fn create(req: &mut Request, element_object: PhpObject) -> Result<Value, Thrown> {
    let class = crate::class(req, "Dom\\TokenList")?;
    let object = php_ext::call::new_object(req, class)?;
    let node = crate::node_of(req, element_object);
    let shared = crate::shared_doc(req, element_object);
    if let Some(d) = dom_mut(req, object) {
        d.obj.set_object(object);
        if let Some(n) = node {
            d.obj.attach(n, shared);
        }
        let cached = node.and_then(class_value);
        let tokens = cached
            .as_deref()
            .map(|v| ordered_set(v, false))
            .unwrap_or_default();
        d.extra = Extra::TokenList(Box::new(TokenSet { tokens, cached }));
    }
    Ok(object.as_value())
}

/// `dom_validate_token`.
fn validate(req: &mut Request, token: &[u8]) -> Result<(), Thrown> {
    if token.is_empty() {
        return dom_error_message(
            req,
            Code::Syntax,
            "The empty string is not a valid token",
            true,
        );
    }
    if token.iter().any(|b| is_ascii_whitespace(*b)) {
        return dom_error_message(
            req,
            Code::InvalidCharacter,
            "The token must not contain any ASCII whitespace",
            true,
        );
    }
    Ok(())
}

/// `Z_PARAM_PATH_STR`: the bytes, php's `ValueError` where they hold a NUL.
fn path_str(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    crate::path_arg(req, position)
}

/// `dom_validate_tokens_varargs` over the call's arguments, as passed:
/// `Z_PARAM_VARIADIC('*')` coerces nothing, so a token that is no string
/// is php's `TypeError`, which names no parameter.
fn varargs(req: &mut Request, method: &str) -> Result<Vec<Vec<u8>>, Thrown> {
    let mut out = Vec::new();
    for i in 0..req.argument_count() {
        let position = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let arg = req
            .argument(position)
            .map(|v| v.deref())
            .unwrap_or_default();
        let Some(bytes) = arg.as_str_bytes().map(<[u8]>::to_vec) else {
            let given = php_ext::coerce::value_name(req, &arg);
            return Err(throw(
                req,
                "TypeError",
                &format!(
                    "Dom\\TokenList::{method}(): Argument #{position} must be of type string, {given} given"
                ),
            ));
        };
        if bytes.contains(&0) {
            return Err(php_ext::raising::arg_value_error(
                req,
                position,
                "not contain any null bytes",
            ));
        }
        validate(req, &bytes)?;
        out.push(bytes);
    }
    Ok(out)
}

/// `dom_token_list_item_read`: the token at `index`, `null` past the end.
fn item_value(req: &mut Request, object: PhpObject, index: i64) -> Result<Value, Thrown> {
    let tokens = tokens(req, object);
    match usize::try_from(index).ok().and_then(|i| tokens.get(i)) {
        Some(token) => string(req, token),
        None => Ok(Value::null()),
    }
}

/// `Dom\TokenList::item(int $index): ?string`.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $e = $d->createElement('p');
/// $e->setAttribute('class', ' a  b a c ');
/// $l = $e->classList;
/// var_dump($l->length, $l->item(1), $l->item(9), $l[0], isset($l[2]), $l->value, count($l));
/// foreach ($l as $k => $t) echo $k, '=', $t, ' ';
/// echo "\n";
/// ```
/// ```expect
/// int(3)
/// string(1) "b"
/// NULL
/// string(1) "a"
/// bool(true)
/// string(10) " a  b a c "
/// int(3)
/// 0=a 1=b 2=c
/// ```
fn item(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let index = long_arg(req, 1, 0)?;
    let this = this(req)?;
    item_value(req, this, index)
}

/// `Dom\TokenList::contains(string $token): bool`.
fn contains(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let token = path_str(req, 1)?;
    let this = this(req)?;
    Ok(Value::bool(tokens(req, this).contains(&token)))
}

/// `Dom\TokenList::add(string ...$tokens): void`.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $e = $d->createElement('p');
/// $l = $e->classList;
/// $l->add('a', 'b', 'a');
/// $l->remove('a', 'z');
/// var_dump($l->toggle('c'), $l->toggle('b'), $l->toggle('d', false), $l->replace('c', 'e'), $l->contains('e'));
/// echo $e->getAttribute('class'), "\n";
/// try { $l->add(''); } catch (DOMException $x) { echo $x->getMessage(), ' ', $x->code, "\n"; }
/// try { $l->add('a b'); } catch (DOMException $x) { echo $x->getMessage(), ' ', $x->code, "\n"; }
/// try { $l->supports('a'); } catch (TypeError $x) { echo $x->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// e
/// The empty string is not a valid token 12
/// The token must not contain any ASCII whitespace 5
/// Attribute "class" does not define any supported tokens
/// ```
fn add(req: &mut Request) -> Result<Value, Thrown> {
    let added = varargs(req, "add")?;
    let this = this(req)?;
    let mut set = tokens(req, this);
    for token in added {
        if !set.contains(&token) {
            set.push(token);
        }
    }
    update(req, this, set);
    Ok(Value::null())
}

/// `Dom\TokenList::remove(string ...$tokens): void`.
fn remove(req: &mut Request) -> Result<Value, Thrown> {
    let removed = varargs(req, "remove")?;
    let this = this(req)?;
    let mut set = tokens(req, this);
    set.retain(|t| !removed.contains(t));
    update(req, this, set);
    Ok(Value::null())
}

/// `Dom\TokenList::toggle(string $token, ?bool $force = null): bool`.
fn toggle(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let token = path_str(req, 1)?;
    let force = nullable_bool_arg(req, 2)?;
    validate(req, &token)?;
    let this = this(req)?;
    let mut set = tokens(req, this);
    if let Some(at) = set.iter().position(|t| *t == token) {
        if force != Some(true) {
            set.remove(at);
            update(req, this, set);
            return Ok(Value::bool(false));
        }
        return Ok(Value::bool(true));
    }
    if force != Some(false) {
        set.push(token);
        update(req, this, set);
        return Ok(Value::bool(true));
    }
    Ok(Value::bool(false))
}

/// `Dom\TokenList::replace(string $token, string $newToken): bool`: the
/// token renamed in place, or dropped where the new one is there already.
fn replace(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let token = path_str(req, 1)?;
    let fresh = path_str(req, 2)?;
    validate(req, &token)?;
    validate(req, &fresh)?;
    let this = this(req)?;
    let mut set = tokens(req, this);
    let Some(at) = set.iter().position(|t| *t == token) else {
        return Ok(Value::bool(false));
    };
    if set.iter().enumerate().any(|(i, t)| i != at && *t == fresh) {
        set.remove(at);
    } else if let Some(slot) = set.get_mut(at) {
        *slot = fresh;
    }
    update(req, this, set);
    Ok(Value::bool(true))
}

/// `Dom\TokenList::supports(string $token): bool`: `classList` defines no
/// supported tokens, so php's `TypeError`.
fn supports(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    path_str(req, 1)?;
    Err(throw(
        req,
        "TypeError",
        "Attribute \"class\" does not define any supported tokens",
    ))
}

/// `Dom\TokenList::count(): int`.
fn count(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let this = this(req)?;
    let n = tokens(req, this).len();
    Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
}

/// `Dom\TokenList::getIterator(): Iterator`.
fn get_iterator(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let this = this(req)?;
    Ok(php_ext::call::internal_iterator(req, &this)?.as_value())
}

/// The methods, `Class::method` to entry.
pub(crate) const METHODS: &[(&str, Entry)] = &[
    ("Dom\\TokenList::item", item),
    ("Dom\\TokenList::contains", contains),
    ("Dom\\TokenList::add", add),
    ("Dom\\TokenList::remove", remove),
    ("Dom\\TokenList::toggle", toggle),
    ("Dom\\TokenList::replace", replace),
    ("Dom\\TokenList::supports", supports),
    ("Dom\\TokenList::count", count),
    ("Dom\\TokenList::getIterator", get_iterator),
];

/// `dom_token_list_length_read`.
#[allow(clippy::unnecessary_wraps)] // the reader shape every row of a property table has
fn length_read(req: &mut Request, object: PhpObject) -> crate::Read {
    let n = tokens(req, object).len();
    Ok(Some(Value::int(i64::try_from(n).unwrap_or(i64::MAX))))
}

/// `dom_token_list_value_read`: the attribute's value, not the set's.
fn value_read(req: &mut Request, object: PhpObject) -> crate::Read {
    let value = element(req, object).and_then(class_value);
    Ok(Some(string(req, &value.unwrap_or_default())?))
}

/// `dom_token_list_value_write`: the attribute set; the set follows
/// lazily.
fn value_write(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let bytes = value.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default();
    if bytes.contains(&0) {
        return Err(throw(
            req,
            "ValueError",
            "Value must not contain any null bytes",
        ));
    }
    if let Some(elem) = element(req, object) {
        elem.set_ns_prop(None, b"class", &bytes);
    }
    Ok(())
}

/// `dom_token_list_prop_handlers`.
pub(crate) const PROPS: &[crate::props::Prop] = &[
    crate::props::Prop {
        name: b"length",
        read: length_read,
        write: None,
    },
    crate::props::Prop {
        name: b"value",
        read: value_read,
        write: Some(value_write),
    },
];

/// `dom_token_list_offset_convert_to_long`: `None` for an offset php
/// refuses.
fn offset(req: &mut Request, value: &Value) -> Result<Option<i64>, Thrown> {
    let v = value.deref();
    Ok(match v.as_ref() {
        ValueRef::Str(s) => match numeric_string(s.as_bytes()) {
            Some(n) if n.as_int().is_some() => n.as_int(),
            _ => None,
        },
        ValueRef::Float(f) => Some(crate::map::float_offset(req, f)?),
        ValueRef::Int(_) | ValueRef::Bool(_) => Some(to_long(req, &v)?),
        _ => None,
    })
}

/// `zend_illegal_container_offset`.
fn illegal_offset(req: &mut Request, object: PhpObject, value: &Value, isset: bool) -> Thrown {
    let given = php_ext::coerce::type_name(req, value);
    let message = if isset {
        format!("Cannot access offset of type {given} in isset or empty")
    } else {
        let class = class_name(req, object.class());
        format!("Cannot access offset of type {given} on {class}")
    };
    throw(req, "TypeError", &message)
}

/// `dom_token_list_object_handlers`.
pub(crate) struct TokenListHandlers;

pub(crate) static HANDLERS: TokenListHandlers = TokenListHandlers;

impl ObjectHandlers for TokenListHandlers {
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        crate::read_property(req, object, name, read)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        crate::write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Option<u32> {
        crate::property_slot(req, object, name)
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        crate::has_property(req, object, name, test)
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        crate::unset_property(req, object, name)
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        crate::debug_info(req, object)
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset_value: Option<&Value>,
    ) -> Result<Value, Thrown> {
        let Some(offset_value) = offset_value else {
            return Err(throw(req, "Error", "Cannot append to Dom\\TokenList"));
        };
        match offset(req, offset_value)? {
            Some(index) => item_value(req, object, index),
            None => Err(illegal_offset(req, object, offset_value, false)),
        }
    }

    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset_value: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        let Some(index) = offset(req, offset_value)? else {
            return Err(illegal_offset(req, object, offset_value, true));
        };
        if not_empty {
            let value = item_value(req, object, index)?;
            let answer = truthy(req, &value);
            req.release(value);
            return Ok(answer);
        }
        let n = tokens(req, object).len();
        Ok(usize::try_from(index).is_ok_and(|i| i < n))
    }

    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<(Value, Value)>, Thrown> {
        let tokens = tokens(req, object);
        let Some(token) = tokens.get(index) else {
            return Ok(None);
        };
        let key = Value::int(i64::try_from(index).unwrap_or(i64::MAX));
        Ok(Some((key, string(req, token)?)))
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some(d) = dom_mut(req, object) {
            d.extra = Extra::None;
        }
    }
}
