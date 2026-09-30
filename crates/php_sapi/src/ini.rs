//! The line's `-d` entries as a host reads them before the engine does
//! (`Z/main/php_ini_builder.c`): split as `php_ini_builder_define` splits
//! them, and the few a host reads itself, php's CGI its `cgi.*` entries
//! and `arg_separator.input`, read as the engine will read them.

/// `php_ini_builder_define`'s split of one `-d`: `name=value` at the
/// first `=`, or `name=1` for a bare name. Its line, quoted or not, and what the INI
/// grammar reads from it are the engine's builder's.
#[must_use]
pub fn define(given: &str) -> (String, String) {
    match given.split_once('=') {
        Some((name, value)) => (name.to_owned(), value.to_owned()),
        None => (given.to_owned(), String::from("1")),
    }
}

/// An entry of the line, the last one of its name.
#[must_use]
pub fn entry<'a>(ini: &'a [(String, String)], name: &str) -> Option<&'a str> {
    ini.iter()
        .rev()
        .find(|(had, _)| had == name)
        .map(|(_, value)| value.as_str())
}

/// An entry read as `OnUpdateBool` reads it, `default` where the line
/// sets none.
#[must_use]
pub fn enabled(ini: &[(String, String)], name: &str, default: bool) -> bool {
    entry(ini, name).map_or(default, |value| {
        let value = value.trim();
        ["on", "yes", "true"]
            .iter()
            .any(|word| value.eq_ignore_ascii_case(word))
            || value.parse::<i64>().is_ok_and(|n| n != 0)
    })
}
