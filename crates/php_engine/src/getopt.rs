//! `php_getopt` (`Z/main/getopt.c`), which php keeps in `main/` for every
//! host's command line, the CLI's, php-cgi's, php-fpm's and phpdbg's, and
//! which `getopt()` reads a program's `argv` with: one switch at a time, a
//! letter from a cluster after `-` or a name after `--`, with its value
//! where its row wants one. The function's two statics, `optchr` and
//! `dash`, are a walker's fields, so each walk starts from nothing; one
//! walk is one host's pass over its line or one `getopt()` call.

/// How much a row wants a value: `need_param`'s 0, 1 and 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Nothing,
    Required,
    /// Taken only attached, `-xvalue` or `--name=value`, never from the
    /// next word.
    Optional,
}

/// One `opt_struct`: the code `php_getopt` answers for the row, a
/// letter a cluster names it by, or a number no letter is for a host's
/// long switch, or `0` for a `getopt()` long option; how much it wants a
/// value; and the name `--` names it by, where it has one. The table's end
/// is the slice's, where php's ends at a row whose code is `-`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opt<'a> {
    pub code: u8,
    pub need: Need,
    pub name: Option<&'a [u8]>,
}

/// `php_opt_error`'s three reasons (`Z/main/getopt.c:20-24`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptError {
    /// `OPTERRCOLON`: a `:` among a cluster's letters.
    Colon,
    /// `OPTERRNF`: a letter no row has.
    NotFound,
    /// `OPTERRARG`: a name no row has, or a value missing where one is
    /// required.
    NoArgument,
}

/// Where `php_getopt` refused and why: the index of the word in `argv`
/// and of the byte in it its error line names, and the reason
/// (`PHP_GETOPT_INVALID_ARG`, which `getopt()` skips and a host prints).
pub type Stopped = (usize, usize, OptError);

/// What one call of `php_getopt` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// A row found: its code, its index in the table, php's
    /// `php_optidx`, and the value it took, `*optarg`.
    Switch {
        code: u8,
        row: usize,
        value: Option<Vec<u8>>,
    },
    /// `EOF`: no switch is left before the words after them.
    End,
}

/// `php_getopt` over a table: `optind`, the word being read, which the
/// caller starts and reads back, and the statics `optchr` and `dash`.
/// `optchr`, where in a cluster the next letter is, outlives the word,
/// which is why the line a host prints for a name it does not know
/// counts from the previous cluster's end; copied as written, since that
/// line is observable.
#[derive(Debug, Clone)]
pub struct Getopt<'a> {
    opts: &'a [Opt<'a>],
    /// `*optind`: the index in `argv` of the word read next.
    pub optind: usize,
    optchr: usize,
    dash: bool,
}

impl<'a> Getopt<'a> {
    /// A walk over `opts` from the word at `optind`: php's hosts start at
    /// 1, past the program's name, as `getopt()` does.
    #[must_use]
    pub fn new(opts: &'a [Opt<'a>], optind: usize) -> Getopt<'a> {
        Getopt {
            opts,
            optind,
            optchr: 0,
            dash: false,
        }
    }

    /// One call of `php_getopt` (`Z/main/getopt.c:55-198`) over `argv`,
    /// each word a C string, so its end is php's NUL.
    ///
    /// # Errors
    ///
    /// [`Stopped`] where php answers `PHP_GETOPT_INVALID_ARG`, the walk
    /// moved past what it refused as php's moves.
    pub fn next(&mut self, argv: &[Vec<u8>]) -> Result<Taken, Stopped> {
        let Some(word) = argv.get(self.optind) else {
            return Ok(Taken::End);
        };
        if !self.dash && (word.first() != Some(&b'-') || word.len() == 1) {
            return Ok(Taken::End);
        }
        let long = word.starts_with(b"--");
        let (row, arg_start) = if long {
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
        let opt = self.opts[row];
        if opt.need != Need::Nothing {
            return self.value(argv, word, opt, row, arg_start);
        }
        // Several letters written as one.
        if arg_start >= 2 && !long {
            self.past_letter(word);
        } else {
            self.optind += 1;
        }
        Ok(Taken::Switch {
            code: opt.code,
            row,
            value: None,
        })
    }

    /// A `--name` or `--name=value` word: its row and where its value
    /// starts. The search for `=` covers all but the word's last byte, as
    /// php's `memchr` over `strlen - 3` bytes from the name does; a name
    /// no row has is `OPTERRARG` at the `optchr` left over from the
    /// previous word.
    fn long(&mut self, word: &[u8]) -> Result<(usize, usize), Stopped> {
        let mut arg_start = 2;
        let mut arg_end = word.len() - 1;
        if let Some(at) = word[arg_start..arg_end].iter().position(|&b| b == b'=') {
            arg_end = at;
            arg_start += 1;
        } else {
            arg_end -= 1;
        }
        let spelled = word.get(2..2 + arg_end);
        let Some(row) =
            (self.opts.iter()).position(|opt| opt.name.is_some() && opt.name == spelled)
        else {
            self.optind += 1;
            return Err((self.optind - 1, self.optchr, OptError::NoArgument));
        };
        self.optchr = 0;
        self.dash = false;
        Ok((row, arg_start + arg_end))
    }

    /// The letter at `optchr` of a `-xyz` word: its row, or `OPTERRNF`
    /// with the cluster moved past it.
    fn short(&mut self, word: &[u8]) -> Result<usize, Stopped> {
        let letter = word.get(self.optchr).copied();
        if let Some(row) = (self.opts.iter()).position(|opt| Some(opt.code) == letter) {
            return Ok(row);
        }
        let at = (self.optind, self.optchr);
        self.past_letter(word);
        Err((at.0, at.1, OptError::NotFound))
    }

    /// Past the letter at `optchr` of a cluster: the next letter, or past
    /// the word where it was the last.
    fn past_letter(&mut self, word: &[u8]) {
        if word.get(self.optchr + 1).is_none() {
            self.dash = false;
            self.optind += 1;
        } else {
            self.optchr += 1;
        }
    }

    /// The value of a row that wants one: attached, after `=`, or the
    /// next word; one that may take a value takes only an attached one.
    fn value(
        &mut self,
        argv: &[Vec<u8>],
        word: &[u8],
        opt: Opt<'a>,
        row: usize,
        arg_start: usize,
    ) -> Result<Taken, Stopped> {
        self.dash = false;
        self.optind += 1;
        let value = match word.get(arg_start) {
            None if self.optind == argv.len() => {
                if opt.need == Need::Required {
                    return Err((self.optind - 1, self.optchr, OptError::NoArgument));
                }
                None
            }
            None if opt.need == Need::Required => {
                self.optind += 1;
                argv.get(self.optind - 1).cloned()
            }
            None => None,
            Some(b'=') => Some(word[arg_start + 1..].to_vec()),
            Some(_) => Some(word[arg_start..].to_vec()),
        };
        Ok(Taken::Switch {
            code: opt.code,
            row,
            value,
        })
    }
}
