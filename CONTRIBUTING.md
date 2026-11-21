# Contributing to Riddim

Contributions are welcome. This page says how they are handled, so that nobody spends an evening on something that cannot be merged.

## How it works

- **Every change is reviewed by the maintainer** (Phil, for Kamino Research Corporation) before it is merged. Nothing goes into `main` any other way, including the maintainer's own changes and changes written by coding agents. There is no second route and no auto-merge.
- **Review takes as long as it takes.** This is a small project with one reviewer. A focused pull request with tests is reviewed far sooner than a large one.
- **A pull request may be declined** even when it is good, if it takes the engine somewhere it is not meant to go. Read [the vision](docs/visions/riddim.md), especially §1 (what Riddim is and is not), before proposing anything large.

## Before you start

1. **Search first.** Look through the issues, the open pull requests and [the decisions log](docs/decisions.md): the thing you have in mind may be done, under way, or decided against, with the reason written down.
2. **For anything larger than a fix, open an issue first** and say what you intend. For a bug, a pull request with a failing test is the best issue there is.
3. **One logical change per pull request.** A fix and an unrelated tidy-up are two pull requests.

## What a pull request needs

- **Tests** for what it changes. A bug fix comes with a test that fails without it.
- **The checks passing**: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. `scripts/ci.sh` runs those and the rest (the type-checks for macOS and Windows, the RealtimeSanitizer suite, the dependency licence check); `scripts/ci-clean.sh` runs the CI job in a clean container.
- **Nothing on the audio thread that allocates, locks or blocks.** The test suites check this and will tell you; see the vision, §3.4.
- **No new dependency under a copyleft licence** (GPL, LGPL, AGPL). `cargo deny check` enforces the list in `deny.toml`; the reason is in the vision, §6.2.
- **Honest claims.** If a change cannot be tried on something (a platform, a plugin, a device), say so in the pull request. "Compiles for macOS, never run there" is a fine thing to write and a bad thing to leave out.
- **A sign-off on every commit**, and agreement to the contributor licence agreement, once: see below.

Platform code (Core Audio, Audio Units, and in time WASAPI or ASIO) belongs in the backend layer: `riddim-io`'s device backends, the per-format hosts in `riddim-plugin`. The rest of the workspace must build for Linux, macOS and Windows, and `scripts/ci.sh` checks that it does.

## Sign-off and the licence agreement

Riddim is open source under the GNU AGPL, version 3 only, and its owner, Kamino Research Corporation, intends to offer it under a commercial licence as well. Two things keep that possible.

**1. The Developer Certificate of Origin.** Every commit carries a line

```
Signed-off-by: Your Name <you@example.com>
```

which `git commit -s` adds for you. By adding it you certify what the [Developer Certificate of Origin 1.1](https://developercertificate.org/) says: in short, that you wrote the change or otherwise have the right to submit it under the project's open source licence.

**2. The contributor licence agreement.** [`CLA.md`](CLA.md) is a licence grant, not an assignment: you keep your copyright, and you give the owner the right to license your contribution under other terms too, with a promise in return that it stays available under an open source licence. You agree to it once, by saying so in your first pull request (the exact sentence is at the end of that file). A pull request from someone who has not agreed cannot be merged, however small.

If you used a coding assistant, the contribution is still yours to answer for: you sign off on it, you agree to the licence terms for it, and you say in the pull request anything about its origin that a reviewer should know.

## Reporting bugs and security problems

Open an issue for a bug, with what you did, what happened, and the platform, plugin and buffer size if it is about audio. For a security problem (for example, a project file or a plugin scan that can be made to run code or write where it should not), do not put the details in a public issue: use GitHub's private vulnerability reporting on this repository if it is enabled, or open an issue that says only that you have something to report privately.

## Licence

By contributing you agree that your contributions are licensed under the GNU AGPL, version 3 only (see `LICENSE` and `COPYRIGHT`), and under the terms of `CLA.md`.
