//! A person's approval of one change to a file `harness.self` scopes
//! (harness-design.md §4.4, §11). What only a person decides stays
//! theirs; once they have approved a change in the session, typing it is
//! the agent's. The approval is their answer to an `AskUserQuestion` call:
//! a question that names the file, whose chosen option's `preview` carries
//! the change as a unified diff and whose label says it applies it. It
//! admits a call that realizes exactly that diff from the file as it
//! stands, and nothing else.
//!
//! The answer is read where the model cannot write it. Claude Code writes
//! a tool's record, `toolUseResult` beside the `tool_result` block, in the
//! session's transcript, and hands the function hook the same record from
//! memory (`$.session.messages()`); the model writes the tool's input and
//! its own text. So an answer counts only as the record of an
//! `AskUserQuestion` call: a `user` record of this session, on the main
//! loop, whose `tool_result` answers the id of an `AskUserQuestion`
//! `tool_use` in an `assistant` record before it. What the model can put in
//! the transcript, its text, what a `Bash` call printed, the input of its own call
//! (whose `answers` the permission component fills and the model must
//! not), is none of that, and neither is an answer the dialog gave for the
//! person when they were away (`afkTimeoutMs`) or a transcript named for
//! another session.
//!
//! A call in a subagent (`agent_id`) is admitted by nothing: the hook hands
//! it the main session's id and transcript, and a subagent cannot ask the
//! person, so only the loop that asked may write.
//!
//! An approval is not counted: a diff applies to the file it was written
//! against, so once the change stands it no longer applies, and the two
//! adapters judging one call each read the same approval.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::call::Call;
use crate::git;

/// The rules an approval lifts: the agent's hand on a harness file.
pub(super) const LIFTED: [&str; 2] = ["harness.self", "harness.self-shell"];

/// The tool whose answers are the person's.
const TOOL: &str = "AskUserQuestion";

/// Where a call's answers are read from: the transcript at the hook's
/// `transcript_path` for the command hook and `permit --after`, the records
/// the function hook read from the engine and handed over as `asked`.
/// Each adapter reads its own and nothing else.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Evidence {
    #[default]
    Transcript,
    Engine,
}

/// One answered `AskUserQuestion` call: the input the model gave and the
/// record Claude Code stored for it.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct Asked {
    tool_use_id: String,
    input: Value,
    result: Value,
}

/// One option of a question, as the audit record names it ([`super::audit`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Choice {
    label: String,
    description: String,
}

/// What the person was asked and answered: the `AskUserQuestion` call,
/// its question, the options it offered, the one chosen and the notes
/// written beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Answered {
    ask: String,
    question: String,
    options: Vec<Choice>,
    answer: String,
    notes: Option<String>,
}

/// One question the person answered by choosing an option that carries a
/// preview.
#[derive(Debug, Clone)]
struct Approval {
    answered: Answered,
    preview: String,
}

/// An approval admitting one change: what the call's note and the audit
/// record carry. `before` and `after` are the file's blob ids, `None` where
/// there is no file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Admission {
    pub(crate) file: String,
    #[serde(flatten)]
    answered: Answered,
    diff: String,
    diff_hash: String,
    before: Option<String>,
    after: Option<String>,
}

impl Admission {
    /// The sentence a note carries.
    pub(super) fn said(&self) -> String {
        format!(
            "{} is changed as the person approved in this session: they answered \"{}\" to \
             AskUserQuestion {} (\"{}\").",
            self.file, self.answered.answer, self.answered.ask, self.answered.question
        )
    }
}

/// The approvals a call carries evidence of.
#[derive(Debug, Clone, Default)]
pub(super) struct Approvals(Vec<Approval>);

impl Approvals {
    /// What the call's adapter reads ([`Evidence`]); none in a subagent.
    pub(super) fn of(call: &Call) -> Approvals {
        if call.in_subagent() {
            return Approvals::default();
        }
        let asked = match call.evidence {
            Evidence::Engine => call.asked.clone().unwrap_or_default(),
            Evidence::Transcript => call
                .transcript_path
                .as_deref()
                .map(|path| transcript(Path::new(path), &call.session_id))
                .unwrap_or_default(),
        };
        Approvals(asked.iter().flat_map(Approval::all).collect())
    }

    /// Whether the call carries no approval at all.
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The approval whose diff, applied to `before`, leaves exactly
    /// `after` at `path`; `None` stands for no file on either side.
    pub(super) fn admits(
        &self,
        path: &Path,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Option<Admission> {
        self.admission(path, before, |left| left.text() == after)
    }

    /// The approval whose diff applies to `path` as it stands: what a
    /// `Bash` line that can write the file needs before it runs, since
    /// its write is not known until it has run; `permit --after` then
    /// holds what it wrote to [`Approvals::admits`].
    pub(super) fn pending(&self, path: &Path, before: Option<&str>) -> Option<Admission> {
        self.admission(path, before, |_| true)
    }

    fn admission(
        &self,
        path: &Path,
        before: Option<&str>,
        fits: impl Fn(&Left) -> bool,
    ) -> Option<Admission> {
        let name = path.to_str()?;
        let blob = |text: Option<&str>| text.map(|text| git::blob_id(text.as_bytes()));
        self.0
            .iter()
            .filter(|approval| approval.answered.question.contains(name))
            .find_map(|approval| {
                let file = FileDiff::all(&approval.preview)
                    .into_iter()
                    .find(|file| file.path == name)?;
                let left = file.apply(before).filter(|left| fits(left))?;
                Some(Admission {
                    file: name.to_owned(),
                    answered: approval.answered.clone(),
                    diff_hash: git::blob_id(file.text.as_bytes()),
                    diff: file.text,
                    before: blob(before),
                    after: blob(left.text()),
                })
            })
    }
}

impl Approval {
    /// Each question of one answered call that the person answered by
    /// choosing an option with a preview whose label says it applies the
    /// change ([`says_apply`]). None where the model filled the
    /// answers in its own input, where the dialog answered for an absent
    /// person, or where the person asked for another round.
    fn all(asked: &Asked) -> Vec<Approval> {
        let (input, result) = (&asked.input, &asked.result);
        let filled = input
            .get("answers")
            .is_some_and(|a| a.as_object().is_none_or(|a| !a.is_empty()));
        if filled
            || result.get("afkTimeoutMs").is_some()
            || result.get("followUp") == Some(&Value::Bool(true))
        {
            return Vec::new();
        }
        let questions = input.get("questions").and_then(Value::as_array);
        questions
            .into_iter()
            .flatten()
            .filter_map(|q| {
                let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
                let question = text(q.get("question"))?;
                let choice = q.get("kind").is_none_or(|k| k == "choice");
                if !choice || q.get("multiSelect") == Some(&Value::Bool(true)) {
                    return None;
                }
                let answer = text(result.get("answers")?.get(&question))?;
                let options = q.get("options")?.as_array()?;
                let chosen = options
                    .iter()
                    .find(|o| text(o.get("label")) == Some(answer.clone()))?;
                if !says_apply(&answer) {
                    return None;
                }
                let preview = text(chosen.get("preview")).filter(|p| !p.is_empty())?;
                let notes = text(
                    result
                        .get("annotations")
                        .and_then(|a| a.get(&question))
                        .and_then(|a| a.get("notes")),
                );
                let options = (options.iter())
                    .map(|o| Choice {
                        label: text(o.get("label")).unwrap_or_default(),
                        description: text(o.get("description")).unwrap_or_default(),
                    })
                    .collect();
                let ask = asked.tool_use_id.clone();
                let answered = Answered {
                    ask,
                    question,
                    options,
                    answer,
                    notes,
                };
                Some(Approval { answered, preview })
            })
            .collect()
    }
}

/// Whether a chosen option's label says it applies the change: it begins
/// "Apply" or "Approve". The person reads the label as much as the
/// preview, so an option that carries the diff under "Leave it" approves
/// nothing.
fn says_apply(label: &str) -> bool {
    let word = label.split_whitespace().next().unwrap_or_default();
    let word = word
        .trim_matches(|c: char| !c.is_alphabetic())
        .to_lowercase();
    ["apply", "approve"].contains(&word.as_str())
}

/// The answered `AskUserQuestion` calls of the transcript at `path`, which
/// must be the session's own: named for it, and only its main loop's
/// records read. A transcript that cannot be read answers nothing.
fn transcript(path: &Path, session: &str) -> Vec<Asked> {
    let named = path.file_stem().and_then(|s| s.to_str()) == Some(session);
    let text = match std::fs::read_to_string(path) {
        Ok(text) if named && !session.is_empty() => text,
        _ => return Vec::new(),
    };
    let ours = |record: &Value, kind: &str| {
        record.get("type").and_then(Value::as_str) == Some(kind)
            && record.get("sessionId").and_then(Value::as_str) == Some(session)
            && record.get("isSidechain") != Some(&Value::Bool(true))
    };
    let blocks = |record: &Value| -> Vec<Value> {
        record
            .pointer("/message/content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut asked = Vec::new();
    // A record is parsed only where its line names the tool, and an
    // answer only where its line names the call: the rest of a long
    // transcript is never read as JSON.
    for (at, line) in lines.iter().enumerate() {
        if !line.contains(TOOL) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !ours(&record, "assistant") {
            continue;
        }
        for block in blocks(&record) {
            let id = match (block.get("type"), block.get("name"), block.get("id")) {
                (Some(t), Some(n), Some(Value::String(id))) if t == "tool_use" && n == TOOL => id,
                _ => continue,
            };
            let answer = lines[at + 1..]
                .iter()
                .filter(|line| line.contains(id.as_str()))
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|record| {
                    ours(record, "user")
                        && blocks(record).iter().any(|b| {
                            b.get("type").is_some_and(|t| t == "tool_result")
                                && b.get("tool_use_id").is_some_and(|t| t == id.as_str())
                                && b.get("is_error") != Some(&Value::Bool(true))
                        })
                });
            if let Some(result) = answer.and_then(|r| r.get("toolUseResult").cloned()) {
                asked.push(Asked {
                    tool_use_id: id.clone(),
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                    result,
                });
            }
        }
    }
    asked
}

/// One file's part of a unified diff: its path, whether it is created or
/// removed, its hunks, and its text as the preview carries it.
#[derive(Debug)]
struct FileDiff {
    path: String,
    created: bool,
    removed: bool,
    hunks: Vec<Hunk>,
    text: String,
}

/// One hunk: where its old lines start, and each line with its tag, its
/// newline kept unless the diff says the file ends without one.
#[derive(Debug)]
struct Hunk {
    old_start: usize,
    old_len: usize,
    lines: Vec<(char, String)>,
}

impl FileDiff {
    /// Every file of the diff `text` carries, in `--- `/`+++ ` headers
    /// as git and `diff -u` write them. A line a hunk does not count is
    /// not the hunk's; an empty line inside one is an empty context line,
    /// whose leading space an editor drops; a last line without its
    /// newline has one, since a preview is text cut at its end, and only
    /// `\ No newline at end of file` says the file ends without one.
    fn all(text: &str) -> Vec<FileDiff> {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut files: Vec<FileDiff> = Vec::new();
        let mut left = (0, 0);
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let next = lines.get(i + 1).copied().unwrap_or_default();
            if let (Some(old), Some(new)) = (line.strip_prefix("--- "), next.strip_prefix("+++ ")) {
                let (old, new) = (header(old, "a/"), header(new, "b/"));
                files.push(FileDiff {
                    created: old.is_none(),
                    removed: new.is_none(),
                    path: new.or(old).unwrap_or_default(),
                    hunks: Vec::new(),
                    text: format!("{line}{next}"),
                });
                left = (0, 0);
                i += 2;
                continue;
            }
            i += 1;
            let Some(file) = files.last_mut() else {
                continue;
            };
            if left == (0, 0) {
                if let Some((hunk, new_len)) = Hunk::header(line) {
                    left = (hunk.old_len, new_len);
                    file.text.push_str(line);
                    file.hunks.push(hunk);
                } else if line.starts_with('\\')
                    && let Some((_, last)) = file.hunks.last_mut().and_then(|h| h.lines.last_mut())
                {
                    no_newline(last);
                    file.text.push_str(line);
                }
                continue;
            }
            let Some(hunk) = file.hunks.last_mut() else {
                continue;
            };
            let (tag, body) = match line.chars().next() {
                Some(tag @ (' ' | '-' | '+')) => (tag, &line[1..]),
                Some('\n') | None => (' ', line),
                Some('\\') => {
                    if let Some((_, last)) = hunk.lines.last_mut() {
                        no_newline(last);
                    }
                    file.text.push_str(line);
                    continue;
                }
                Some(_) => {
                    left = (0, 0);
                    continue;
                }
            };
            match tag {
                ' ' => left = (left.0.saturating_sub(1), left.1.saturating_sub(1)),
                '-' => left.0 = left.0.saturating_sub(1),
                _ => left.1 = left.1.saturating_sub(1),
            }
            // A preview cut at its last line, as the dialog's text is,
            // still ends that line: only the marker says a file does not.
            let mut body = body.to_owned();
            if !body.ends_with('\n') {
                body.push('\n');
            }
            hunk.lines.push((tag, body));
            file.text.push_str(line);
        }
        files
    }

    /// The file this diff leaves when applied to `before`, `None` for a
    /// removed one; `None` outright where it does not apply, every context
    /// and removed line held to the line it names.
    fn apply(&self, before: Option<&str>) -> Option<Left> {
        let before = before.unwrap_or_default();
        if self.created && !before.is_empty() {
            return None;
        }
        let lines: Vec<&str> = before.split_inclusive('\n').collect();
        let mut out = String::new();
        let mut cursor = 0;
        for hunk in &self.hunks {
            let start = if hunk.old_len == 0 {
                hunk.old_start
            } else {
                hunk.old_start.checked_sub(1)?
            };
            out.extend(lines.get(cursor..start)?.iter().copied());
            cursor = start;
            for (tag, line) in &hunk.lines {
                if *tag != '+' {
                    (lines.get(cursor) == Some(&line.as_str())).then_some(())?;
                    cursor += 1;
                }
                if *tag != '-' {
                    out.push_str(line);
                }
            }
        }
        out.extend(lines.get(cursor..)?.iter().copied());
        if self.removed {
            out.is_empty().then_some(Left::Removed)
        } else {
            Some(Left::Text(out))
        }
    }
}

/// What a diff leaves of a file: its text, or no file.
#[derive(Debug, PartialEq, Eq)]
enum Left {
    Text(String),
    Removed,
}

impl Left {
    fn text(&self) -> Option<&str> {
        match self {
            Left::Text(text) => Some(text),
            Left::Removed => None,
        }
    }
}

/// A `--- `/`+++ ` header's path, less git's prefix and `diff -u`'s
/// timestamp; `None` for `/dev/null`.
fn header(text: &str, prefix: &str) -> Option<String> {
    let path = text.split('\t').next().unwrap_or_default().trim_end();
    (path != "/dev/null").then(|| path.strip_prefix(prefix).unwrap_or(path).to_owned())
}

/// The line `\ No newline at end of file` follows: its newline dropped.
fn no_newline(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
    }
}

impl Hunk {
    /// `@@ -a,b +c,d @@`, a length left out being 1, with the new side's
    /// length, which the hunk's counting needs.
    fn header(line: &str) -> Option<(Hunk, usize)> {
        let ranges = line.strip_prefix("@@ -")?.split(" @@").next()?;
        let (old, new) = ranges.split_once(" +")?;
        let range = |r: &str| -> Option<(usize, usize)> {
            match r.split_once(',') {
                Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
                None => Some((r.parse().ok()?, 1)),
            }
        };
        let ((old_start, old_len), (_, new_len)) = (range(old)?, range(new)?);
        let hunk = Hunk {
            old_start,
            old_len,
            lines: Vec::new(),
        };
        Some((hunk, new_len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(diff: &str, before: &str) -> Option<String> {
        let files = FileDiff::all(diff);
        assert_eq!(files.len(), 1, "{files:?}");
        files[0]
            .apply(Some(before))
            .and_then(|left| left.text().map(str::to_owned))
    }

    const SEAMS: &str = "a = 1\nb = 2\nc = 3\nd = 4\n";

    #[test]
    fn a_diff_applies_where_its_lines_stand_and_nowhere_else() {
        let diff = "diff --git a/seams.toml b/seams.toml\nindex 1..2 100644\n--- a/seams.toml\n+++ b/seams.toml\n@@ -2,2 +2,2 @@\n b = 2\n-c = 3\n+c = 30\n";
        assert_eq!(
            applied(diff, SEAMS),
            Some("a = 1\nb = 2\nc = 30\nd = 4\n".to_owned())
        );
        assert_eq!(FileDiff::all(diff)[0].path, "seams.toml");
        // Applied again, the line it removes no longer stands.
        assert_eq!(applied(diff, "a = 1\nb = 2\nc = 30\nd = 4\n"), None);
        // A diff whose lines stand elsewhere does not apply.
        assert_eq!(applied(diff, "x\na = 1\nb = 2\nc = 3\n"), None);
    }

    #[test]
    fn an_insertion_an_empty_context_line_and_a_missing_final_newline() {
        let diff =
            "--- seams.toml\t2026-10-07\n+++ seams.toml\t2026-10-07\n@@ -1,0 +2 @@\n+x = 9\n";
        assert_eq!(
            applied(diff, SEAMS),
            Some("a = 1\nx = 9\nb = 2\nc = 3\nd = 4\n".to_owned())
        );
        let diff = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n";
        assert_eq!(applied(diff, "a\n\nb"), Some("a\n\nc".to_owned()));
        let diff = "--- /dev/null\n+++ b/new.toml\n@@ -0,0 +1 @@\n+n = 1\n";
        assert_eq!(applied(diff, ""), Some("n = 1\n".to_owned()));
        assert_eq!(applied(diff, "n = 0\n"), None);
    }

    #[test]
    fn a_preview_carries_one_part_per_file_and_text_outside_hunks_is_none_of_theirs() {
        let diff = "Raise two ceilings:\n--- a/seams.toml\n+++ b/seams.toml\n@@ -1 +1 @@\n-a = 1\n+a = 2\nand the oracle:\n--- a/oracle.toml\n+++ b/oracle.toml\n@@ -1 +1 @@\n-v = 1\n+v = 2\n";
        let files = FileDiff::all(diff);
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["seams.toml", "oracle.toml"]);
        assert_eq!(files[0].hunks[0].lines.len(), 2, "{files:?}");
        assert_eq!(
            files[0].apply(Some("a = 1\n")),
            Some(Left::Text("a = 2\n".to_owned()))
        );
    }
}
