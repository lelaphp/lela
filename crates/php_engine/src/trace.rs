//! The witness runner's trace (harness §4.5 condition 2, revision 58):
//! the natives a request enters, recorded by the entry a `php_macros`
//! expansion builds where the request was asked to record them, and
//! appended to the file a host names. In every build, switched on where a
//! host builds its engine or a test serves a request, so the witness lane
//! serves a fence through the host's own binary (#2168).

use std::path::Path;

/// One native a traced request entered, named as the macro expansion
/// that built its entry names it (harness §4.5 condition 2, revision
/// 58): the crate it is declared in, `CARGO_CRATE_NAME`; the Rust type
/// whose `#[php_class]` member impl declares it, `None` for a
/// `#[php_function]`; and its Rust identifier, `r#` stripped. These are
/// the names `xtask witness` reads off the source, so a subject and what
/// a fence entered meet without a translation between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entered {
    pub krate: &'static str,
    pub owner: Option<&'static str>,
    pub name: &'static str,
}

impl Entered {
    /// The line a traced engine appends for it to the file a host named
    /// ([`EngineBuilder::trace_natives_to`](crate::EngineBuilder::trace_natives_to)):
    /// the crate, the owning type or nothing, and the identifier, a tab
    /// between each and a newline after. The names are Rust identifiers,
    /// so neither character is in one.
    #[must_use]
    pub(crate) fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\n",
            self.krate,
            self.owner.unwrap_or_default(),
            self.name
        )
    }
}

/// `trace` appended to `path` in one write, so a forked child's request
/// and its parent's land whole beside each other: each native once per
/// request ([`Request::entered`](crate::api::request::Request)). A file that cannot be
/// written loses the lines, and the harness that reads it reports the
/// natives as not entered.
pub(crate) fn append_trace(path: &Path, trace: &[Entered]) {
    use std::io::Write;
    let lines: String = trace.iter().map(Entered::line).collect();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        file.write_all(lines.as_bytes()).ok();
    }
}
