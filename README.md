# riddim

Riddim is a native audio engine written in Rust. It handles playback, recording, the timeline, mixing and AU/VST3 plugin hosting, and it powers the Whiplash DAW.

**Status:** design stage, with no code yet. Start with the vision: [`docs/visions/riddim.md`](docs/visions/riddim.md).

- **Platform:** macOS first (CoreAudio, Apple Silicon); Windows later.
- **Plugin formats:** AU and VST3 for v1, then CLAP.
- **Architecture:** real-time safe by design, and deterministic in offline render.
- **First client:** [Whiplash](https://github.com/hejijunhao/whiplash), through a defined command/query/event API with a C ABI.
