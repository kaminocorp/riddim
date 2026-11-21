# Phase 1: hosting spike

Completion notes for phase 1 of the vision (§4.5 and the first row of §5).

**Where it stands (2026-11-21):** the code for the phase is written. Everything that can run on Linux has been run and passes. **Nothing macOS-specific has been run on a Mac**, so the phase's own exit criteria, which are about a Mac, are **not met yet**: they wait on the checklist at the end of this file.

Two things changed the target during the phase:

- Phil has not bought the four commercial plugins yet (his note of 2026-11-21). The phase was built and tested against **free plugins** instead: Riddim's own test plugins, Surge XT 1.3.4, and (once a Mac runs them) Apple's built-in Audio Units. Wherever the vision says "Kontakt/Noire", read "Surge XT" for now.
- **GitHub Actions cannot start any job on this repository**: every job ends at once with "recent account payments have failed or your spending limit needs to be increased". The workflows are in place and correct as far as can be told without running them; until billing is sorted, the same checks are run locally with `scripts/ci.sh` and the results are recorded here. This also means the macOS workflow, which would have run the Audio Unit host against Apple's units on a real Mac, has not run.

## What was built

| Piece | Where | State |
|---|---|---|
| Real-time building blocks: allocation guard, deferred drop, queues, denormals, smoothing | `crates/riddim-rt` | Tested on Linux |
| Plugin contract: descriptors, instance and processor traits, events, state blobs | `crates/riddim-plugin` | Tested on Linux |
| VST3 host | `crates/riddim-plugin/src/format/vst3` | Tested on Linux against the fixtures and Surge XT. macOS bundle loading and `NSView` editors unverified |
| Audio Unit host | `crates/riddim-plugin/src/format/au` | Type-checks for Apple Silicon. **Never run** |
| Out-of-process scanner (`riddim-scan`) | `crates/riddim-plugin/src/scan.rs`, `src/bin/riddim-scan.rs` | Tested on Linux, including crash, segfault and hang |
| Conformance runner | `crates/riddim-plugin/src/check.rs` | Tested on Linux |
| Editor window host | `crates/riddim-plugin/src/editor_window.rs` | Type-checks for Apple Silicon. **Never run** |
| Deviceless audio backend, MIDI parser | `crates/riddim-io` | Tested on Linux |
| CoreAudio HAL output, CoreMIDI input | `crates/riddim-io/src/coreaudio.rs`, `midi.rs` | Type-check for Apple Silicon. **Never run** |
| `riddim` CLI | `crates/riddim-cli` | Tested on Linux (with the deviceless backend) |
| Test plugins and support | `fixtures/` | Used by the tests above |

## Exit criteria

The vision's five criteria, and the honest status of each.

| # | Criterion | Status |
|---|---|---|
| 1 | The target instrument loads **as VST3 and as AU**; its editor opens, resizes and closes cleanly 10 times in a row | **Not met.** VST3: Surge XT loads, plays and passes the conformance run on Linux; its editor has not been opened (Linux editors are out of scope). AU: not run. Editor cycling: not run. Mac checklist steps 3 to 6 |
| 2 | A MIDI note sounds within one buffer of input, with no glitches over 10 minutes at 128 frames | **Not met.** By construction, live MIDI is applied at the start of the next block. A 10-minute run of Surge XT at 128 frames on the deviceless backend completed with no failed blocks and no host-side heap operations (see "Local results"), but that backend is a timer thread, not an audio device, so it says nothing about glitches. Mac checklist step 7 |
| 3 | Save, quit, relaunch, restore: the patch and edited parameters come back, and save → restore → save gives an identical blob | **Met on Linux for VST3** (fixtures and Surge XT, in one process and across processes through state files). **AU not run.** Mac checklist step 8 |
| 4 | A deliberately crashing test plugin kills only the scanner, never the host | **Met on Linux** (abort, segfault and hang are all contained). Expected to hold on macOS; checklist step 2 confirms |
| 5 | A write-up: per-format surprises, thread requirements, whether `instantiate` needs the main thread | **Partly.** Linux findings are below. The questions that need a Mac, including the main-thread one, are listed in checklist step 9 |

## Findings so far

What hosting real plugins on Linux has taught us. All of it is from the fixtures and Surge XT.

- **Bus activation defaults are not what you would guess.** Surge XT's first input bus is a sidechain, and both Surge plugins mark their sidechain "default active". Forcing "bus 0 is always on" would have treated an instrument as an effect. The host now follows the plugin's flag for everything except a main bus in first position (decision D18).
- **A plugin can have thousands of parameters.** Surge XT exposes 2,855 (774 automatable). Anything per-parameter has to be cheap; the phase 3 automation design should not assume small counts.
- **Processor and controller really are separate objects that talk through host-provided messages.** Surge XT (built with JUCE) needs the host's `IMessage` and `IAttributeList` implementations as soon as it is connected. Those are implemented and exercised.
- **State is deterministic for these plugins.** Surge XT's 50 kB state survives save → restore → save bit for bit.
- **The first block after a note-on already carries sound** for Surge XT; the conformance run nevertheless waits up to 400 blocks, because samplers that load asynchronously will not.
- **Plugin tails can be long**: Surge reports 88,200 samples. Bounce and "stop" logic in later phases must honour it.

Not yet learned, because it needs a Mac: everything about Audio Units, editor resizing, CFBundle loading, and which calls plugins insist on receiving on the main thread.

## Local results

Run on the Linux build machine (x86-64, Rust 1.100.0) at the commit this file was last changed in.

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| the same for `--target aarch64-apple-darwin` (type-check of all macOS code and macOS-only tests) | clean |
| `cargo test --workspace` | 66 passed, 0 failed. The macOS-only tests (`au_host`, `mac_editor`, `coreaudio_device`) compile for the Apple Silicon target but cannot run here |
| third-party conformance run, Surge XT and Surge XT Effects | passed, including "identical blob" and "0 heap operation(s) on the host side of process" |
| `cargo deny --all-features check` | advisories, bans, licences, sources all ok |
| 10 minutes of Surge XT playing the test arpeggio at 48 kHz / 128 frames, release build, deviceless backend | 224,965 blocks, 0 failed blocks, 0 audio-thread heap operations, peak 0.42 |
| state saved at the end of that run, restored in a new process, saved again | identical (50,259 bytes) |

## Known gaps

- **Audio Unit version 3 view controllers** are not requested; such units get the generic view (decision D19).
- **MIDI output from Audio Units** is not wired up (planned for phase 4 with the rest of plugin MIDI out). VST3 event output is.
- **Different input and output devices** are not supported by the CoreAudio backend yet (decision D20).
- **No scan cache or blocklist on disk.** Every `riddim` invocation that needs to find a plugin by name rescans. The cache is phase 3 work in the vision.
- **VST3 editors on Linux** are not supported and are not planned.
- **Live MIDI is not sample-accurate within the block**: messages are applied at the start of the next block. That meets the phase's "within one buffer"; timestamp-accurate placement comes with the engine in phase 2.

## Mac verification checklist

For Phil's Mac. Each step says what to run and what "good" looks like. Please paste back the output of any step that fails; steps 3 onward only need Surge XT (free, from https://surge-synthesizer.github.io) installed with its VST3 and AU components.

**0. Build and run the test suite**

```sh
git checkout feat/p1-hosting-spike
cargo build --workspace --release
cargo test --workspace -- --nocapture 2>&1 | tee p1-mac-tests.log
```

Good: every test passes. This is the first time the Audio Unit host, the editor window host and the CoreAudio code run at all. The `au_host` tests drive Apple's built-in units; `mac_editor` opens and closes real windows 20 times (expect them to flash on screen); `coreaudio_device` lists devices and runs a silent stream for under a second.

**1. Devices**

```sh
./target/release/riddim devices
```

Good: your interface and the built-in devices are listed with plausible channel counts, sample rate and buffer range.

**2. Scan (and crash containment)**

```sh
./target/release/riddim scan
```

Good: Surge XT appears twice (`vst3` and `au`), Apple's units appear as `au`, and the command finishes even if some installed plugin misbehaves (those are reported as warnings on stderr). Note how long it takes.

**3. Conformance run, both formats**

```sh
./target/release/riddim check "/Library/Audio/Plug-Ins/VST3/Surge XT.vst3"
./target/release/riddim check "au:aumu:SgXT:VmbA"
```

(The `au:` id is from memory and may be wrong: copy the real one for Surge XT from the scan output in step 2, and use it wherever an `au:` id appears below.) Good: both end with `=> passed`, and both show `[PASS] state round trip  identical blob` and `0 heap operation(s)`.

**4. Play it (VST3)**

```sh
./target/release/riddim play "/Library/Audio/Plug-Ins/VST3/Surge XT.vst3" --sequence --buffer 128
```

Good: you hear an arpeggio; Surge's editor window opens and is usable; pressing Enter in the terminal stops cleanly and prints a line with `failed 0`, `xruns 0` and `audio-thread heap operations 0`.

**5. Play it (AU)** — the same with the `au:` id. Good: as step 4.

**6. Editor cycling, both formats**

```sh
./target/release/riddim editor-cycle "/Library/Audio/Plug-Ins/VST3/Surge XT.vst3" --count 10
./target/release/riddim editor-cycle "au:aumu:SgXT:VmbA" --count 10
```

Good: the window opens, resizes and closes ten times with no crash and no leftover windows, and the command prints `10 open/resize/close cycle(s) completed`. Also try dragging the window's corner by hand during step 4: the plugin should follow.

**7. Live MIDI and the 10-minute run**

With a MIDI keyboard connected:

```sh
./target/release/riddim play "/Library/Audio/Plug-Ins/VST3/Surge XT.vst3" --buffer 128 --seconds 600
```

Good: the keyboard is listed as `midi in:` at startup; notes sound with no perceptible lag; over ten minutes of playing there are no clicks or dropouts, and the final line shows `xruns 0`.

**8. State across a relaunch, both formats**

```sh
./target/release/riddim play "<plugin>" --save-state /tmp/surge.rstate     # pick another patch, turn a knob, press Enter
./target/release/riddim play "<plugin>" --state /tmp/surge.rstate          # relaunch
./target/release/riddim state roundtrip "<plugin>" --state /tmp/surge.rstate
```

Good: on relaunch the patch and the knob are as you left them, and the last command prints `identical`. If it prints `DIFFERENT`, the numbers it gives are what we need.

**9. Questions only a Mac can answer** (for the phase's write-up)

- Did anything in step 0 fail or crash? Which test?
- Does Surge's AU editor show its own interface, or Apple's generic sliders?
- Does resizing work in both formats?
- Any console messages about "main thread" from either format?
- How long did the scan in step 2 take?

**Later, once bought:** repeat steps 3 to 8 with Kontakt 8 Player + Noire. That is the vision's actual exit bar for this phase, and it stays open until then.
