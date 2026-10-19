//! What a value of a trace may be (`jit-design.md` §4.2): [`Types`], the
//! subset of php's `MAY_BE_*` bits (`Z/Zend/zend_type_info.h`) the
//! inference keeps, and the one transfer per opcode the trace SSA reads,
//! php's `zend_update_type_info` (`Z/Zend/Optimizer/zend_inference.c`)
//! over those bits for the scalar core and "anything" past it. php's
//! reference counts and ranges are not kept: a long arithmetic is long
//! or double here wherever php's range inference does not prove it
//! stays long, which costs a guard and never a wrong answer.

use php_heap::{Zval, ZvalType};
use php_unit::Opcode;

/// A set of the kinds a value may be, php's `MAY_BE_*` with one bit per
/// `IS_*` from `IS_UNDEF` to `IS_RESOURCE` at php's places, `MAY_BE_REF`,
/// and an array's two layouts, php's `MAY_BE_ARRAY_PACKED` and
/// `MAY_BE_ARRAY_HASH`, which mean something only beside `ARRAY`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct Types(u16);

impl Types {
    pub(crate) const NONE: Types = Types(0);
    pub(crate) const UNDEF: Types = Types(1 << 0);
    pub(crate) const NULL: Types = Types(1 << 1);
    pub(crate) const FALSE: Types = Types(1 << 2);
    pub(crate) const TRUE: Types = Types(1 << 3);
    pub(crate) const LONG: Types = Types(1 << 4);
    pub(crate) const DOUBLE: Types = Types(1 << 5);
    pub(crate) const STRING: Types = Types(1 << 6);
    pub(crate) const ARRAY: Types = Types(1 << 7);
    pub(crate) const OBJECT: Types = Types(1 << 8);
    pub(crate) const RESOURCE: Types = Types(1 << 9);
    /// php's `MAY_BE_REF`: the slot may hold a reference.
    pub(crate) const REF: Types = Types(1 << 10);
    /// An array that may be packed, php's `MAY_BE_ARRAY_PACKED`.
    pub(crate) const PACKED: Types = Types(1 << 11);
    /// An array that may be a hash, php's `MAY_BE_ARRAY_HASH`.
    pub(crate) const HASH: Types = Types(1 << 12);

    pub(crate) const BOOL: Types = Types(Types::FALSE.0 | Types::TRUE.0);
    pub(crate) const NUMBER: Types = Types(Types::LONG.0 | Types::DOUBLE.0);
    /// php's `MAY_BE_ANY`: every kind a value can be, an array of either
    /// layout.
    pub(crate) const ANY: Types = Types(0x3fe | Types::PACKED.0 | Types::HASH.0);
    /// What a variable nothing narrowed may hold: anything, unset, or a
    /// reference to anything.
    pub(crate) const UNKNOWN: Types = Types(Types::ANY.0 | Types::UNDEF.0 | Types::REF.0);
    /// The kinds, without the layouts and without `REF`.
    const KINDS: u16 = 0x3ff;
    const LAYOUTS: u16 = Types::PACKED.0 | Types::HASH.0;

    /// Both.
    #[must_use]
    pub(crate) const fn or(self, other: Types) -> Types {
        Types(self.0 | other.0)
    }

    /// What both allow.
    #[must_use]
    pub(crate) const fn and(self, other: Types) -> Types {
        Types(self.0 & other.0)
    }

    /// These less `other`.
    #[must_use]
    pub(crate) const fn without(self, other: Types) -> Types {
        Types(self.0 & !other.0)
    }

    /// Whether any kind of `other` is one of these.
    pub(crate) const fn meets(self, other: Types) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether every kind of these is one of `other`.
    pub(crate) const fn within(self, other: Types) -> bool {
        self.0 & !other.0 == 0
    }

    /// The kinds alone, php's `info & (MAY_BE_ANY|MAY_BE_UNDEF)`.
    #[must_use]
    pub(crate) const fn kinds(self) -> Types {
        Types(self.0 & Types::KINDS)
    }

    /// The one kind these are, where they are one and no reference: what
    /// php's `zend_jit_trace_add_op_guard` compares a recorded type with.
    pub(crate) fn tag(self) -> Option<ZvalType> {
        if self.meets(Types::REF) {
            return None;
        }
        TAGS.iter()
            .find(|(_, bit)| bit.0 == self.kinds().0)
            .map(|(tag, _)| *tag)
    }

    /// The kinds of one tag, an array's of either layout, php's
    /// `zend_jit_trace_type_to_info`; none for a tag no value has.
    pub(crate) fn of_tag(tag: ZvalType) -> Types {
        match tag {
            ZvalType::Array => Types::ARRAY.or(Types::PACKED).or(Types::HASH),
            other => TAGS
                .iter()
                .find(|(t, _)| *t == other)
                .map_or(Types::NONE, |(_, bit)| *bit),
        }
    }

    /// These narrowed to `tag`, php's `zend_jit_trace_type_to_info_ex`:
    /// an array keeps the layouts it may have.
    #[must_use]
    pub(crate) fn narrowed(self, tag: ZvalType) -> Types {
        let to = Types::of_tag(tag);
        if tag == ZvalType::Array {
            let layouts = Types(self.0 & Types::LAYOUTS);
            return Types::ARRAY.or(if layouts == Types::NONE {
                Types(Types::LAYOUTS)
            } else {
                layouts
            });
        }
        to
    }

    /// An array of one layout, where these are an array of either.
    #[must_use]
    pub(crate) fn laid_out(self, packed: bool) -> Types {
        self.without(Types(Types::LAYOUTS))
            .or(if packed { Types::PACKED } else { Types::HASH })
    }

    /// Whether these are an array that may be either layout, php's
    /// `MAY_BE_PACKED(t) && MAY_BE_HASH(t) && (t & ANY|UNDEF) == ARRAY`.
    pub(crate) fn either_layout(self) -> bool {
        self.kinds() == Types::ARRAY && self.0 & Types::LAYOUTS == Types::LAYOUTS
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
        let bits = u16::try_from(mask & 0x3fe).unwrap_or(0);
        let mut types = Types(bits).or(Types::OBJECT).or(Types::REF);
        if types.meets(Types::ARRAY) {
            types = types.or(Types::PACKED).or(Types::HASH);
        }
        types
    }

    /// The types of a constant operand, php's `_const_op_type`.
    pub(crate) fn of_constant(value: &Zval) -> Types {
        match value.kind() {
            ZvalType::Array => value
                .as_arr()
                .map_or(Types::of_tag(ZvalType::Array), |arr| {
                    Types::ARRAY.laid_out(arr.is_packed())
                }),
            tag => Types::of_tag(tag),
        }
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

/// php's `zend_dump_type_info` words: `undef, ref, null, false, true,
/// long, double, string, array, object, resource`, `bool` for both
/// booleans, `any` for every kind, and an array's layout before it,
/// `packed array` or `hash array`, where it is one.
impl std::fmt::Display for Types {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut words: Vec<&str> = Vec::new();
        if self.meets(Types::UNDEF) {
            words.push("undef");
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

/// What a binary operator yields over operands of `a` and `b`, php's
/// `zend_update_type_info` for `ADD` to `POW`, the comparisons and the
/// bitwise operators, without ranges.
pub(crate) fn binary(op: Opcode, a: Types, b: Types) -> Types {
    let (a, b) = (deref(a), deref(b));
    let numbers = a.kinds().within(Types::NUMBER) && b.kinds().within(Types::NUMBER);
    let doubled = (a.kinds() == Types::DOUBLE && b.kinds().within(Types::NUMBER))
        || (b.kinds() == Types::DOUBLE && a.kinds().within(Types::NUMBER));
    match op {
        Opcode::Add | Opcode::Sub | Opcode::Mul => {
            if doubled {
                Types::DOUBLE
            } else if numbers {
                Types::NUMBER
            } else if op == Opcode::Add && a.kinds() == Types::ARRAY && b.kinds() == Types::ARRAY {
                Types::of_tag(ZvalType::Array)
            } else {
                Types::ANY
            }
        }
        Opcode::Div | Opcode::Pow => {
            if doubled {
                Types::DOUBLE
            } else if numbers {
                Types::NUMBER
            } else {
                Types::ANY
            }
        }
        Opcode::Mod | Opcode::Sl | Opcode::Sr => Types::LONG,
        Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor => {
            if a.kinds()
                .within(Types::NUMBER.or(Types::BOOL).or(Types::NULL))
                && b.kinds()
                    .within(Types::NUMBER.or(Types::BOOL).or(Types::NULL))
            {
                Types::LONG
            } else {
                Types::LONG.or(Types::STRING)
            }
        }
        Opcode::Concat | Opcode::FastConcat => Types::STRING,
        Opcode::IsEqual
        | Opcode::IsNotEqual
        | Opcode::IsSmaller
        | Opcode::IsSmallerOrEqual
        | Opcode::IsIdentical
        | Opcode::IsNotIdentical
        | Opcode::Case
        | Opcode::CaseStrict
        | Opcode::BoolXor => Types::BOOL,
        _ => Types::ANY,
    }
}

/// What `++` or `--` makes of `a`, php's `zend_update_type_info` for
/// `PRE_INC` and its kin: a long may overflow to a double, a null becomes
/// a long or stays null, and anything else is anything.
pub(crate) fn stepped(a: Types) -> Types {
    let a = deref(a);
    if a.kinds() == Types::LONG {
        Types::NUMBER
    } else if a.kinds() == Types::DOUBLE {
        Types::DOUBLE
    } else if a.kinds().within(Types::NUMBER) {
        Types::NUMBER
    } else {
        Types::ANY
    }
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
    use php_unit::Opcode;

    use super::{Types, binary, stepped};

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

    /// Long arithmetic may overflow to a double, a double makes a double,
    /// and a comparison is a boolean, as php infers with no ranges.
    #[test]
    fn arithmetic_is_inferred_as_php_infers_it() {
        assert_eq!(binary(Opcode::Add, Types::LONG, Types::LONG), Types::NUMBER);
        assert_eq!(
            binary(Opcode::Mul, Types::LONG, Types::DOUBLE),
            Types::DOUBLE
        );
        assert_eq!(
            binary(Opcode::IsSmaller, Types::ANY, Types::LONG),
            Types::BOOL
        );
        assert_eq!(binary(Opcode::BwAnd, Types::LONG, Types::LONG), Types::LONG);
        assert_eq!(stepped(Types::LONG), Types::NUMBER);
        // What a reference holds is anything.
        assert_eq!(stepped(Types::LONG.or(Types::REF)), Types::ANY);
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
        assert_eq!(format!("{}", Types::ARRAY.laid_out(true)), "packed array");
    }
}
