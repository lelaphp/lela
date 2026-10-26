//! What `xtask witness --accept --bodies` writes into one stream of a
//! plain fence the oracle does not meet (#3455, as Mitchell ruled on
//! #3429): the oracle's lines from the instrument's own two runs, and each
//! authored line whose wildcards the oracle's line still meets kept as
//! written, so a fence written for one machine with `%d` or `%s` stays
//! written for every machine (#783).
//!
//! An authored line is held to the oracle's by the matcher the judge
//! reads a fence with, [`Expectation::lines`], so a line is kept by the
//! same rule a phpt miss is found by and never by a second reading of the
//! wildcards. The authored lines are matched to the oracle's in order, the
//! most kept that the two runs allow ([`aligned`]): a line the two runs
//! print alike is written as the oracle prints it where no authored line
//! is kept for it, as text that matches it alone ([`literal`]), and one
//! they print apart is written only by an authored line that meets it in
//! both; any other is refused, named, and nothing of the fence is
//! written.

use crate::oracle::expect::{self, Expectation, Line};
use crate::oracle::{Normalise, normalise};

/// The stream `authored`, a fence's body as the grammar read it, rewritten
/// from `runs`, the oracle's two runs of it under the recipe's
/// normalisation: each authored line with a wildcard that meets its
/// oracle line in both runs kept as written, and the first run's line for
/// the rest. `Err` names the line the two runs differ on that no kept line
/// meets, or the count of lines they differ in.
pub(super) fn kept(
    rules: &Normalise,
    stream: &str,
    authored: &str,
    runs: [&str; 2],
) -> Result<String, String> {
    let [one, two] = runs.map(|run| run.lines().collect::<Vec<_>>());
    if one.len() != two.len() {
        return Err(format!(
            "{stream} is {} lines from one scratch directory and {} from another",
            one.len(),
            two.len()
        ));
    }
    // The matcher reads the authored text as the judge does, normalised
    // whole, whose trim drops the lines a stream opens with that hold
    // nothing; each line it reads is the authored line that many after.
    let read = Expectation::Format(normalise(rules, authored.as_bytes().to_vec())).lines();
    let raw: Vec<&str> = authored.lines().collect();
    let opens = raw
        .iter()
        .take_while(|line| line.chars().all(|c| rules.trim.contains(c)))
        .count();
    let wildcards: Vec<Option<(&Line, &str)>> = (read.iter().enumerate())
        .map(|(at, line)| {
            let written = raw.get(opens + at)?;
            written.contains('%').then_some((line, *written))
        })
        .collect();
    aligned(rules, &wildcards, &one, &two)
        .map(|lines| lines.join("\n"))
        .ok_or_else(|| moved(stream, &wildcards, &one, &two))
}

/// The oracle's `text` as a fence writes it: as text that matches it
/// alone ([`expect::literal`]), where a `%s` it printed would otherwise
/// be read as a wildcard and the fence would meet more than the oracle
/// printed, the recipe's scratch token left as the token it is.
pub(super) fn literal(rules: &Normalise, text: &str) -> String {
    let token = rules.scratch_token.as_str();
    if token.is_empty() {
        return expect::literal(text).into_owned();
    }
    let parts: Vec<_> = text.split(token).map(expect::literal).collect();
    parts.join(token)
}

/// What a refused stream names: the first line the two runs differ on
/// that no authored wildcard meets in both, or the first they differ on
/// where each is met by one but no order of the authored lines meets them
/// all.
fn moved(stream: &str, wildcards: &[Option<(&Line, &str)>], one: &[&str], two: &[&str]) -> String {
    let met =
        |at: usize| (wildcards.iter().flatten()).any(|(line, _)| meets(line, one[at], two[at]));
    let differ = (0..one.len()).filter(|&at| one[at] != two[at]);
    let at = differ
        .clone()
        .find(|&at| !met(at))
        .or(differ.min())
        .unwrap_or(0);
    format!(
        "{stream} line {} is `{}` from one scratch directory and `{}` from another, and no authored line's wildcards meet both in their order",
        at + 1,
        one.get(at).unwrap_or(&""),
        two.get(at).unwrap_or(&"")
    )
}

/// Whether `line` meets the oracle's line in both runs.
fn meets(line: &Line, one: &str, two: &str) -> bool {
    line.meets(one.as_bytes()) && line.meets(two.as_bytes())
}

/// How one cell of [`aligned`]'s table was reached: the oracle's line
/// written, an authored line passed over, or one kept over the oracle
/// lines up to the end it names.
#[derive(Clone, Copy)]
enum Step {
    Oracle,
    Pass,
    Keep(usize),
}

/// The stream's lines with the most authored wildcard lines kept, each
/// over the oracle lines it meets in both runs, one, or a run of them
/// for a line that may stand for more than one ([`Line::spans`]); every
/// other oracle line the two runs print alike, as the first prints it.
/// None where some line the runs print apart is met by no kept line. The
/// table is the longest common subsequence's, over a fence's lines.
fn aligned(
    rules: &Normalise,
    wildcards: &[Option<(&Line, &str)>],
    one: &[&str],
    two: &[&str],
) -> Option<Vec<String>> {
    let (n, m) = (one.len(), wildcards.len());
    // most[i][k]: the most authored lines kept over oracle lines i.. and
    // authored lines k.., none where the rest cannot be written.
    let mut most = vec![vec![None::<usize>; m + 1]; n + 1];
    let mut step = vec![vec![Step::Pass; m + 1]; n + 1];
    for i in (0..=n).rev() {
        for k in (0..=m).rev() {
            if i == n {
                most[i][k] = Some(0);
                continue;
            }
            let mut best: Option<(usize, Step)> = None;
            let mut offer = |kept: Option<usize>, how: Step| {
                if let Some(kept) = kept
                    && best.is_none_or(|(had, _)| kept > had)
                {
                    best = Some((kept, how));
                }
            };
            if one[i] == two[i] {
                offer(most[i + 1][k], Step::Oracle);
            }
            if k < m {
                offer(most[i][k + 1], Step::Pass);
                if let Some((line, _)) = wildcards[k] {
                    let mut end = i;
                    while end < n && (end == i || line.spans()) && meets(line, one[end], two[end]) {
                        end += 1;
                        offer(most[end][k + 1].map(|kept| kept + 1), Step::Keep(end));
                    }
                }
            }
            if let Some((kept, how)) = best {
                most[i][k] = Some(kept);
                step[i][k] = how;
            }
        }
    }
    most[0][0]?;
    let (mut i, mut k, mut out) = (0, 0, Vec::new());
    while i < n {
        match step[i][k] {
            Step::Oracle => {
                out.push(literal(rules, one[i]));
                i += 1;
            }
            Step::Pass => k += 1,
            Step::Keep(end) => {
                out.push(wildcards[k]?.1.to_owned());
                (i, k) = (end, k + 1);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::kept;
    use crate::oracle::Normalise;

    /// The recipe's normalisation as `oracle.toml` writes it.
    fn rules() -> Normalise {
        Normalise {
            crlf_to_lf: true,
            trim: " \t\n\r\u{0}\u{b}".to_owned(),
            scratch_token: "%SCRATCH%".to_owned(),
            extension_number_token: Some("%N%".to_owned()),
        }
    }

    fn body(authored: &str, one: &str, two: &str) -> Result<String, String> {
        kept(&rules(), "stdout", authored, [one, two])
    }

    /// An authored line whose wildcards the oracle's still meets is kept
    /// as written, in both runs where they differ on it, and every other
    /// line is the oracle's: a line the oracle moved, one it added and one
    /// it dropped.
    #[test]
    fn a_wildcard_line_the_oracle_still_meets_is_kept_and_the_rest_are_the_oracle_s() {
        let authored = "uno\nstring(%d) \"%s\"\ngone\nPID %d";
        let one = "one\nstring(31) \"%SCRATCH%/witness.php\"\nadded\nPID 41";
        let two = "one\nstring(40) \"%SCRATCH%/witness.php\"\nadded\nPID 42";
        assert_eq!(
            body(authored, one, two),
            Ok("one\nstring(%d) \"%s\"\nadded\nPID %d".to_owned())
        );
        assert_eq!(body("", "", ""), Ok(String::new()));
        assert_eq!(
            body("\n\n  %d apples", "3 apples", "3 apples"),
            Ok("  %d apples".to_owned()),
            "the authored line the trim leaves is the one kept"
        );
        assert_eq!(
            body(
                "extension #%d ctype",
                "extension #%N% ctype",
                "extension #%N% ctype"
            ),
            Ok("extension #%d ctype".to_owned()),
            "matched as the judge matches, under the recipe's tokens"
        );
    }

    /// A line of the oracle's written as the oracle's is written as text
    /// that matches it alone: a `%s` it printed is no wildcard in the
    /// fence, and the recipe's scratch token stays the token.
    #[test]
    fn an_oracle_line_is_written_to_match_what_it_printed_alone() {
        assert_eq!(
            body("x", "100%|%s|x", "100%|%s|x"),
            Ok("100%|%r[%]%rs|x".to_owned())
        );
        assert_eq!(
            body("x", "%SCRATCH%/a %a", "%SCRATCH%/a %a"),
            Ok("%SCRATCH%/a %r[%]%ra".to_owned())
        );
    }

    /// A line that may stand for more than one, `%a`, is kept over the run
    /// of oracle lines it meets; one that may not is kept over one.
    #[test]
    fn a_spanning_wildcard_is_kept_over_the_lines_it_meets() {
        let (one, two) = ("head\nx 1\nx 2\ntail", "head\nx 3\nx 4\ntail");
        assert_eq!(
            body("head\n%a\ntail", one, two),
            Ok("head\n%a\ntail".to_owned())
        );
        assert!(
            body("head\nx %d\ntail", one, two).is_err(),
            "one line, two moved"
        );
    }

    /// A line the two runs differ on that no authored wildcard meets is
    /// refused naming it, and so is a count of lines the runs differ in.
    #[test]
    fn a_line_the_two_runs_differ_on_is_refused_unless_a_kept_line_meets_it() {
        let refused = body("stale", "string(31) \"x\"", "string(40) \"x\"").unwrap_err();
        assert!(
            refused.starts_with(
                "stdout line 1 is `string(31) \"x\"` from one scratch directory and `string(40) \"x\"` from another"
            ),
            "{refused}"
        );
        let refused = body("%d", "1\n2", "1").unwrap_err();
        assert_eq!(
            refused,
            "stdout is 2 lines from one scratch directory and 1 from another"
        );
    }
}
