# riddim

Riddim is a native audio engine written in Rust. It handles playback, recording, the timeline, mixing and AU/VST3 plugin hosting, and it powers the Whiplash DAW.

**Status:** under construction, phase by phase. See the [changelog](CHANGELOG.md) for what exists, and the per-phase notes in [`docs/phases/`](docs/phases/) for what has actually been run and on what. The design is in the vision: [`docs/visions/riddim.md`](docs/visions/riddim.md). Decisions taken along the way are in [`docs/decisions.md`](docs/decisions.md).

- **Platform:** macOS first (CoreAudio, Apple Silicon); Windows later. Everything platform-neutral also builds and is tested on Linux.
- **Plugin formats:** AU and VST3 for v1, then CLAP.
- **Architecture:** real-time safe by design, and deterministic in offline render.
- **First client:** [Whiplash](https://github.com/hejijunhao/whiplash), through a defined command/query/event API with a C ABI.

## Layout

| Path | What |
|---|---|
| `crates/riddim-rt` | Real-time building blocks and the allocation guard that tests them |
| `crates/riddim-plugin` | Plugin hosting: the format-agnostic contract, VST3 and Audio Unit hosts, the out-of-process scanner |
| `crates/riddim-io` | Audio and MIDI device I/O |
| `crates/riddim-cli` | `riddim`, a headless tool for exercising all of the above |
| `fixtures/` | Test plugins and test support; never shipped |
| `docs/` | Vision, decisions, per-phase notes |

## Building

Rust 1.100.0 (pinned in `rust-toolchain.toml`; `rustup` installs it on first use).

```sh
cargo build --workspace
cargo test --workspace
scripts/ci.sh            # everything CI runs
```

Then, for example:

```sh
cargo run -p riddim-cli -- scan
cargo run -p riddim-cli -- check "Surge XT"
cargo run -p riddim-cli -- play "Surge XT" --sequence
```

`riddim --help` lists the commands.

## Licence

AGPL-3.0-only (see `LICENSE`); a commercial licence is planned. See the vision, §6.
