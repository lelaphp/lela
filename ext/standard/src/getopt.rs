//! The table `getopt()` hands `php_getopt`, read off its two arguments as
//! `Z/ext/standard/basic_functions.c` reads them: the short options'
//! string by `parse_opts` and each long option's spelling. The walk over
//! the program's `argv` is php's one `php_getopt`, the engine's, which
//! every host reads its own command line with (`php_ext::syntax::Getopt`).

use php_ext::syntax::{Need, Opt};

/// `parse_opts` (`Z/ext/standard/basic_functions.c:890`): the letters
/// and digits at the start of the string, each followed by `:` for a
/// value it needs or `::` for one it may take. php sizes the table by
/// every letter and digit in the string and fills only the leading ones;
/// the slots it leaves zeroed match nothing, so they are left out here.
pub(super) fn short_options(spec: &[u8]) -> Vec<Opt<'static>> {
    let mut opts = Vec::new();
    let mut at = 0;
    while let Some(&letter) = spec.get(at).filter(|c| c.is_ascii_alphanumeric()) {
        at += 1;
        let mut need = Need::Nothing;
        if spec.get(at) == Some(&b':') {
            need = Need::Required;
            at += 1;
            if spec.get(at) == Some(&b':') {
                need = Need::Optional;
                at += 1;
            }
        }
        opts.push(Opt {
            code: letter,
            need,
            name: None,
        });
    }
    opts
}

/// A long option as `getopt` reads its spelling: its name, with a
/// trailing `:` or `::` read off as the value it needs or may take. Its
/// row's code is php's `0`, which no letter is.
pub(super) fn long_option(spelled: &[u8]) -> (Vec<u8>, Need) {
    let mut name = spelled.to_vec();
    let mut need = Need::Nothing;
    if name.last() == Some(&b':') {
        need = Need::Required;
        name.pop();
        if name.last() == Some(&b':') {
            need = Need::Optional;
            name.pop();
        }
    }
    (name, need)
}
