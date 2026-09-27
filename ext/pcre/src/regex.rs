//! The compiled-pattern face this crate exports beyond `extension()`
//! (design §4.10): what `php_pcre.h` declares `PHPAPI` for the two
//! extensions that include it, ext/spl's `RegexIterator` and ext/filter's
//! `FILTER_VALIDATE_REGEXP`, so neither compiles a pattern twice. A
//! [`Regex`] is a pattern out of the engine's cache, and each method is
//! the `php_pcre_*_impl` of the same name, leaving `preg_last_error` as
//! php's does.

use std::sync::Arc;

use php_ext::abi::IntoReturn;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value};

use crate::ffi::Subject;
use crate::matching::{Scratch, match_impl};
use crate::pattern::{Pattern, compiled};
use crate::replace::{limit_of, replace_impl};
use crate::split::split_impl;

/// A compiled pattern, php's `pcre_cache_entry *`: shared with the
/// engine's cache and every request that compiled the same string.
#[derive(Clone)]
pub struct Regex(Arc<Pattern>);

impl Regex {
    /// `pcre_get_compiled_regex_cache`: the pattern php's syntax
    /// writes, or `None` with php's warning raised and
    /// `PREG_INTERNAL_ERROR` set.
    pub fn compile(req: &mut Request, pattern: &[u8]) -> Option<Regex> {
        compiled(req, pattern).map(Regex)
    }

    /// `capture_count`, the groups the pattern declares.
    #[must_use]
    pub fn capture_count(&self) -> usize {
        self.0.groups - 1
    }

    /// `pcre2_match` from offset zero with no options over
    /// `php_pcre_mctx()`, what `FILTER_VALIDATE_REGEXP` and
    /// `RegexIterator::MATCH` ask: whether it matched, an error being no
    /// match, and the error code left alone.
    pub fn is_match(&self, req: &mut Request, subject: &[u8]) -> bool {
        let Some(mut scratch) = Scratch::take(req, &self.0) else {
            return false;
        };
        let rc = scratch.run(&self.0, &mut Subject::new(subject), 0, 0, false);
        scratch.give_back(req);
        rc >= 0
    }

    /// `php_pcre_match_impl`: `preg_match` or, with `global`,
    /// `preg_match_all`, answering the count or `false` and the groups
    /// as `$matches` holds them.
    pub fn match_impl(
        &self,
        req: &mut Request,
        subject: PhpString,
        global: bool,
        flags: i64,
        offset: i64,
    ) -> Result<(Value, PhpArray), Thrown> {
        let (value, matches) = match_impl(req, &self.0, subject, true, global, flags, offset)?;
        let matches = match matches {
            Some(matches) => matches,
            None => PhpArray::new(req)?,
        };
        Ok((value.into_return(req)?, matches))
    }

    /// `php_pcre_split_impl`: what `preg_split` answers.
    pub fn split_impl(
        &self,
        req: &mut Request,
        subject: &[u8],
        limit: i64,
        flags: i64,
    ) -> Result<Value, Thrown> {
        split_impl(req, &self.0, subject, limit, flags)?.into_return(req)
    }

    /// `php_pcre_replace_impl`: what `preg_replace` makes of one
    /// subject, `None` for php's `NULL`, the replacements made added to
    /// `count`.
    pub fn replace_impl(
        &self,
        req: &mut Request,
        subject: PhpString,
        replacement: &[u8],
        limit: i64,
        count: &mut usize,
    ) -> Result<Option<PhpString>, Thrown> {
        replace_impl(req, &self.0, subject, replacement, limit_of(limit), count)
    }
}
