//! `mysqli_object_handlers` and the property tables of `mysqli_prop.c`,
//! `mysqli_driver.c` and `mysqli_warning.c`: one table of handlers over
//! each class's [`Props`], as php copies one table five times and changes
//! `free_obj` alone. A name the class's entries answer is read through its
//! read function, written only where it has a write function, tested by
//! reading it quietly, and shown by `var_dump` in the table's order; every
//! other name is the standard handlers'.

use std::marker::PhantomData;

use php_ext::abi::{
    CloneObj, DebugInfo, ObjectHandlers, PropertyPlace, PropertyRead, PropertyTest, Std,
};
use php_ext::coerce::truthy;
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::class_name;

/// One class's `mysqli_property_entry` rows and `free_obj`.
pub(crate) trait Props: 'static {
    /// The names, in the table's order.
    const NAMES: &'static [&'static str];

    /// The read function of `name`: `Some` for php's `SUCCESS`, `None`
    /// for a `FAILURE` that, where not `quiet`, threw.
    fn read(
        req: &mut Request,
        object: PhpObject,
        name: &str,
        quiet: bool,
    ) -> Result<Option<Value>, Thrown>;

    /// The names with a write function, which only `report_mode` has.
    const WRITABLE: &'static [&'static str] = &[];

    /// The write function of `name`, served once the standard handler has
    /// held the value to the declared type and stored it, as
    /// `mysqli_write_property` verifies a copy before `write_func`.
    fn wrote(req: &mut Request, object: PhpObject, name: &str) -> Result<(), Thrown> {
        let _ = (req, object, name);
        Ok(())
    }

    /// `free_obj` past `zend_object_std_dtor`.
    fn free(req: &mut Request, object: PhpObject);
}

/// `mysqli_object_handlers` with the class's entries.
pub(crate) struct Handlers<P>(PhantomData<fn() -> P>);

impl<P> Handlers<P> {
    pub(crate) const fn new() -> Handlers<P> {
        Handlers(PhantomData)
    }
}

fn handled<P: Props>(name: &[u8]) -> Option<&'static str> {
    P::NAMES.iter().copied().find(|n| n.as_bytes() == name)
}

impl<P: Props> ObjectHandlers for Handlers<P> {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        P::free(req, object);
        Std.free_obj(req, object);
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    /// `mysqli_read_property`: what the read function answers, `null`
    /// where it fails.
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        match handled::<P>(name) {
            Some(name) => {
                Ok(P::read(req, object, name, read == PropertyRead::Silent)?.unwrap_or_default())
            }
            None => Std.read_property(req, object, name, read),
        }
    }

    /// `mysqli_write_property`: a read-only name refused, `report_mode`
    /// held to its declared `int` first.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        let Some(handled) = handled::<P>(name.as_bytes()) else {
            return Std.write_property(req, object, name, value);
        };
        if P::WRITABLE.contains(&handled) {
            Std.write_property(req, object, name, value)?;
            return P::wrote(req, object, handled);
        }
        req.release(Value::string(name));
        req.release(value);
        let class = class_name(req, object.class());
        let text = format!("Cannot write read-only property {class}::${handled}");
        Err(throw(req, "Error", &text))
    }

    fn write_overloaded(&self) -> bool {
        true
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        match handled::<P>(name) {
            Some(_) => Ok(PropertyPlace::Handlers),
            None => Std.get_property_ptr_ptr(req, object, name),
        }
    }

    /// `mysqli_object_has_property`.
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let Some(name) = handled::<P>(name) else {
            return Std.has_property(req, object, name, test);
        };
        if test == PropertyTest::Exists {
            return Ok(true);
        }
        let Some(value) = P::read(req, object, name, true)? else {
            return Ok(false);
        };
        let answer = match test {
            PropertyTest::NotEmpty => truthy(req, &value),
            _ => !matches!(value.as_ref(), ValueRef::Null),
        };
        req.release(value);
        Ok(answer)
    }

    /// `mysqli_object_get_debug_info`: every name of the table that reads,
    /// in the table's order.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        let mut table = PhpArray::new(req)?;
        for name in P::NAMES {
            if let Some(value) = P::read(req, object, name, true)? {
                let key = Key::Str(PhpString::new(req, name.as_bytes())?);
                table.set(req, key, value)?;
            }
        }
        Ok(Some(DebugInfo::from(table)))
    }
}

/// `CHECK_STATUS`'s refusal where not quiet: "Property access is not
/// allowed yet".
pub(crate) fn not_yet(req: &mut Request, quiet: bool) -> Result<Option<Value>, Thrown> {
    if quiet {
        return Ok(None);
    }
    Err(throw(req, "Error", "Property access is not allowed yet"))
}

/// `MYSQLI_GET_*`'s refusal of a closed object where not quiet.
pub(crate) fn gone(
    req: &mut Request,
    object: PhpObject,
    quiet: bool,
) -> Result<Option<Value>, Thrown> {
    if quiet {
        return Ok(None);
    }
    Err(crate::closed(req, object))
}

/// The `errno`, `sqlstate` and `error` rows of an error list, as
/// `link_error_list_read` and `mysqli_error_list` build them.
pub(crate) fn error_list(
    req: &mut Request,
    list: &[(u32, [u8; 5], Vec<u8>)],
) -> Result<Value, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (errno, sqlstate, error) in list {
        let mut row = PhpArray::new(req)?;
        let state_end = sqlstate.iter().position(|b| *b == 0).unwrap_or(5);
        for (key, value) in [
            ("errno", Value::int(i64::from(*errno))),
            ("sqlstate", crate::string(req, &sqlstate[..state_end])?),
            ("error", crate::string(req, error)?),
        ] {
            let key = Key::Str(PhpString::new(req, key.as_bytes())?);
            row.set(req, key, value)?;
        }
        out.append(req, Value::array(row))?;
    }
    Ok(Value::array(out))
}
