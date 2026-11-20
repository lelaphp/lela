# scripts/ci

What CI's jobs run on, as Mitchell ruled on 2026-11-17 and 2026-11-19: hosted runners alone, a fresh machine each job, and no machine of the project's own. The Linux jobs take their runner from the repository variable `LELA_BUILD_RUNNER`. The Mac's jobs, both `oracle` jobs and nightly.yml's `tailcall`, run on a macOS 15 image, where `.github/actions/oracle-runner` lays the tools a fresh Mac lacks. While the repository is private they run nightly and by hand alone, since a push's Mac minutes are billed then (the two `oracle` jobs took 259 minutes on GitHub's 3-core Mac, measured 2026-11-19) and a public repository's on GitHub's `macos-15` are not; neither `oracle` job runs after a push until it is public. The label follows the same switch: Blacksmith's 12-core `blacksmith-12vcpu-macos-15`, GitHub's macos-15 image on an M4, while the repository is private, and GitHub's `macos-15` once it is public. Each Mac job sets `XTASK_SEATS=1`, so the one instrument it runs at a time has every core.

- `pinned-php.sh` pours the keg `oracle.toml` pins, from the Homebrew formula the pin names, `php` or `php@8.5`, at the bottle its path names, `8.5.13_1` for a keg Homebrew bottled again (#4383), holds it to the pin's md5 and `brew pin`s its keg, after pouring and `brew pin`ning each keg of its `[closure]` at the recorded version (#2657). Both `oracle` jobs and nightly.yml's `tailcall` run it. `DRY_RUN=1` says what it would pour and pours nothing.
- `oracle-variants.sh` lays each oracle variant the pin names for this machine at its path, held to its md5: a binary already there with the pin's md5 is left, and an absent or other one is built with `scripts/oracle-build.py` from the pinned tree the corpus action checked out (#2298). A build that does not reproduce the md5 is laid nowhere, kept beside the path as a refusal the later jobs repeat without building again, and fails its step, which both jobs run with `continue-on-error`, so the lanes under the variant read the binary as absent or not the pin's and every other lane runs; the difference is a person's to settle. Both `oracle` jobs run it after the corpus action, and it makes each variant's directory the runner's with `sudo`, which a hosted runner's user is given with no password. `DRY_RUN=1` says what it would build and builds nothing.
- `red.sh` files a red job once, under one rule: two reds in a row file one ticket, a third comments on it, and the first green after them comments that it is green again and closes nothing. `red.sh main` is harness.yml's `red-main` job, which files a red `main` under `ci:red-main` (#1156). `red.sh nightly <job>` is nightly.yml's `red-nightly` job, run for `soak`, `tailcall` and `deny`, which fail no per-commit run: each files its red nightly under `ci:red-soak`, `ci:red-tailcall` or `ci:red-deny`, and a nightly in which the job was skipped is passed over (#2980). `DRY_RUN=1` prints what it would write.
- `tailcall.sh` is the lane that holds the threaded dispatch, `cfg(dispatch = "tailcall")` (#2905), on a dated nightly, as Mitchell ruled on #2970 (harness-design.md §7). `check` is harness.yml's per-commit `cargo +<nightly> check -p php_engine` under the cfg; `phpt <corpus> <tree>` is nightly.yml's nightly, the hosts built under the cfg in `release-da` and `Zend/tests` and each directory of `tests/` run with `--engine` on that build, the runs `phpt` keeps set aside before the subsets and put back after, which `restore` does where a killed run left them set aside. The nightly is `tailcall.sh`'s `TOOLCHAIN` line, the one place it is named, so moving it is that line; a nightly that breaks the check is moved past, not followed. It installs the nightly and builds in `target/tailcall`, apart from the shipped build.

## `LELA_BUILD_RUNNER`: the Linux jobs' runner

One repository variable names the runner of every Linux job:

```yaml
runs-on: ${{ fromJSON(vars.LELA_BUILD_RUNNER || '["ubuntu-26.04"]') }}
```

That is `regions`, `per-commit`, `miri`, `tailcall`, `store` and `red-main` in `harness.yml`, `soak`, `deny` and `red-nightly` in `nightly.yml`, `bench` in `bench.yml`, and `amd64`, `join` and `prune` in `images.yml`.

- **Unset** (or empty), each runs on GitHub's `ubuntu-26.04`, the release `packages.txt` names (#1724).
- **Set**, its value is a JSON list of runner labels, as another provider's hosted runner of the same release is named:

  ```sh
  gh variable set LELA_BUILD_RUNNER --body '["<label>"]'
  ```

  The variable decides nothing for the Mac's jobs, both `oracle` jobs and nightly.yml's `tailcall`: they run on a macOS 15 image whatever it holds.

### `LELA_BUILD_RUNNER_ARM64`: the arm64 image build's runner

`images.yml`'s `arm64` job builds and smokes the `linux/arm64` images on a runner of that architecture, so `dist/smoke.sh --image` reads the platform's own answers and no emulation's. Its runner is this second variable's, a JSON list of labels read as the first is:

```yaml
runs-on: ${{ fromJSON(vars.LELA_BUILD_RUNNER_ARM64 || '["ubuntu-26.04-arm"]') }}
```

As Mitchell ruled on 2026-11-19 (#5), it is Blacksmith's arm64 runner, the size the amd64 runner has:

```sh
gh variable set LELA_BUILD_RUNNER_ARM64 --body '["blacksmith-8vcpu-ubuntu-2604-arm"]'
```

Unset, the job asks for GitHub's `ubuntu-26.04-arm`. No other job reads it.

### What the jobs need of a hosted Linux image

Nothing beyond what they install. `packages.txt` installs the platform libraries through `.github/actions/platform-libraries` in `per-commit`, the one Linux job that links them (xtask links none, #2166). The rest GitHub's `ubuntu-26.04` carries (image 20260927):

| the jobs use | on `ubuntu-26.04` |
|---|---|
| a C toolchain, `ca-certificates`, `curl`, `git`, `jq`, `unzip`, `zstd`, `locales`, `sudo` | preinstalled: GCC 13 to 15 with `g++` and `make`, git 2.55, curl, jq, unzip, zstd, `locales`, sudo |
| Docker with Compose | Docker CE 29.4 with the Compose 5.1 plugin, which `docker compose` in `.github/actions/mysql` runs |
| Node.js and npm | Node.js 24.21 and npm 11.19 |
| `gh` | GitHub CLI 2.101 |
| rustup | rustup 1.29; the workspace action installs `rust-toolchain.toml`'s channel |
| a user in `docker` with sudo for `apt-get`, `locale-gen`, `localedef` | the image's `runner` user, in `docker`, with passwordless sudo |

Two things to know of it:

- The image preinstalls MySQL 8.4 with its service disabled, so port 3306 is free for the compose server.
- A hosted runner starts empty. It has no `target/`, so each job builds from scratch, and GitHub's image comes nearly full, which `per-commit` and `bench` make room on before they build.

### What a hosted image measures as provisional

The bench, which `bench.yml` runs by hand on the runner this variable names, as Mitchell ruled on 2026-11-19. A hosted VM shares its host, so a reading taken there is offered as provisional and never as a number a gate rests on: `xtask bench --machine hosted` measures each bar between two runs of its A/A control and refuses any label but smoke and provisional (`docs/perf/measurement-protocol.md` §7). Whatever job runs it, `xtask bench` asked as a shared or a dedicated machine refuses a runner whose `RUNNER_ENVIRONMENT` is `github-hosted` and measures nothing.

## The stash: a run's artifacts

What one job of a run hands a later one goes through `.github/actions/stash`, `put` in one job and `get` in another, and is an artifact of the run, kept for the `retention-days` its put names. GitHub counts artifact storage against the account over its billing month, so a hand-off a later job reads minutes later is kept a day. No workflow names the artifact actions itself, which a test of the harness holds. `xtask phpt --against`'s fetch line downloads the newest `phpt-runs` artifact with `gh`, which nightly.yml's `oracle` job puts. No workflow runs on a pull request (#4372), so no secret reaches a run of one.
