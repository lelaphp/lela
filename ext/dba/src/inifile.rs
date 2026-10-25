//! The `inifile` handler: `dba_inifile.c` over php's bundled
//! `libinifile/inifile.c`. A key is `[group]name` or a bare `name` in
//! the group-less head of the file; groups and names compare without
//! regard to ASCII case, and a write rewrites the file from the group it
//! touches on through php's temporary streams. Every string the C reads
//! is a C string, so a line and a key end at their first NUL.

use php_ext::coerce::c_str;
use php_ext::raising::{Level, Thrown, docref, throw_error};
use php_ext::request::{OwnedStream as _, Request, Stream, Streams as _, Whence};
use php_ext::values::Owner;

use crate::flatfile::gets;

/// `etrim`: the bytes without the blanks `" \t\r\n"` at either end.
fn etrim(bytes: &[u8]) -> Vec<u8> {
    let blank = |b: &u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
    let start = bytes.iter().position(|b| !blank(b)).unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !blank(b))
        .map_or(start, |at| at + 1);
    bytes[start..end.max(start)].to_vec()
}

/// `key_type`: a group and a name, either unset (`NULL`).
#[derive(Default, Clone)]
pub(crate) struct Key {
    group: Option<Vec<u8>>,
    name: Option<Vec<u8>>,
}

impl Key {
    /// `inifile_key_split`: `[group]name`, or a bare name in the group
    /// `""`.
    pub(crate) fn split(key: &[u8]) -> Key {
        let key = c_str(key);
        if key.first() == Some(&b'[')
            && let Some(close) = key.iter().position(|&b| b == b']')
        {
            return Key {
                group: Some(key[1..close].to_vec()),
                name: Some(key[close + 1..].to_vec()),
            };
        }
        Key {
            group: Some(Vec::new()),
            name: Some(key.to_vec()),
        }
    }

    /// `inifile_key_string`: `[group]name` where the group is not
    /// empty, the name alone otherwise, `None` with neither.
    pub(crate) fn string(&self) -> Option<Vec<u8>> {
        match (&self.group, &self.name) {
            (Some(group), name) if !group.is_empty() => {
                let mut out = Vec::with_capacity(group.len() + 2);
                out.push(b'[');
                out.extend_from_slice(group);
                out.push(b']');
                out.extend_from_slice(name.as_deref().unwrap_or_default());
                Some(out)
            }
            (_, Some(name)) => Some(name.clone()),
            _ => None,
        }
    }

    fn group(&self) -> &[u8] {
        self.group.as_deref().unwrap_or_default()
    }

    fn name(&self) -> &[u8] {
        self.name.as_deref().unwrap_or_default()
    }
}

/// What `inifile_key_cmp` finds of two keys.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Compared {
    /// `0`: the same group and name.
    Equal,
    /// `1`: the same group, another name.
    SameGroup,
    /// `2`: another group.
    Different,
}

fn compare(a: &Key, b: &Key) -> Compared {
    if !a.group().eq_ignore_ascii_case(b.group()) {
        Compared::Different
    } else if a.name().eq_ignore_ascii_case(b.name()) {
        Compared::Equal
    } else {
        Compared::SameGroup
    }
}

/// `line_type`: a key, its value and the position past its line.
#[derive(Default)]
struct Line {
    key: Key,
    value: Option<Vec<u8>>,
    pos: i64,
}

impl Line {
    /// `inifile_line_free`.
    fn free(&mut self) {
        *self = Line::default();
    }
}

/// `inifile_read`: the next `[group]` or `name=value` line into `ln`,
/// lines of neither shape passed over; `false` at the end, `ln` freed.
fn read(req: &mut Request, file: Stream, ln: &mut Line) -> Result<bool, Thrown> {
    ln.value = None;
    while let Some(line) = gets(req, file, None)? {
        let line = c_str(&line);
        if line.first() == Some(&b'[') {
            // A value name cannot start with `[`: a group, or nothing.
            if let Some(close) = line[1..].iter().position(|&b| b == b']') {
                ln.key.group = Some(etrim(&line[1..=close]));
                ln.key.name = Some(Vec::new());
                ln.pos = file.tell(req);
                return Ok(true);
            }
        } else if let Some(eq) = line.iter().position(|&b| b == b'=') {
            if ln.key.group.is_none() {
                ln.key.group = Some(Vec::new());
            }
            ln.key.name = Some(etrim(&line[..eq]));
            ln.value = Some(etrim(&line[eq + 1..]));
            ln.pos = file.tell(req);
            return Ok(true);
        }
    }
    ln.free();
    Ok(false)
}

/// `php_stream_copy_to_stream_ex`: `len` bytes, or all of them for
/// `None`, from one stream to the other.
fn copy(req: &mut Request, from: Stream, to: Stream, len: Option<i64>) -> bool {
    let mut left = len.map(|n| usize::try_from(n).unwrap_or(0));
    let mut buf = [0u8; 8192];
    loop {
        let want = left.map_or(buf.len(), |n| n.min(buf.len()));
        if want == 0 {
            return true;
        }
        let got = match from.read_into(req, &mut buf[..want]) {
            Some(0) | None => return left.is_none(),
            Some(n) => n,
        };
        if to.write(req, &buf[..got]) != Some(got) {
            return false;
        }
        left = left.map(|n| n - got);
    }
}

/// The temporary streams closed and their owners given up,
/// `php_stream_free`.
fn free(req: &mut Request, held: [Option<Owner<Stream>>; 2]) {
    for held in held.into_iter().flatten() {
        held.free(req);
    }
}

/// `php_stream_temp_create(0, 64 * 1024)`.
fn temp(req: &mut Request) -> Result<Option<Owner<Stream>>, Thrown> {
    let made = req.open_stream(b"php://temp/maxmemory:65536", b"w+b", false)?;
    if made.is_none() {
        docref(req, Level::Warning, "Could not create temporary stream");
    }
    Ok(made)
}

/// `inifile`: the walk's line and the line the last fetch found.
#[derive(Default)]
pub(crate) struct Inifile {
    curr: Line,
    next: Line,
}

impl Inifile {
    /// `inifile_fetch`: the value of the `skip`th line under `key`; a
    /// skip of `-1` goes on from the line the last fetch of the same key
    /// found.
    pub(crate) fn fetch(
        &mut self,
        req: &mut Request,
        file: Stream,
        key: &Key,
        skip: i64,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let mut ln = Line::default();
        if skip == -1
            && self.next.key.group.is_some()
            && self.next.key.name.is_some()
            && compare(&self.next.key, key) == Compared::Equal
        {
            file.seek(req, self.next.pos, Whence::Set);
            ln.key.group.clone_from(&self.next.key.group);
        } else {
            file.seek(req, 0, Whence::Set);
            self.next.free();
        }
        let mut skip = skip.max(0);
        let mut in_group = false;
        while read(req, file, &mut ln)? {
            match compare(&ln.key, key) {
                Compared::Equal => {
                    if skip == 0 {
                        let value = ln.value.clone().unwrap_or_default();
                        self.next = ln;
                        self.next.pos = file.tell(req);
                        return Ok(Some(value));
                    }
                    skip -= 1;
                }
                Compared::SameGroup => in_group = true,
                // Leaving the group: the key is not in the file.
                Compared::Different if in_group => break,
                Compared::Different => {}
            }
        }
        self.next.pos = file.tell(req);
        Ok(None)
    }

    /// `inifile_firstkey`.
    pub(crate) fn first_key(
        &mut self,
        req: &mut Request,
        file: Stream,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        self.curr.free();
        self.next_key(req, file)
    }

    /// `inifile_nextkey`, behind `DBA_NEXTKEY_FUNC(inifile)`'s refusal of
    /// a walk that ended.
    pub(crate) fn next_key(
        &mut self,
        req: &mut Request,
        file: Stream,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        file.seek(req, self.curr.pos, Whence::Set);
        let mut ln = Line::default();
        ln.key.group = Some(self.curr.key.group.clone().unwrap_or_default());
        read(req, file, &mut ln)?;
        self.curr = ln;
        Ok(self.curr.key.string())
    }

    /// Whether a walk has a line to go on from.
    pub(crate) fn walking(&self) -> bool {
        self.curr.key.group.is_some() || self.curr.key.name.is_some()
    }

    /// `inifile_find_group`: where the group's first line starts; the
    /// end of the file where the group is not there.
    fn find_group(
        &mut self,
        req: &mut Request,
        file: Stream,
        key: &Key,
        start: &mut i64,
    ) -> Result<(), Thrown> {
        file.flush(req);
        file.seek(req, 0, Whence::Set);
        self.curr.free();
        self.next.free();
        if key.group().is_empty() {
            *start = 0;
            return Ok(());
        }
        let mut ln = Line::default();
        let mut found = false;
        while read(req, file, &mut ln)? {
            if compare(&ln.key, key) != Compared::Different {
                found = true;
                break;
            }
            *start = file.tell(req);
        }
        if !found {
            *start = file.tell(req);
        }
        Ok(())
    }

    /// `inifile_next_group`: where the next group starts, the end of the
    /// file where none does.
    fn next_group(req: &mut Request, file: Stream, key: &Key) -> Result<i64, Thrown> {
        let mut next = file.tell(req);
        let mut ln = Line::default();
        ln.key.group.clone_from(&key.group);
        while read(req, file, &mut ln)? {
            if compare(&ln.key, key) == Compared::Different {
                break;
            }
            next = file.tell(req);
        }
        Ok(next)
    }

    /// `inifile_filter`: the group's lines copied back from `from`, the
    /// ones under `key` left out; whether one was.
    fn filter(
        req: &mut Request,
        file: Stream,
        from: Stream,
        key: &Key,
        found: &mut bool,
    ) -> Result<bool, Thrown> {
        let (mut start, mut next) = (0, 0);
        let mut ok = true;
        from.seek(req, 0, Whence::Set);
        file.seek(req, 0, Whence::End);
        let mut ln = Line::default();
        let copied = |req: &mut Request, start: i64, next: i64| {
            from.seek(req, start, Whence::Set);
            if copy(req, from, file, Some(next - start)) {
                return true;
            }
            docref(
                req,
                Level::Warning,
                &format!("Could not copy [{next} - {start}] from temporary stream"),
            );
            false
        };
        while read(req, from, &mut ln)? {
            match compare(&ln.key, key) {
                Compared::Equal => {
                    *found = true;
                    let curr = from.tell(req);
                    if start != next {
                        ok &= copied(req, start, next);
                        from.seek(req, curr, Whence::Set);
                    }
                    start = curr;
                    next = curr;
                }
                Compared::SameGroup => next = from.tell(req),
                Compared::Different => {}
            }
        }
        if start != next {
            ok &= copied(req, start, next);
        }
        Ok(ok)
    }

    /// `inifile_delete_replace_append`: the group found, what follows it
    /// set aside, the file truncated, the group's other lines copied back
    /// (not for an append), the line written, and what followed written
    /// back. `Ok(false)` is php's `FAILURE`.
    pub(crate) fn rewrite(
        &mut self,
        req: &mut Request,
        file: Stream,
        key: &Key,
        value: Option<&[u8]>,
        append: bool,
        found: &mut bool,
    ) -> Result<bool, Thrown> {
        let mut start = 0;
        self.find_group(req, file, key, &mut start)?;
        let next = Self::next_group(req, file, key)?;
        let mut ok = true;
        let mut group = None;
        if !append && start != next {
            match temp(req)? {
                Some(held) => {
                    file.seek(req, start, Whence::Set);
                    let copied = copy(req, file, *held, Some(next - start));
                    group = Some(held);
                    if !copied {
                        docref(
                            req,
                            Level::Warning,
                            &format!("Could not copy group [{start} - {next}] to temporary stream"),
                        );
                        ok = false;
                    }
                }
                None => ok = false,
            }
        }
        let mut rest = None;
        if ok {
            rest = match temp(req) {
                Ok(rest) => rest,
                Err(thrown) => {
                    free(req, [group, None]);
                    return Err(thrown);
                }
            };
            match rest.as_deref().copied() {
                None => ok = false,
                Some(rest) => {
                    file.seek(req, 0, Whence::End);
                    if next != file.tell(req) {
                        file.seek(req, next, Whence::Set);
                        if !copy(req, file, rest, None) {
                            docref(
                                req,
                                Level::Warning,
                                "Could not copy remainder to temporary stream",
                            );
                            ok = false;
                        }
                    }
                }
            }
        }
        let named = !key.name().is_empty();
        if ok && (value.is_none() || named) {
            ok = truncate(req, file, if append { next } else { start });
        }
        let mut result = Ok(ok);
        if ok {
            if named {
                if let Some(held) = group.as_deref().copied() {
                    ok = match Self::filter(req, file, held, key, found) {
                        Ok(ok) => ok,
                        Err(thrown) => {
                            free(req, [group, rest]);
                            return Err(thrown);
                        }
                    };
                }
                if let Some(value) = value {
                    if start == next && !key.group().is_empty() {
                        let mut header = b"[".to_vec();
                        header.extend_from_slice(key.group());
                        header.extend_from_slice(b"]\n");
                        file.write(req, &header);
                    }
                    let mut line = key.name().to_vec();
                    line.push(b'=');
                    line.extend_from_slice(c_str(value));
                    line.push(b'\n');
                    file.write(req, &line);
                }
            }
            result = Ok(ok);
            if let Some(rest) = rest.as_deref().copied()
                && rest.tell(req) != 0
            {
                rest.seek(req, 0, Whence::Set);
                file.seek(req, 0, Whence::End);
                if !copy(req, rest, file, None) {
                    result = Err(throw_error(
                        req,
                        "Error",
                        "Could not copy from temporary stream - ini file truncated",
                    ));
                }
            }
        }
        free(req, [group, rest]);
        file.flush(req);
        file.seek(req, 0, Whence::Set);
        result
    }
}

/// `inifile_truncate`: the file cut at `size`, the stream left there.
fn truncate(req: &mut Request, file: Stream, size: i64) -> bool {
    let cut = usize::try_from(size).unwrap_or(0);
    if file.truncate(req, cut).ok().flatten() != Some(true) {
        docref(req, Level::Warning, "Error in ftruncate: -1");
        return false;
    }
    file.seek(req, size, Whence::Set);
    true
}

/// `inifile_version`.
pub(crate) const VERSION: &str = "1.0, $Id: 4dc687ea6681ffc42006a5fa64ac97f828c52ce1 $";
