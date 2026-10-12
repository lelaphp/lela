//! Source listings (`Z/sapi/phpdbg/phpdbg_list.c`): a file's lines as
//! phpdbg split them when it compiled the file, each numbered to five
//! places, the current one marked, and the `zend_compile_file` link that
//! keeps them as each file compiles.

use std::os::unix::ffi::OsStrExt as _;

use php_ext::abi::{Beneath, Compiled, chain};
use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::out;
use crate::read;
use crate::session::{self, Session, Source};

/// The source of `file`: the code `stdin` read where that is the file,
/// and the bytes a file was compiled from where the request compiled it;
/// none for a file not compiled, of which php has no information.
fn source<'s>(session: &'s Session, file: &[u8]) -> Option<&'s [u8]> {
    if file == b"Standard input code" {
        return session.stdin_code.as_deref();
    }
    let kept = session.sources.iter().find(|kept| kept.file == file)?;
    Some(&kept.bytes)
}

/// `phpdbg_compile_file` (`Z/sapi/phpdbg/phpdbg_list.c:229-275`), the
/// debugger's `zend_compile_file`: the file's bytes copied before the
/// ones beneath compile it, as `zend_stream_fixup` reads them, and where
/// they compiled it, kept with its oplines among the session's sources
/// under the compiled file's name, in place of what an earlier compile of
/// it kept, and the breakpoints pending on its name resolved
/// (`phpdbg_resolve_pending_file_break`). So an included file is listed
/// from the bytes it was compiled from, once it has compiled and before
/// its code runs, and not before:
///
/// ```php
/// file_put_contents(__DIR__ . '/inc.php', "<?php\necho \"inc\\n\";\n");
/// include __DIR__ . '/inc.php';
/// file_put_contents(__DIR__ . '/inc.php', "<?php\n// changed\n");
/// echo "end\n";
/// unlink(__DIR__ . '/inc.php');
/// ```
/// ```request phpdbg
/// b 3
/// b inc.php:2
/// r
/// list inc.php:2
/// c
/// list inc.php:2
/// b witness.php:5
/// c
/// list inc.php:2
/// c
/// list inc.php:2
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// prompt> [Pending breakpoint #1 added at inc.php:2]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003: include __DIR__ . '/inc.php';
///  00004: file_put_contents(__DIR__ . '/inc.php', "<?php\n// changed\n");
///  00005: echo "end\n";
/// prompt> [Could not find information about included file...]
/// prompt> [Breakpoint #1 at %SCRATCH%/inc.php:2, hits: 1]
/// >00002: echo "inc\n";
///  00003:%s
/// prompt>  00001: <?php
///  00002: echo "inc\n";
/// prompt> [Breakpoint #2 added at %SCRATCH%/witness.php:5]
/// prompt> inc
/// [Breakpoint #2 at %SCRATCH%/witness.php:5, hits: 1]
/// >00005: echo "end\n";
///  00006: unlink(__DIR__ . '/inc.php');
/// prompt>  00001: <?php
///  00002: echo "inc\n";
/// prompt> end
/// [Script ended normally]
/// prompt> [Could not find information about included file...]
/// prompt>
/// ```
pub fn compile_file(
    req: &mut Request,
    beneath: Beneath<'_, chain::CompileFile>,
) -> Result<Option<Compiled>, Thrown> {
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(beneath.path()));
    let copied = std::fs::read(path).ok();
    let compiled = beneath.serve(req)?;
    if let (Some(bytes), Some(code)) = (copied, compiled.as_ref().and_then(Compiled::op_array)) {
        let file = code.filename().to_vec();
        let ops = read::ops_of(&code);
        session::with(|s| {
            s.sources.retain(|kept| kept.file != file);
            s.breaks.resolve_pending(&file);
            s.sources.push(Source { file, ops, bytes });
        });
    }
    Ok(compiled)
}

/// `phpdbg_list_file`: `count` lines from `offset`, the zero-based
/// index of the first, the line numbered `highlight` marked with `>`.
pub fn file(session: &Session, file: &[u8], count: i64, offset: i64, highlight: u32) {
    let Some(buf) = source(session, file) else {
        out::error(b"Could not find information about included file...");
        return;
    };
    // `data->line[]`: where each line starts, and one past the end.
    let mut starts = vec![0usize];
    for (at, byte) in buf.iter().enumerate() {
        if *byte == b'\n' {
            starts.push(at + 1);
        }
    }
    let lines = starts.len();
    starts.push(buf.len());
    let (mut count, mut offset) = (count, offset);
    if offset < 0 {
        count += offset;
        offset = 0;
    }
    let last = usize::try_from(offset.saturating_add(count).max(0))
        .unwrap_or(0)
        .min(lines);
    let mut line = usize::try_from(offset).unwrap_or(0);
    while line < last {
        let start = starts[line];
        line += 1;
        let end = starts[line];
        let text = &buf[start..end];
        let mark = if highlight != 0 && u32::try_from(line).ok() == Some(highlight) {
            b'>'
        } else {
            b' '
        };
        let mut shown = vec![mark];
        shown.extend_from_slice(format!("{line:05}: ").as_bytes());
        shown.extend_from_slice(text);
        out::write(&shown);
        if text.last() != Some(&b'\n') || text.is_empty() {
            out::out(b"\n");
        }
    }
}

/// `list_code`: three lines from the current one, it marked.
pub fn code(session: &Session, file: &[u8], line: u32) {
    self::file(session, file, 3, i64::from(line) - 1, line);
}
