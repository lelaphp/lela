//! A debug build's handler tables served as the extension that declared
//! them (#2113). #1993 tags each allocation with the extension whose
//! native or lifecycle hook is served; an extension's object handlers
//! are called from every site that reaches an object's table, so what
//! one allocates carried the origin of whatever was served, `Core`
//! where a handler of the dispatch loop called it, and a leak from an
//! extension class's `free_obj` or `read_property` was named as Core's.
//! Each table an extension declares is therefore laid in the engine's
//! handler rows inside a [`Served`], which serves every slot that serves
//! code as that extension ([`Request::serve_for`]), and an iterator a
//! table hands out inside a [`ServedIterator`]. The slots that answer a
//! function rather than serve one are served where the function is
//! called ([`Request::serve_as_table`]). A release build lays the
//! extension's table down as it is.

use std::cmp::Ordering;
use std::sync::{Mutex, PoisonError};

use crate::api::abi::{
    ClassIterator, ClassUnserialize, CloneObj, CloneObjWith, CoreTable, CurrentPlace, DebugInfo,
    DimensionRead, DtorObj, GcTable, IteratorElement, ObjectHandlers, PropertiesFor, PropertyPlace,
    PropertyPurpose, PropertyRead, PropertyTest, Refusals,
};
use crate::api::call::Function;
use crate::api::coerce::BinaryOp;
use crate::api::raising::Thrown;
use crate::api::request::{ExtensionId, Request};
use crate::api::values::{
    ClassId, ClosureInfo, Owner, PhpObject, PhpString, Place, TypeTag, Value, walk,
};

/// One extension's handler table and the extension that declared it.
pub(crate) struct Served {
    table: &'static dyn ObjectHandlers,
    extension: ExtensionId,
}

/// `table` as `extension` declared it, laid in the handler rows: the one
/// [`Served`] for the pair, made the first time any engine of the process
/// links it, so engines built one after another share it and the
/// process holds one per table and extension, never one per build.
pub(crate) fn served(
    table: &'static dyn ObjectHandlers,
    extension: ExtensionId,
) -> &'static dyn ObjectHandlers {
    static MADE: Mutex<Vec<&'static Served>> = Mutex::new(Vec::new());
    let mut made = MADE.lock().unwrap_or_else(PoisonError::into_inner);
    let held = made
        .iter()
        .find(|held| held.extension == extension && std::ptr::addr_eq(held.table, table));
    if let Some(held) = held {
        return *held;
    }
    let laid: &'static Served = Box::leak(Box::new(Served { table, extension }));
    made.push(laid);
    laid
}

/// The slots that take the request, each served as the table's
/// extension.
macro_rules! serve {
    ($(fn $slot:ident(&self, req: &mut Request $(, $arg:ident: $ty:ty)*) $(-> $answer:ty)?;)*) => {
        $(
            fn $slot(&self, req: &mut Request $(, $arg: $ty)*) $(-> $answer)? {
                let table = self.table;
                req.serve_for(self.extension, |req| table.$slot(req $(, $arg)*))
            }
        )*
    };
}

// Every slot is forwarded, so a slot added to the trait and missed here
// is a lint and never `Std`'s default standing in for the extension's.
#[deny(clippy::missing_trait_methods)]
impl ObjectHandlers for Served {
    serve! {
        fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown>;
        fn free_obj(&self, req: &mut Request, object: PhpObject);
        fn read_property(&self, req: &mut Request, object: PhpObject, name: &[u8], read: PropertyRead) -> Result<Owner<Value>, Thrown>;
        fn write_property(&self, req: &mut Request, object: PhpObject, name: Owner<PhpString>, value: Owner<Value>) -> Result<(), Thrown>;
        fn read_dimension(&self, req: &mut Request, object: PhpObject, offset: Option<&Value>, read: DimensionRead) -> Result<Owner<Value>, Thrown>;
        fn dimension_place(&self, req: &mut Request, object: PhpObject, offset: Option<&Value>, fetch: DimensionRead) -> Result<Option<Place>, Thrown>;
        fn write_dimension(&self, req: &mut Request, object: PhpObject, offset: Option<&Value>, value: Owner<Value>) -> Result<(), Thrown>;
        fn get_property_ptr_ptr(&self, req: &mut Request, object: PhpObject, name: &[u8]) -> Result<PropertyPlace, Thrown>;
        fn has_property(&self, req: &mut Request, object: PhpObject, name: &[u8], test: PropertyTest) -> Result<bool, Thrown>;
        fn unset_property(&self, req: &mut Request, object: PhpObject, name: &[u8]) -> Result<(), Thrown>;
        fn has_dimension(&self, req: &mut Request, object: PhpObject, offset: &Value, not_empty: bool) -> Result<bool, Thrown>;
        fn unset_dimension(&self, req: &mut Request, object: PhpObject, offset: &Value) -> Result<(), Thrown>;
        fn get_properties(&self, req: &mut Request, object: PhpObject) -> Result<walk::Table, Thrown>;
        fn get_method(&self, req: &mut Request, object: PhpObject, lowered: &[u8]) -> Result<Option<Function>, Thrown>;
        fn get_static_method(&self, req: &mut Request, class: ClassId, lowered: &[u8], written: &[u8]) -> Result<Option<Function>, Thrown>;
        fn get_method_as(&self, req: &mut Request, object: PhpObject, lowered: &[u8], written: &[u8]) -> Result<Option<Function>, Thrown>;
        fn method_receiver(&self, req: &mut Request, object: PhpObject) -> Option<PhpObject>;
        fn get_constructor(&self, req: &mut Request, object: PhpObject) -> Result<Option<Function>, Thrown>;
        fn get_class_name(&self, req: &mut Request, object: PhpObject) -> Vec<u8>;
        fn cast_object(&self, req: &mut Request, object: PhpObject, to: TypeTag) -> Result<Option<Owner<Value>>, Thrown>;
        fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown>;
        fn get_debug_info(&self, req: &mut Request, object: PhpObject) -> Result<Option<DebugInfo>, Thrown>;
        fn get_closure(&self, req: &mut Request, object: PhpObject) -> Option<ClosureInfo>;
        fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) -> GcTable;
        fn do_operation(&self, req: &mut Request, op: BinaryOp, left: &Value, right: Option<&Value>) -> Result<Option<Owner<Value>>, Thrown>;
        fn compare(&self, req: &mut Request, left: &Value, right: &Value) -> Result<Option<Ordering>, Thrown>;
        fn iterator_element(&self, req: &mut Request, object: PhpObject, index: usize) -> Result<Option<IteratorElement>, Thrown>;
        fn get_properties_for(&self, req: &mut Request, object: PhpObject, purpose: PropertyPurpose) -> Result<PropertiesFor, Thrown>;
    }

    fn refusals(&self) -> Refusals {
        self.table.refusals()
    }

    fn core_table(&self) -> CoreTable {
        self.table.core_table()
    }

    fn dtor_obj(&self) -> DtorObj {
        self.table.dtor_obj()
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        self.table.clone_obj()
    }

    fn clone_obj_with(&self) -> Option<CloneObjWith> {
        self.table.clone_obj_with()
    }

    fn current_place(&self) -> Option<CurrentPlace> {
        self.table.current_place()
    }

    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        let (table, extension) = (self.table, self.extension);
        let made = req.serve_for(extension, |req| table.get_iterator(req, object, by_ref))?;
        Ok(made.map(|iterator| {
            Box::new(ServedIterator {
                iterator,
                extension,
            }) as Box<dyn ClassIterator>
        }))
    }

    fn own_iterator(&self) -> bool {
        self.table.own_iterator()
    }

    fn iterator_size(&self) -> usize {
        self.table.iterator_size()
    }

    fn properties_overloaded(&self) -> bool {
        self.table.properties_overloaded()
    }

    fn write_overloaded(&self) -> bool {
        self.table.write_overloaded()
    }

    fn unserialize(&self) -> Option<ClassUnserialize> {
        self.table.unserialize()
    }
}

/// An iterator an extension's table handed out, each step served as
/// that extension.
struct ServedIterator {
    iterator: Box<dyn ClassIterator>,
    extension: ExtensionId,
}

/// The steps, each served as the iterator's extension.
macro_rules! step {
    ($(fn $step:ident(&mut self, req: &mut Request, object: PhpObject) $(-> $answer:ty)?;)*) => {
        $(
            fn $step(&mut self, req: &mut Request, object: PhpObject) $(-> $answer)? {
                let iterator = &mut self.iterator;
                req.serve_for(self.extension, |req| iterator.$step(req, object))
            }
        )*
    };
}

#[deny(clippy::missing_trait_methods)]
impl ClassIterator for ServedIterator {
    step! {
        fn made(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown>;
        fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown>;
        fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown>;
        fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown>;
        fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown>;
        fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown>;
        fn release(&mut self, req: &mut Request, object: PhpObject);
    }
}
