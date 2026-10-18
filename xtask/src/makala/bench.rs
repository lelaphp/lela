//! The production server's bars of `xtask bench` (#2351): makala against
//! php-fpm behind a `FastCGI` client, over each bench workload committed
//! under `docs/perf/workloads/` (measurement protocol §11), and makala
//! against itself over the same, the A/A control that says what the rig
//! can resolve at that scale.
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
//! the heads differ by what each host is (#2570, #2335). Both run under the
//! recipe's ini with php.ini-production's values for the two entries the
//! recipe sets for correctness and that cost every request:
//! `zend.assertions=-1` and `opcache.protect_memory=0` ([`INI`]).
//!
//! **How each is held.** `makala-fpm/<workload>` is a series, held to no
//! bound until a person writes one in the manifest once the nightly
//! series is read (#2336's ruling, after #1902's); its reading states the
//! paired delta and how far makala stands behind php-fpm, harness §3's
//! "`FastCGI` ≥ php-fpm" as the distance to go. `makala-aa/<workload>` is an
//! A/A control. A machine without the pinned php-fpm measures neither,
//! each a skip that says so.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::{Entry, workloads};
use crate::bench::{Declared, Held, Sample, Side, Sides, Stream};
use crate::manifest::Manifest;
use crate::oracle::hosts::MAKALA;
use crate::oracle::profile;
use crate::scratch::Tree;
use crate::witness::makala::{Fpm, Http, Ports, Server, split_head};

/// The ini entries both arms run under after the recipe's: the two the
/// recipe sets for correctness at a cost to every request, at
/// php.ini-production's values.
const INI: [&str; 2] = ["zend.assertions=-1", "opcache.protect_memory=0"];

/// [`INI`] as the hosts' starts take it.
fn ini() -> Vec<String> {
    INI.map(str::to_owned).to_vec()
}

/// The bars over each workload of the workspace at `root`: makala against
/// php-fpm, a series, and makala against itself, its A/A control.
pub(crate) fn declared(root: &Path) -> Result<Vec<Declared>> {
    let mut bars = Vec::new();
    for entry in workloads(root)? {
        let name = Path::new(&entry.id).file_stem().map_or_else(
            || entry.id.clone(),
            |stem| stem.to_string_lossy().into_owned(),
        );
        let (fpm_root, aa_root, aa_entry) = (root.to_path_buf(), root.to_path_buf(), entry.clone());
        bars.push(Declared {
            id: format!("makala-fpm/{name}"),
            about: format!(
                "makala against php-fpm behind a FastCGI client over {}",
                entry.id
            ),
            held: Held::Series,
            start: Box::new(move || against_fpm(&fpm_root, &entry)),
        });
        bars.push(Declared {
            id: format!("makala-aa/{name}"),
            about: format!("makala against itself over {}", aa_entry.id),
            held: Held::Control,
            start: Box::new(move || against_itself(&aa_root, &aa_entry)),
        });
    }
    Ok(bars)
}

/// makala's arm and the pinned php-fpm's over `entry`.
fn against_fpm(root: &Path, entry: &Entry) -> Result<Sides> {
    let Some(Started {
        manifest,
        fpm,
        served,
    }) = started(root, entry)?
    else {
        return Ok(Err(NO_FPM.to_owned()));
    };
    let oracle = manifest.pin.oracle();
    let ports = Ports::default();
    let pool = Fpm::start(
        &oracle,
        &fpm,
        &served.program(),
        &entry.http,
        (&ini(), ports),
    )?;
    Ok(Ok([
        Box::new(ByMakala {
            name: "makala",
            served,
        }),
        Box::new(ByFpm { pool, program: fpm }),
    ]))
}

/// Two arms of one makala over `entry`, measured only where php-fpm's is
/// too, so the control is never read beside no series.
fn against_itself(root: &Path, entry: &Entry) -> Result<Sides> {
    let Some(Started { served, .. }) = started(root, entry)? else {
        return Ok(Err(NO_FPM.to_owned()));
    };
    Ok(Ok([
        Box::new(ByMakala {
            name: "makala-a",
            served: Rc::clone(&served),
        }),
        Box::new(ByMakala {
            name: "makala-b",
            served,
        }),
    ]))
}

/// What a bar of the production server starts from: the manifest, the
/// pinned php-fpm held to its digest and the pin's closure, and a makala serving the workload.
struct Started {
    manifest: Manifest,
    fpm: PathBuf,
    served: Rc<Served>,
}

/// The start of a bar over `entry`, or none where this machine has not
/// the pinned php-fpm.
fn started(root: &Path, entry: &Entry) -> Result<Option<Started>> {
    let manifest = Manifest::load(root)?;
    let Some(fpm) = (manifest.pin.sapi.fpm.as_ref()).filter(|fpm| fpm.path.is_file()) else {
        return Ok(None);
    };
    manifest.pin.oracle().host_asserted(fpm)?;
    let fpm = fpm.path.clone();
    let served = Rc::new(Served::start(root, &manifest, entry)?);
    Ok(Some(Started {
        manifest,
        fpm,
        served,
    }))
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
    fn start(root: &Path, manifest: &Manifest, entry: &Entry) -> Result<Served> {
        let recipe = &manifest.pin.recipe;
        let binary = profile::released(root, &MAKALA.package(), MAKALA.binary, None)?;
        let own = Tree::new("bench-makala")?;
        own.write(&entry.script, &entry.program)?;
        let log = own.path().join("errors");
        let server = Server::start(recipe, &binary, own.path(), &ini(), None, &log, &[])?;
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

    /// The workload's request sent once more: the response as it came.
    fn ask(&self) -> Result<Vec<u8>> {
        let (answer, _) = self.server.ask(&self.http, &self.script, self.timeout)?;
        Ok(answer)
    }
}

/// An arm that sends the workload's request to a makala.
struct ByMakala {
    name: &'static str,
    served: Rc<Served>,
}

impl Side for ByMakala {
    fn name(&self) -> &str {
        self.name
    }

    fn program(&self) -> &Path {
        &self.served.binary
    }

    fn stream(&self) -> Stream {
        Stream::Body
    }

    fn once(&mut self) -> Result<Sample> {
        let start = Instant::now();
        let answer = self.served.ask()?;
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let (head, body) = split_head(&answer);
        let status = head.lines().next().unwrap_or_default();
        if status.split(' ').nth(1) != Some("200") {
            bail!(
                "the arm {} answered {status:?}: {}",
                self.name,
                String::from_utf8_lossy(body)
            );
        }
        Ok(Sample {
            ms,
            wrote: body.to_vec(),
        })
    }
}

/// An arm that hands the workload's request to the pinned php-fpm.
struct ByFpm {
    pool: Fpm,
    program: PathBuf,
}

impl Side for ByFpm {
    fn name(&self) -> &'static str {
        "php-fpm"
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn stream(&self) -> Stream {
        Stream::Body
    }

    fn once(&mut self) -> Result<Sample> {
        let start = Instant::now();
        let (stdout, stderr) = self.pool.ask()?;
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let (head, body) = split_head(&stdout);
        if let Some(status) = head.lines().find_map(|line| line.strip_prefix("Status: "))
            && !status.starts_with("200")
        {
            bail!(
                "the arm php-fpm answered {status:?}: {}{}",
                String::from_utf8_lossy(body),
                String::from_utf8_lossy(&stderr)
            );
        }
        Ok(Sample {
            ms,
            wrote: body.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each workload is two bars, the series against php-fpm and its A/A
    /// control, named by the workload's file.
    #[test]
    fn each_workload_is_a_series_against_php_fpm_and_its_a_a_control() {
        let root = crate::package_dir().join("..");
        let bars = declared(&root).unwrap();
        let named: Vec<(&str, Held)> = bars.iter().map(|bar| (bar.id.as_str(), bar.held)).collect();
        assert!(
            named.contains(&("makala-fpm/ucall", Held::Series)),
            "{named:?}"
        );
        assert!(
            named.contains(&("makala-aa/ucall", Held::Control)),
            "{named:?}"
        );
        assert!(
            named.contains(&("makala-fpm/bench_quiet", Held::Series)),
            "{named:?}"
        );
        assert_eq!(bars.len(), 2 * workloads(&root).unwrap().len());
    }
}
