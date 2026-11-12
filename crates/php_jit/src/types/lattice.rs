//! What a value of a trace may be (`jit-design.md` §4.2): [`Types`],
//! php's `MAY_BE_*` bits (`Z/Zend/zend_type_info.h`) as its inference
//! types a variable, and the part of them the trace decides by.

use php_engine::api::abi::EngineState;
use php_heap::{Zval, ZvalType};
use php_ssa::Types as Php;
use php_unit::Opcode;

use super::Layout;

/// What a value may be: php's `MAY_BE_*` bits (`Z/Zend/zend_type_info.h`)
/// whole, as php's inference types a variable, the kinds from `IS_UNDEF`
/// to `IS_RESOURCE` and `MAY_BE_REF` at php's places, an array's keys and
/// elements, its reference counts and `MAY_BE_INDIRECT`. What the trace
/// decides by is a part of them, its *view*: the kinds, a reference, the
/// indirection, and an array's two layouts, php's `MAY_BE_PACKED` and
/// `MAY_BE_HASH` (an array whose keys php does not know, or that may be
/// php's empty array, being of either), which mean something only beside
/// `ARRAY`. Two sets are equal, and one is within another, by their
/// views: what php knows of an array's elements or a count rides along.
#[derive(Clone, Copy, Default, EngineState)]
pub(crate) struct Types(Php);

impl PartialEq for Types {
    fn eq(&self, other: &Types) -> bool {
        self.view() == other.view()
    }
}

impl Eq for Types {}

impl std::hash::Hash for Types {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.view().hash(state);
    }
}

/// The kinds and `MAY_BE_REF`, at php's places.
const KINDS_REF: u32 = 0x7ff;
/// The view's bits for a layout: php's `MAY_BE_ARRAY_PACKED`, and one of
/// php's two hash bits standing for both.
const VIEW_PACKED: u32 = 1 << 21;
const VIEW_HASH: u32 = 1 << 22;
/// php's `MAY_BE_INDIRECT`.
const VIEW_INDIRECT: u32 = 1 << 25;

impl Types {
    /// The bits the trace decides by.
    fn view(self) -> u32 {
        let bits = self.0.0;
        let mut view = bits & (KINDS_REF | VIEW_INDIRECT);
        if bits & Php::ARRAY.0 != 0 {
            let packed = bits & Php::PACKED.0 != 0;
            let hash = bits & (Php::NUMERIC_HASH.0 | Php::STRING_HASH.0) != 0;
            let empty = bits & Php::EMPTY.0 != 0;
            view |= match (packed, hash || empty) {
                (true, false) => VIEW_PACKED,
                (false, true) => VIEW_HASH,
                _ => VIEW_PACKED | VIEW_HASH,
            };
        } else {
            view |= bits & Php::PACKED.0;
            if bits & (Php::NUMERIC_HASH.0 | Php::STRING_HASH.0) != 0 {
                view |= VIEW_HASH;
            }
        }
        view
    }

    /// php's bits, as its inference reads and writes them.
    pub(crate) const fn info(self) -> Php {
        self.0
    }

    /// The types php's bits `info` name, its code generator's marks,
    /// `MAY_BE_GUARD` and `MAY_BE_PACKED_GUARD` (`MAY_BE_CLASS_GUARD`'s
    /// bit), left out.
    pub(crate) fn of_info(info: Php) -> Types {
        Types(info.without(Php::GUARD | Php(1 << 27)))
    }

    /// The bits, as a recorded trace's text form writes a native's
    /// declared return ([`Types::of_bits`]): the view, in the compact
    /// form the text has always carried.
    pub(crate) fn bits(self) -> u16 {
        let view = self.view();
        let mut bits = u16::try_from(view & KINDS_REF).unwrap_or(0);
        if view & VIEW_PACKED != 0 {
            bits |= 1 << 11;
        }
        if view & VIEW_HASH != 0 {
            bits |= 1 << 12;
        }
        if view & VIEW_INDIRECT != 0 {
            bits |= 1 << 13;
        }
        bits
    }

    /// The types `bits` names; none for a bit no type has. A kind is
    /// php's of its tag, its counts and an array's elements anything.
    pub(crate) fn of_bits(bits: u16) -> Option<Types> {
        if bits & !0x3fff != 0 {
            return None;
        }
        let mut types = Types::NONE;
        for (tag, bit) in TAGS {
            if u32::from(bits) & bit.0.0 != 0 {
                types = types.or(Types::of_tag(tag));
            }
        }
        if bits & (1 << 10) != 0 {
            types = types.or(Types::REF);
        }
        if bits & (1 << 13) != 0 {
            types = types.or(Types::INDIRECT);
        }
        if types.meets(Types::ARRAY) {
            let layouts = match (bits & (1 << 11) != 0, bits & (1 << 12) != 0) {
                (true, false) => Some(Layout::Packed),
                (false, true) => Some(Layout::Hash),
                _ => None,
            };
            if let Some(layout) = layouts {
                types = types.laid_out(layout);
            }
        }
        Some(types)
    }

    pub(crate) const NONE: Types = Types(Php::NONE);
    pub(crate) const UNDEF: Types = Types(Php::UNDEF);
    pub(crate) const NULL: Types = Types(Php::NULL);
    pub(crate) const FALSE: Types = Types(Php::FALSE);
    pub(crate) const TRUE: Types = Types(Php::TRUE);
    pub(crate) const LONG: Types = Types(Php::LONG);
    pub(crate) const DOUBLE: Types = Types(Php::DOUBLE);
    pub(crate) const STRING: Types = Types(Php::STRING);
    pub(crate) const ARRAY: Types = Types(Php::ARRAY);
    pub(crate) const OBJECT: Types = Types(Php::OBJECT);
    pub(crate) const RESOURCE: Types = Types(Php::RESOURCE);
    /// php's `MAY_BE_REF`: the slot may hold a reference.
    pub(crate) const REF: Types = Types(Php::REF);
    /// An array that may be packed, php's `MAY_BE_ARRAY_PACKED`.
    pub(crate) const PACKED: Types = Types(Php::PACKED);
    /// An array that may be a hash, php's `MAY_BE_HASH`: either of its
    /// hash keys.
    pub(crate) const HASH: Types = Types(Php(Php::NUMERIC_HASH.0 | Php::STRING_HASH.0));
    /// php's `MAY_BE_INDIRECT`: the slot holds the address of a zval,
    /// `IS_INDIRECT`, which a fetch for writing leaves in its `VAR`.
    pub(crate) const INDIRECT: Types = Types(Php::INDIRECT);

    pub(crate) const BOOL: Types = Types(Php::BOOL);
    pub(crate) const NUMBER: Types = Types(Php(Php::LONG.0 | Php::DOUBLE.0));
    /// Every kind a value can be, php's `MAY_BE_ANY` with an array of any
    /// keys and elements and both counts, what php's inference knows of a
    /// value it knows nothing of.
    pub(crate) const ANY: Types = Types(Php::UNKNOWN);
    /// What a variable nothing narrowed may hold: anything, unset, or a
    /// reference to anything.
    pub(crate) const UNKNOWN: Types = Types(Php(Php::UNKNOWN.0 | Php::UNDEF.0 | Php::REF.0));
    /// The kinds, without the layouts and without `REF`.
    const KINDS: u32 = 0x3ff;

    /// Both.
    #[must_use]
    pub(crate) const fn or(self, other: Types) -> Types {
        Types(Php(self.0.0 | other.0.0))
    }

    /// What both allow.
    #[must_use]
    pub(crate) const fn and(self, other: Types) -> Types {
        Types(Php(self.0.0 & other.0.0))
    }

    /// These less `other`.
    #[must_use]
    pub(crate) const fn without(self, other: Types) -> Types {
        Types(Php(self.0.0 & !other.0.0))
    }

    /// Whether any kind of `other` is one of these.
    pub(crate) fn meets(self, other: Types) -> bool {
        self.view() & other.view() != 0
    }

    /// Whether every kind of these is one of `other`.
    pub(crate) fn within(self, other: Types) -> bool {
        self.view() & !other.view() == 0
    }

    /// The kinds alone, php's `info & (MAY_BE_ANY|MAY_BE_UNDEF)`.
    #[must_use]
    pub(crate) const fn kinds(self) -> Types {
        Types(Php(self.0.0 & Types::KINDS))
    }

    /// The one kind these are, where they are one and no reference: what
    /// php's `zend_jit_trace_add_op_guard` compares a recorded type with.
    pub(crate) fn tag(self) -> Option<ZvalType> {
        if self.meets(Types::REF) {
            return None;
        }
        TAGS.iter()
            .find(|(_, bit)| bit.kinds() == self.kinds())
            .map(|(tag, _)| *tag)
    }

    /// The type bytes a slot holding one of these may show, where they
    /// rule some out: none where they may be a reference or are every
    /// kind, when any byte may stand.
    pub(crate) fn tags(self) -> Option<impl Iterator<Item = ZvalType>> {
        let every = Types::ANY.or(Types::UNDEF).kinds();
        (!self.meets(Types::REF) && self.kinds() != every).then(move || {
            (TAGS.into_iter())
                .filter(move |(_, bit)| self.meets(*bit))
                .map(|(tag, _)| tag)
        })
    }

    /// The kinds of one tag, php's `zend_jit_trace_type_to_info`: a
    /// counted kind with both counts, an array of any keys and elements;
    /// none for a tag no value has.
    pub(crate) fn of_tag(tag: ZvalType) -> Types {
        Types::ANY.or(Types::UNDEF).narrowed(tag)
    }

    /// These narrowed to `tag`, php's `zend_jit_trace_type_to_info_ex`
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:356-369`): a scalar its kind
    /// alone, a counted kind its counts kept, an array its keys, elements
    /// and counts; none for a tag no value has.
    #[must_use]
    pub(crate) fn narrowed(self, tag: ZvalType) -> Types {
        let Some((_, bit)) = TAGS.iter().find(|(t, _)| *t == tag) else {
            return Types::NONE;
        };
        let info = self.0;
        let kept = match tag {
            ZvalType::Undef
            | ZvalType::Null
            | ZvalType::False
            | ZvalType::True
            | ZvalType::Long
            | ZvalType::Double => Php::NONE,
            ZvalType::Array => info & (Php::OF_ANY | Php::OF_REF | Php::KEY_ANY | Php::RC),
            _ => info & Php::RC,
        };
        Types(bit.0 | kept)
    }

    /// An array of one layout, where these are an array of either: a
    /// packed array no hash and not php's empty one, which is no packed
    /// array, as php's packed guard leaves it.
    #[must_use]
    pub(crate) fn laid_out(self, layout: Layout) -> Types {
        let mut info = self.0;
        if info & Php::KEY_ANY == Php::NONE {
            info |= Php::KEY_ANY;
        }
        Types(match layout {
            Layout::Packed => {
                info.without(Php::NUMERIC_HASH | Php::STRING_HASH | Php::EMPTY) | Php::PACKED
            }
            Layout::Hash => {
                let hash = info & (Php::NUMERIC_HASH | Php::STRING_HASH);
                let hash = if hash == Php::NONE {
                    Php::NUMERIC_HASH | Php::STRING_HASH
                } else {
                    hash
                };
                info.without(Php::PACKED | Php::NUMERIC_HASH | Php::STRING_HASH) | hash
            }
        })
    }

    /// Whether these are an array that may be either layout, php's
    /// `MAY_BE_PACKED(t) && MAY_BE_HASH(t) && (t & ANY|UNDEF) == ARRAY`.
    pub(crate) fn either_layout(self) -> bool {
        let view = self.view();
        self.kinds() == Types::ARRAY && view & VIEW_PACKED != 0 && view & VIEW_HASH != 0
    }

    /// Whether a value of these kinds lives in a register alone: one
    /// scalar, or a boolean of either value.
    pub(crate) fn scalar(self) -> bool {
        !self.meets(Types::REF)
            && self.kinds() != Types::NONE
            && (self.kinds() == Types::BOOL || self.tag().is_some_and(is_scalar_tag))
    }

    /// The types of the parameter a `RECV`'s `op2` mask declares
    /// (`Type::mask`, php's `ZEND_TYPE_PURE_MASK`): the mask's kinds, and
    /// beside them an object and a reference, which the mask cannot
    /// carry, since a class name adds no bit to it and passing by
    /// reference is the declaration's.
    pub(crate) fn of_mask(mask: u32) -> Types {
        (Types::NONE.narrowed_kinds(mask & 0x3fe))
            .or(Types::of_tag(ZvalType::Object))
            .or(Types::REF)
    }

    /// Each kind of php's `MAY_BE_*` `mask` as `of_tag` makes it, beside
    /// these.
    #[must_use]
    fn narrowed_kinds(self, mask: u32) -> Types {
        (TAGS.iter())
            .filter(|(_, bit)| mask & bit.0.0 != 0)
            .fold(self, |types, (tag, _)| types.or(Types::of_tag(*tag)))
    }

    /// The types of a constant operand, php's `_const_op_type`: a string's
    /// or an array's no count, as an interned or immutable value has none
    /// (`Z/Zend/Optimizer/zend_inference.c:4130-4150`).
    pub(crate) fn of_constant(value: &Zval) -> Types {
        match value.kind() {
            ZvalType::Array => value
                .as_arr()
                .map_or(Types::of_tag(ZvalType::Array), |arr| {
                    Types::of_tag(ZvalType::Array).laid_out(if arr.is_packed() {
                        Layout::Packed
                    } else {
                        Layout::Hash
                    })
                })
                .without(Types(Php::RC)),
            tag => Types::of_tag(tag).without(Types(Php::RC)),
        }
    }

    /// These narrowed by what php's inference over the op array knows of
    /// the same value, `zend_jit_trace_restrict_ssa_var_info`'s `type &=`
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:1059-1100`); these where the
    /// two share no kind, a value php's inference holds unreachable.
    #[must_use]
    pub(crate) fn restricted(self, info: Php) -> Types {
        let narrowed = self.and(Types::of_info(info));
        if narrowed.kinds() == Types::NONE {
            return self;
        }
        narrowed
    }
}

/// The scalar tags, which a register holds.
fn is_scalar_tag(tag: ZvalType) -> bool {
    matches!(
        tag,
        ZvalType::Null | ZvalType::False | ZvalType::True | ZvalType::Long | ZvalType::Double
    )
}

/// php's `IS_*` and its `MAY_BE_*`.
const TAGS: [(ZvalType, Types); 10] = [
    (ZvalType::Undef, Types::UNDEF),
    (ZvalType::Null, Types::NULL),
    (ZvalType::False, Types::FALSE),
    (ZvalType::True, Types::TRUE),
    (ZvalType::Long, Types::LONG),
    (ZvalType::Double, Types::DOUBLE),
    (ZvalType::String, Types::STRING),
    (ZvalType::Array, Types::ARRAY),
    (ZvalType::Object, Types::OBJECT),
    (ZvalType::Resource, Types::RESOURCE),
];

impl std::fmt::Debug for Types {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{self}]")
    }
}

/// php's `zend_dump_type_info` words: `undef, ind, ref, null, false, true,
/// long, double, string, array, object, resource`, `bool` for both
/// booleans, `any` for every kind, and an array's layout before it,
/// `packed array` or `hash array`, where it is one.
impl std::fmt::Display for Types {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut words: Vec<&str> = Vec::new();
        if self.meets(Types::UNDEF) {
            words.push("undef");
        }
        if self.meets(Types::INDIRECT) {
            words.push("ind");
        }
        if self.meets(Types::REF) {
            words.push("ref");
        }
        if self.within(Types::UNKNOWN) && Types::ANY.within(*self) {
            words.push("any");
        } else {
            if self.meets(Types::NULL) {
                words.push("null");
            }
            if Types::BOOL.within(*self) {
                words.push("bool");
            } else if self.meets(Types::FALSE) {
                words.push("false");
            } else if self.meets(Types::TRUE) {
                words.push("true");
            }
            for (bit, word) in [
                (Types::LONG, "long"),
                (Types::DOUBLE, "double"),
                (Types::STRING, "string"),
            ] {
                if self.meets(bit) {
                    words.push(word);
                }
            }
            if self.meets(Types::ARRAY) {
                words.push(match (self.meets(Types::PACKED), self.meets(Types::HASH)) {
                    (true, false) => "packed array",
                    (false, true) => "hash array",
                    _ => "array",
                });
            }
            if self.meets(Types::OBJECT) {
                words.push("object");
            }
            if self.meets(Types::RESOURCE) {
                words.push("resource");
            }
        }
        f.write_str(&words.join(", "))
    }
}

/// php's `ZEND_ADD` .. `ZEND_POW` numbers, which an `ASSIGN_OP`'s and an
/// `ASSIGN_DIM_OP`'s `extended_value` name the operator by.
pub(crate) fn binary_of(extended: u32) -> Option<Opcode> {
    Opcode::from_u8(u8::try_from(extended).ok()?)
}

/// A value read out of a slot that may hold a reference: what the
/// reference holds, which is anything where the slot may be one.
pub(crate) fn deref(a: Types) -> Types {
    if a.meets(Types::REF) {
        a.without(Types::REF).or(Types::ANY)
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use php_heap::ZvalType;

    use super::Types;
    use crate::types::Layout;

    /// A tag is the one kind of a set, as php's guard compares it; an
    /// array's layouts and a reference are no tag of their own.
    #[test]
    fn a_tag_is_one_kind() {
        assert_eq!(Types::LONG.tag(), Some(ZvalType::Long));
        assert_eq!(Types::of_tag(ZvalType::Array).tag(), Some(ZvalType::Array));
        assert_eq!(Types::NUMBER.tag(), None);
        assert_eq!(Types::LONG.or(Types::REF).tag(), None);
        assert_eq!(Types::UNKNOWN.narrowed(ZvalType::Long), Types::LONG);
    }

    /// A long, a boolean and a double are each one scalar; a number
    /// is two.
    #[test]
    fn a_scalar_is_one_kind() {
        assert!(!Types::NUMBER.scalar() && Types::BOOL.scalar() && Types::DOUBLE.scalar());
    }

    /// A parameter's mask keeps an object and a reference beside its
    /// kinds, which the mask cannot carry.
    #[test]
    fn a_parameter_may_be_an_object_or_a_reference() {
        let int = Types::of_mask(1 << 4);
        assert_eq!(int, Types::LONG.or(Types::OBJECT).or(Types::REF));
        assert_eq!(format!("{int}"), "ref, long, object");
        assert_eq!(format!("{}", Types::UNKNOWN), "undef, ref, any");
        assert_eq!(
            format!("{}", Types::ARRAY.laid_out(Layout::Packed)),
            "packed array"
        );
    }
}
