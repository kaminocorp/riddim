# Changelog

All notable changes to Riddim. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Nothing has been released yet; versions will follow semantic versioning once there is an API to keep stable.

Entries say what exists and where it has been run. "Unverified on macOS" means the code type-checks for Apple Silicon but has not been run on a Mac.

## [Unreleased]

### v1.x

#### Decided (Phil's answers to the vision's open questions, 2026-11-21; decisions D77 to D83)
- **Owner and licence notices.** The copyright owner is Kamino Research Corporation (legal entity: Bradgate Capital Management Pte Ltd, Singapore): `COPYRIGHT`, the C header, crate manifests and the README say so. The licence stays AGPL-3.0-only.
- **Public.** The repository is public as of 2026-11-21.
- **Open to contributions.** `CONTRIBUTING.md` (the maintainer reviews every change) and `CLA.md` (a licence grant that keeps a commercial licence possible; a draft no lawyer has read yet), with a Developer Certificate of Origin sign-off on every commit.
- **Windows later, portable now.** The whole workspace is type-checked and linted for `x86_64-pc-windows-msvc` in `scripts/ci.sh`. WASAPI and ASIO are the future device backends.
- **Whiplash is a Tauri 2 application** linking the engine in process: [`docs/api/embedding.md`](docs/api/embedding.md).
- **Target machine**: a Mac Mini with an M4 Pro and a Scarlett 2i2. **Helix** is a future test plugin, not a requirement.

#### Changed for in-process hosts
- **`Engine` is `Send`.** It used to hold the plugin-carrying main-thread half until asked for it, so it could not be kept in an application's shared state. It now makes that half when `take_main_thread` is called, on the calling thread.
- `TransportCommand` can be read from JSON; `Applied`, `Rejected`, `Status` and `StreamInfo` can be written as JSON.

#### Added
- **CLAP hosting** on `clack-host`: scanning, instantiation, audio and note ports, parameters, sample-offset events both ways, transport, state, latency, and an editor through the `gui` extension (*the editor is unverified*). `.clap` files are scanned and cached with everything else.
- **The out-of-process bridge**: a plugin slot marked `bridged` runs in a `riddim-bridge` child process. Shared memory for audio and events, a socket hand-off per block, JSON for everything else. A plugin that crashes or hangs is taken out of the mix (`plugin_crashed`) and the session carries on. Unix only; no editor for bridged plugins yet.
- **`riddim-render`**: a headless renderer. A model as JSON in, a WAV and a line of JSON out.
- **A stability policy** ([`docs/api/stability.md`](docs/api/stability.md)) and the checks behind it: golden schema-1 project and ops files that must keep loading with nothing lost. `Event`, `EngineError` and `Status` are marked non-exhaustive.
- `riddim session new --bridged`; `EngineConfig::bridge_executable`; `RIDDIM_TEST_CLAP` for the third-party tests.
- `riddim session new --copies N --looped`: the instrument track with its material and inserts, N times over, looping. Four copies with five inserts is the shape of the reference session (4 instruments, 20 effects), so the hour at 64 frames can be tried with whatever plugins are installed.
- `scripts/ci-clean.sh`: the CI job in a clean Ubuntu 24.04 container on two cores, for when Actions cannot run it.
- **CLAP fixture plugins** (`fixtures/clapplug`: a synth and a delay with latency, written against the CLAP C ABI directly) and the tests that use them, so that every test run loads CLAP plugins: host tests, the conformance run, the engine (latency compensated to the sample, state saved with the project, bounce, bridged) and the CLI.

#### Changed
- **A plugin's first block no longer runs on the audio thread.** After a plugin is activated, the thread that loaded it gives it one block of silence and resets it before the audio thread gets it, so whatever the plugin does once in its first block is not a dropout. With four Surge XT instruments at 64 frames the worst block at start-up went from about 254% of its time to about 47%. A bridged plugin that is slow over its first block is no longer taken for hung: that block is given thirty seconds. Not done for offline renders. `EngineConfig::warm_up_plugins` (on by default; `"warm_up_plugins"` in the C API's configuration, `--no-warm-up` on the command line) turns it off. *Not tried with any commercial plugin.*
- A parameter change a plugin makes on its audio side is written into automation when its lane is being written (this is how a CLAP editor's moves arrive).
- The `peak dsp load` in the CLI's summary after playing or recording is the worst of the whole run (it was the last second's), with the number of seconds that had a block over its time.

#### Fixed
- The scan cache could blocklist a healthy plugin when two Riddim processes ran at once: found here, fixed on the phase 3 branch, where it came from (see that section).
- A bounce did not tell CLAP plugins that it was offline (VST3 and Audio Unit plugins were told). It now does, through CLAP's `render` extension, so a sampler can wait for its disk instead of dropping notes.
- The CLAP host passed a block larger than the plugin was activated for on to the plugin, and for an effect indexed past its own input buffers (a panic on the audio thread). Such a block is now refused and silenced, as the VST3 host does. The engine never sends one; the CLAP fixture found it.

#### Verified
- On Linux: Surge XT's CLAP builds pass every step of the conformance run, and the demo song plays, saves, reopens, bounces and reroutes through them; a bridged fixture plugin has the same buses, parameters, state, events and audio (bit for bit) as in process; a project with bridged plugins bounces to the same bytes; a bridged plugin that aborts, and one that hangs, cost only themselves while a timer-driven device keeps running.

#### Not verified
- Anything on a Mac: CLAP hosting, CLAP editors, and the bridge on macOS have never run. Kontakt bridged at 128 frames (the bridge's first exit criterion) needs Kontakt. See [`docs/phases/v1x.md`](docs/phases/v1x.md).

### Phase 4: v1

#### Added
- **Multi-out instruments**: a plugin's extra output buses routed to bus tracks, each with its own fader, inserts and sends.
- **Sidechains**: a plugin's sidechain input fed from any track, pre- or post-fader, aligned with its main input.
- **Plugin MIDI out**: an instrument track can also play the notes that come out of another track's plugins.
- **Loop recording with takes**: every time round the loop becomes a take, cut from one continuous file; the newest whole take is audible, the rest muted. MIDI likewise.
- **Punch recording**: a take confined to a range, ending by itself while the transport plays on, with what was on the track trimmed or split to make room in the same undoable edit.
- **Parallel rendering**: render threads share the plan's steps with the audio thread, lock-free; the output is identical bit for bit whatever the thread count. On macOS they join the device's audio workgroup (*unverified on macOS*).
- **Runtime buffer-size changes**: `Engine::set_buffer_size` / `riddim_set_buffer_size`, 16 to 4096 frames, without reopening the project.
- **Read-ahead streaming**: long files are played from disk a page at a time; a disk that falls behind is heard as silence and counted, never waited for.
- **Events**: `plugin_restarted`, `plugin_gesture`, `plugin_params_changed`, `plugin_state_dirty`, `plugin_editor_resize`, `jumped`, `xrun`, `load`, `playback_starved`, `stream_changed`; take numbers on `recording_finished`. The whole set is documented in [`docs/api/events.md`](docs/api/events.md).
- **Status**: DSP load, lost messages, frames of streamed playback missed.
- **Private aggregate devices** on macOS, so input and output can be different devices (*unverified on macOS*).
- **A stress run** (`scripts/stress.sh`): a 34-plugin project at a 64-frame buffer with render threads, churned, on a slow disk.
- **`riddim` CLI**: `session record --loop / --punch`, `session apply` (ops from a JSON file), `--threads` for renders, `--render-threads` and `--switch-buffer` for live runs.

#### Changed
- Recording is counted in frames rolled since the take began rather than in timeline position, so the loop can wrap during it.
- Plugins are restarted (deactivated, reconfigured, activated; the instance kept) when their buses or the block size change, or when they ask.
- Plugin latency is polled on every pump, not only when announced.
- The plugin host loads at most one plugin per pump, so an application pumping from its UI thread gets a turn in between.
- The timer-driven device counts late blocks as xruns.
- The transport cannot be located while recording.

#### Fixed
- A slot emptied and refilled before the audio thread had let go of the old plugin ended with the new instance unloaded in its place.
- Parameter changes still waiting for a plugin when it was restarted were lost.
- Record pressed straight after Stop, before the finished take had come back, was dropped.
- Notes played during a count-in were placed at the start of the take.

#### Verified
- On Linux: a second plugin output through its own bus; sidechains; one instrument playing another's notes; four loop takes placed and cut to the sample through a simulated loopback; a punch that replaces the middle of a clip and undoes exactly; a 23-track project bouncing to the same bytes with 0, 1, 2, 3 and 7 render threads; the same project playing identically at buffer sizes from 32 to 2048 frames after live changes; a streamed file playing the same samples as one held in memory; every stageable event; the stress run under RealtimeSanitizer.

#### Not verified
- Anything on a Mac or on real audio hardware, including audio workgroups, aggregate devices and buffer-size changes on CoreAudio; nothing with Kontakt, Superior Drummer 3, EZbass or Helix Native. The phase's first, second and fourth exit criteria cannot be met from here. See [`docs/phases/p4-v1.md`](docs/phases/p4-v1.md).

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

#### Fixed (after the phase's pull request was opened)
- **A healthy plugin could end up on the scan blocklist when two Riddim processes ran at once.** A process that read the scan cache while another was probing a plugin later wrote its stale copy of the "being probed" mark over the other's finished result; the plugin then read as having crashed its host. A process now writes only the entries it changed itself.

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
