//! lib.rs's tests, one file per concern, over the fixtures in
//! `fixture`.

use super::*;
use fixture::*;
use php_link::Names;
use php_unit::decl::{ConstExpr, ConstantName, Name, NameKind, Parameter, Type, Visibility};
use php_unit::{AutoGlobal, OpArray, Opcode, OperandKind, OplineIdx, frameless_table};

mod arrays;
mod branches;
mod calls;
mod classes;
mod constants;
mod dimensions;
mod fixture;
mod functions;
mod interpolation;
mod loops;
mod objects;
mod operators;
mod properties;
mod special_functions;
mod try_finally;
mod units;
mod variables;
