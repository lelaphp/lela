//! `php_getopt` (`Z/main/getopt.c`), which php's CLI and CGI hosts read
//! their command lines with, each over its own `OPTIONS` table.

use std::ffi::OsString;

/// One row of a host's `OPTIONS`: the code `php_getopt` returns, a
/// letter or php's number for a long switch, whether it takes a value
/// (`1`), may (`2`) or does not (`0`), and its long name.
pub(crate) struct Opt {
    pub(crate) code: u8,
    pub(crate) need: u8,
    pub(crate) name: &'static str,
}

pub(crate) const fn opt(code: u8, need: u8, name: &'static str) -> Opt {
    Opt { code, need, name }
}

/// `php_opt_error`'s three reasons, `main/getopt.c:20-24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptError {
    Colon,
    NotFound,
    NoArgument,
}

/// Where `php_getopt` stopped and why: the argument and the character
/// its line names, and the reason.
pub(crate) type Stopped = (usize, usize, OptError);

/// `php_getopt` (`Z/main/getopt.c:55-198`) over `argv[1..]`, with the
/// state it keeps in statics, `optchr` and `dash`, kept here: `optchr`
/// is where in a clustered word the next letter is and outlives the
/// word, which is why the line php prints for a switch it does not know
/// counts from the previous cluster's end. Copied as written, since that
/// line is observable.
pub(crate) struct Getopt {
    options: &'static [Opt],
    pub(crate) optind: usize,
    optchr: usize,
    dash: bool,
}

/// One thing `php_getopt` returned: a switch with its value, or the end
/// of the switches.
pub(crate) enum Taken {
    Switch(u8, Option<Vec<u8>>),
    End,
}

impl Getopt {
    pub(crate) fn new(options: &'static [Opt]) -> Getopt {
        Getopt {
            options,
            optind: 0,
            optchr: 0,
            dash: false,
        }
    }

    /// The next switch, `php_getopt` with `arg_start` 2.
    pub(crate) fn next(&mut self, argv: &[Vec<u8>]) -> Result<Taken, Stopped> {
        let Some(word) = argv.get(self.optind) else {
            return Ok(Taken::End);
        };
        if !self.dash && (word.first() != Some(&b'-') || word.len() == 1) {
            return Ok(Taken::End);
        }
        let (index, arg_start) = if word.starts_with(b"--") {
            if word.len() == 2 {
                self.optind += 1;
                return Ok(Taken::End);
            }
            self.long(word)?
        } else {
            if !self.dash {
                self.dash = true;
                self.optchr = 1;
            }
            if word.get(self.optchr) == Some(&b':') {
                self.dash = false;
                self.optind += 1;
                return Err((self.optind - 1, self.optchr, OptError::Colon));
            }
            (self.short(word)?, 1 + self.optchr)
        };
        let row = &self.options[index];
        if row.need != 0 {
            return self.value(argv, word, row, arg_start);
        }
        if arg_start >= 2 && !word.starts_with(b"--") {
            if word.get(self.optchr + 1).is_none() {
                self.dash = false;
                self.optind += 1;
            } else {
                self.optchr += 1;
            }
        } else {
            self.optind += 1;
        }
        Ok(Taken::Switch(row.code, None))
    }

    /// A `--name` or `--name=value` word: its row and where its value
    /// starts; a name no row has is `OPTERRARG` at the `optchr` left
    /// over from the previous word.
    fn long(&mut self, word: &[u8]) -> Result<(usize, usize), Stopped> {
        let mut arg_start = 2;
        let mut arg_end = word.len() - 1;
        if let Some(at) = word[arg_start..arg_end].iter().position(|&b| b == b'=') {
            arg_end = at;
            arg_start += 1;
        } else {
            arg_end -= 1;
        }
        let found = self.options.iter().position(|row| {
            row.code == b'-'
                || (row.name.len() == arg_end && word[2..].starts_with(row.name.as_bytes()))
        });
        let index = match found {
            Some(index) if self.options[index].code != b'-' => index,
            _ => {
                self.optind += 1;
                return Err((self.optind - 1, self.optchr, OptError::NoArgument));
            }
        };
        self.optchr = 0;
        self.dash = false;
        Ok((index, arg_start + self.options[index].name.len()))
    }

    /// The letter at `optchr` of a `-xyz` word: its row, or `OPTERRNF`
    /// with the cluster advanced past it.
    fn short(&mut self, word: &[u8]) -> Result<usize, Stopped> {
        let letter = word.get(self.optchr).copied();
        if let Some(index) = self
            .options
            .iter()
            .position(|row| row.code != b'-' && Some(row.code) == letter)
        {
            return Ok(index);
        }
        let at = (self.optind, self.optchr);
        if word.get(self.optchr + 1).is_none() {
            self.dash = false;
            self.optind += 1;
        } else {
            self.optchr += 1;
        }
        Err((at.0, at.1, OptError::NotFound))
    }

    /// The value of a switch that takes one: attached, after `=`, or the
    /// next word; a switch that may take one (`--ini`) takes only an
    /// attached one.
    fn value(
        &mut self,
        argv: &[Vec<u8>],
        word: &[u8],
        row: &Opt,
        arg_start: usize,
    ) -> Result<Taken, Stopped> {
        self.dash = false;
        self.optind += 1;
        let value = match word.get(arg_start) {
            None if self.optind == argv.len() => {
                if row.need == 1 {
                    return Err((self.optind - 1, self.optchr, OptError::NoArgument));
                }
                None
            }
            None if row.need == 1 => {
                self.optind += 1;
                argv.get(self.optind - 1).cloned()
            }
            None => None,
            Some(b'=') => Some(word[arg_start + 1..].to_vec()),
            Some(_) => Some(word[arg_start..].to_vec()),
        };
        Ok(Taken::Switch(row.code, value))
    }
}

/// `php_opt_error`'s line on stderr (`Z/main/getopt.c:28-50`), the
/// argument counted from the program's name and the character from
/// one; the host's usage follows it on stdout.
pub(crate) fn getopt_line(argv: &[Vec<u8>], argument: usize, at: usize, error: OptError) -> String {
    let letter = argv
        .get(argument)
        .and_then(|word| word.get(at))
        .map_or(String::new(), |&b| {
            String::from_utf8_lossy(&[b]).into_owned()
        });
    let reason = match error {
        OptError::Colon => String::from(": in flags"),
        OptError::NotFound => format!("option not found {letter}"),
        OptError::NoArgument => format!("no argument for option {letter}"),
    };
    format!(
        "Error in argument {}, char {}: {reason}\n",
        argument + 1,
        at + 1
    )
}

/// Bytes of the command line as a word the host hands on.
pub(crate) fn os(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}
