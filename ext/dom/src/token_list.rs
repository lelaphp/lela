//! `Dom\TokenList` (`token_list.c`): an element's `class` attribute as an
//! ordered set of tokens, `Element::$classList`. The object shares its
//! element's node and document, as `dom_token_list_ctor` counts both, and
//! keeps the set parsed from the attribute value it last saw, parsing
//! again whenever the attribute has changed under it.

use php_ext::abi::{
    ClassIterator, DimensionRead, Instance, ObjectHandlers, Path, PropertyPlace, PropertyRead,
    PropertyTest, Raw, Refusals, Std, This, Variadic,
};
use php_ext::attr::php_class;
use php_ext::coerce::{to_long, truthy};
use php_ext::raising::{Thrown, throw_error};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Key, Owner, PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::Node;

use crate::object::{Code, Dom, Extra};
use crate::{dom_error_message, dom_mut, string};

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

/// `dom_token_list_object`'s token set, php's `HashTable` of tokens: its
/// buckets in the order the tokens came, a removed token's left empty,
/// so an iterator's `HashPosition` outlives the removal, until the table
/// grows (`zend_hash_do_resize` compacts one with holes) or is parsed
/// again; the value it was parsed from, `cached_string`; and
/// `cache_tag`, moved on by every update and every parse.
#[derive(Default)]
pub(crate) struct TokenSet {
    buckets: Vec<Option<Vec<u8>>>,
    capacity: usize,
    cached: Option<Vec<u8>>,
    tag: u64,
    held: Held,
}

/// What php's `token_set` and `cached_string` hold on the request's
/// heap for a [`TokenSet`], kept to the set's shape by
/// [`TokenSet::settle`] (design §5.13): the table's data, `HT_SIZE` of a
/// mixed table of its capacity, forty bytes a bucket; each token the
/// parse made, as `zend_string_init` allocates it, where a token `add`,
/// `toggle` or `replace` was handed is the caller's string, one more
/// owner and no allocation; and `cached_string`'s `estrdup`.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString('<p class="a b">x</p><p class="c">y</p>', LIBXML_NOERROR);
/// $w = $d->body->firstChild->classList; $e = $d->body->lastChild;
/// $m = memory_get_usage(); $t = $e->classList; echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $t->add('dd', 'eee'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $t->remove('c'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($t); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 528
/// 0
/// -32
/// 0
/// ```
#[derive(Default)]
struct Held {
    data: Option<Emalloc>,
    /// A bucket's string where the parse made it, by bucket.
    tokens: Vec<Option<Emalloc>>,
    cached: Option<Emalloc>,
    /// Strings a change gave up, to go back at the next settle.
    dropped: Vec<Emalloc>,
    /// Whether the buckets were parsed since the last settle.
    parsed: bool,
}

/// `_ZSTR_STRUCT_SIZE(len)`: a `zend_string`'s header, the bytes and the
/// NUL.
fn string_size(len: usize) -> usize {
    24 + len + 1
}

/// `slot` fitted to `want` bytes, none for `None`, what it held given
/// back or resized; the `Err` is the memory limit's fatal.
fn fit(req: &mut Request, slot: &mut Option<Emalloc>, want: Option<usize>) -> Result<(), Thrown> {
    *slot = match (slot.take(), want) {
        (None, None) => None,
        (Some(held), None) => {
            held.free(req);
            None
        }
        (None, Some(len)) => Some(Emalloc::new(req, len)?),
        (Some(held), Some(len)) if held.len() == len => Some(held),
        (Some(held), Some(len)) => Some(held.resize(req, len)?),
    };
    Ok(())
}

impl TokenSet {
    /// The set's allocations brought to its shape: the table's data
    /// where it has had a bucket, a string per token and the cached
    /// value's copy.
    fn settle(&mut self, req: &mut Request) -> Result<(), Thrown> {
        const BUCKET: usize = 40;
        for token in std::mem::take(&mut self.held.dropped) {
            token.free(req);
        }
        if std::mem::take(&mut self.held.parsed) {
            for token in std::mem::take(&mut self.held.tokens).into_iter().flatten() {
                token.free(req);
            }
            for bucket in &self.buckets {
                let token = match bucket {
                    Some(token) => Some(Emalloc::new(req, string_size(token.len()))?),
                    None => None,
                };
                self.held.tokens.push(token);
            }
        }
        self.held.tokens.resize_with(self.buckets.len(), || None);
        for (at, bucket) in self.buckets.iter().enumerate() {
            if bucket.is_none() {
                fit(req, &mut self.held.tokens[at], None)?;
            }
        }
        let data =
            (self.capacity != 0 || !self.buckets.is_empty()).then(|| self.capacity.max(8) * BUCKET);
        fit(req, &mut self.held.data, data)?;
        let cached = self.cached.as_ref().map(|value| value.len() + 1);
        fit(req, &mut self.held.cached, cached)
    }

    /// The bucket at `at` given `token`, `zend_hash_set_bucket_key` or
    /// the bucket emptied: the string it held is given up.
    fn rename(&mut self, at: usize, token: Option<Vec<u8>>) {
        self.buckets[at] = token;
        if let Some(held) = self.held.tokens.get_mut(at).and_then(Option::take) {
            self.held.dropped.push(held);
        }
    }

    /// Everything the set holds on the request's heap given back,
    /// `zend_hash_destroy` and `efree(intern->cached_string)`.
    fn release(&mut self, req: &mut Request) {
        let held = std::mem::take(&mut self.held);
        for block in held
            .tokens
            .into_iter()
            .flatten()
            .chain(held.dropped)
            .chain(held.data)
            .chain(held.cached)
        {
            block.free(req);
        }
    }

    /// The tokens, in order.
    fn live(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.buckets.iter().flatten()
    }

    /// The bucket that holds `token`.
    fn find(&self, token: &[u8]) -> Option<usize> {
        self.buckets
            .iter()
            .position(|b| b.as_deref() == Some(token))
    }

    /// `zend_hash_add_empty_element` of a token the set has not: a new
    /// bucket past the last, the table compacted first where it is full
    /// of holes and doubled where it is full of tokens.
    fn add(&mut self, token: Vec<u8>) {
        if self.buckets.len() >= self.capacity.max(8) {
            let live = self.live().count();
            if self.buckets.len() > live + (live >> 5) {
                self.held.tokens.resize_with(self.buckets.len(), || None);
                let held = std::mem::take(&mut self.held.tokens);
                for (bucket, token) in std::mem::take(&mut self.buckets).into_iter().zip(held) {
                    match bucket {
                        Some(bucket) => {
                            self.buckets.push(Some(bucket));
                            self.held.tokens.push(token);
                        }
                        None => self.held.dropped.extend(token),
                    }
                }
            } else {
                self.capacity = self.capacity.max(8) * 2;
            }
        }
        self.buckets.push(Some(token));
    }

    /// `_zend_hash_get_valid_pos`: the first bucket at or past `pos` that
    /// holds a token, or the end.
    fn valid_from(&self, pos: usize) -> usize {
        (pos..self.buckets.len())
            .find(|&at| self.buckets[at].is_some())
            .unwrap_or(self.buckets.len())
    }

    /// The set parsed from `value`, in a fresh table.
    fn parse(&mut self, value: Option<Vec<u8>>) {
        self.buckets.clear();
        self.capacity = 0;
        for token in value
            .as_deref()
            .map(|v| ordered_set(v, false))
            .unwrap_or_default()
        {
            self.add(token);
        }
        self.cached = value;
        self.tag += 1;
        self.held.parsed = true;
    }
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

/// `dom_token_list_ensure_set_up_to_date`, then `f` over the set: parsed
/// again where the attribute moved on.
fn with_set<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut TokenSet) -> R,
) -> Option<R> {
    let value = element(req, object).and_then(class_value);
    let mut set = taken(req, object)?;
    // `xmlStrEqual` treats null and the empty string alike.
    if value.as_deref().unwrap_or_default() != set.cached.as_deref().unwrap_or_default() {
        set.parse(value);
    }
    let out = f(&mut set);
    put(req, object, set);
    Some(out)
}

/// The set taken out of its object for a change the request's heap sees.
fn taken(req: &mut Request, object: PhpObject) -> Option<TokenSet> {
    match dom_mut(req, object) {
        Some(Dom {
            extra: Extra::TokenList(set),
            ..
        }) => Some(std::mem::take(&mut **set)),
        _ => None,
    }
}

/// The set put back once its allocations are brought to its shape; the
/// memory limit's fatal is recorded on the request, which it ends.
fn put(req: &mut Request, object: PhpObject, mut set: TokenSet) {
    set.settle(req).ok();
    if let Some(Dom {
        extra: Extra::TokenList(slot),
        ..
    }) = dom_mut(req, object)
    {
        **slot = set;
    } else {
        set.release(req);
    }
}

/// The tokens, the set brought up to date.
fn tokens(req: &mut Request, object: PhpObject) -> Vec<Vec<u8>> {
    with_set(req, object, |set| set.live().cloned().collect()).unwrap_or_default()
}

/// `dom_token_list_update`: the set written back as the attribute's
/// value, unless there is no attribute and no token; the cache tag moved
/// on either way.
fn update(req: &mut Request, object: PhpObject) {
    let Some(elem) = element(req, object) else {
        return;
    };
    let has_attr = elem.has_ns_prop(b"class", None).is_some();
    let Some(mut set) = taken(req, object) else {
        return;
    };
    set.tag += 1;
    if has_attr || set.live().next().is_some() {
        let value = set.live().cloned().collect::<Vec<_>>().join(&b' ');
        elem.set_ns_prop(None, b"class", &value);
        set.cached = Some(value);
    }
    put(req, object, set);
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
pub(crate) fn create(req: &mut Request, element_object: PhpObject) -> Result<Owner<Value>, Thrown> {
    let class = crate::class(req, "Dom\\TokenList")?;
    let object = php_ext::call::new_object(req, class)?;
    let node = crate::node_of(req, element_object);
    let shared = crate::shared_doc(req, element_object);
    if let Some(mut obj) = crate::libxml_half(req, *object)
        && let Some(n) = node
    {
        obj.attach(req, n, shared);
    }
    if let Some(d) = dom_mut(req, *object) {
        d.extra = Extra::TokenList(Box::default());
    }
    let mut set = TokenSet::default();
    set.parse(node.and_then(class_value));
    set.tag = 0;
    put(req, *object, set);
    Ok(object.into_value())
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

/// `dom_token_list_item_read`: the token at `index`, `null` past the end.
fn item_value(req: &mut Request, object: PhpObject, index: i64) -> Result<Owner<Value>, Thrown> {
    let tokens = tokens(req, object);
    match usize::try_from(index).ok().and_then(|i| tokens.get(i)) {
        Some(token) => string(req, token),
        None => Ok(Owner::null()),
    }
}

/// `dom_token_list_length_read`.
#[allow(clippy::unnecessary_wraps)] // the reader shape every row of a property table has
fn length_read(req: &mut Request, object: PhpObject) -> crate::Read {
    let n = tokens(req, object).len();
    Ok(Some(Owner::int(i64::try_from(n).unwrap_or(i64::MAX))))
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
        return Err(throw_error(
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

/// `dom_token_list_it`: a `HashPosition` into the token set, the index
/// the walk's caller counts, php's `iter->index`, moved with each step and
/// counted again from the set where it changed since the walk's cache tag,
/// which is never taken again, so a set changed under a walk reads its keys
/// by position from then on.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r class="A B C D E F"/>');
/// $l = $d->documentElement->classList;
/// foreach ($l as $i => $t) { echo "$i$t;"; $l->remove('A', 'D'); }
/// ```
/// ```expect
/// 0A;0B;1C;2E;3F;
/// ```
struct TokenIter {
    pos: usize,
    index: i64,
    tag: u64,
}

impl ClassIterator for TokenIter {
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        self.pos = with_set(req, object, |set| set.valid_from(0)).unwrap_or_default();
        self.index = 0;
        Ok(())
    }

    fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
        let pos = self.pos;
        let (at, used) = with_set(req, object, |set| (set.valid_from(pos), set.buckets.len()))
            .unwrap_or_default();
        self.pos = at;
        Ok(at < used)
    }

    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        let pos = self.pos;
        let token = with_set(req, object, |set| {
            let at = set.valid_from(pos);
            set.buckets.get(at).cloned().flatten()
        })
        .flatten();
        match token {
            Some(token) => string(req, &token),
            None => Ok(Owner::null()),
        }
    }

    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        let (pos, tag) = (self.pos, self.tag);
        let counted = with_set(req, object, |set| {
            (set.tag != tag).then(|| {
                set.buckets[..pos.min(set.buckets.len())]
                    .iter()
                    .flatten()
                    .count()
            })
        })
        .flatten();
        if let Some(counted) = counted {
            self.index = i64::try_from(counted).unwrap_or(i64::MAX);
        }
        Ok(Owner::int(self.index))
    }

    /// `dom_token_list_it_move_forward`: where a change under the walk
    /// moved it on already, to the token that now stands there and no
    /// further; one step otherwise.
    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        let pos = self.pos;
        self.pos = with_set(req, object, |set| {
            let validated = set.valid_from(pos);
            if validated == pos {
                set.valid_from(pos + 1)
            } else {
                validated
            }
        })
        .unwrap_or(pos);
        self.index += 1;
        Ok(())
    }
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
        ValueRef::Str(s) => match Key::lookup(s) {
            Key::Int(n) => Some(n),
            Key::Str(_) => None,
        },
        ValueRef::Float(f) => Some(crate::map::float_offset(req, f)?),
        ValueRef::Int(_) | ValueRef::Bool(_) => Some(to_long(req, &v)?),
        ValueRef::Resource(r) => {
            let id = r.id();
            php_ext::raising::warning(
                req,
                &format!("Resource ID#{id} used as offset, casting to integer ({id})"),
            );
            Some(id)
        }
        _ => None,
    })
}

/// `zend_illegal_container_offset(object->ce->name, offset, read)`.
fn illegal_offset(
    req: &mut Request,
    object: PhpObject,
    value: &Value,
    read: DimensionRead,
) -> Thrown {
    let class = req.symbols().class_name(object.class());
    php_ext::raising::illegal_container_offset(req, &class, value, read)
}

/// `dom_token_list_object_handlers`.
pub(crate) struct TokenListHandlers;

pub(crate) static HANDLERS: TokenListHandlers = TokenListHandlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for TokenListHandlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `dom_token_list_free_obj`: `zend_object_std_dtor`, then the set's
    /// table and strings and the cached value given back.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        Std.free_obj(req, object);
        if let Some(mut set) = taken(req, object) {
            set.release(req);
        }
        if let Some(d) = dom_mut(req, object) {
            d.extra = Extra::None;
        }
        crate::release_libxml(req, object);
    }

    /// `dom_token_list_it`: the iterator, its position and the cache tag.
    fn iterator_size(&self) -> usize {
        104
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        crate::read_property(req, object, name, read)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        crate::write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        crate::property_place(req, object, name)
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
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        crate::debug_info(req, object)
    }

    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset_value: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(offset_value) = offset_value else {
            return Err(throw_error(req, "Error", "Cannot append to Dom\\TokenList"));
        };
        match offset(req, offset_value)? {
            Some(index) => item_value(req, object, index),
            None => Err(illegal_offset(req, object, offset_value, read)),
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
            return Err(illegal_offset(
                req,
                object,
                offset_value,
                DimensionRead::Silent,
            ));
        };
        if not_empty {
            let value = item_value(req, object, index)?;
            let answer = truthy(req, &value);
            value.give_up(req);
            let answer = answer?;
            return Ok(answer);
        }
        let n = tokens(req, object).len();
        Ok(usize::try_from(index).is_ok_and(|i| i < n))
    }

    /// `dom_token_list_get_iterator`: the walk made at the set's first
    /// token, its cache tag taken, and refused by reference.
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        if by_ref {
            let message = "An iterator cannot be used with foreach by reference";
            return Err(throw_error(req, "Error", message));
        }
        let (pos, tag) =
            with_set(req, object, |set| (set.valid_from(0), set.tag)).unwrap_or_default();
        Ok(Some(Box::new(TokenIter { pos, index: 0, tag })))
    }
}

/// `dom_validate_tokens_varargs` over the tokens as passed:
/// `Z_PARAM_VARIADIC('*')` coerces nothing, so a token that is no string
/// is php's `TypeError`, which names no parameter.
fn varargs(req: &mut Request, tokens: &[Raw<PhpString>]) -> Result<Vec<Vec<u8>>, Thrown> {
    let mut out = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let position = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let arg = token.value();
        let Some(bytes) = arg.as_str_bytes().map(<[u8]>::to_vec) else {
            let given = php_ext::coerce::value_name(req, &arg);
            return Err(php_ext::raising::arg_type_error(
                req,
                position,
                &format!("be of type string, {given} given"),
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

/// `final class Dom\TokenList implements IteratorAggregate, Countable`,
/// `@not-serializable` and `@strict-properties`: an element's `class`
/// attribute as an ordered set, its `length` and `value` `@virtual`,
/// served by [`HANDLERS`]; `Dom\Node`'s refusing constructor.
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
#[php_class(
    namespace(Dom),
    final,
    implements(crate::IteratorAggregate, crate::Countable),
    not_serializable,
    dynamic_properties = forbidden,
    size = 152, payload = Dom,
    handlers = HANDLERS
)]
pub(crate) struct TokenList;

#[php_class]
impl TokenList {
    #[php(property, virtual)]
    const length: i64;
    #[php(property, virtual)]
    const value: PhpString;

    #[php(private, implementation = crate::modern::Node::__construct)]
    fn __construct(this: This<Self>);

    /// `Dom\TokenList::item(int $index): ?string`.
    fn item(
        req: &mut Request,
        this: This<Self>,
        index: i64,
    ) -> Result<Owner<Raw<Option<PhpString>>>, Thrown> {
        let token = item_value(req, this.object(), index)?;
        crate::returned(token)
    }

    /// `Dom\TokenList::contains(string $token): bool`.
    fn contains(req: &mut Request, this: This<Self>, token: Path) -> bool {
        let token = token.0.as_bytes();
        tokens(req, this.object())
            .iter()
            .any(|t| t.as_slice() == token)
    }

    /// `Dom\TokenList::add(string ...$tokens): void`.
    fn add(
        req: &mut Request,
        this: This<Self>,
        tokens: Variadic<Raw<PhpString>>,
    ) -> Result<(), Thrown> {
        let added = varargs(req, &tokens.0)?;
        let this = this.object();
        with_set(req, this, |set| {
            for token in added {
                if set.find(&token).is_none() {
                    set.add(token);
                }
            }
        });
        update(req, this);
        Ok(())
    }

    /// `Dom\TokenList::remove(string ...$tokens): void`.
    fn remove(
        req: &mut Request,
        this: This<Self>,
        tokens: Variadic<Raw<PhpString>>,
    ) -> Result<(), Thrown> {
        let removed = varargs(req, &tokens.0)?;
        let this = this.object();
        with_set(req, this, |set| {
            for bucket in &mut set.buckets {
                if bucket.as_ref().is_some_and(|t| removed.contains(t)) {
                    *bucket = None;
                }
            }
        });
        update(req, this);
        Ok(())
    }

    /// `Dom\TokenList::toggle(string $token, ?bool $force = null): bool`.
    fn toggle(
        req: &mut Request,
        this: This<Self>,
        token: Path,
        #[php(default = null)] force: Option<bool>,
    ) -> Result<bool, Thrown> {
        let token = token.0.as_bytes().to_vec();
        validate(req, &token)?;
        let this = this.object();
        let at = with_set(req, this, |set| set.find(&token)).flatten();
        let answer = match at {
            Some(_) if force == Some(true) => return Ok(true),
            None if force == Some(false) => return Ok(false),
            Some(at) => with_set(req, this, |set| set.rename(at, None)).map(|()| false),
            None => with_set(req, this, |set| set.add(token)).map(|()| true),
        };
        update(req, this);
        Ok(answer.unwrap_or(false))
    }

    /// `Dom\TokenList::replace(string $token, string $newToken): bool`: the
    /// token renamed in place, or dropped where the new one is there
    /// already.
    fn replace(
        req: &mut Request,
        this: This<Self>,
        token: Path,
        newToken: Path,
    ) -> Result<bool, Thrown> {
        let (token, fresh) = (token.0.as_bytes(), newToken.0.as_bytes().to_vec());
        validate(req, token)?;
        validate(req, &fresh)?;
        let this = this.object();
        // `zend_hash_set_bucket_key`, or the bucket dropped where the new
        // token is in the set already.
        let replaced = with_set(req, this, |set| {
            let at = set.find(token)?;
            let present = set.find(&fresh).is_some_and(|other| other != at);
            set.rename(at, (!present).then_some(fresh));
            Some(())
        })
        .flatten();
        if replaced.is_none() {
            return Ok(false);
        }
        update(req, this);
        Ok(true)
    }

    /// `Dom\TokenList::supports(string $token): bool`: `classList` defines
    /// no supported tokens, so php's `TypeError`.
    fn supports(req: &mut Request, _this: This<Self>, token: Path) -> Result<bool, Thrown> {
        let _ = token;
        Err(throw_error(
            req,
            "TypeError",
            "Attribute \"class\" does not define any supported tokens",
        ))
    }

    /// `Dom\TokenList::count(): int`.
    fn count(req: &mut Request, this: This<Self>) -> i64 {
        let n = tokens(req, this.object()).len();
        i64::try_from(n).unwrap_or(i64::MAX)
    }

    /// `Dom\TokenList::getIterator(): Iterator`: an `InternalIterator`
    /// over the list's own iterator, which rewinds.
    ///
    /// ```php
    /// $l = Dom\XMLDocument::createFromString('<r class="A B"/>')->documentElement->classList;
    /// $it = $l->getIterator();
    /// var_dump(get_class($it));
    /// foreach ($it as $k => $v) echo "$k=$v;";
    /// $it->rewind();
    /// var_dump($it->current());
    /// ```
    /// ```expect
    /// string(16) "InternalIterator"
    /// 0=A;1=B;string(1) "A"
    /// ```
    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<crate::Iterator>>, Thrown> {
        php_ext::call::internal_iterator(req, &this.object()).map(Owner::into_instance)
    }
}
