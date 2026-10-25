//! What an arm's processes retired beside the time or rate a rep
//! measured: instructions, cycles, CPU time and runnable time, the time a
//! process ran or waited for a CPU. On a shared machine a makala bar's
//! rate moves with the load while these hold to a fraction of a percent
//! (#3010), so a reading taken under load states a cost the spread does
//! not hide (#3013); and a process arm that asks for them states what one
//! opline of its run retired, the `step` bar's cost in instructions
//! (#2946).
//!
//! Each is read from `proc_pid_rusage`: for a server, summed over its
//! processes, makala's one and php-fpm's master and children, before and
//! after a rep, the difference divided by the requests answered in the
//! rep; for a process arm, over its life, read once it has exited and
//! before it is reaped. An arm's figure is the median over its reps. The
//! call is macOS's, read in `read`, xtask's one `unsafe` home (#3062's
//! ruling); elsewhere every such arm says its figures are absent.

use super::stats;

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "seams.toml rule unsafe.homes: xtask::bench::spent::read"
)]
mod read;

/// The four figures, in the order [`Retired`] and [`Spent::Per`] keep
/// them: each one's key under an arm's prefix in a reading, and what one
/// request's, run's or opline's is divided by to be stated in it, a count
/// or milliseconds of nanoseconds.
const FIGURES: [(&str, f64); 4] = [
    ("instructions", 1.0),
    ("cycles", 1.0),
    ("cpu_ms", 1e6),
    ("runnable_ms", 1e6),
];

/// Why an arm states none of its figures: on macOS, a read of its process
/// that failed, the process gone or not the reader's to read; elsewhere,
/// a platform without the call.
#[cfg(target_os = "macos")]
pub(super) const ABSENT: &str = "proc_pid_rusage could not read the server's process";
#[cfg(not(target_os = "macos"))]
pub(super) const ABSENT: &str = "this platform has no proc_pid_rusage";
/// [`ABSENT`] for a process arm.
#[cfg(target_os = "macos")]
const ABSENT_RUN: &str = "proc_pid_rusage could not read the arm's process";
#[cfg(not(target_os = "macos"))]
const ABSENT_RUN: &str = ABSENT;

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

    /// What the child `pid` retired over its life, and its peak physical
    /// footprint in bytes, waited for until it exits and left unreaped for
    /// its owner's wait; none where it cannot be read.
    #[cfg(target_os = "macos")]
    pub(crate) fn exited(pid: u32) -> Option<(Retired, u64)> {
        read::exited(pid).map(|(figures, peak)| (Retired(figures), peak))
    }

    /// None: this platform has no `proc_pid_rusage`.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn exited(_pid: u32) -> Option<(Retired, u64)> {
        None
    }

    /// What each of `n` units retired from `self` to `after`; a total
    /// that went back, a process of the server having ended in the rep,
    /// is no figure at all.
    #[allow(clippy::cast_precision_loss)]
    fn per(self, after: Retired, n: f64, each: Each) -> Spent {
        let mut per = [0.0; 4];
        for (i, (_, scale)) in FIGURES.iter().enumerate() {
            let Some(spent) = after.0[i].checked_sub(self.0[i]) else {
                return Spent::Absent(each, "a process of the server ended during the rep");
            };
            per[i] = spent as f64 / scale / n.max(1.0);
        }
        Spent::Per(each, per)
    }
}

/// What a figure is stated per: a request a server answered, a process
/// arm's whole run, or an opline that run executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Each {
    Request,
    Run,
    Opline,
}

impl Each {
    /// What a figure is stated per, as the text names it.
    fn as_str(self) -> &'static str {
        match self {
            Each::Request => "request",
            Each::Run => "run",
            Each::Opline => "opline",
        }
    }
}

/// What an arm states of what its rep cost beside its time or rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Spent {
    /// An arm that states its time and nothing beside it.
    Unasked,
    /// An arm whose figures, per what, this build or this rep does not
    /// have, and why.
    Absent(Each, &'static str),
    /// An arm's figures per request, run or opline, in [`FIGURES`]' order.
    Per(Each, [f64; 4]),
}

impl Spent {
    /// What a server arm's rep cost, `answered` requests between the reads
    /// `before` and `after` of its processes.
    pub(crate) fn over(before: Option<Retired>, after: Option<Retired>, answered: u32) -> Spent {
        match before.zip(after) {
            Some((before, after)) => before.per(after, f64::from(answered), Each::Request),
            None => Spent::Absent(Each::Request, ABSENT),
        }
    }

    /// What a process arm's whole run retired, as [`Retired::exited`] read
    /// it.
    pub(crate) fn run(retired: Option<Retired>) -> Spent {
        retired.map_or(Spent::Absent(Each::Run, ABSENT_RUN), |r| {
            Retired([0; 4]).per(r, 1.0, Each::Run)
        })
    }

    /// A run's figures stated per each of the `n` oplines it executed.
    pub(crate) fn per_opline(self, n: f64) -> Spent {
        match self {
            Spent::Per(Each::Run, figures) => Spent::Per(Each::Opline, figures.map(|f| f / n)),
            Spent::Absent(Each::Run, why) => Spent::Absent(Each::Opline, why),
            other => other,
        }
    }

    /// An arm's reps as one: each figure's median where every rep has
    /// them, the first absence where one has not.
    pub(crate) fn median(reps: &[Spent]) -> Spent {
        let mut per = Vec::new();
        for rep in reps {
            match rep {
                Spent::Per(each, figures) => per.push((*each, *figures)),
                other => return *other,
            }
        }
        let Some(&(each, _)) = per.first() else {
            return Spent::Unasked;
        };
        Spent::Per(
            each,
            std::array::from_fn(|i| {
                stats(&per.iter().map(|(_, f)| f[i]).collect::<Vec<_>>()).median
            }),
        )
    }

    /// What an arm's line and its reading's message say it spent, after
    /// what it is stated per; none for an arm that states nothing.
    pub(crate) fn text(self) -> Option<String> {
        let (each, said) = match self {
            Spent::Unasked => return None,
            Spent::Absent(each, why) => (each, format!("absent: {why}")),
            Spent::Per(Each::Opline, [instructions, cycles, cpu, runnable]) => (
                Each::Opline,
                format!(
                    "{instructions:.1} instructions, {cycles:.1} cycles, {:.2} CPU ns, {:.2} runnable ns",
                    cpu * 1e6,
                    runnable * 1e6
                ),
            ),
            Spent::Per(each, [instructions, cycles, cpu, runnable]) => (
                each,
                format!(
                    "{:.1}M instructions, {:.1}M cycles, {cpu:.2} CPU ms, {runnable:.2} runnable ms",
                    instructions / 1e6,
                    cycles / 1e6
                ),
            ),
        };
        Some(format!("per {}: {said}", each.as_str()))
    }

    /// The figures as a reading keeps them under the arm's `prefix`.
    pub(crate) fn numbers(self, prefix: &str) -> Vec<(String, f64)> {
        let Spent::Per(_, figures) = self else {
            return Vec::new();
        };
        (FIGURES.iter().zip(figures))
            .map(|((key, _), n)| (format!("{prefix}_{key}"), n))
            .collect()
    }

    /// The instructions the figures state, where they state any.
    pub(crate) fn instructions(self) -> Option<f64> {
        match self {
            Spent::Per(_, [instructions, ..]) => Some(instructions),
            _ => None,
        }
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
        assert_eq!(spent, Spent::Per(Each::Request, [2e9, 2.5e8, 60.0, 100.0]));
        let text = spent.text().unwrap();
        assert!(
            text.contains("2000.0M instructions, 250.0M cycles, 60.00 CPU ms"),
            "{text}"
        );
        assert_eq!(spent.numbers("a")[2], ("a_cpu_ms".to_owned(), 60.0));
        let ended = Spent::over(Some(after), Some(before), 4);
        assert!(matches!(ended, Spent::Absent(_, why) if why.contains("ended")));
        assert_eq!(
            Spent::over(None, Some(after), 4),
            Spent::Absent(Each::Request, ABSENT)
        );
    }

    /// An arm's figure is each one's median over its reps; one rep without
    /// figures makes the arm's absent, saying why, and its reading keeps
    /// no number of them; a process arm states nothing.
    #[test]
    fn an_arm_states_each_figure_s_median_or_says_why_it_has_none() {
        let reps = [1.0, 3.0, 2.0].map(|n| Spent::Per(Each::Request, [n, n * 10.0, n, n]));
        assert_eq!(
            Spent::median(&reps),
            Spent::Per(Each::Request, [2.0, 20.0, 2.0, 2.0])
        );
        let absent = [reps[0], Spent::Absent(Each::Request, ABSENT), reps[1]];
        let arm = Spent::median(&absent);
        assert_eq!(arm, Spent::Absent(Each::Request, ABSENT));
        let text = arm.text().unwrap();
        assert_eq!(text, format!("per request: absent: {ABSENT}"));
        assert!(arm.numbers("b").is_empty());
        assert_eq!(Spent::median(&[Spent::Unasked]), Spent::Unasked);
        assert_eq!(Spent::Unasked.text(), None);
    }

    /// A process arm's run states what it retired over its life, and the
    /// `step` bar each figure over the oplines the run executed (#2946):
    /// 2,000 instructions over 400 oplines are 5 per opline, and the
    /// figures a run could not read stay absent.
    #[test]
    fn a_run_s_figures_are_stated_per_opline_over_the_oplines_it_executed() {
        let run = Spent::run(Some(Retired([2_000, 4_000, 800_000, 1_200_000])));
        assert_eq!(run, Spent::Per(Each::Run, [2_000.0, 4_000.0, 0.8, 1.2]));
        let opline = run.per_opline(400.0);
        assert_eq!(opline, Spent::Per(Each::Opline, [5.0, 10.0, 0.002, 0.003]));
        let text = opline.text().unwrap();
        assert_eq!(
            text,
            "per opline: 5.0 instructions, 10.0 cycles, 2000.00 CPU ns, 3000.00 runnable ns"
        );
        assert_eq!(opline.instructions(), Some(5.0));
        assert_eq!(
            Spent::run(None).per_opline(400.0),
            Spent::Absent(Each::Opline, ABSENT_RUN)
        );
    }
}
