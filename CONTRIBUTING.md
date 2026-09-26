# Contributing

## Everything runs in Docker, on a remote host

Never build or test with the toolchain on your machine. `scripts/ci-docker.sh` (or `make gate`) runs the same checks as `.github/workflows/ci.yml` inside a pinned image, and `tests/ci_parity.rs` fails if the two drift. A red run should mean the code is wrong, not that a laptop updated its compiler overnight.

The containers don't run on your machine either. Every script sources `scripts/nas-lib.sh`, which sends the tree over ssh to an x86_64 Docker host and runs everything there. It defaults to my build box, so point `RESES_NAS=user@host` at your own. It needs:

- x86_64 Linux with Docker, `tar`, and your user in the `docker` group. It doesn't need git or anything else, and it can be the machine you're on if that's x86_64 Linux running sshd.
- ssh without a password prompt, and the host key already in `known_hosts`, since the scripts never answer ssh's first-connection question. Run `ssh user@host true` once by hand.
- bash or zsh as the login shell there, because commands go over quoted by bash's `printf %q`.
- Outbound access to Docker Hub, ghcr.io, cgr.dev and raw.githubusercontent.com, and a few GB of disk for the images and the cargo caches.

Each run's tree lives in `~/workspace/reses-ci/<lane>` and is cleared when the run ends, Ctrl-C included. A lane is locked while a run uses it, so a second run in the same lane is turned away rather than pushing over the first one's tree. If a killed run leaves the lock behind, `scripts/ci-docker.sh --nas-unlock <lane>` clears it and stops any container that run left going. The per-lane cargo target volumes, the shared cargo home and the images stay between runs on purpose, so builds are fast. `scripts/ci-docker.sh --nas-clean` removes all of it: every reses container, network, volume, image and directory. It refuses while any lane is locked, unless you add `--force`. It leaves Docker's build cache alone, since other builds on the host share it, so run `docker builder prune` there yourself if you want that space back.

Only the files git would see go over (tracked, plus untracked ones that aren't ignored). An untracked file that looks like a key or a credentials file, or whose first lines look like a stored mail message (raw SES mail has no extension), stops the push. Fake mail belongs in `tests/fixtures/`.

If you work in more than one worktree at once, give each its own `RESES_LANE=<name>` so they don't share a scratch directory or a cargo target volume.

## Every PR closes an issue

Every pull request says which issue it resolves with a closing keyword on its own line, `Closes #N` (one per issue). If there's no issue yet, open one first. Never point a closing keyword at an epic, or it closes the first time any sub-issue lands. Check the link before merging:

```
gh pr view <n> --json closingIssuesReferences
```

## Tests come first

Open the PR as a draft with the failing tests already in it, then push the fix on top. A bug report gets its reproducing tests red before anything changes; a feature gets its tests before its implementation. Tests that were written after the fix, or that went green on their first run, haven't shown anything.

Before trusting a test, break the code it covers and watch it go red.

## Bugs

File them with these four headings, in this order, and fill all four:

```
## What was expected
## What actually happened
## Proposed solution
## Alternatives
```

A bug in the terminal UI comes with an animated recording of it in the issue body.

## The mail decoder

Each `tests/fixtures/mail/NAME.eml` has its expected output next to it (`NAME.out`, `NAME.html.out` and `NAME.saved`). The goldens come from `tests/mail_oracle.py`, which reads each fixture with Python's standard `email` package, and `tests/mail_oracle.rs` checks them against it on every test run. `tests/fixtures/mail/regen-goldens.sh` rewrites them in the CI container after an intended change. A few are pinned by hand where the standards and Python disagree, or where the body comes from HTML; `tests/fixtures/mail/HAND-PINNED` lists them with the reason for each. Never regenerate a golden by running reses itself, because a golden made by the code under test can't catch that code being wrong. Fixtures are synthetic (`example.com` addresses): never commit real mail.

## Secrets

Tests never read or write the real `~/.aws` or `~/.config/reses`; they use temp dirs and pass paths in. Test keys are the obviously fake AWS examples. Nothing ever writes a real key to disk or puts one on a command line.

## macOS binaries in dist/

Every binary written into `dist/` goes through `scripts/place-binary.sh`, which copies beside the destination and renames over it. `tests/ci_parity.rs` fails if a `cp` or `mv` into `dist/` appears anywhere else in the Makefile or `scripts/ci-docker.sh`. The reason is #15: on Apple Silicon, overwriting a binary that has already run, in place, while any process holds it open, makes macOS kill it on every later exec, and Docker Desktop used to hold everything under a mounted folder. Builds run remotely now, but the rule stays, since a running `reses` holds its binary just the same.

One side effect is expected: a build killed partway through can leave a `dist/*.tmp.XXXXXX` file behind; nothing ever runs it, and it's safe to delete.

## Releasing

A push to the `release` branch publishes a release (`.github/workflows/release.yml`). It builds Linux x86_64 and arm64 (static musl) and macOS arm64, checks each binary against the mail goldens on its own platform, and attaches the three archives and a `SHA256SUMS` file to a release tagged `v<version>`.

1. On main, bump `version` in `Cargo.toml` and run `scripts/ci-docker.sh --exec 'cargo update -p reses'` so `Cargo.lock` agrees, then merge that the usual way.
2. Wait for CI on that main commit to finish green. The release checks it: it refuses a commit unless every `ci.yml` push run on it passed, every job included, and it waits up to an hour for runs still going.
3. `git push origin main:release`. That's a fast-forward, so `release` never carries commits of its own. The very first release is just the first push, which creates the branch.
4. Watch the Release run. A red **Version** job means the version wasn't bumped (its tag already exists), the commit isn't on main, or CI didn't pass on it. Fix that on main and push again.

The `release` branch is protected by a ruleset: it can't be deleted or force-pushed. The workflow also refuses any commit that isn't on main or that CI didn't pass, and runs the tests and the MinIO suite itself before building anything it publishes. If a publish fails partway through, the run removes the tag and draft it created, so pushing again after the fix starts clean.
