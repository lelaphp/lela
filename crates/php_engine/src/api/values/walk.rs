//! The callback walks (#1980 candidate 3): a native that runs code of
//! the program for each row of a table, a callback, a magic method or a
//! handler a diagnostic reaches, walks the table here, and the walk owns
//! what it walks while that code runs. Each walk is one of php's
//! iteration modes, ported once:
//!
//! - [`held`] owns the table for the walk, php's `ZVAL_COPY` or
//!   `GC_ADDREF` of a table its body could otherwise give up, and
//!   [`held_guarded`] guards its recursion beside, php's
//!   `GC_PROTECT_RECURSION`. The default for any body that needs the
//!   request mutably: `extract`, `serialize`, the dumpers.
//!
//! Every walk has one shape. It takes the table, lends the body each
//! row's key and value for that step, owned by the walk, so a body
//! keeps a row past its step only by taking an owner of its own
//! ([`crate::api::values::Owner::take`]). The body answers a [`Step`]:
//! std's `ControlFlow` to go on or stop early with a value, and `Err`
//! for a throwable, which `?` hands on. The walk owns the loop and calls
//! the body, so what it took is given back on every way out, `?`
//! included, by the walk itself and never by a `Drop` (design §5.1:
//! nothing runs implicitly on drop).

use std::ops::ControlFlow;

use crate::api::raising::{Thrown, fatal};
use crate::api::request::Request;
use crate::api::values::{Key, Owner, PhpArray, Value};

/// What a walk's body answers for one row: `Continue` to go on to the
/// next, `Break` to end the walk there with what the walk answers, and
/// `Err`, what `?` hands on, a throwable that ends the walk as php's
/// loops end on `EG(exception)`.
pub type Step<B = ()> = Result<ControlFlow<B>, Thrown>;

/// How a walk that guards its recursion ended ([`held_guarded`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a walk inside the table already walked nothing, which the caller shows"]
pub enum Guarded<B> {
    /// The rows were walked: `Some` what the body broke with, `None`
    /// past the last row.
    Walked(Option<B>),
    /// A guarded walk is inside the table already, `GC_IS_RECURSIVE`,
    /// and nothing was walked: what a dumper prints `*RECURSION*` for.
    Recursive,
}

/// php's walk over a table it holds: the rows of the array `reached`
/// names in order, each lent to `body` with its key, a reference row
/// lent as the reference. `reached` is the value as the caller came by
/// it, a reference not yet followed. A table reached through a reference
/// is one the program shares, and code the body runs may assign over
/// the reference and give the table its last owner up, so the walk
/// holds one owner of it until it ends, as php's `extract` copies its
/// array first (`ZVAL_COPY`, "The array might be stored in a local
/// variable that will be overwritten") and `zend_array_sort_ex` holds
/// what it sorts; under that owner what writes the table separates it,
/// and the rows the walk lends stand. A table reached as an array is
/// held by what it was reached from, the call's argument or a row of a
/// table held in turn, which cannot be written without separating
/// first, so the walk takes nothing and the refcounts the body's code
/// sees are php's.
///
/// The answer is what `body` broke with, `None` where the walk went
/// past the last row; what reaches no array is the `InternalError` it is.
///
/// ```php
/// function f() { $k = 'kkkkk'; $v = 'vvvvv'; $z = 'ZZZZZ';
///   $arr = ['arr' => 1, $k . $k => $v . $v];
///   echo extract($arr), ','; $x = $z . $z; echo $arr, ',', $kkkkkkkkkk, "\n"; }
/// f();
/// ```
/// ```expect
/// 2,1,vvvvvvvvvv
/// ```
pub fn held<B>(
    req: &mut Request,
    reached: &Value,
    mut body: impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let table = table_of(req, reached)?;
    let owner = reached.as_php_ref().map(|_| Owner::take(req, table));
    let walked = rows(req, table, &mut body);
    if let Some(owner) = owner {
        owner.give_up(req);
    }
    walked
}

/// [`held`] guarding its recursion, as php's dumpers and encoders guard
/// a table they descend into: where a guarded walk is inside the table
/// already (`GC_IS_RECURSIVE`) nothing is walked and the answer says
/// so; otherwise the table is marked for the walk
/// (`GC_PROTECT_RECURSION`) and unmarked as it ends, on every way out.
/// An immutable table is marked by neither, as php's is not, since
/// nothing can make one hold itself.
///
/// ```php
/// $a = [1]; $a[] = &$a;
/// print_r($a); echo "\n";
/// ```
/// ```expect
/// Array
/// (
///     [0] => 1
///     [1] => Array
///  *RECURSION*
/// )
///
/// ```
pub fn held_guarded<B>(
    req: &mut Request,
    reached: &Value,
    mut body: impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Guarded<B>, Thrown> {
    let table = table_of(req, reached)?;
    if table.is_recursive() {
        return Ok(Guarded::Recursive);
    }
    let owner = reached.as_php_ref().map(|_| Owner::take(req, table));
    table.protect();
    let walked = rows(req, table, &mut body);
    table.unprotect();
    if let Some(owner) = owner {
        owner.give_up(req);
    }
    walked.map(Guarded::Walked)
}

/// The array `reached` names, a reference followed.
fn table_of(req: &mut Request, reached: &Value) -> Result<PhpArray, Thrown> {
    match reached.deref().as_array() {
        Some(table) => Ok(*table),
        None => Err(fatal(req, "InternalError: a held walk was handed no array")),
    }
}

/// The rows of `table` from its first bucket, each lent to `body`, until
/// the last or the first that breaks or throws. The table stands for
/// the walk and is read again at each step, so a row stands as long as
/// the bucket that holds it.
fn rows<B>(
    req: &mut Request,
    table: PhpArray,
    body: &mut impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let mut pos = 0;
    while let Some((at, key, value)) = table.live_from(pos) {
        pos = at.saturating_add(1);
        if let ControlFlow::Break(broke) = body(req, &key, &value)? {
            return Ok(Some(broke));
        }
    }
    Ok(None)
}
