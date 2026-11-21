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
| `crates/riddim` | **The public Rust API**: start here |
| `crates/riddim-ffi` | The C ABI and its header (`include/riddim.h`) |
| `crates/riddim-engine` | The engine: control side, audio side, transport, mixing, recording |
| `crates/riddim-model` | The engine model and the edit ops that change it |
| `crates/riddim-graph` | Render-plan compilation: order, latency compensation, schedule |
| `crates/riddim-time` | Integer time: superclock, beat ticks, tempo and meter map |
| `crates/riddim-plugin` | Plugin hosting: the format-agnostic contract, VST3 and Audio Unit hosts, the out-of-process scanner |
| `crates/riddim-io` | Audio and MIDI device I/O |
| `crates/riddim-disk` | WAV and MIDI files, the capture thread |
| `crates/riddim-rt` | Real-time building blocks and the guard that tests them |
| `crates/riddim-cli` | `riddim`, a headless tool for exercising all of the above |
| `fixtures/` | Test plugins and test support; never shipped |
| `docs/` | Vision, decisions, per-phase notes |

## Building

Rust 1.100.0 (pinned in `rust-toolchain.toml`; `rustup` installs it on first use).

```sh
cargo build --workspace
cargo test --workspace
scripts/ci.sh            # what CI runs, plus the heavier checks kept out of CI
scripts/rtsan.sh         # the real-time sections under RealtimeSanitizer
```

Then, for example:

```sh
cargo run -p riddim-cli -- scan
cargo run -p riddim-cli -- check "Surge XT"
cargo run -p riddim-cli -- play "Surge XT" --sequence
cargo run -p riddim-cli -- session new song.json --instrument "Surge XT" --sequence --audio-input 1-2
cargo run -p riddim-cli -- session record song.json
cargo run -p riddim-cli -- session render song.json -o song.wav
cargo run -p riddim-cli -- session new mix.json --instrument "Surge XT" --sequence \
    --send-effect "Surge XT Effects" --fade-in 4
cargo run -p riddim-cli -- session render mix.json -o mix.wav --tail 2
cargo run -p riddim-cli -- session render mix.json -o return.wav --stem 1000
```

`riddim --help` lists the commands.

## Licence

AGPL-3.0-only (see `LICENSE`); a commercial licence is planned. See the vision, §6.
