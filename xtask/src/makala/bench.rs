//! The production server's bars of `xtask bench`: makala against
//! php-fpm behind a `FastCGI` client, over each workload
//! `docs/perf/workloads.toml` declares in a role served on the web, every
//! role but a native, to run as a file with no arguments (measurement
//! protocol §11, [`workloads`]), and makala against itself over the
//! same, the A/A control that says what the rig can resolve at that
//! scale. A native workload has no bar, as ruled 2026-11-09: its time is
//! spent in an extension's natives, and what a served request adds to it
//! the other workloads' bars measure already.
//!
//! **The arms.** One makala, built in `release`, the bench lane's profile,
//! serving the workload as the recipe's `script_name` in a directory of its
//! own; and one pool of the pinned php-fpm handed the same request under
//! the pin's `[fastcgi]` mapping (`witness::makala::Fpm`), the client a web
//! server in front of it would be. Each is started once for the bar and a
//! rep is one request: the connection, the request sent, the response read
//! to its end, timed as the client pays for it. The two clients are what
//! each host is reached by: HTTP/1.0 over loopback TCP for makala, and
//! `FastCGI` over a Unix socket for php-fpm, with no web server in front,
//! so the series times makala's HTTP against php-fpm's `FastCGI` and not
//! against nginx and php-fpm together. What the two arms' reps
//! are held equal by is the response's body, the bytes PHP wrote, since
//! the heads differ by what each host is. Both run under the recipe's ini
//! with php.ini-production's values for the two entries the recipe sets
//! for correctness and that cost every request:
//! `zend.assertions=-1` and `opcache.protect_memory=0` ([`PRODUCTION`]).
//!
//! **Two loads** ([`Load`]). `makala-fpm/<workload>` sends one request a
//! rep from one client, so it measures latency, against a makala at its
//! default request threads and a pool of one child. `makala-fpm-c8/<workload>`
//! is §9.2's worker-throughput bar, "8 threads, one process ≥ php-fpm
//! with 8 workers" (ruled 2026-10-11): [`CLIENTS`] clients a rep, each
//! asking until [`ASKING`] have passed and finishing the request it has
//! in flight, against a makala of [`CLIENTS`] request threads and a pool of
//! `pm = static`, `pm.max_children` [`CLIENTS`], behind the same clients as
//! the latency series, one connection a request. Each arm's rep is the
//! requests a second it was answered at over the time its last client
//! took to end, and every answer of the rep carries the body the rep is
//! held to (§5), so a rep is refused on the first answer that differs.
//!
//! **The heap's opt-in** (ruling 4 of 2026-11-09). `makala-zeroed-c8/<workload>`
//! is the throughput load against two makalas, the first started with
//! [`ZEROED`] set, so each request's heap zeroes the pages it touched
//! before its chunks are kept, and the second without: what the opt-in
//! costs, the measurement a person reads before it could ever become a
//! default. It is the throughput load because the zeroing runs as a
//! request's heap ends, after its response is written, where one client
//! waiting on one request would not pay for it; eight clients on eight
//! request threads do, and each arm's rep reads what its server retired.
//! Its A/A control is `makala-c8-aa/<workload>`, under the same load.
//!
//! **How each is held.** Each series is held to no bound until a person
//! writes one in the manifest once the nightly series is read (ruled
//! 2026-10-09, after the ruling of 2026-10-07 on the engine's bars); its
//! reading states the paired delta and how far makala stands behind
//! php-fpm, harness §3's "`FastCGI` ≥ php-fpm" as the distance to go.
//! `makala-aa/<workload>` and `makala-c8-aa/<workload>` are their A/A
//! controls, makala against itself under the same load. A machine without
//! the pinned php-fpm measures none of them, each a skip that says so.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::{Entry, workloads};
use crate::bench::spent::{Retired, Spent};
use crate::bench::{Arms, Declared, Held, PRODUCTION, PROTOCOL, Sample, Side, Sides, Stream, Unit};
use crate::manifest::Manifest;
use crate::oracle::hosts::MAKALA;
use crate::oracle::profile;
use crate::scratch::Tree;
use crate::witness::makala::{Fpm, Http, Ports, Server, split_head};

/// [`PRODUCTION`] as the hosts' starts take it.
fn ini() -> Vec<String> {
    PRODUCTION.map(str::to_owned).to_vec()
}

/// How a rep asks a bar's hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Load {
    /// One request, its milliseconds as its client pays for them.
    One,
    /// [`CLIENTS`] clients for [`ASKING`], the requests a second they
    /// were answered at.
    Clients,
}

/// The clients of a [`Load::Clients`] rep, and the request threads and
/// php-fpm children each host has for them (§9.2).
const CLIENTS: usize = 8;

/// How long the clients of a rep ask, each then finishing the request it
/// has in flight.
const ASKING: Duration = Duration::from_secs(3);

impl Load {
    /// What a rep measures under the load.
    fn unit(self) -> Unit {
        match self {
            Load::One => Unit::Ms,
            Load::Clients => Unit::Rps,
        }
    }

    /// The hosts the load is asked of: makala's request threads, its
    /// default where none, and php-fpm's children.
    fn hosts(self) -> (Option<usize>, usize) {
        match self {
            Load::One => (None, 1),
            Load::Clients => (Some(CLIENTS), CLIENTS),
        }
    }

    /// What a bar's `about` says of its clients.
    fn clients(self) -> String {
        match self {
            Load::One => String::new(),
            Load::Clients => format!(", {CLIENTS} clients for {}s a rep", ASKING.as_secs()),
        }
    }
}

/// What a bar's makala is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Against {
    /// The pinned php-fpm behind a `FastCGI` client: a series.
    Fpm,
    /// Itself: the A/A control.
    Itself,
    /// A makala whose heap zeroes nothing, the first started with
    /// [`ZEROED`]: a series.
    Unzeroed,
}

impl Against {
    /// How a bar against this is held.
    fn held(self) -> Held {
        match self {
            Against::Itself => Held::Control,
            Against::Fpm | Against::Unzeroed => Held::Series,
        }
    }

    /// What a bar's `about` names its makala and its second arm by.
    fn said(self) -> &'static str {
        match self {
            Against::Fpm => "makala against php-fpm behind a FastCGI client",
            Against::Itself => "makala against itself",
            Against::Unzeroed => {
                "makala zeroing the pages each request touched against makala zeroing none"
            }
        }
    }
}

/// The variable `php_heap` reads once per process to zero the pages each
/// request touched before its chunks are kept, set to a number that is
/// not zero: a copy, since the harness links no engine crate, which
/// [`tests`] holds to `php_heap`'s.
const ZEROED: &str = "LELA_ZERO_TOUCHED_PAGES";

/// Each workload's bars: the name before the workload's, what its makala
/// is measured against and the load each rep asks under.
const BARS: [(&str, Against, Load); 5] = [
    ("makala-fpm", Against::Fpm, Load::One),
    ("makala-aa", Against::Itself, Load::One),
    ("makala-fpm-c8", Against::Fpm, Load::Clients),
    ("makala-c8-aa", Against::Itself, Load::Clients),
    ("makala-zeroed-c8", Against::Unzeroed, Load::Clients),
];

/// The bars over each workload of the workspace at `root`: makala against
/// php-fpm, a series, and makala against itself, its A/A control, under
/// each [`Load`].
pub(crate) fn declared(root: &Path) -> Result<Vec<Declared>> {
    let mut bars = Vec::new();
    for entry in workloads(root)? {
        let name = Path::new(&entry.id).file_stem().map_or_else(
            || entry.id.clone(),
            |stem| stem.to_string_lossy().into_owned(),
        );
        for (bar, against, load) in BARS {
            let (root, entry) = (root.to_path_buf(), entry.clone());
            bars.push(Declared {
                id: format!("{bar}/{name}"),
                about: format!("{} over {}{}", against.said(), entry.id, load.clients()),
                held: against.held(),
                start: Box::new(move || sides(&root, &entry, against, load)),
            });
        }
    }
    Ok(bars)
}

/// The arms of a bar over `entry`: makala's and the pinned php-fpm's for a
/// series against php-fpm, two of one makala for a control, and a makala
/// started with [`ZEROED`] and one without for the opt-in's series, each
/// measured only where php-fpm's is, so a control is never read beside no
/// series nor a series beside no control.
fn sides(root: &Path, entry: &Entry, against: Against, load: Load) -> Result<Sides> {
    let manifest = Manifest::load(root)?;
    let pin = manifest.pin()?;
    let Some(fpm) = (pin.sapi.fpm.as_ref()).filter(|fpm| fpm.path.is_file()) else {
        return Ok(Err(NO_FPM.to_owned()));
    };
    let oracle = pin.oracle();
    oracle.host_asserted(fpm)?;
    let (threads, children) = load.hosts();
    let served = Arc::new(Served::start(root, &manifest, entry, threads, &[])?);
    let makala = |name, served: &Arc<Served>| -> Box<dyn Side> {
        let (pid, program) = (served.server.pid(), served.binary.clone());
        let served = Arc::clone(served);
        let ask = Box::new(move || served.body());
        Box::new(ByRequest {
            name,
            program,
            load,
            ask,
            pid,
        })
    };
    match against {
        Against::Itself => {
            let pair = [makala("makala-a", &served), makala("makala-b", &served)];
            return Ok(Ok(Arms::Pair(pair)));
        }
        Against::Unzeroed => {
            let zeroed = [(ZEROED.to_owned(), OsString::from("1"))];
            let zeroing = Arc::new(Served::start(root, &manifest, entry, threads, &zeroed)?);
            let pair = [makala("makala-zeroed", &zeroing), makala("makala", &served)];
            return Ok(Ok(Arms::Pair(pair)));
        }
        Against::Fpm => {}
    }
    let (program, http) = (fpm.path.clone(), &entry.http);
    let pool = Fpm::start(
        &oracle,
        &program,
        &served.program(),
        http,
        (&ini(), Ports::default()),
        children,
    )?;
    let pid = pool.pid();
    let ask = Box::new(move || fpm_body(pool.ask()?));
    let fpm = ByRequest {
        name: "php-fpm",
        program,
        load,
        ask,
        pid,
    };
    Ok(Ok(Arms::Pair([makala("makala", &served), Box::new(fpm)])))
}

/// Why a bar of the production server is not measured on this machine.
const NO_FPM: &str = "this machine has not the pinned php-fpm oracle.toml's [sapi] names";

/// One makala over a directory holding the workload as the recipe's
/// `script_name`, kept for every rep of a bar; the directory goes after
/// the server.
struct Served {
    server: Server,
    binary: PathBuf,
    http: Http,
    script: String,
    timeout: Duration,
    own: Tree,
}

impl Served {
    /// The makala, with `threads` request threads or its default, and
    /// `env` over the recipe's environment.
    fn start(
        root: &Path,
        manifest: &Manifest,
        entry: &Entry,
        threads: Option<usize>,
        env: &[(String, OsString)],
    ) -> Result<Served> {
        let recipe = &manifest.pin()?.recipe;
        let binary =
            profile::released(root, (&MAKALA.package(), MAKALA.binary), None, (None, None))?;
        let own = Tree::new("bench-makala")?;
        own.write(&entry.script, &entry.program)?;
        let log = own.path().join("errors");
        let server = Server::start(recipe, &binary, own.path(), &ini(), threads, &log, env)?;
        Ok(Served {
            server,
            binary,
            http: entry.http.clone(),
            script: entry.script.clone(),
            timeout: Duration::from_secs(recipe.limits.timeout_s),
            own,
        })
    }

    fn program(&self) -> PathBuf {
        self.own.join(&self.script)
    }

    /// The workload's request sent once more: the response's body,
    /// refused where its status is not 200.
    fn body(&self) -> Result<Vec<u8>> {
        let (answer, _) = self.server.ask(&self.http, &self.script, self.timeout)?;
        let (head, body) = split_head(&answer);
        let status = head.lines().next().unwrap_or_default();
        if status.split(' ').nth(1) != Some("200") {
            bail!("answered {status:?}: {}", String::from_utf8_lossy(body));
        }
        Ok(body.to_vec())
    }
}

/// What php-fpm wrote on `FCGI_STDOUT`, its head off, refused where its
/// `Status:` is not 200.
fn fpm_body((stdout, stderr): (Vec<u8>, Vec<u8>)) -> Result<Vec<u8>> {
    let (head, body) = split_head(&stdout);
    if let Some(status) = head.lines().find_map(|line| line.strip_prefix("Status: "))
        && !status.starts_with("200")
    {
        bail!(
            "answered {status:?}: {}{}",
            String::from_utf8_lossy(body),
            String::from_utf8_lossy(&stderr)
        );
    }
    Ok(body.to_vec())
}

/// An arm that asks a host the workload's request, `ask` giving the
/// response's body, under its bar's [`Load`], and reads what the server's
/// process `pid` and its children retired across each rep.
struct ByRequest {
    name: &'static str,
    program: PathBuf,
    load: Load,
    ask: Box<dyn Fn() -> Result<Vec<u8>> + Sync>,
    pid: u32,
}

impl Side for ByRequest {
    fn name(&self) -> &str {
        self.name
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn stream(&self) -> Stream {
        Stream::Body
    }

    fn unit(&self) -> Unit {
        self.load.unit()
    }

    fn once(&mut self) -> Result<Sample> {
        let before = Retired::of(self.pid);
        let start = Instant::now();
        let sample = match self.load {
            Load::One => (self.ask)()
                .map(|wrote| (Sample::of(start.elapsed().as_secs_f64() * 1000.0, wrote), 1)),
            Load::Clients => clients(&*self.ask, CLIENTS, ASKING),
        };
        let (sample, answered) = sample.with_context(|| format!("the arm {}", self.name))?;
        let spent = Spent::over(before, Retired::of(self.pid), answered);
        Ok(Sample { spent, ..sample })
    }
}

/// `count` clients asking `ask` until `time` has passed since they
/// began, each finishing the request it has in flight: the requests a
/// second the rep was answered at, over the time its last client took to
/// end, and the body every answer carried, an answer that carried another
/// refusing the rep (§5); and how many were answered.
fn clients(
    ask: &(dyn Fn() -> Result<Vec<u8>> + Sync),
    count: usize,
    time: Duration,
) -> Result<(Sample, u32)> {
    let start = Instant::now();
    let until = start + time;
    let each: Vec<(u32, Vec<u8>)> = std::thread::scope(|scope| {
        let asking: Vec<_> = (0..count)
            .map(|_| {
                scope.spawn(|| -> Result<(u32, Vec<u8>)> {
                    let (mut answered, body) = (1, ask()?);
                    while Instant::now() < until {
                        if ask()? != body {
                            bail!(
                                "two answers of one rep carried different bodies ({PROTOCOL} §5)"
                            );
                        }
                        answered += 1;
                    }
                    Ok((answered, body))
                })
            })
            .collect();
        let joined = asking.into_iter().map(std::thread::ScopedJoinHandle::join);
        joined
            .map(|client| client.map_err(|_| anyhow!("a client panicked"))?)
            .collect::<Result<Vec<_>>>()
    })?;
    let seconds = start.elapsed().as_secs_f64();
    let wrote = each
        .first()
        .map(|(_, body)| body.clone())
        .unwrap_or_default();
    if each.iter().any(|(_, body)| *body != wrote) {
        bail!("two clients of one rep were answered different bodies ({PROTOCOL} §5)");
    }
    let answered: u32 = each.iter().map(|(n, _)| n).sum();
    Ok((Sample::of(f64::from(answered) / seconds, wrote), answered))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each workload is four bars: the latency series against php-fpm and
    /// its A/A control, and the throughput series at [`CLIENTS`] clients
    /// and its control, named by the workload's stem and stating their
    /// clients; a native workload has none, the six `uri-*` and
    /// the three profiled programs among them, nor one declared
    /// with a pinned file or code of its own.
    #[cfg(tracked)]
    #[test]
    fn each_workload_is_a_latency_and_a_throughput_series_each_with_its_control() {
        let root = crate::package_dir().join("..");
        let bars = declared(&root).unwrap();
        let named: Vec<(&str, Held)> = bars.iter().map(|bar| (bar.id.as_str(), bar.held)).collect();
        for (bar, held) in [
            ("makala-fpm/ucall", Held::Series),
            ("makala-aa/ucall", Held::Control),
            ("makala-fpm-c8/ucall", Held::Series),
            ("makala-c8-aa/ucall", Held::Control),
            ("makala-zeroed-c8/ucall", Held::Series),
            ("makala-fpm/bench_quiet", Held::Series),
        ] {
            assert!(named.contains(&(bar, held)), "{bar}: {named:?}");
        }
        assert_eq!(bars.len(), 5 * workloads(&root).unwrap().len());
        let unserved = [
            "fileinfo",
            "compression",
            "mbstring_profiles_2831",
            "uri-rfc-read-update",
            "uri-rfc-unicode",
            "uri-rfc-validation",
            "uri-whatwg-read-update",
            "uri-whatwg-unicode",
            "uri-whatwg-validation",
            "micro_bench",
            "loop-add",
        ];
        for unserved in unserved {
            let over = format!("/{unserved}");
            let any = bars.iter().find(|bar| bar.id.ends_with(&over));
            assert!(any.is_none(), "{}", any.map_or("", |bar| &bar.id));
        }
        let c8 = bars
            .iter()
            .find(|bar| bar.id == "makala-fpm-c8/ucall")
            .unwrap();
        assert!(
            c8.about.ends_with(", 8 clients for 3s a rep"),
            "{}",
            c8.about
        );
        assert_eq!(
            BARS.map(|(_, _, load)| load == Load::Clients),
            [false, false, true, true, true]
        );
    }

    /// The opt-in's bar measures something only while its first makala is
    /// started with the variable `php_heap` reads: a rename there that
    /// left this copy behind would make the bar an A/A control in all but
    /// its name, so the copy is held to `php_heap`'s constant, and its
    /// arms named for what each is.
    #[cfg(tracked)]
    #[test]
    fn the_opt_ins_bar_starts_its_first_makala_with_the_variable_php_heap_reads() {
        let root = crate::package_dir().join("..");
        let cache = std::fs::read_to_string(root.join("crates/php_heap/src/mm/cache.rs")).unwrap();
        let declared = format!("pub(super) const ZEROED: &str = {ZEROED:?};");
        assert!(cache.contains(&declared), "php_heap declares no {declared}");
        assert_eq!(Against::Unzeroed.held(), Held::Series);
        assert_eq!(Against::Itself.held(), Held::Control);
        let (_, against, load) = BARS[4];
        assert_eq!((against, load), (Against::Unzeroed, Load::Clients));
        assert!(Against::Unzeroed.said().starts_with("makala zeroing"));
    }

    /// An arm of the latency series is a time and one of the throughput
    /// series a rate, asked of a makala of 8 request threads and a pool of
    /// 8 children, where the latency series asks makala's default and one.
    #[test]
    fn a_throughput_arm_is_8_clients_reading_requests_a_second() {
        let arm = |load| ByRequest {
            name: "a",
            program: PathBuf::new(),
            load,
            ask: Box::new(|| Ok(b"x".to_vec())),
            pid: std::process::id(),
        };
        assert_eq!(
            (arm(Load::One).unit(), arm(Load::Clients).unit()),
            (Unit::Ms, Unit::Rps)
        );
        assert_eq!(
            (Load::One.hosts(), Load::Clients.hosts()),
            ((None, 1), (Some(8), 8))
        );
    }

    /// The clients' requests a second are those answered over the time the
    /// last client took: 4 clients each answered every 10 ms for 50 ms
    /// read about 400 a second, the body the one they all carried; an
    /// answer of another body refuses the rep.
    #[test]
    fn the_clients_read_the_requests_a_second_they_were_answered_at() {
        let slow = || {
            std::thread::sleep(Duration::from_millis(10));
            Ok(b"body".to_vec())
        };
        let (sample, answered) = clients(&slow, 4, Duration::from_millis(50)).unwrap();
        assert!((8..=21).contains(&answered), "{answered}");
        assert_eq!(sample.wrote, b"body");
        assert!((150.0..=420.0).contains(&sample.value), "{}", sample.value);
        let asked = std::sync::atomic::AtomicU32::new(0);
        let moving = || {
            let n = asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(1));
            Ok(if n == 5 {
                b"other".to_vec()
            } else {
                b"body".to_vec()
            })
        };
        let Err(refused) = clients(&moving, 2, Duration::from_millis(30)) else {
            panic!("a rep whose answers differ is refused");
        };
        assert!(
            format!("{refused}").contains("different bodies"),
            "{refused}"
        );
    }
}
