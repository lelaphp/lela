# scripts/ci

What CI's runners are made of, as Mitchell ruled on 2026-10-07 (#2218, #2253): two self-hosted machines, the Linux server labelled `dax-build` and the Mac mini labelled `dax-oracle`.

- `runner.sh linux|mac` sets up one machine as a runner of this repository. Its header says what each one gets.
- `wizard.sh` walks a person through the steps only a person can take: ordering both machines, the SSH key, the registration tokens, auto-login, the Cloudflare secret, billing, and the variable below. It runs `runner.sh` on each machine over SSH. `DRY_RUN=1` prints each command that would change a machine, GitHub or a secret, instead of running it.
- `pinned-php.sh` pours the keg `oracle.toml` pins and holds it to the pin's md5, after pouring and `brew pin`ning each keg of its `[closure]` at the recorded version (#2657). Both `oracle` jobs and `runner.sh mac` run it. `DRY_RUN=1` says what it would pour and pours nothing.
- `oracle-variants.sh` lays each oracle variant the pin names for this machine at its path, held to its md5: a binary already there with the pin's md5 is left, and an absent or other one is built with `scripts/oracle-build.py` from the pinned tree the corpus action checked out (#2298). A build that does not reproduce the md5 is laid nowhere, kept beside the path as a refusal the later jobs repeat without building again, and fails its step, which both jobs run with `continue-on-error`, so the lanes under the variant read the binary as absent or not the pin's and every other lane runs; the difference is a person's to settle. Both `oracle` jobs run it after the corpus action, and `runner.sh mac` makes each variant's directory the runner's. `DRY_RUN=1` says what it would build and builds nothing.
- `bound-target.sh` holds a runner's `target/` under `DAX_TARGET_GIB`, the bound `runner.sh` writes into each runner's environment (#2299). Every building job runs it through `.github/actions/workspace`: it logs the directory's size, and where that is past the bound, or a job of `main` finds the toolchain moved since `main`'s last job, it removes everything under `target/` but the phpt targets under the current pin (`xtask status --section corpus`'s `targets`) and uninstalls the toolchain that moved. The bound is provisional until the runners' growth is measured; `runner.sh` says why it is the number it is.
- `red-main.sh` files a red `main` once (#1156). `DRY_RUN=1` prints what it would write.
- `tailcall.sh` is the lane that holds the threaded dispatch, `cfg(dispatch = "tailcall")` (#2905), on a dated nightly, as Mitchell ruled on #2970 (harness-design.md §7). `check` is harness.yml's per-commit `cargo +<nightly> check -p php_engine` under the cfg; `phpt <corpus> <tree>` is dashboard.yml's nightly, the hosts built under the cfg in `release-da` and `Zend/tests` and each directory of `tests/` run with `--engine` on that build. The nightly is `tailcall.sh`'s `TOOLCHAIN` line, the one place it is named, so moving it is that line; a nightly that breaks the check is moved past, not followed. It installs the nightly where the runner has not yet, and builds in `target/tailcall`, so the shipped build stays warm.

## `DAX_BUILD_RUNNER`: the Linux jobs on a hosted image

Until `dax-build` is online (#2296), the Linux jobs run on GitHub-hosted Linux, as Mitchell ruled on 2026-10-09 (#2218, #2477). One repository variable is the switch. Every job that would run on `dax-build` takes its runner from it:

```yaml
runs-on: ${{ fromJSON(vars.DAX_BUILD_RUNNER || '["self-hosted","linux","dax-build"]') }}
```

That is `regions`, `per-commit`, `miri`, `tailcall`, `done` and `red-main` in `harness.yml`, and `measure`, `soak`, `publish` and `deploy` in `dashboard.yml`.

- **Unset** (or empty), every job runs on `[self-hosted, linux, dax-build]`, as before the variable existed.
- **Set**, its value is a JSON list of runner labels. The value to set is the hosted Ubuntu 26.04 image, the release `packages.txt` names (#1724):

  ```sh
  gh variable set DAX_BUILD_RUNNER --body '["ubuntu-26.04"]'
  ```

  A set variable also says the self-hosted pair is not online. So both `oracle` jobs, which only the Mac can run, are skipped. Queued instead, each would wait the 24 hours GitHub holds a job no runner takes, and `done` and `publish`, which need them, would wait with them.
- **Deleted** once both runners are registered. The wizard's last stage does this: `gh variable delete DAX_BUILD_RUNNER`. Nothing in the workflows is reverted.

### What a skipped oracle job means

- **`done`** (harness.yml) runs `xtask done --not-run witness --not-run crosscheck`, the two instruments the oracle job writes readings for. A missing reading of either is a note that it did not run, not an unmet condition. The head line reads "every condition of done is met over what ran; witness and crosscheck did not run in this run". The `done` reading counts `not_run` and its message says the same. The step adds a warning annotation, "the oracle lane did not run", and a line in the run's summary. Every other condition is judged as always, so `done` is red for anything else that is unmet. An oracle job that ran and failed is not skipped, so `done` judges the store it left.
- **`publish`** (dashboard.yml) folds the `measure` store alone and builds the site. It says, with the same warning and a summary line, that this run folded no phpt, witness or parity reading, so the site's are the ones the history already holds, each keyed by the commit it was taken at. It does not fail for the absence.

### What the hosted image needs that the server has

Nothing beyond what the jobs already install. The image is the same release as the server, so `packages.txt` installs the same packages under the same names through `.github/actions/platform-libraries` in `per-commit`, the one Linux job that links them (xtask links none, #2166), and `runner.sh`'s release check stays at 26.04. What `runner.sh linux` adds to the server, the image already carries:

| `runner.sh linux` installs | on `ubuntu-26.04` (image 20260927) |
|---|---|
| `build-essential`, `ca-certificates`, `curl`, `git`, `jq`, `unzip`, `zstd`, `locales`, `sudo` | preinstalled: GCC 13 to 15 with `g++` and `make`, git 2.55, curl, jq, unzip, zstd, `locales`, sudo |
| `docker.io`, `docker-compose-v2` | Docker CE 29.4 with the Compose 5.1 plugin, so `docker compose` in `.github/actions/mysql` works the same |
| `nodejs`, `npm` | Node.js 24.21 and npm 11.19 |
| `gh` | GitHub CLI 2.101 |
| rustup, no toolchain | rustup 1.29; the workspace action installs `rust-toolchain.toml`'s channel |
| a `runner` user in `docker` with sudo for `apt-get`, `locale-gen`, `localedef` | the image's `runner` user, in `docker`, with passwordless sudo |

Two things differ, and neither needs a step:

- The image preinstalls MySQL 8.4 with its service disabled, so port 3306 is free for the compose server.
- A hosted runner starts empty. It has no warm `target/`, so each job builds from scratch.

### What never runs on a hosted image

`bench` and the nightly series (#2259). Their numbers need the quiet machine. A hosted VM shares its host, and its reading would be noise a person could mistake for a measurement. Neither is a job yet. When one is, it takes the literal self-hosted label, not this variable. Whatever job runs it, `xtask bench` refuses a runner whose `RUNNER_ENVIRONMENT` is `github-hosted` and measures nothing.
