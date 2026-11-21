//! phpdbg's help (`Z/sapi/phpdbg/phpdbg_help.c`): its pages, written as
//! `pretty_print` formats them and through the pager. The overview is the
//! one page held here; the others are not yet.

use crate::out::{self, Colours, Paged};
use crate::session::Session;

/// The columns `pretty_print` wraps at: `phpdbg_get_terminal_width`'s
/// where stdout is no terminal.
const WIDTH: usize = 80;

/// `phpdbg_help_text`'s `overview!`, its `**` toggling bold and each
/// paragraph one line, for `pretty_print` to wrap.
const OVERVIEW: &str = "
**phpdbg** is a lightweight, powerful and easy to use debugging platform for PHP
It supports the following commands:

**Information**
  **list**      list PHP source
  **info**      displays information on the debug session
  **print**     show opcodes
  **frame**     select a stack frame and print a stack frame summary
  **generator** show active generators or select a generator frame
  **back**      shows the current backtrace
  **help**      provide help on a topic

**Starting and Stopping Execution**
  **exec**      set execution context
  **stdin**     set executing script from stdin
  **run**       attempt execution
  **step**      continue execution until other line is reached
  **continue**  continue execution
  **until**     continue execution up to the given location
  **next**      continue execution up to the given location and halt on the first line after it
  **finish**    continue up to end of the current execution frame
  **leave**     continue up to end of the current execution frame and halt after the calling instruction
  **break**     set a breakpoint at the specified target
  **watch**     set a watchpoint on $variable
  **clear**     clear one or all breakpoints
  **clean**     clean the execution environment

**Miscellaneous**
  **set**       set the phpdbg configuration
  **source**    execute a phpdbginit script
  **register**  register a phpdbginit function as a command alias
  **sh**        shell a command
  **ev**        evaluate some code
  **quit**      exit phpdbg

Type **help <command>** or (**help alias**) to get detailed help on any of the above commands, \
for example **help list** or **h l**.  Note that help will also match partial commands if unique \
(and list out options if not unique), so **help exp** will give help on the **export** command, \
but **help ex** will list the summary for **exec** and **export**.

Type **help aliases** to show a full alias list, including any registered phpdbginit functions
Type **help syntax** for a general introduction to the command syntax.
Type **help options** for a list of phpdbg command line options.
Type **help phpdbginit** to show how to customize the debugger environment.";

/// `get_help`: the page under `key`, none where the table holds no such
/// key.
fn page(key: &[u8]) -> Option<&'static str> {
    (key == b"overview!").then_some(OVERVIEW)
}

/// `pretty_print`: `text` with `**` toggling bold while colours are on,
/// `$P` the prompt, `\` taking the next byte as it is, and each line
/// wrapped at its last blank before [`WIDTH`] printable bytes, then a
/// newline, through the pager.
fn pretty_print(s: &mut Session, text: &str) -> Paged {
    let colours = s.out.colours() == Colours::On;
    let prompt = [s.prompt.as_slice(), b" "].concat();
    let mut new = Vec::with_capacity(text.len() + 1);
    let mut in_bold = false;
    let mut last_blank: Option<usize> = None;
    let mut last_blank_count = 0;
    let mut line_count = 0;
    let mut bytes = text.as_bytes().iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            b' ' => {
                last_blank = Some(new.len());
                last_blank_count = line_count;
                line_count += 1;
                new.push(b' ');
            }
            b'\n' => {
                last_blank = None;
                new.push(b'\n');
                last_blank_count = 0;
                line_count = 0;
            }
            b'*' if bytes.peek() == Some(&b'*') => {
                bytes.next();
                if colours {
                    in_bold = !in_bold;
                    new.extend_from_slice(if in_bold { b"\x1b[1m" } else { b"\x1b[0m" });
                }
            }
            b'$' if bytes.peek() == Some(&b'P') => {
                bytes.next();
                new.extend_from_slice(&prompt);
                line_count += s.prompt.len();
            }
            b'\\' => {
                new.extend(bytes.next());
                line_count += 1;
            }
            _ => {
                new.push(byte);
                line_count += 1;
            }
        }
        if line_count >= WIDTH
            && let Some(at) = last_blank.take()
        {
            new[at] = b'\n';
            line_count -= last_blank_count;
            last_blank_count = 0;
        }
    }
    new.push(b'\n');
    out::paged(s, &new)
}

/// `PHPDBG_COMMAND(help)` with no topic: the overview.
///
/// ```php
/// echo "x\n";
/// ```
/// ```request phpdbg
/// help
///
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt>%s
/// phpdbg is a lightweight, powerful and easy to use debugging platform for PHP
/// It supports the following commands:
///
/// Information
///   list      list PHP source
///   info      displays information on the debug session
///   print     show opcodes
///   frame     select a stack frame and print a stack frame summary
///   generator show active generators or select a generator frame
///   back      shows the current backtrace
///   help      provide help on a topic
///
/// Starting and Stopping Execution
///   exec      set execution context
///   stdin     set executing script from stdin
///   run       attempt execution
///   step      continue execution until other line is reached
///   continue  continue execution
///   until     continue execution up to the given location
///   next      continue execution up to the given location and halt on the first
/// line after it
///   finish    continue up to end of the current execution frame
///   leave     continue up to end of the current execution frame and halt after
/// the calling instruction
///   break     set a breakpoint at the specified target
///   watch     set a watchpoint on $variable
///   clear     clear one or all breakpoints
///   clean     clean the execution environment
///
/// Miscellaneous
///   set       set the phpdbg configuration
///   source    execute a phpdbginit script
///   register  register a phpdbginit function as a command alias
///   sh        shell a command
///   ev        evaluate some code
///   quit      exit phpdbg
///
/// Type help <command> or (help alias) to get detailed help on any of the above
/// commands, for example help list or h l.  Note that help will also match
/// %r\r%r---Type <return> to continue or q <return> to quit---%r\r%rpartial commands if unique (and list out options if not unique), so help exp
/// will give help on the export command, but help ex will list the summary for
/// exec and export.
///
/// Type help aliases to show a full alias list, including any registered
/// phpdbginit functions
/// Type help syntax for a general introduction to the command syntax.
/// Type help options for a list of phpdbg command line options.
/// Type help phpdbginit to show how to customize the debugger environment.
/// prompt>
/// ```
pub fn overview(s: &mut Session) -> Paged {
    pretty_print(s, OVERVIEW)
}

/// `phpdbg_do_help_cmd`, what `-h` writes: the page the line's word
/// names, or the overview where it names none; a word that names no page
/// writes the overview, then that none was found.
pub fn line(s: &mut Session, word: Option<&[u8]>) -> Paged {
    let Some(word) = word else {
        return overview(s);
    };
    if let Some(text) = page(word) {
        return pretty_print(s, text);
    }
    match overview(s) {
        Paged::Written => pretty_print(s, "\nrequested help page could not be found"),
        Paged::Ended => Paged::Ended,
    }
}
