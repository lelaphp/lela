//! What a server arm's processes retired per request over a rep, beside
//! the rate the rep measured (#3013): instructions, cycles, CPU time and
//! runnable time, the time a request ran or waited for a CPU. On a shared
//! machine a makala bar's rate moves with the load while these hold to a
//! fraction of a percent (#3010), so a reading taken under load states a
//! cost the spread does not hide.
//!
//! Each is read from `proc_pid_rusage` summed over the server's processes,
//! makala's one and php-fpm's master and children, before and after a rep;
//! their difference divided by the requests answered in the rep is the
//! rep's, and an arm's figure is the median over its reps. The call is
//! macOS's, read in `read`, xtask's one `unsafe` home (#3062's ruling);
//! elsewhere every server arm says its figures are absent.

use super::stats;

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "seams.toml rule unsafe.homes: xtask::bench::spent::read"
)]
mod read;

/// The four figures, in the order [`Retired`] and [`Spent::PerRequest`]
/// keep them: each one's key under an arm's prefix in a reading, and what
/// one request's is divided by to be stated in it, a count or
/// milliseconds of nanoseconds.
const FIGURES: [(&str, f64); 4] = [
    ("instructions", 1.0),
    ("cycles", 1.0),
    ("cpu_ms", 1e6),
    ("runnable_ms", 1e6),
];

/// Why a server arm states none of its figures: on macOS, a read of its
/// server's process that failed, the process gone or not the reader's to
/// read; elsewhere, a platform without the call.
#[cfg(target_os = "macos")]
pub(super) const ABSENT: &str = "proc_pid_rusage could not read the server's process";
#[cfg(not(target_os = "macos"))]
pub(super) const ABSENT: &str = "this platform has no proc_pid_rusage";

/// What a server's processes had retired when read, summed over them:
/// instructions, cycles, CPU and runnable nanoseconds, in [`FIGURES`]'
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retired([u64; 4]);

impl Retired {
    /// The process `pid` and its children as they stand, or none where
    /// they cannot be read.
    #[cfg(target_os = "macos")]
    pub(crate) fn of(pid: u32) -> Option<Retired> {
        read::totals(pid).map(Retired)
    }

    /// None: this platform has no `proc_pid_rusage`.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn of(_pid: u32) -> Option<Retired> {
        None
    }

    /// What each of `answered` requests retired from `self` to `after`; a
    /// total that went back, a process of the server having ended in the
    /// rep, is no figure at all.
    #[allow(clippy::cast_precision_loss)]
    fn per_request(self, after: Retired, answered: u32) -> Spent {
        let mut per = [0.0; 4];
        for (i, (_, scale)) in FIGURES.iter().enumerate() {
            let Some(spent) = after.0[i].checked_sub(self.0[i]) else {
                return Spent::Absent("a process of the server ended during the rep");
            };
            per[i] = spent as f64 / scale / f64::from(answered.max(1));
        }
        Spent::PerRequest(per)
    }
}

/// What an arm states of what its rep cost beside its time or rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Spent {
    /// An arm that is a process of its own, its run its whole rep: it
    /// states its time and nothing beside it.
    Unasked,
    /// A server arm whose figures this build or this rep does not have,
    /// and why.
    Absent(&'static str),
    /// A server arm's figures per request, in [`FIGURES`]' order.
    PerRequest([f64; 4]),
}

impl Spent {
    /// What a server arm's rep cost, `answered` requests between the reads
    /// `before` and `after` of its processes.
    pub(crate) fn over(before: Option<Retired>, after: Option<Retired>, answered: u32) -> Spent {
        match before.zip(after) {
            Some((before, after)) => before.per_request(after, answered),
            None => Spent::Absent(ABSENT),
        }
    }

    /// An arm's reps as one: each figure's median where every rep has
    /// them, the first absence where one has not.
    pub(crate) fn median(reps: &[Spent]) -> Spent {
        let mut per = Vec::new();
        for rep in reps {
            match rep {
                Spent::PerRequest(figures) => per.push(*figures),
                other => return *other,
            }
        }
        if per.is_empty() {
            return Spent::Unasked;
        }
        Spent::PerRequest(std::array::from_fn(|i| {
            stats(&per.iter().map(|f| f[i]).collect::<Vec<_>>()).median
        }))
    }

    /// What an arm's line and its reading's message say it spent per
    /// request; none for an arm that states nothing.
    pub(crate) fn text(self) -> Option<String> {
        match self {
            Spent::Unasked => None,
            Spent::Absent(why) => Some(format!("absent: {why}")),
            Spent::PerRequest([instructions, cycles, cpu, runnable]) => Some(format!(
                "{:.1}M instructions, {:.1}M cycles, {cpu:.2} CPU ms, {runnable:.2} runnable ms",
                instructions / 1e6,
                cycles / 1e6
            )),
        }
    }

    /// The figures as a reading keeps them under the arm's `prefix`.
    pub(crate) fn numbers(self, prefix: &str) -> Vec<(String, f64)> {
        let Spent::PerRequest(figures) = self else {
            return Vec::new();
        };
        (FIGURES.iter().zip(figures))
            .map(|((key, _), n)| (format!("{prefix}_{key}"), n))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rep's figures are its totals' difference over the requests it
    /// answered, times in milliseconds: 4 requests that retired 8,000M
    /// instructions, 1,000M cycles, 240 ms of CPU and 400 ms runnable are
    /// 2,000M, 250M, 60 ms and 100 ms each; a total that went back states
    /// none, and so does a read this build cannot make.
    #[test]
    fn a_rep_s_figures_are_its_totals_difference_over_the_requests_answered() {
        let before = Retired([1_000, 2_000, 5_000_000, 7_000_000]);
        let after = Retired([8_000_001_000, 1_000_002_000, 245_000_000, 407_000_000]);
        let spent = Spent::over(Some(before), Some(after), 4);
        assert_eq!(spent, Spent::PerRequest([2e9, 2.5e8, 60.0, 100.0]));
        let text = spent.text().unwrap();
        assert!(
            text.contains("2000.0M instructions, 250.0M cycles, 60.00 CPU ms"),
            "{text}"
        );
        assert_eq!(spent.numbers("a")[2], ("a_cpu_ms".to_owned(), 60.0));
        let ended = Spent::over(Some(after), Some(before), 4);
        assert!(matches!(ended, Spent::Absent(why) if why.contains("ended")));
        assert_eq!(Spent::over(None, Some(after), 4), Spent::Absent(ABSENT));
    }

    /// An arm's figure is each one's median over its reps; one rep without
    /// figures makes the arm's absent, saying why, and its reading keeps
    /// no number of them; a process arm states nothing.
    #[test]
    fn an_arm_states_each_figure_s_median_or_says_why_it_has_none() {
        let reps = [1.0, 3.0, 2.0].map(|n| Spent::PerRequest([n, n * 10.0, n, n]));
        assert_eq!(
            Spent::median(&reps),
            Spent::PerRequest([2.0, 20.0, 2.0, 2.0])
        );
        let absent = [reps[0], Spent::Absent(ABSENT), reps[1]];
        let arm = Spent::median(&absent);
        assert_eq!(arm, Spent::Absent(ABSENT));
        let text = arm.text().unwrap();
        assert_eq!(text, format!("absent: {ABSENT}"));
        assert!(arm.numbers("b").is_empty());
        assert_eq!(Spent::median(&[Spent::Unasked]), Spent::Unasked);
        assert_eq!(Spent::Unasked.text(), None);
    }
}
