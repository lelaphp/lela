//! `Z/main/getopt.c`'s `php_getopt`, which php keeps in `main/` for the
//! CLI's own switches and `getopt()` reads the program's `argv` with:
//! one option at a time, a short one from a sequence of letters after a `-`
//! or a long one after `--`, with its value where it wants one. The
//! C function's two statics, `optchr` and `dash`, are the walker's
//! fields, so one `getopt()` call starts from nothing.

/// How much an option wants a value: `need_param`'s 0, 1 and 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Need {
    Nothing,
    Required,
    Optional,
}

/// One `opt_struct`: a short option's letter, or a long option's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Named {
    Short(u8),
    Long(Vec<u8>),
}

/// One option `getopt` looks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Opt {
    pub(super) named: Named,
    pub(super) need: Need,
}

impl Opt {
    /// The letter `php_getopt` compares a short option by; a long one
    /// has the `0` php stores, which no byte of an argument is.
    fn letter(&self) -> Option<u8> {
        match &self.named {
            Named::Short(letter) => Some(*letter),
            Named::Long(_) => None,
        }
    }

    fn name(&self) -> Option<&[u8]> {
        match &self.named {
            Named::Short(_) => None,
            Named::Long(name) => Some(name),
        }
    }
}

/// `parse_opts` (`Z/ext/standard/basic_functions.c:890`): the letters
/// and digits at the start of the string, each followed by `:` for a
/// value it needs or `::` for one it may take. php sizes the table by
/// every letter and digit in the string and fills only the leading ones;
/// the slots it leaves zeroed match nothing, so they are left out here.
pub(super) fn short_options(spec: &[u8]) -> Vec<Opt> {
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
            named: Named::Short(letter),
            need,
        });
    }
    opts
}

/// A long option as `getopt` reads its spelling: a trailing `:` or `::`
/// read off as the value it needs or may take.
pub(super) fn long_option(spelled: &[u8]) -> Opt {
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
    Opt {
        named: Named::Long(name),
        need,
    }
}

/// What one call of `php_getopt` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Got {
    /// `EOF`: no option is left before the arguments.
    End,
    /// `PHP_GETOPT_INVALID_ARG`: an option the table has no row for, a
    /// `:` among the letters, or a value missing where one is needed.
    Invalid,
    /// The row found, and its value where it took one.
    Found(usize, Option<Vec<u8>>),
}

/// `php_getopt(argc, argv, opts, &optarg, &optind, 0, 1)` over one
/// `argv`, its two statics held here.
pub(super) struct Getopt<'a> {
    argv: &'a [Vec<u8>],
    opts: &'a [Opt],
    /// `*optind`: the argument being read, `1` to begin with.
    pub(super) optind: usize,
    optchr: usize,
    dash: bool,
}

impl<'a> Getopt<'a> {
    pub(super) fn new(argv: &'a [Vec<u8>], opts: &'a [Opt]) -> Getopt<'a> {
        Getopt {
            argv,
            opts,
            optind: 1,
            optchr: 0,
            dash: false,
        }
    }

    /// `argv[i][j]`, NUL past the end as C reads it.
    fn at(&self, i: usize, j: usize) -> u8 {
        self.argv
            .get(i)
            .and_then(|arg| arg.get(j))
            .copied()
            .unwrap_or(0)
    }

    /// `&argv[i][j]` as a string.
    fn rest(&self, i: usize, j: usize) -> Vec<u8> {
        self.argv
            .get(i)
            .and_then(|arg| arg.get(j..))
            .unwrap_or_default()
            .to_vec()
    }

    /// One call of `php_getopt` (`Z/main/getopt.c:55`).
    pub(super) fn next(&mut self) -> Got {
        let i = self.optind;
        if i >= self.argv.len() {
            return Got::End;
        }
        if !self.dash && (self.at(i, 0) != b'-' || self.at(i, 1) == 0) {
            return Got::End;
        }
        let long = self.at(i, 0) == b'-' && self.at(i, 1) == b'-';
        let (found, arg_start) = if long {
            if self.at(i, 2) == 0 {
                self.optind += 1;
                return Got::End;
            }
            let Some(found) = self.long(i) else {
                self.optind += 1;
                return Got::Invalid;
            };
            found
        } else {
            if !self.dash {
                self.dash = true;
                self.optchr = 1;
            }
            if self.at(i, self.optchr) == b':' {
                self.dash = false;
                self.optind += 1;
                return Got::Invalid;
            }
            let letter = self.at(i, self.optchr);
            let Some(found) = self
                .opts
                .iter()
                .position(|opt| opt.letter() == Some(letter))
            else {
                if self.at(i, self.optchr + 1) == 0 {
                    self.dash = false;
                    self.optind += 1;
                } else {
                    self.optchr += 1;
                }
                return Got::Invalid;
            };
            (found, 1 + self.optchr)
        };
        match self.opts[found].need {
            Need::Nothing => {
                // Several short options written as one.
                if arg_start >= 2 && !long {
                    if self.at(i, self.optchr + 1) == 0 {
                        self.dash = false;
                        self.optind += 1;
                    } else {
                        self.optchr += 1;
                    }
                } else {
                    self.optind += 1;
                }
                Got::Found(found, None)
            }
            need => {
                // `-<arg> <val>`, `-<arg>=<val>` or `-<arg><val>`; an
                // optional value is never the next argument.
                self.dash = false;
                let value = match self.at(i, arg_start) {
                    0 => {
                        self.optind += 1;
                        if self.optind == self.argv.len() {
                            if need == Need::Required {
                                return Got::Invalid;
                            }
                            None
                        } else if need == Need::Required {
                            let value = self.argv[self.optind].clone();
                            self.optind += 1;
                            Some(value)
                        } else {
                            None
                        }
                    }
                    b'=' => {
                        self.optind += 1;
                        Some(self.rest(i, arg_start + 1))
                    }
                    _ => {
                        self.optind += 1;
                        Some(self.rest(i, arg_start))
                    }
                };
                Got::Found(found, value)
            }
        }
    }

    /// The `--name` and `--name=value` branch: the row whose name the
    /// argument spells, and where its value would start. The search for
    /// `=` covers all but the last byte, as php's `memchr` over
    /// `strlen - 3` bytes from the name does.
    fn long(&mut self, i: usize) -> Option<(usize, usize)> {
        let arg = &self.argv[i];
        let mut arg_start = 2;
        let mut arg_end = arg.len() - 1;
        match arg[2..arg_end].iter().position(|c| *c == b'=') {
            Some(at) => {
                arg_end = at;
                arg_start += 1;
            }
            None => arg_end -= 1,
        }
        let spelled = arg.get(2..2 + arg_end)?;
        let found = self
            .opts
            .iter()
            .position(|opt| opt.name() == Some(spelled))?;
        self.optchr = 0;
        self.dash = false;
        Some((found, arg_start + arg_end))
    }
}
