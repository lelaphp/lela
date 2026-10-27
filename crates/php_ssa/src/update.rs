//! `_zend_update_type_info`: the types an opline gives the variables it
//! defines from the types of those it reads, opcode by opcode, with
//! `zend_inference.c`'s functions for arrays, binary operations, property
//! and class lookups beside it.

use php_unit::{Opcode, OperandKind, Opline};

use crate::bits::Bits;
use crate::infer::{BIND_REF, ELEMENT_REF, Infer, Narrowed, binary_opcode};
use crate::ssa::SsaOp;
use crate::types::{Types, bit, of_declared};
use crate::{LinkedTable, Property, SsaVar};

/// A declared type's mask and the class it names.
type Declared<C> = (Types, Option<C>);

/// `ZEND_FETCH_CLASS_MASK` and the fetch kinds under it.
pub(crate) const FETCH_CLASS_MASK: u32 = 0x0f;
pub(crate) const FETCH_CLASS_SELF: u32 = 1;
const FETCH_CLASS_PARENT: u32 = 2;
pub(crate) const FETCH_CLASS_STATIC: u32 = 3;
/// `ZEND_FETCH_OBJ_FLAGS` and `ZEND_FETCH_DIM_WRITE` beside it.
const FETCH_OBJ_FLAGS: u32 = 3;
const FETCH_DIM_WRITE: u32 = 2;
/// `ZEND_SHORT_CIRCUITING_CHAIN_MASK` and its kinds.
const CHAIN_MASK: u32 = 0x3;
const CHAIN_EXPR: u32 = 0;
const CHAIN_ISSET: u32 = 1;
/// `ZEND_RETURNS_FUNCTION`.
const RETURNS_FUNCTION: u32 = 1;
/// `ZEND_BIND_IMPLICIT`.
const BIND_IMPLICIT: u32 = 2;
/// `IS_STRING`, `IS_ARRAY` and `IS_OBJECT`, as a `CAST` names them.
const IS_STRING: u32 = 6;
const IS_ARRAY: u32 = 7;
const IS_OBJECT: u32 = 8;

/// `zend_array_element_type`: what an element of a `t1` holds when read,
/// written or appended.
fn element_type(t1: Types, op_type: OperandKind, write: bool, insert: bool) -> Types {
    let mut tmp = Types::NONE;
    if t1.has(Types::OBJECT) {
        // Never a reference when read, by `ZVAL_COPY_DEREF`.
        tmp |= if write {
            Types::REF_ANY
        } else {
            Types::UNKNOWN
        };
        if write {
            tmp |= Types::INDIRECT;
        }
    }
    if t1.has(Types::ARRAY) {
        if insert {
            tmp |= Types::NULL;
        } else {
            tmp |= Types::NULL | t1.elements();
            if tmp.has(Types::ARRAY) {
                tmp |= Types::ARRAY_ANY;
            }
            if tmp.has(Types::STRING | Types::ARRAY | Types::OBJECT | Types::RESOURCE) {
                if !write {
                    tmp |= Types::RCN;
                    if matches!(op_type, OperandKind::Var | OperandKind::Tmp) && t1.has(Types::RC1)
                    {
                        tmp |= Types::RC1;
                    }
                } else if t1.has(Types::OF_REF) {
                    tmp |= Types::REF | Types::RC;
                } else {
                    tmp |= Types::RC;
                }
            }
        }
        if write {
            tmp |= Types::INDIRECT;
        }
    }
    if t1.has(Types::STRING) {
        tmp |= Types::STRING | Types::RC1;
        if write {
            tmp |= Types::NULL;
        }
    }
    if t1.has(Types::UNDEF | Types::NULL | Types::FALSE) {
        tmp |= Types::NULL;
        if write {
            tmp |= Types::INDIRECT;
        }
    }
    if t1.has(Types::TRUE | Types::LONG | Types::DOUBLE | Types::RESOURCE) && !write {
        tmp |= Types::NULL;
    }
    tmp
}

/// The keys a write of a `dim` of `dim_type` adds to an array of
/// `arr_type`, the part `assign_dim_array_result_type` and the `FETCH_DIM`
/// writes share: an integer key packs a new array and hashes into one
/// that is all hash; a string key from a variable may be numeric.
fn written_keys(arr_type: Types, dim_type: Types, dim_kind: OperandKind) -> Types {
    let long = |keys: &mut Types| {
        if arr_type.has(Types::UNDEF | Types::NULL | Types::FALSE) {
            *keys |= Types::PACKED;
        }
        *keys |= if arr_type.hash_only() {
            Types::NUMERIC_HASH
        } else {
            Types::KEY_LONG
        };
    };
    let mut keys = Types::NONE;
    if dim_kind == OperandKind::Unused {
        long(&mut keys);
        return keys;
    }
    if dim_type.has(Types::LONG | Types::FALSE | Types::TRUE | Types::RESOURCE | Types::DOUBLE) {
        long(&mut keys);
    }
    if dim_type.has(Types::STRING) {
        keys |= Types::KEY_STRING;
        if dim_kind != OperandKind::Const {
            long(&mut keys);
        }
    }
    if dim_type.has(Types::UNDEF | Types::NULL) {
        keys |= Types::KEY_STRING;
    }
    keys
}

/// `assign_dim_array_result_type`.
fn assign_dim_array_result(
    arr_type: Types,
    dim_type: Types,
    value_type: Types,
    dim_kind: OperandKind,
) -> Types {
    let mut tmp = Types::NONE;
    let mut value_type = value_type;
    // A key type stands iff a value type does, even in dead code.
    if value_type.has(Types::ANY | Types::UNDEF) {
        if value_type.has(Types::UNDEF) {
            value_type |= Types::NULL;
        }
        tmp |= written_keys(arr_type, dim_type, dim_kind);
    }
    if tmp.has(Types::KEY_ANY) {
        tmp |= value_type.as_elements();
    }
    tmp.without(Types::EMPTY)
}

/// `assign_dim_result_type`.
fn assign_dim_result(
    arr_type: Types,
    dim_type: Types,
    value_type: Types,
    dim_kind: OperandKind,
) -> Types {
    let mut tmp = arr_type.without(Types::RC);
    if arr_type.has(Types::UNDEF | Types::NULL | Types::FALSE) {
        tmp = tmp.without(Types::UNDEF | Types::NULL | Types::FALSE);
        tmp |= Types::ARRAY | Types::RC1;
    }
    if tmp.has(Types::ARRAY | Types::STRING) {
        tmp |= Types::RC1;
    }
    if tmp.has(Types::OBJECT | Types::RESOURCE) {
        tmp |= Types::RC;
    }
    if tmp.has(Types::ARRAY) {
        tmp |= assign_dim_array_result(arr_type, dim_type, value_type, dim_kind);
    }
    tmp
}

impl<T: LinkedTable> Infer<'_, T> {
    /// `binary_op_result_type`, the range of an integer sum read from
    /// the variable `result`.
    fn binary_result(&self, opcode: Opcode, t1: Types, t2: Types, result: Option<SsaVar>) -> Types {
        let mut tmp = Types::NONE;
        let ty = |t: Types| {
            (t & Types::ANY)
                | if t.has(Types::UNDEF) {
                    Types::NULL
                } else {
                    Types::NONE
                }
        };
        let (t1_type, t2_type) = (ty(t1), ty(t2));
        // Potentially overloaded operators, GMP's way.
        if t1_type.has(Types::OBJECT) || t2_type.has(Types::OBJECT) {
            tmp |= Types::OBJECT | Types::FALSE | Types::RC1;
        }
        let exact = || {
            self.var_info(result)
                .and_then(|i| i.range)
                .is_some_and(|r| !r.underflow && !r.overflow)
        };
        let array_parts = Types::ARRAY_ANY;
        match opcode {
            Opcode::Add => {
                if t1_type == Types::LONG && t2_type == Types::LONG {
                    tmp |= if exact() {
                        Types::LONG
                    } else {
                        Types::LONG | Types::DOUBLE
                    };
                } else if t1_type == Types::DOUBLE || t2_type == Types::DOUBLE {
                    tmp |= Types::DOUBLE;
                } else if t1_type == Types::ARRAY && t2_type == Types::ARRAY {
                    tmp |= Types::ARRAY | Types::RC1 | (t1 & array_parts) | (t2 & array_parts);
                } else {
                    tmp |= Types::LONG | Types::DOUBLE;
                    if t1_type.has(Types::ARRAY) && t2_type.has(Types::ARRAY) {
                        tmp |= Types::ARRAY | Types::RC1 | (t1 & array_parts) | (t2 & array_parts);
                    }
                }
            }
            Opcode::Sub | Opcode::Mul => {
                if t1_type == Types::LONG && t2_type == Types::LONG {
                    tmp |= if exact() {
                        Types::LONG
                    } else {
                        Types::LONG | Types::DOUBLE
                    };
                } else if t1_type == Types::DOUBLE || t2_type == Types::DOUBLE {
                    tmp |= Types::DOUBLE;
                } else {
                    tmp |= Types::LONG | Types::DOUBLE;
                }
            }
            Opcode::Div | Opcode::Pow => {
                tmp |= if t1_type == Types::DOUBLE || t2_type == Types::DOUBLE {
                    Types::DOUBLE
                } else {
                    Types::LONG | Types::DOUBLE
                };
            }
            Opcode::Mod | Opcode::Sl | Opcode::Sr => tmp |= Types::LONG,
            Opcode::BwOr | Opcode::BwAnd | Opcode::BwXor => {
                if t1_type.has(Types::STRING) && t2_type.has(Types::STRING) {
                    tmp |= Types::STRING | Types::RC;
                }
                if t1_type.has(!Types::STRING) || t2_type.has(!Types::STRING) {
                    tmp |= Types::LONG;
                }
            }
            Opcode::Concat | Opcode::FastConcat => tmp = Types::STRING | Types::RC,
            _ => {}
        }
        tmp
    }

    /// `_ssa_op1_info` of the opline at `at`.
    pub(crate) fn op1_types(&self, at: usize) -> Types {
        let (Some(op), ssa_op) = (self.body.oplines.get(at), self.op(at)) else {
            return Types::NONE;
        };
        self.operand_types(op.op1_type, op.op1, ssa_op.op1_use)
    }

    /// A string constant's bytes.
    fn str_literal(&self, n: u32) -> Option<&[u8]> {
        self.literal(n)?.str_bytes()
    }

    /// The class a constant `op` names ([`crate::class_named`]).
    fn class_named(&self, n: u32) -> Option<T::Class> {
        crate::class_named(self.function, self.table, n)
    }

    /// `zend_optimizer_get_class_entry_from_op1` ([`crate::class_of_op1`]).
    fn class_of_op1(&self, op: &Opline) -> Option<T::Class> {
        crate::class_of_op1(self.function, self.table, op)
    }

    /// `zend_fetch_prop_info`: the declared type of the instance property
    /// an object fetch at `op` names, where its class is known.
    fn prop_info(&self, op: &Opline, ssa_op: &SsaOp) -> Option<Declared<T::Class>> {
        if op.op2_type != OperandKind::Const {
            return None;
        }
        let class = if op.op1_type == OperandKind::Unused {
            self.table.scope()
        } else {
            self.var_info(ssa_op.op1_use)
                .and_then(|i| i.class)
                .map(|c| c.class)
        }?;
        let prop = self
            .table
            .property(class, self.str_literal(op.op2)?, self.table.scope())?;
        (!prop.is_static).then(|| self.declared(&prop))
    }

    /// `zend_fetch_static_prop_info`, as `prop_info`.
    fn static_prop_info(&self, op: &Opline) -> Option<Declared<T::Class>> {
        if op.op1_type != OperandKind::Const {
            return None;
        }
        let class = match op.op2_type {
            OperandKind::Unused => match op.op2 & FETCH_CLASS_MASK {
                FETCH_CLASS_SELF | FETCH_CLASS_STATIC => self.table.scope(),
                FETCH_CLASS_PARENT => self.table.scope().and_then(|s| self.table.parent(s)),
                _ => None,
            },
            OperandKind::Const => self.class_named(op.op2),
            _ => None,
        }?;
        let prop = self
            .table
            .property(class, self.str_literal(op.op1)?, self.table.scope())?;
        prop.is_static.then(|| self.declared(&prop))
    }

    /// What a property's declared type admits, and the class it names.
    fn declared(&self, prop: &Property<'_, T::Class>) -> Declared<T::Class> {
        of_declared(prop.ty, self.table)
    }

    /// `zend_fetch_prop_type`: what a property holds, and the class its
    /// type names; anything where the property is unknown.
    fn prop_type(prop: Option<Declared<T::Class>>) -> Declared<T::Class> {
        prop.unwrap_or((Types::UNKNOWN, None))
    }

    /// `result_may_be_separated`.
    fn may_be_separated(&self, at: usize, ssa_op: &SsaOp) -> bool {
        let Some(var) = ssa_op.out_def.and_then(|v| self.ssa.var(v)) else {
            return true;
        };
        if let Some(first) = var.use_chain
            && var.phi_use_chain.is_none()
            && first as usize == at + 1
        {
            let next = self.op(at + 1);
            if (next.op1_use == ssa_op.out_def && next.op1_use_chain.is_none())
                || (next.op2_use == ssa_op.out_def && next.op2_use_chain.is_none())
            {
                return false;
            }
        }
        true
    }

    /// Whether the first reader of `var` is the opline after `at`, which
    /// takes its value before any refcount drops.
    fn read_next(&self, at: usize, var: Option<SsaVar>) -> bool {
        var.and_then(|v| self.ssa.var(v))
            .and_then(|v| v.use_chain)
            .is_some_and(|first| first as usize == at + 1)
    }

    /// The constant class `zend_fetch_class_const_info` finds for a
    /// `FETCH_CLASS_CONSTANT`, and its declared type.
    fn class_const_type(&self, op: &Opline) -> Option<(Types, Option<T::Class>)> {
        if op.op2_type != OperandKind::Const {
            return None;
        }
        let name = self.str_literal(op.op2)?;
        let scope = self.table.scope();
        let class = match op.op1_type {
            // With no script php looks in the class table alone.
            OperandKind::Const => {
                self.literal(op.op1)?.as_str()?;
                self.table.class(self.str_literal(op.op1 + 1)?)
            }
            OperandKind::Unused if scope.is_some_and(|s| self.table.is_trait(s)) => None,
            OperandKind::Unused => match op.op1 & FETCH_CLASS_MASK {
                FETCH_CLASS_SELF | FETCH_CLASS_STATIC => scope,
                FETCH_CLASS_PARENT => scope.and_then(|s| self.table.parent(s)),
                _ => None,
            },
            _ => None,
        }
        .filter(|c| !self.table.is_trait(*c))?;
        let constant = self.table.constant(class, name)?;
        if constant.is_deprecated || (!constant.is_public && Some(constant.declared_in) != scope) {
            return None;
        }
        let ty = constant.ty?;
        Some(of_declared(Some(ty), self.table))
    }

    /// `_zend_update_type_info` for the opline at `at`, or the assignment
    /// before it where it is an `OP_DATA`.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub(crate) fn update(&mut self, at: usize, wl: &mut Bits) -> Result<(), Narrowed> {
        let body = self.body;
        let mut at = at;
        if body
            .oplines
            .get(at)
            .is_some_and(|op| op.opcode == Opcode::OpData)
            && at > 0
        {
            at -= 1;
        }
        let Some(&op) = body.oplines.get(at) else {
            return Ok(());
        };
        let ssa_op = self.op(at);
        let data_op = self.op(at + 1);
        let data = body.oplines.get(at + 1).copied();
        let mut t1 = self.operand_types(op.op1_type, op.op1, ssa_op.op1_use);
        let mut t2 = self.operand_types(op.op2_type, op.op2, ssa_op.op2_use);
        let res_use = self.operand_types(op.out_type, op.out, ssa_op.out_use);
        let data_types = |this: &Self| this.op1_types(at + 1);
        let some = Types::ANY | Types::UNDEF | Types::CLASS;
        let assigns_data = matches!(
            op.opcode,
            Opcode::AssignDimOp
                | Opcode::AssignObjOp
                | Opcode::AssignStaticPropOp
                | Opcode::AssignDim
                | Opcode::AssignObj
        );
        // An operand of no type comes from unreachable code: its result is
        // empty too.
        if !t1.has(some)
            || !t2.has(some)
            || (ssa_op.out_use.is_some() && !res_use.has(some))
            || (assigns_data && !data_types(self).has(some))
        {
            let mut defs = vec![ssa_op.out_def, ssa_op.op1_def, ssa_op.op2_def];
            if assigns_data {
                defs.push(data_op.op1_def);
            }
            for def in defs.into_iter().flatten() {
                if !self.var_types(Some(def)).has(Types::REF) {
                    self.set_types(Types::NONE, Some(def), wl)?;
                }
            }
            return Ok(());
        }
        let result = ssa_op.out_def;
        let rc_str = Types::STRING | Types::ARRAY | Types::OBJECT | Types::RESOURCE;
        match op.opcode {
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Pow
            | Opcode::Mod
            | Opcode::BwOr
            | Opcode::BwAnd
            | Opcode::BwXor
            | Opcode::Sl
            | Opcode::Sr
            | Opcode::Concat => {
                let tmp = self.binary_result(op.opcode, t1, t2, result);
                self.set_types(tmp, result, wl)?;
            }
            Opcode::BwNot => {
                let mut tmp = Types::NONE;
                if t1.has(Types::STRING) {
                    tmp |= Types::STRING | Types::RC;
                }
                if t1.has(Types::ANY.without(Types::STRING)) {
                    tmp |= Types::LONG;
                }
                if t1.has(Types::OBJECT) {
                    tmp |= Types::OBJECT | Types::RC1;
                }
                self.set_types(tmp, result, wl)?;
            }
            Opcode::BeginSilence => self.set_types(Types::LONG, result, wl)?,
            Opcode::BoolNot
            | Opcode::BoolXor
            | Opcode::IsIdentical
            | Opcode::IsNotIdentical
            | Opcode::IsEqual
            | Opcode::IsNotEqual
            | Opcode::IsSmaller
            | Opcode::IsSmallerOrEqual
            | Opcode::Instanceof
            | Opcode::JmpzEx
            | Opcode::JmpnzEx
            | Opcode::Case
            | Opcode::CaseStrict
            | Opcode::Bool
            | Opcode::IssetIsemptyCv
            | Opcode::IssetIsemptyVar
            | Opcode::IssetIsemptyDimObj
            | Opcode::IssetIsemptyPropObj
            | Opcode::IssetIsemptyStaticProp
            | Opcode::AssertCheck
            | Opcode::InArray
            | Opcode::ArrayKeyExists
            | Opcode::Defined
            | Opcode::IssetIsemptyThis => self.set_types(Types::BOOL, result, wl)?,
            Opcode::Cast => {
                if ssa_op.op1_def.is_some() {
                    let mut tmp = t1;
                    let to = op.extended_value;
                    if (t1.has(Types::ARRAY | Types::OBJECT) && (to == IS_ARRAY || to == IS_OBJECT))
                        || (t1.has(Types::STRING) && to == IS_STRING)
                    {
                        tmp |= Types::RCN;
                    }
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
                let to = op.extended_value;
                let mut tmp = bit(to);
                if tmp.has(rc_str) {
                    if (tmp & Types::ANY) == (t1 & Types::ANY) {
                        tmp |= (t1 & Types::RC1) | Types::RCN;
                    } else if ((to == IS_ARRAY || to == IS_OBJECT)
                        && t1.has(Types::ARRAY | Types::OBJECT))
                        || (to == IS_STRING && t1.has(Types::STRING | Types::OBJECT))
                    {
                        tmp |= Types::RC;
                    } else {
                        tmp |= Types::RC1;
                        if to == IS_ARRAY && t1.has(Types::UNDEF | Types::NULL) {
                            tmp |= Types::RCN;
                        }
                    }
                }
                if to == IS_ARRAY {
                    if t1.has(Types::UNDEF | Types::NULL) {
                        tmp |= Types::EMPTY;
                    }
                    if t1.has(Types::ARRAY) {
                        tmp |= t1 & Types::ARRAY_ANY;
                    }
                    if t1.has(Types::OBJECT) {
                        tmp |= Types::ARRAY_ANY;
                    } else if t1.has(Types::ANY.without(Types::NULL)) {
                        tmp |= t1.without(Types::NULL).as_elements();
                        tmp |= if t1.has(Types::NULL) {
                            Types::KEY_LONG
                        } else {
                            Types::PACKED
                        };
                    }
                }
                self.set_types(tmp, result, wl)?;
            }
            Opcode::QmAssign | Opcode::JmpSet | Opcode::Coalesce | Opcode::CopyTmp => {
                if ssa_op.op1_def.is_some() {
                    let mut tmp = t1;
                    if t1.has(Types::RC1 | Types::REF) {
                        tmp |= Types::RCN;
                    }
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
                let mut tmp = t1.without(Types::UNDEF);
                if op.opcode != Opcode::CopyTmp || op.op1_type != OperandKind::Var {
                    tmp = tmp.without(Types::REF);
                }
                if t1.has(Types::UNDEF) {
                    tmp |= Types::NULL;
                }
                if t1.has(Types::RC) {
                    tmp |= t1 & Types::RC;
                    if op.opcode == Opcode::CopyTmp || op.op1_type == OperandKind::Cv {
                        tmp |= Types::RCN;
                    }
                }
                if matches!(op.opcode, Opcode::Coalesce | Opcode::JmpSet) {
                    tmp = tmp.without(Types::NULL);
                    if op.opcode == Opcode::JmpSet {
                        tmp = tmp.without(Types::FALSE);
                    }
                }
                self.set_types(tmp, result, wl)?;
                self.copy_class(ssa_op.op1_use, result, wl);
            }
            Opcode::JmpNull => {
                let tmp = match op.extended_value & CHAIN_MASK {
                    CHAIN_EXPR => Types::NULL,
                    CHAIN_ISSET => Types::FALSE,
                    _ => Types::TRUE,
                };
                self.set_types(tmp, result, wl)?;
            }
            Opcode::AssignOp
            | Opcode::AssignDimOp
            | Opcode::AssignObjOp
            | Opcode::AssignStaticPropOp => {
                let mut prop = None;
                let mut orig = Types::NONE;
                let mut tmp = Types::NONE;
                match op.opcode {
                    Opcode::AssignObjOp => {
                        prop = self.prop_info(&op, &ssa_op);
                        orig = t1;
                        t1 = Self::prop_type(prop).0;
                        t2 = data_types(self);
                    }
                    Opcode::AssignDimOp => {
                        if t1.has(Types::OF_REF) {
                            tmp |= Types::REF;
                        }
                        orig = t1;
                        t1 = element_type(t1, op.op1_type, true, false);
                        t2 = data_types(self);
                    }
                    Opcode::AssignStaticPropOp => {
                        prop = self.static_prop_info(&op);
                        t1 = Self::prop_type(prop).0;
                        t2 = data_types(self);
                    }
                    _ => {
                        if t1.has(Types::REF) {
                            tmp |= Types::REF;
                        }
                    }
                }
                let range_var = (op.opcode == Opcode::AssignOp)
                    .then_some(ssa_op.op1_def)
                    .flatten();
                if let Some(binary) = binary_opcode(op.extended_value) {
                    tmp |= self.binary_result(binary, t1, t2, range_var);
                }
                if tmp.has(Types::STRING | Types::ARRAY) {
                    tmp |= Types::RC;
                }
                if tmp.has(Types::OBJECT | Types::RESOURCE) {
                    tmp |= Types::RC;
                }
                match op.opcode {
                    Opcode::AssignDimOp => {
                        if op.op1_type == OperandKind::Cv {
                            let dim = self.operand_types(op.op2_type, op.op2, ssa_op.op2_use);
                            let orig = assign_dim_result(orig, dim, tmp, op.op2_type);
                            self.set_types(orig, ssa_op.op1_def, wl)?;
                            self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                        }
                    }
                    Opcode::AssignObjOp => {
                        if op.op1_type == OperandKind::Cv {
                            let orig = (orig & (Types::REF | Types::OBJECT)) | Types::RC;
                            self.set_types(orig, ssa_op.op1_def, wl)?;
                            self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                        }
                    }
                    Opcode::AssignStaticPropOp => {}
                    _ => {
                        if result.is_some() && tmp.has(Types::RC1) {
                            tmp |= Types::RCN;
                        }
                        self.set_types(tmp, ssa_op.op1_def, wl)?;
                    }
                }
                if result.is_some() {
                    let mut class = None;
                    match op.opcode {
                        Opcode::AssignDimOp => {
                            // An append past the last key, a key of an
                            // array or object, or a scalar not made an
                            // array: each returns null.
                            if op.op2_type == OperandKind::Unused
                                || t2.has(Types::ARRAY | Types::OBJECT)
                            {
                                tmp |= Types::NULL;
                            }
                            if t1.has(
                                Types::ANY.without(
                                    Types::NULL | Types::FALSE | Types::STRING | Types::ARRAY,
                                ),
                            ) {
                                tmp |= Types::NULL;
                            }
                            if tmp.has(Types::REF) {
                                // A typed reference may convert.
                                tmp |= Types::ANY;
                            }
                        }
                        Opcode::AssignObjOp | Opcode::AssignStaticPropOp => {
                            if prop.is_some() {
                                let (pt, pc) = Self::prop_type(prop);
                                class = pc;
                                if (pt & (Types::LONG | Types::DOUBLE)) == Types::LONG
                                    && (tmp & (Types::LONG | Types::DOUBLE)) == Types::DOUBLE
                                {
                                    tmp |= Types::LONG;
                                    tmp = tmp.without(Types::DOUBLE);
                                } else if (pt & (Types::LONG | Types::DOUBLE | Types::STRING))
                                    == Types::STRING
                                    && tmp.has(Types::LONG | Types::DOUBLE)
                                {
                                    tmp |= Types::STRING;
                                    tmp = tmp.without(Types::LONG | Types::DOUBLE);
                                }
                                tmp &= pt;
                            } else {
                                tmp |= Types::LONG | Types::STRING;
                            }
                        }
                        _ => {
                            if tmp.has(Types::REF) {
                                tmp |= Types::ANY;
                            }
                        }
                    }
                    tmp = tmp.without(Types::REF);
                    self.set_types(tmp, result, wl)?;
                    if class.is_some() {
                        self.set_class(class, true, result, wl);
                    }
                }
            }
            Opcode::PreInc | Opcode::PreDec => {
                let mut tmp = Types::NONE;
                if t1.has(Types::RC) {
                    tmp |= Types::RC1;
                    if result.is_some() {
                        tmp |= Types::RCN;
                    }
                }
                tmp |= self.stepped(op.opcode, t1, ssa_op.op1_use, false);
                if result.is_some() {
                    self.set_types(tmp, result, wl)?;
                }
                if ssa_op.op1_def.is_some() {
                    if t1.has(Types::REF) {
                        tmp |= Types::REF;
                    }
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                }
            }
            Opcode::PostInc | Opcode::PostDec => {
                if result.is_some() {
                    let mut tmp = Types::NONE;
                    if t1.has(Types::RC) {
                        tmp |= Types::RC;
                    }
                    tmp |= t1.without(Types::UNDEF | Types::REF | Types::RCN);
                    if t1.has(Types::UNDEF) {
                        tmp |= Types::NULL;
                    }
                    self.set_types(tmp, result, wl)?;
                }
                let mut tmp = Types::NONE;
                if t1.has(Types::REF) {
                    tmp |= Types::REF;
                }
                if t1.has(Types::RC) {
                    tmp |= Types::RC1;
                }
                tmp |= self.stepped(op.opcode, t1, ssa_op.op1_use, true);
                if ssa_op.op1_def.is_some() {
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                }
            }
            Opcode::AssignDim => {
                let value = data_types(self);
                if op.op1_type == OperandKind::Cv {
                    let mut tmp = assign_dim_result(t1, t2, value, op.op2_type);
                    tmp |= self.var_types(ssa_op.op1_def)
                        & (Types::PACKED | Types::NUMERIC_HASH | Types::STRING_HASH);
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
                if result.is_some() {
                    let mut tmp = Types::NONE;
                    if t1.has(Types::STRING) {
                        tmp |= Types::STRING | Types::NULL;
                    }
                    if t1.has(Types::OBJECT) {
                        tmp |= Types::ANY | Types::ARRAY_ANY;
                    }
                    if t1.has(Types::ARRAY | Types::FALSE | Types::NULL | Types::UNDEF) {
                        tmp |= value & (Types::ANY | Types::ARRAY_ANY);
                        if value.has(Types::UNDEF) {
                            tmp |= Types::NULL;
                        }
                        if t1.has(Types::OF_REF) {
                            // A typed reference may convert a scalar.
                            tmp |= Types::NULL
                                | Types::BOOL
                                | Types::LONG
                                | Types::DOUBLE
                                | Types::STRING;
                        }
                    }
                    if t1.has(Types::TRUE | Types::LONG | Types::DOUBLE | Types::RESOURCE) {
                        tmp |= Types::NULL;
                    }
                    tmp |= Types::RC;
                    self.set_types(tmp, result, wl)?;
                }
                self.data_written(at, true, wl)?;
            }
            Opcode::AssignObj => {
                if op.op1_type == OperandKind::Cv {
                    let class = self.var_info(ssa_op.op1_use).and_then(|i| i.class);
                    let add_rc = t1.has(Types::OBJECT | Types::REF)
                        && class.is_none_or(|c| {
                            let shape = self.table.object(c.class);
                            shape.set || !shape.standard || c.instanceof
                        });
                    let mut tmp = t1 & (Types::REF | Types::OBJECT | Types::RC);
                    if add_rc {
                        tmp |= Types::RC;
                    }
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
                if result.is_some() {
                    let (tmp, class) = Self::prop_type(self.prop_info(&op, &ssa_op));
                    self.set_types(tmp, result, wl)?;
                    if class.is_some() {
                        self.set_class(class, true, result, wl);
                    }
                }
                self.data_written(at, false, wl)?;
            }
            Opcode::AssignStaticProp => {
                if result.is_some() {
                    self.set_types(Types::UNKNOWN, result, wl)?;
                }
                self.data_written(at, false, wl)?;
            }
            Opcode::PreIncObj | Opcode::PreDecObj | Opcode::PostIncObj | Opcode::PostDecObj => {
                if op.op1_type == OperandKind::Cv {
                    let tmp = (t1 & (Types::REF | Types::OBJECT)) | Types::RC;
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
                if result.is_some() {
                    self.set_types(Types::UNKNOWN, result, wl)?;
                }
            }
            Opcode::Assign => {
                if ssa_op.op2_def.is_some() {
                    let mut tmp = t2;
                    if tmp.has(Types::RC1) {
                        tmp |= Types::RCN;
                    }
                    self.set_types(tmp, ssa_op.op2_def, wl)?;
                }
                let mut tmp = t2.without(Types::UNDEF | Types::REF | Types::RC);
                if t2.has(Types::UNDEF) {
                    tmp |= Types::NULL;
                }
                if t1.has(Types::REF) {
                    tmp |= Types::REF;
                }
                if t2.has(Types::REF) {
                    tmp |= Types::RC;
                } else if matches!(op.op2_type, OperandKind::Tmp | OperandKind::Var) {
                    tmp |= t2 & Types::RC;
                } else if t2.has(Types::RC) {
                    tmp |= Types::RCN;
                }
                if op.out_type != OperandKind::Unused && tmp.has(Types::RC1) {
                    tmp |= Types::RCN;
                }
                if ssa_op.op1_def.is_some() {
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op2_use, ssa_op.op1_def, wl);
                }
                if result.is_some() {
                    if tmp.has(Types::REF) {
                        // A typed reference may convert a scalar.
                        tmp = tmp.without(Types::REF);
                        tmp |= Types::NULL
                            | Types::BOOL
                            | Types::LONG
                            | Types::DOUBLE
                            | Types::STRING
                            | Types::RC;
                    }
                    if (tmp & Types::RC) == Types::RCN && !self.read_next(at, result) {
                        // The count may drop before a later reader.
                        tmp |= Types::RC1;
                    }
                    self.set_types(tmp, result, wl)?;
                    self.copy_class(ssa_op.op2_use, result, wl);
                }
            }
            Opcode::AssignRef => {
                if op.op2_type == OperandKind::Cv {
                    let mut tmp = (Types::REF | t2).without(Types::UNDEF | Types::RC);
                    if t2.has(Types::UNDEF) {
                        tmp |= Types::NULL;
                    }
                    self.set_types(tmp, ssa_op.op2_def, wl)?;
                }
                let mut tmp =
                    if op.op2_type == OperandKind::Var && op.extended_value == RETURNS_FUNCTION {
                        (Types::REF | Types::RC | t2).without(Types::UNDEF)
                    } else {
                        (Types::REF | t2).without(Types::UNDEF | Types::RC)
                    };
                if t2.has(Types::UNDEF) {
                    tmp |= Types::NULL;
                }
                self.set_types(tmp, ssa_op.op1_def, wl)?;
                if result.is_some() {
                    self.set_types(tmp, result, wl)?;
                }
            }
            Opcode::AssignObjRef => {
                if op.op1_type == OperandKind::Cv && ssa_op.op1_def.is_some() {
                    let mut tmp = t1;
                    if tmp.has(Types::OBJECT) {
                        tmp |= Types::RC;
                    }
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
                let t2 = data_types(self);
                let data_kind = data.map_or(OperandKind::Unused, |d| d.op1_type);
                let mut tmp =
                    if data_kind == OperandKind::Var && op.extended_value & RETURNS_FUNCTION != 0 {
                        (Types::REF | Types::RC | t2).without(Types::UNDEF)
                    } else {
                        (Types::REF | t2).without(Types::UNDEF | Types::RC)
                    };
                if t2.has(Types::UNDEF) {
                    tmp |= Types::NULL;
                }
                if result.is_some() {
                    self.set_types(tmp, result, wl)?;
                }
                if data_kind == OperandKind::Cv {
                    let mut tmp = (Types::REF | t2).without(Types::UNDEF | Types::RC);
                    if t2.has(Types::UNDEF) {
                        tmp |= Types::NULL;
                    }
                    self.set_types(tmp, data_op.op1_def, wl)?;
                }
            }
            Opcode::AssignStaticPropRef => {
                if result.is_some() {
                    self.set_types(Types::REF, result, wl)?;
                }
                if data.is_some_and(|d| d.op1_type == OperandKind::Cv) {
                    self.set_types(Types::REF, data_op.op1_def, wl)?;
                }
            }
            Opcode::BindGlobal => {
                self.set_types(
                    Types::REF | Types::ANY | Types::ARRAY_ANY,
                    ssa_op.op1_def,
                    wl,
                )?;
            }
            Opcode::BindStatic => {
                let mut tmp = Types::ANY | Types::ARRAY_ANY;
                tmp |= if op.extended_value & BIND_REF != 0 {
                    Types::REF
                } else {
                    Types::RC
                };
                if op.extended_value & BIND_IMPLICIT != 0 {
                    tmp |= Types::UNDEF;
                }
                self.set_types(tmp, ssa_op.op1_def, wl)?;
            }
            Opcode::BindInitStaticOrJmp => {
                let tmp = Types::UNDEF | Types::ANY | Types::ARRAY_ANY | Types::REF;
                self.set_types(tmp, ssa_op.op1_def, wl)?;
            }
            Opcode::SendVar => {
                if ssa_op.op1_def.is_some() {
                    let mut tmp = t1;
                    if t1.has(Types::RC1 | Types::REF) {
                        tmp |= Types::RCN;
                    }
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
            }
            Opcode::BindLexical => {
                if ssa_op.op2_def.is_some() {
                    let tmp = if op.extended_value & BIND_REF != 0 {
                        t2 | Types::REF
                    } else {
                        Self::shared(t2)
                    };
                    self.set_types(tmp, ssa_op.op2_def, wl)?;
                    self.copy_class(ssa_op.op2_use, ssa_op.op2_def, wl);
                }
            }
            Opcode::Yield => {
                if ssa_op.op1_def.is_some() {
                    let tmp = if self.function.decl.is_some_and(|d| d.returns_by_ref) {
                        t1 | Types::REF
                    } else {
                        Self::shared(t1)
                    };
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
                if result.is_some() {
                    self.set_types(Types::UNKNOWN, result, wl)?;
                }
            }
            Opcode::SendVarEx | Opcode::SendFuncArg => {
                if ssa_op.op1_def.is_some() {
                    let tmp = (t1 & Types::UNDEF) | Types::REF_ANY;
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                }
            }
            Opcode::SendRef => {
                if ssa_op.op1_def.is_some() {
                    self.set_types(Types::REF_ANY, ssa_op.op1_def, wl)?;
                }
            }
            Opcode::SendUnpack => {
                if ssa_op.op1_def.is_some() {
                    let mut tmp = t1;
                    if t1.has(Types::ARRAY) {
                        tmp |= Types::RC;
                        if t1.has(Types::OF_ANY) {
                            // It may take references into the array.
                            tmp |= Types::OF_ANY | Types::OF_REF;
                        }
                    }
                    if t1.has(Types::OBJECT) {
                        tmp |= Types::RC;
                    }
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                }
            }
            Opcode::FastConcat | Opcode::RopeInit | Opcode::RopeAdd | Opcode::RopeEnd => {
                self.set_types(Types::STRING | Types::RC, result, wl)?;
            }
            Opcode::Recv | Opcode::RecvInit | Opcode::RecvVariadic => {
                let param = self.parameter(op.op1);
                let (mut tmp, mut class) =
                    of_declared(param.and_then(|p| p.ty.as_ref()), self.table);
                if param.is_some_and(|p| p.by_ref || p.prefer_ref) {
                    tmp |= Types::REF;
                    class = None;
                }
                if op.opcode == Opcode::RecvVariadic {
                    let elements = if tmp.has(Types::REF) {
                        Types::OF_ANY | Types::OF_REF
                    } else {
                        tmp.as_elements()
                    };
                    tmp = Types::RC | Types::ARRAY | Types::KEY_ANY | elements;
                    class = None;
                }
                self.set_types(tmp, result, wl)?;
                self.set_class(class, class.is_some(), result, wl);
            }
            Opcode::DeclareAnonClass => {
                self.set_types(Types::CLASS, result, wl)?;
                if let Some(class) = self
                    .str_literal(op.op1)
                    .and_then(|key| crate::class_entry(self.function, self.table, key))
                {
                    self.set_class(Some(class), false, result, wl);
                }
            }
            Opcode::FetchClass => {
                self.set_types(Types::CLASS, result, wl)?;
                match op.op2_type {
                    OperandKind::Unused => {
                        let scope = self.table.scope();
                        let class = match op.op1 & FETCH_CLASS_MASK {
                            FETCH_CLASS_SELF => scope,
                            FETCH_CLASS_PARENT => scope.and_then(|s| self.table.parent(s)),
                            FETCH_CLASS_STATIC => scope.filter(|s| self.table.is_final(*s)),
                            _ => None,
                        };
                        self.set_class(class, false, result, wl);
                    }
                    OperandKind::Const => {
                        let class = self.class_named(op.op2);
                        self.set_class(class, false, result, wl);
                    }
                    _ => self.copy_class(ssa_op.op2_use, result, wl),
                }
            }
            Opcode::New => {
                let mut tmp = Types::RC | Types::OBJECT;
                let mut class = self.class_of_op1(&op);
                if class.is_some() {
                    self.set_class(class, false, result, wl);
                } else if let Some(from) = self.var_info(ssa_op.op1_use).and_then(|i| i.class)
                    && t1.has(Types::CLASS)
                {
                    self.set_class(Some(from.class), from.instanceof, result, wl);
                    if self
                        .var_info(result)
                        .and_then(|i| i.class)
                        .is_some_and(|c| !c.instanceof)
                    {
                        class = Some(from.class);
                    }
                } else {
                    self.set_class(None, false, result, wl);
                }
                // An object no constructor sees cannot escape.
                if let Some(class) = class {
                    let shape = self.table.object(class);
                    if !shape.constructor && shape.standard {
                        tmp = tmp.without(Types::RCN);
                    }
                }
                self.set_types(tmp, result, wl)?;
            }
            Opcode::Clone => {
                self.set_types(Types::RC | Types::OBJECT, result, wl)?;
                self.copy_class(ssa_op.op1_use, result, wl);
            }
            Opcode::InitArray | Opcode::AddArrayElement => {
                if ssa_op.op1_def.is_some() {
                    let by_ref = op.extended_value & ELEMENT_REF != 0;
                    let tmp = if by_ref || (t1 & (Types::REF | Types::RC)) == Types::REF {
                        let mut tmp = (Types::REF | t1).without(Types::UNDEF | Types::RC);
                        if t1.has(Types::UNDEF) {
                            tmp |= Types::NULL;
                        }
                        tmp
                    } else if t1.has(Types::REF) {
                        Types::RC | Types::REF | t1
                    } else if t1.has(Types::RC1) {
                        t1 | Types::RCN
                    } else {
                        t1
                    };
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                }
                if result.is_some() {
                    let arr_type = if op.opcode == Opcode::InitArray {
                        Types::NONE
                    } else {
                        res_use
                    };
                    let mut tmp = Types::RC1 | Types::ARRAY | arr_type;
                    if op.opcode == Opcode::InitArray && op.op1_type == OperandKind::Unused {
                        tmp |= Types::EMPTY;
                    }
                    let keyed = Types::UNDEF
                        | Types::NULL
                        | Types::BOOL
                        | Types::LONG
                        | Types::DOUBLE
                        | Types::RESOURCE
                        | Types::STRING;
                    if op.op1_type != OperandKind::Unused
                        && (op.op2_type == OperandKind::Unused || t2.has(keyed))
                    {
                        tmp |= assign_dim_array_result(arr_type, t2, t1, op.op2_type);
                        if op.extended_value & ELEMENT_REF != 0 {
                            tmp |= Types::OF_ANY | Types::OF_REF;
                        }
                    }
                    self.set_types(tmp, result, wl)?;
                }
            }
            Opcode::AddArrayUnpack => {
                let mut tmp = self.var_types(ssa_op.out_use);
                tmp |= t1 & Types::ARRAY_ANY;
                if t1.has(Types::OBJECT) {
                    tmp |= Types::KEY_ANY | Types::OF_ANY;
                }
                self.set_types(tmp, result, wl)?;
            }
            Opcode::UnsetCv => {
                let mut tmp = Types::UNDEF;
                if self.function.name().is_none() {
                    // In a file's own code nothing is known.
                    tmp |= Types::REF;
                }
                self.set_types(tmp, ssa_op.op1_def, wl)?;
            }
            Opcode::UnsetDim | Opcode::UnsetObj => {
                if ssa_op.op1_def.is_some() {
                    self.set_types(t1, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
            }
            Opcode::FeResetR | Opcode::FeResetRw => {
                let rw = op.opcode == Opcode::FeResetRw;
                if ssa_op.op1_def.is_some() {
                    let mut tmp = t1;
                    if rw {
                        tmp |= Types::REF;
                    } else if t1.has(Types::RC1) {
                        tmp |= Types::RCN;
                    }
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                    self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                }
                let mut tmp = if rw {
                    Types::REF | (t1 & (Types::ARRAY | Types::OBJECT))
                } else {
                    Types::RC | (t1 & (Types::ARRAY | Types::OBJECT | Types::ARRAY_ANY))
                };
                // An input foreach cannot walk leaves the result undefined.
                if (t1 & (Types::ANY | Types::UNDEF)).has(!(Types::ARRAY | Types::OBJECT)) {
                    tmp |= Types::UNDEF;
                }
                self.set_types(tmp, result, wl)?;
                self.copy_class(ssa_op.op1_use, result, wl);
            }
            Opcode::FeFetchR | Opcode::FeFetchRw => {
                let rw = op.opcode == Opcode::FeFetchRw;
                let cv = op.op2_type == OperandKind::Cv;
                let mut tmp = Types::NONE;
                if cv {
                    tmp = t2 & Types::REF;
                }
                if t1.has(Types::OBJECT) {
                    if rw {
                        tmp |= Types::REF | Types::ANY | Types::ARRAY_ANY;
                    } else {
                        tmp |= Types::RCN | Types::ANY | Types::ARRAY_ANY;
                        if !cv {
                            tmp |= Types::REF;
                        }
                    }
                }
                if t1.has(Types::ARRAY) {
                    if rw {
                        tmp |= Types::REF | Types::RCN | Types::ANY | Types::ARRAY_ANY;
                    } else {
                        tmp |= t1.elements();
                        if tmp.has(Types::ARRAY) {
                            tmp |= Types::ARRAY_ANY;
                        }
                        if t1.has(Types::OF_REF) {
                            tmp |= Types::RC;
                            if !cv {
                                tmp |= Types::REF;
                            }
                        } else if tmp.has(rc_str) {
                            tmp |= Types::RC;
                        }
                    }
                }
                self.set_types(tmp, ssa_op.op2_def, wl)?;
                if result.is_some() {
                    let mut tmp = if ssa_op.out_use.is_some() {
                        res_use
                    } else {
                        Types::NONE
                    };
                    if t1.has(Types::OBJECT) {
                        tmp |= Types::UNKNOWN;
                    }
                    if t1.has(Types::ARRAY) {
                        if t1.has(Types::KEY_LONG) {
                            tmp |= Types::LONG;
                        }
                        if t1.has(Types::KEY_STRING) {
                            tmp |= Types::STRING | Types::RCN;
                        }
                    }
                    self.set_types(tmp, result, wl)?;
                }
            }
            Opcode::FetchDimR
            | Opcode::FetchDimIs
            | Opcode::FetchDimRw
            | Opcode::FetchDimW
            | Opcode::FetchDimUnset
            | Opcode::FetchDimFuncArg
            | Opcode::FetchListR
            | Opcode::FetchListW => {
                if ssa_op.op1_def.is_some() {
                    match self.fetched_container(at, &op, &ssa_op, t1, t2) {
                        Some(tmp) => {
                            self.set_types(tmp, ssa_op.op1_def, wl)?;
                            self.copy_class(ssa_op.op1_use, ssa_op.op1_def, wl);
                        }
                        // An invalid key type.
                        None => return Ok(()),
                    }
                }
                // `FETCH_LIST_R` of a string reads as `FETCH_R` of null.
                let container = if op.opcode == Opcode::FetchListR {
                    t1.without(Types::STRING) | Types::NULL
                } else {
                    t1
                };
                let write = !matches!(
                    op.opcode,
                    Opcode::FetchDimR | Opcode::FetchDimIs | Opcode::FetchListR
                );
                let mut tmp = element_type(
                    container,
                    op.op1_type,
                    write,
                    op.op2_type == OperandKind::Unused,
                );
                if op.opcode == Opcode::FetchDimFuncArg
                    && t1.has(Types::TRUE | Types::LONG | Types::DOUBLE | Types::RESOURCE)
                {
                    tmp |= Types::NULL;
                }
                if op.opcode == Opcode::FetchDimIs && t1.has(Types::STRING) {
                    tmp |= Types::NULL;
                }
                if (tmp & Types::RC) == Types::RCN
                    && op.out_type == OperandKind::Tmp
                    && !self.read_next(at, result)
                {
                    tmp |= Types::RC1;
                }
                self.set_types(tmp, result, wl)?;
            }
            Opcode::FetchThis => {
                self.set_class(self.table.scope(), true, result, wl);
                self.set_types(Types::RCN | Types::OBJECT, result, wl)?;
            }
            Opcode::FetchObjR
            | Opcode::FetchObjIs
            | Opcode::FetchObjRw
            | Opcode::FetchObjW
            | Opcode::FetchObjUnset
            | Opcode::FetchObjFuncArg => {
                if result.is_some() {
                    let mut tmp = Types::NONE;
                    let mut class = None;
                    let unused = op.op1_type == OperandKind::Unused;
                    if !unused && (t1 & (Types::ANY | Types::UNDEF)).has(!Types::OBJECT) {
                        tmp |= Types::NULL;
                    }
                    if unused || t1.has(Types::OBJECT) {
                        let (pt, pc) = Self::prop_type(self.prop_info(&op, &ssa_op));
                        tmp |= pt;
                        class = pc;
                        if !matches!(op.opcode, Opcode::FetchObjR | Opcode::FetchObjIs) {
                            tmp |= Types::REF | Types::INDIRECT;
                            if op.extended_value & FETCH_OBJ_FLAGS == FETCH_DIM_WRITE {
                                tmp |= Types::UNDEF;
                            }
                            class = None;
                        } else if !matches!(op.op1_type, OperandKind::Var | OperandKind::Tmp)
                            || !t1.has(Types::RC1)
                        {
                            let of = if unused {
                                self.table.scope()
                            } else {
                                self.var_info(ssa_op.op1_use)
                                    .and_then(|i| i.class)
                                    .filter(|c| !c.instanceof)
                                    .map(|c| c.class)
                            };
                            // An unset property falls back to `__get`.
                            let plain = of.is_some_and(|c| {
                                let shape = self.table.object(c);
                                shape.standard && !shape.get
                            });
                            if plain && !self.may_be_separated(at, &ssa_op) {
                                tmp = tmp.without(Types::RC1);
                            }
                            if op.opcode == Opcode::FetchObjIs {
                                // An uninitialized typed property is null here.
                                tmp |= Types::NULL;
                            }
                        }
                    }
                    self.set_types(tmp, result, wl)?;
                    if class.is_some() {
                        self.set_class(class, true, result, wl);
                    }
                }
            }
            Opcode::FetchStaticPropR
            | Opcode::FetchStaticPropIs
            | Opcode::FetchStaticPropRw
            | Opcode::FetchStaticPropW
            | Opcode::FetchStaticPropUnset
            | Opcode::FetchStaticPropFuncArg => {
                let (mut tmp, mut class) = Self::prop_type(self.static_prop_info(&op));
                if matches!(
                    op.opcode,
                    Opcode::FetchStaticPropR | Opcode::FetchStaticPropIs
                ) {
                    if !self.may_be_separated(at, &ssa_op) {
                        tmp = tmp.without(Types::RC1);
                    }
                    if op.opcode == Opcode::FetchStaticPropIs {
                        tmp |= Types::UNDEF;
                    }
                } else {
                    tmp |= Types::REF | Types::INDIRECT;
                    if op.extended_value & FETCH_OBJ_FLAGS == FETCH_DIM_WRITE {
                        tmp |= Types::UNDEF;
                    }
                    class = None;
                }
                self.set_types(tmp, result, wl)?;
                if class.is_some() {
                    self.set_class(class, true, result, wl);
                }
            }
            Opcode::FramelessIcall1 | Opcode::FramelessIcall2 | Opcode::FramelessIcall3 => {
                if ssa_op.op1_def.is_some() {
                    let tmp = Self::shared_rc1(self.var_types(ssa_op.op1_use));
                    self.set_types(tmp, ssa_op.op1_def, wl)?;
                }
                if ssa_op.op2_def.is_some() {
                    let tmp = Self::shared_rc1(self.var_types(ssa_op.op2_use));
                    self.set_types(tmp, ssa_op.op2_def, wl)?;
                }
                if op.opcode == Opcode::FramelessIcall3 && data_op.op1_def.is_some() {
                    let tmp = Self::shared_rc1(self.var_types(data_op.op1_use));
                    self.set_types(tmp, data_op.op1_def, wl)?;
                }
                self.call_result(at, &op, result, wl)?;
            }
            Opcode::FramelessIcall0
            | Opcode::DoFcall
            | Opcode::DoIcall
            | Opcode::DoUcall
            | Opcode::DoFcallByName => {
                self.call_result(at, &op, result, wl)?;
            }
            Opcode::CallableConvert | Opcode::DeclareLambdaFunction => {
                self.set_types(Types::OBJECT | Types::RC, result, wl)?;
                self.set_class(self.table.engine_class(b"closure"), false, result, wl);
            }
            Opcode::FetchConstant => {
                self.set_types(
                    Types::RC | Types::ANY | Types::KEY_ANY | Types::OF_ANY,
                    result,
                    wl,
                )?;
            }
            Opcode::FetchClassConstant => match self.class_const_type(&op) {
                None => {
                    self.set_types(
                        Types::RC | Types::ANY | Types::KEY_ANY | Types::OF_ANY,
                        result,
                        wl,
                    )?;
                }
                Some((tmp, class)) => {
                    self.set_types(tmp, result, wl)?;
                    if class.is_some() {
                        self.set_class(class, true, result, wl);
                    }
                }
            },
            Opcode::Strlen | Opcode::Count | Opcode::FuncNumArgs | Opcode::Spaceship => {
                self.set_types(Types::LONG, result, wl)?;
            }
            Opcode::FuncGetArgs => {
                let tmp = Types::RC | Types::ARRAY | Types::EMPTY | Types::PACKED | Types::OF_ANY;
                self.set_types(tmp, result, wl)?;
            }
            Opcode::GetClass | Opcode::GetCalledClass | Opcode::FetchClassName => {
                self.set_types(Types::STRING | Types::RCN, result, wl)?;
            }
            Opcode::GetType => self.set_types(Types::STRING | Types::RC, result, wl)?,
            Opcode::TypeCheck => {
                let expected = Types(op.extended_value);
                if t1.has(Types::UNDEF) {
                    t1 |= Types::NULL;
                }
                let mut tmp = Types::NONE;
                if t1.has(expected) {
                    tmp |= Types::TRUE;
                    if (t1 & expected).has(Types::RESOURCE) {
                        tmp |= Types::FALSE;
                    }
                }
                if t1.has(Types::ANY.without(expected)) {
                    tmp |= Types::FALSE;
                }
                self.set_types(tmp, result, wl)?;
            }
            Opcode::VerifyReturnType => {
                let (tmp, class) = if t1.has(Types::REF) {
                    (t1, None)
                } else {
                    let declared = self.function.decl.and_then(|d| d.return_type.as_ref());
                    let (mut tmp, class) = of_declared(declared, self.table);
                    if tmp.has(Types::NULL) && op.op1_type == OperandKind::Cv {
                        tmp |= Types::UNDEF;
                    }
                    tmp |= t1 & Types::INDIRECT;
                    if t1.without(tmp).is_empty() {
                        tmp &= t1;
                    }
                    (tmp, class)
                };
                let to = if matches!(
                    op.op1_type,
                    OperandKind::Tmp | OperandKind::Var | OperandKind::Cv
                ) {
                    ssa_op.op1_def
                } else {
                    result
                };
                self.set_types(tmp, to, wl)?;
                self.set_class(class, class.is_some(), to, wl);
            }
            Opcode::MakeRef => {
                self.set_types(Types::REF_ANY, result, wl)?;
                if ssa_op.op1_def.is_some() {
                    self.set_types(Types::REF_ANY, ssa_op.op1_def, wl)?;
                }
            }
            Opcode::PreDecStaticProp
            | Opcode::PreIncStaticProp
            | Opcode::PostDecStaticProp
            | Opcode::PostIncStaticProp => {
                if result.is_some() {
                    let (tmp, _) = Self::prop_type(self.static_prop_info(&op));
                    // An engine object may yield anything.
                    if tmp.has(Types::OBJECT) {
                        self.unknown_result(&op, result, wl)?;
                    } else {
                        let mut tmp = tmp
                            & (Types::LONG
                                | Types::DOUBLE
                                | Types::STRING
                                | Types::BOOL
                                | Types::NULL);
                        if tmp.has(Types::STRING) {
                            tmp |= Types::RC;
                        }
                        self.set_types(tmp, result, wl)?;
                    }
                }
            }
            Opcode::FetchGlobals => {
                self.set_types(Types::ARRAY | Types::ARRAY_ANY | Types::RC, result, wl)?;
            }
            _ => {
                if ssa_op.op1_def.is_some() {
                    self.set_types(Types::REF_ANY, ssa_op.op1_def, wl)?;
                }
                self.unknown_result(&op, result, wl)?;
            }
        }
        Ok(())
    }

    /// A count shared once copied: `t` with its counts made `RCN`.
    fn shared(t: Types) -> Types {
        let mut tmp = t.without(Types::RC);
        if t.has(Types::RC) {
            tmp |= Types::RCN;
        }
        tmp
    }

    /// `t`, shared where it is counted once.
    fn shared_rc1(t: Types) -> Types {
        if t.has(Types::RC1) { t | Types::RCN } else { t }
    }

    /// The types an increment or decrement leaves, past its counts: an
    /// integer that may pass its bound becomes a double, by the range of
    /// the variable read.
    fn stepped(&self, opcode: Opcode, t1: Types, used: Option<SsaVar>, post: bool) -> Types {
        let inc = matches!(opcode, Opcode::PreInc | Opcode::PostInc);
        let mut tmp = Types::NONE;
        if (t1 & (Types::ANY | Types::UNDEF)) == Types::LONG {
            let range = self.var_info(used).and_then(|i| i.range);
            let overflows = range.is_none_or(|r| {
                if inc {
                    r.overflow || r.max == i64::MAX
                } else {
                    r.underflow || r.min == i64::MIN
                }
            });
            tmp |= if overflows {
                Types::LONG | Types::DOUBLE
            } else {
                Types::LONG
            };
        } else {
            if t1.has(Types::UNDEF | Types::NULL) {
                tmp |= if inc { Types::LONG } else { Types::NULL };
            }
            if t1.has(Types::LONG) {
                tmp |= Types::LONG | Types::DOUBLE;
            }
            if t1.has(Types::DOUBLE) {
                tmp |= Types::DOUBLE;
            }
            if t1.has(Types::STRING) {
                tmp |= Types::STRING | Types::LONG | Types::DOUBLE;
            }
            tmp |= t1 & (Types::BOOL | Types::OBJECT);
            if post {
                tmp |= t1 & (Types::RESOURCE | Types::ARRAY | Types::ARRAY_ANY);
            }
        }
        tmp
    }

    /// The `OP_DATA` after an assignment at `at` writes its CV: the value
    /// read, shared where it is counted once (or anything a reference
    /// holds, for an `ASSIGN_DIM`).
    fn data_written(&mut self, at: usize, dim: bool, wl: &mut Bits) -> Result<(), Narrowed> {
        let data_op = self.op(at + 1);
        if data_op.op1_def.is_none() {
            return Ok(());
        }
        let mut tmp = self.op1_types(at + 1);
        if (!dim || tmp.has(Types::ANY | Types::REF)) && tmp.has(Types::RC1) {
            tmp |= Types::RCN;
        }
        self.set_types(tmp, data_op.op1_def, wl)
    }

    /// The container a `FETCH_DIM_*` or `FETCH_LIST_*` leaves in its CV:
    /// a write makes an array of a null, with the keys and elements the
    /// next opline's use of the fetched element implies; none where the
    /// key cannot index an array, which leaves the CV as it is.
    fn fetched_container(
        &self,
        at: usize,
        op: &Opline,
        ssa_op: &SsaOp,
        t1: Types,
        t2: Types,
    ) -> Option<Types> {
        let mut tmp = t1.without(Types::RC);
        let mut keys = Types::NONE;
        let writes = matches!(
            op.opcode,
            Opcode::FetchDimW | Opcode::FetchDimRw | Opcode::FetchDimFuncArg | Opcode::FetchListW
        );
        if writes {
            if t1.has(Types::UNDEF | Types::NULL | Types::FALSE) {
                if op.opcode != Opcode::FetchDimFuncArg {
                    tmp = tmp.without(Types::UNDEF | Types::NULL | Types::FALSE);
                }
                tmp |= Types::ARRAY | Types::RC1;
            }
            if t1.has(Types::STRING | Types::ARRAY) {
                tmp |= Types::RC1;
                if op.opcode == Opcode::FetchDimFuncArg {
                    tmp |= t1 & Types::RCN;
                }
            }
            if t1.has(Types::OBJECT | Types::RESOURCE) {
                tmp |= t1 & Types::RC;
            }
            if op.op2_type == OperandKind::Unused {
                if t1.has(Types::UNDEF | Types::NULL | Types::FALSE) {
                    keys |= Types::PACKED;
                }
                if t1.has(Types::ARRAY) {
                    keys |= if t1.hash_only() {
                        Types::NUMERIC_HASH
                    } else {
                        Types::KEY_LONG
                    };
                }
            } else {
                let unpacked = t1.has(Types::UNDEF | Types::NULL | Types::FALSE);
                let long = |keys: &mut Types| {
                    if unpacked {
                        *keys |= Types::PACKED;
                    }
                    if t1.has(Types::ARRAY) {
                        *keys |= if t1.hash_only() {
                            Types::NUMERIC_HASH
                        } else {
                            Types::KEY_LONG
                        };
                    }
                };
                if t2
                    .has(Types::LONG | Types::FALSE | Types::TRUE | Types::RESOURCE | Types::DOUBLE)
                {
                    long(&mut keys);
                }
                if t2.has(Types::STRING) {
                    keys |= Types::KEY_STRING;
                    if op.op2_type != OperandKind::Const {
                        long(&mut keys);
                    }
                }
                if t2.has(Types::UNDEF | Types::NULL) {
                    keys |= Types::KEY_STRING;
                }
            }
        } else if op.opcode == Opcode::FetchDimUnset {
            if t1.has(Types::ARRAY) {
                tmp |= Types::RC1;
            }
            if t1.has(Types::OBJECT | Types::RESOURCE) {
                tmp |= t1 & Types::RC;
            }
        }
        if keys.has(Types::KEY_LONG | Types::KEY_STRING) && writes {
            let result = ssa_op.out_def;
            let mut next = result
                .and_then(|v| self.ssa.var(v))
                .and_then(|v| v.use_chain);
            if next.is_none() {
                tmp |= keys | Types::ARRAY | Types::OF_NULL;
            }
            while let Some(j) = next {
                if j as usize != at + 1 {
                    // The use must be the next opline.
                    tmp |= keys | Types::OF_ANY | Types::OF_REF;
                    break;
                }
                match self.body.oplines.get(j as usize).map(|o| o.opcode) {
                    Some(
                        Opcode::FetchDimW
                        | Opcode::FetchDimRw
                        | Opcode::FetchDimFuncArg
                        | Opcode::FetchListW
                        | Opcode::AssignDim
                        | Opcode::AssignDimOp,
                    ) => tmp |= keys | Types::ARRAY | Types::OF_ARRAY,
                    Some(
                        Opcode::SendVarEx
                        | Opcode::SendFuncArg
                        | Opcode::SendVarNoRef
                        | Opcode::SendVarNoRefEx
                        | Opcode::SendRef
                        | Opcode::AssignRef
                        | Opcode::Yield
                        | Opcode::InitArray
                        | Opcode::AddArrayElement
                        | Opcode::ReturnByRef
                        | Opcode::VerifyReturnType
                        | Opcode::MakeRef
                        | Opcode::FeResetRw,
                    ) => tmp |= keys | Types::OF_ANY | Types::OF_REF,
                    Some(Opcode::PreInc | Opcode::PreDec | Opcode::PostInc | Opcode::PostDec) => {
                        if tmp.has(Types::OF_LONG) {
                            // May overflow.
                            tmp |= keys | Types::OF_DOUBLE;
                        } else if !tmp.has(Types::OF_LONG | Types::OF_DOUBLE) {
                            tmp |= keys | Types::OF_LONG | Types::OF_DOUBLE;
                        }
                    }
                    // An object write errors unless the element is one;
                    // the rest leave the container as it is.
                    _ => {}
                }
                next = result.and_then(|v| self.ssa.next_use(v, j));
                if next.is_some() {
                    tmp |= keys | Types::OF_ANY | Types::OF_REF;
                    break;
                }
            }
            if op.opcode != Opcode::FetchDimFuncArg {
                tmp = tmp.without(Types::EMPTY);
            }
        }
        let valid = !tmp.has(Types::ARRAY)
            || tmp.has(Types::KEY_ANY)
            || matches!(
                op.opcode,
                Opcode::FetchDimFuncArg
                    | Opcode::FetchDimR
                    | Opcode::FetchDimIs
                    | Opcode::FetchDimUnset
                    | Opcode::FetchListR
            );
        valid.then_some(tmp)
    }

    /// What a call returns, typed by what php knows of the callee the call map
    /// names at `at` (`zend_get_func_info`), and as an opline it knows
    /// nothing of where it names none.
    fn call_result(
        &mut self,
        at: usize,
        op: &Opline,
        result: Option<SsaVar>,
        wl: &mut Bits,
    ) -> Result<(), Narrowed> {
        if result.is_none() {
            return Ok(());
        }
        let Some(call) = self.calls.and_then(|calls| calls.at(at)) else {
            return self.unknown_result(op, result, wl);
        };
        let (tmp, class) = self.func_info(call);
        self.set_types(tmp, result, wl)?;
        if let Some(class) = class {
            self.set_class(Some(class.class), class.instanceof, result, wl);
        }
        Ok(())
    }

    /// `unknown_opcode`: a value php knows nothing of but its kind.
    fn unknown_result(
        &mut self,
        op: &Opline,
        result: Option<SsaVar>,
        wl: &mut Bits,
    ) -> Result<(), Narrowed> {
        if result.is_none() {
            return Ok(());
        }
        let mut tmp = Types::ANY | Types::ARRAY_ANY;
        match op.out_type {
            OperandKind::Tmp | OperandKind::Cv => tmp |= Types::RC,
            _ => {
                tmp |= Types::REF | Types::RC;
                if matches!(
                    op.opcode,
                    Opcode::FetchW
                        | Opcode::FetchRw
                        | Opcode::FetchFuncArg
                        | Opcode::FetchUnset
                        | Opcode::FetchDimW
                        | Opcode::FetchDimRw
                        | Opcode::FetchDimFuncArg
                        | Opcode::FetchDimUnset
                        | Opcode::FetchObjW
                        | Opcode::FetchObjRw
                        | Opcode::FetchObjFuncArg
                        | Opcode::FetchObjUnset
                        | Opcode::FetchStaticPropW
                        | Opcode::FetchStaticPropRw
                        | Opcode::FetchStaticPropFuncArg
                        | Opcode::FetchStaticPropUnset
                ) {
                    tmp |= Types::INDIRECT;
                }
            }
        }
        self.set_types(tmp, result, wl)
    }
}
