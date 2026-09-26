# Contributing

## Everything runs in Docker

Never build or test with the toolchain on your machine. `scripts/ci-docker.sh` (or `make gate`) runs the same checks as `.github/workflows/ci.yml` inside a pinned image, and `tests/ci_parity.rs` fails if the two drift. A red run should mean the code is wrong, not that a laptop updated its compiler overnight.

If you work in more than one worktree at once, give each its own `RESES_LANE=<name>` so they don't share a cargo target volume.

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

`src/mail` is pinned to `python/reses.py`. If a decoding change is intended, change the Python first, regenerate the goldens with `scripts/ci-docker.sh --exec tests/fixtures/mail/regen.sh`, and commit both. Fixtures are synthetic (`example.com` addresses): never commit real mail.

## Secrets

Tests never read or write the real `~/.aws` or `~/.config/reses`; they use temp dirs and pass paths in. Test keys are the obviously fake AWS examples. Nothing ever writes a real key to disk or puts one on a command line.

## macOS binaries in dist/

Every binary written into `dist/` goes through `scripts/place-binary.sh`, which copies beside the destination and renames over it. `tests/ci_parity.rs` fails if a `cp` or `mv` into `dist/` appears anywhere else in the Makefile or `scripts/ci-docker.sh`. The reason is #15: on Apple Silicon, overwriting a binary that has already run, in place, while any process holds it open, makes macOS kill it on every later exec, and Docker Desktop holds everything under a mounted folder.

Two side effects are expected. Docker Desktop keeps the old, now unlinked binaries open, so each `make darwin` leaves a few MB on disk that comes back when Docker restarts. And a build killed partway through can leave a `dist/*.tmp.XXXXXX` file behind; nothing ever runs it, and it's safe to delete.
