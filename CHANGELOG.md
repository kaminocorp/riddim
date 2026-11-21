# Changelog

All notable changes to Riddim. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Nothing has been released yet; versions will follow semantic versioning once there is an API to keep stable.

Entries say what exists and where it has been run. "Unverified on macOS" means the code type-checks for Apple Silicon but has not been run on a Mac.

## [Unreleased]

### Phase 3: mix

#### Added
- **Sends**: any number per track, to buses, pre- or post-fader, with gain and mute; compensated for latency like every other path. **Polarity invert** on every strip.
- **Automation**: lanes for fader, pan, send gain and plugin parameters, with linear and hold shapes, in beats. Read per sample for faders, pans and sends (exact at breakpoints, independent of buffer size); per block with sample-offset breakpoints for plugin parameters. **Touch, latch and write** passes, committed as one undoable edit each.
- **Offline bounce**: `bounce(model, request)` renders a model to a WAV in an engine of its own: mix or stems, any range, tails, the plan's latency trimmed. Plugins are told they are rendering offline.
- **Scan cache and blocklist** in `riddim-plugin`: unchanged bundles are not loaded again; bundles that crash, hang or fail are skipped until they change. Safe against the host dying mid-scan and against two hosts scanning at once.
- **C ABI**: `riddim_touch` and `riddim_bounce`.
- **`riddim` CLI**: `scan --rescan | --blocklist | --unblock <bundle>`; `session new --insert <plugin> --send-effect <plugin> --fade-in <beats>`; `session render --tail <seconds> --stem <track>`; `session play --save` (keeps what was changed in the plugins' editors); `session info` lists sends and automation.
- A fixture plugin that takes 400 ms to initialise, and a demo-song test for third-party plugins (`RIDDIM_TEST_VST3`).

#### Changed
- **Solo follows the signal through sends**: soloing an effect return gives the return alone; soloing a track keeps its share of its returns. Tracks with post-fader sends get a switch node after the strip.
- **A bypassed plugin keeps its latency** (it used to shift the track by that amount).
- `riddim render` and `riddim session render` go through `bounce`, so their files start at the timeline's start rather than a plan-latency late.
- The audio thread reports loop wraps and locates to the control thread.

#### Verified
- On Linux, on the deterministic manual device: a dry path and a path through a plugin with latency cancel exactly (silence, not merely −120 dBFS) through a bus, through a send, and with the plugin bypassed; two bounces of a project are the same file byte for byte and equal the live render sample for sample; automation steps land on their sample; a slow plugin load neither stalls the audio callback nor delays an edit; a demo song through Surge XT and Surge XT Effects plays, saves, reopens and bounces.

#### Not verified
- Anything on a Mac or on real audio hardware, and nothing with Kontakt, Superior Drummer 3, EZbass or Helix Native. See [`docs/phases/p3-mix.md`](docs/phases/p3-mix.md).

### Phase 2: engine core ("tape")

#### Added
- **`riddim-time`**: integer time. Superclock and beat ticks, exact sample conversions, a tempo and meter map with bar/beat arithmetic.
- **`riddim-model`**: the engine model (tracks, clips, media, plugin slots, routing, loop and metronome settings) and the edit ops that change it. Every op is validated, applies atomically, and returns its exact inverse. JSON with a schema version.
- **`riddim-graph`**: a pure compiler from nodes and edges to a deterministic render plan with latency-compensation delays and a parallel schedule, and the mapping from a model to its graph.
- **`riddim-disk`**: an incremental WAV/RF64 writer whose file is valid after every flush; a reader that recovers interrupted recordings; the capture ("butler") thread; a resampler for import; Standard MIDI Files.
- **`riddim-engine`**: the engine. A control thread that owns the model and compiles a plan per edit; an audio callback with transport, looping, a single-threaded plan executor, clip and MIDI playback, live MIDI, metronome and count-in; recording of audio and MIDI with latency-corrected placement; a plugin host on a main thread that is engine-owned or app-pumped; a mixer with ramps, balance, mute, solo-in-place, buses and meters.
- **`riddim`**: the public Rust API (commands, queries, events) and file import helpers.
- **`riddim-ffi`**: C ABI v0 with a generated header (`crates/riddim-ffi/include/riddim.h`).
- **`riddim-cli`**: `play` and `render` now run the engine and accept MIDI files; new `session new|info|play|record|render` and `loopback` (measures recording alignment through a cable).
- **RealtimeSanitizer**: `scripts/rtsan.sh` (part of `scripts/ci.sh`) runs the real-time sections under LLVM's RTSan, with a self-check that the sanitizer is active.

#### Changed
- The phase 1 spike player is gone; `riddim play` is the engine.
- `Status`/`RiddimStatus` report the recording backlog (frames waiting for the disk).

#### Verified
- On Linux, on the deterministic manual device: sample-accurate clip, MIDI and metronome timing; looping; mixing; plugin latency compensation; save, reopen and bit-identical render; recording placed exactly on the sample through a simulated loopback at several latencies; a take surviving `kill -9`; an hour of timeline on eight tracks with no frame lost; the whole suite under RealtimeSanitizer; a C program driving the engine through the header.

#### Not verified
- Anything on a Mac or on real audio hardware. See [`docs/phases/p2-engine-core.md`](docs/phases/p2-engine-core.md).

### Phase 1: hosting spike

#### Added
- **Workspace** with a pinned toolchain, a `cargo-deny` dependency policy, one lean GitHub Actions job (format, lints, tests), and `scripts/ci.sh` for the fuller local run (Apple Silicon type-check, third-party plugins, licences).
- **`riddim-rt`**: real-time section guard with a counting allocator (`guard`), deferred drop (`garbage`), wait-free SPSC queues (re-exported `rtrb`), denormal flushing, linear smoothing, atomic floats, and allocation-free planar buffer views.
- **`riddim-plugin`**:
  - The format-agnostic contract: `PluginDescriptor`, `PluginInstance` (control side), `PluginProcessor` (audio side), sample-timestamped events, transport info, opaque versioned `StateBlob`.
  - A **VST3 host** written on the `vst3` bindings: module loading, class scanning, component/controller setup and connection, bus activation, parameter queues, MIDI-to-parameter mapping, event input and output, state, latency and tail, editor attach and resize.
  - An **Audio Unit host** on the AudioToolbox C API: registry scan, instantiation (including the asynchronous path), stream formats, render with input callbacks, MIDI, parameters, host transport callbacks, state, latency, Cocoa and generic views. *Unverified on macOS.*
  - **Out-of-process scanning**: the `riddim-scan` executable, a JSON-lines protocol, timeouts, and crash containment.
  - A **conformance runner** (`check`) that takes a plugin through the lifecycle and reports each step.
  - An **editor window host** for macOS. *Unverified on macOS.*
- **`riddim-io`**: stream and callback types; a deviceless backend (manual and timer-paced, with simulated loopback); a **CoreAudio HAL** backend and **CoreMIDI** input (*both unverified on macOS*); a MIDI byte-stream parser.
- **`riddim-cli`** (`riddim`): `scan`, `info`, `check`, `render`, `state save`, `state roundtrip`, `play`, `editor-cycle`, `devices`.
- **Fixtures**: three deterministic VST3 test plugins (synth, latency-reporting delay, single-component gain), a plugin that crashes its scanner, and test support that bundles them.
- **Docs**: decisions log, phase 1 completion notes with a Mac verification checklist.

#### Verified
- On Linux (x86-64): the VST3 host, scanner, conformance runner, deviceless I/O and the CLI, against the fixture plugins and **Surge XT 1.3.4** (instrument and effect). Zero host-side heap operations on the processing path in every test that measures it.

#### Not verified
- Everything macOS-specific, and every commercial plugin. See [`docs/phases/p1-hosting-spike.md`](docs/phases/p1-hosting-spike.md).
