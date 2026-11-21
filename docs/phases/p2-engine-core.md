# Phase 2: engine core ("tape")

Completion notes for phase 2 of the vision (the second row of §5): the engine that plays, records and mixes, behind the public API and the C ABI.

**Where it stands (2026-11-21):** the engine is written and everything that can run without a Mac passes, on a deterministic simulated device. **Nothing here has run on a Mac or on real audio hardware.** The phase's first exit criterion (Phil records a take over an instrument in Whiplash, saves, reopens and hears it identically) cannot be met from here: it needs a Mac, real hardware and Whiplash. A terminal stand-in for that flow exists (`riddim session`) and is what the Mac checklist below asks to be run.

Still true from phase 1: GitHub Actions could not start jobs on this repository (billing), and CI is deliberately one small Linux job in any case (decision D12), so the fuller checks are run locally and recorded here; and the commercial plugins have not been bought, so Surge XT stands in for the instrument.

## What was built

| Piece | Where | State |
|---|---|---|
| Integer time: superclock, ticks, tempo and meter map | `crates/riddim-time` | Tested, including by property |
| Engine model and edit ops with exact inverses | `crates/riddim-model` | Tested, including by property |
| Render-plan compiler (order, latency compensation, schedule) | `crates/riddim-graph` | Tested, including by property |
| WAV/RF64 files, capture thread, import resampler, MIDI files | `crates/riddim-disk` | Tested on Linux |
| The engine: control, audio and main-thread sides | `crates/riddim-engine` | Tested on Linux on the manual device |
| Public Rust API | `crates/riddim` | Tested on Linux |
| C ABI v0 and header | `crates/riddim-ffi` | A C program drives the engine through it in the tests |
| Engine-based CLI: `play`, `render`, `session ...`, `loopback` | `crates/riddim-cli` | Tested on Linux with the null device |
| CoreAudio capture (same device as output), CoreMIDI | `crates/riddim-io` | Type-checks for Apple Silicon. **Never run** |

## Exit criteria

| # | Criterion | Status |
|---|---|---|
| 1 | Whiplash v0's exit: Phil records a take over the instrument, saves, reopens and hears it identically | **Not met**, and cannot be from here. What exists: the same flow from a terminal (`riddim session new / record / play / render`), run on Linux with Surge XT through a simulated cable. With deterministic plugins a saved and reopened project renders bit for bit the same (tested). Mac checklist steps 4 to 6 |
| 2 | Recorded audio lands within ±1 sample of a loopback-calibrated position | **Met in simulation, exactly** (0 samples) for round-trip latencies of 128, 300, 777 and 1024 frames, including punch-in while rolling and after a count-in. On real hardware it depends on the device reporting its latency truthfully; `riddim loopback` measures it. Mac checklist step 2 |
| 3 | Zero allocations on the audio thread under the CI RealtimeSanitizer run | **Met locally.** The engine, host, I/O and disk test suites pass under RealtimeSanitizer, with a self-check proving the sanitizer is live. Every engine test also asserts zero heap operations on the audio path with our own counting allocator. It runs locally as part of `scripts/ci.sh`; it is not a CI job, because Actions minutes are a hard budget (D12) |
| 4 | Ops round-trip: apply → inverse → apply restores an identical model (property test) | **Met.** 400 random edit histories per run; also checked through the engine |
| 5 | 60 minutes of continuous recording on 8 tracks with no dropped buffers, and `kill -9` mid-take keeps everything up to the last flush | **Met as far as it can be without a clock.** An hour of timeline on eight tracks, fast-forwarded, loses no frame (172,799,744 frames per track, all accounted for). A recording process killed with `SIGKILL` leaves a readable take holding everything that reached the file. A real-time hour on real disks is Mac checklist step 7 |

## What the tests actually establish

All on Linux, on a simulated device that advances only when told to, so every result is exact and repeatable.

- **Timing is sample-accurate.** An audio clip at sample 1000 plays at sample 1000, bit for bit. A MIDI note on beat 2 sounds at sample 24,000 (the fixture synth's first non-zero sample is 24,001 by construction). Loops wrap on the sample. The metronome clicks on 0, 24,000, 48,000...
- **Edits never disturb what they do not touch.** Plugin processors, delay lines, held notes, gain ramps and meters survive every plan swap; a plugin can be removed and re-added while playing.
- **Latency compensation works where paths meet.** A track through a plugin with 64 samples of latency and a dry track carrying the same impulse sum to a single sample of amplitude 2.
- **A project survives save and reopen.** Model to JSON, new engine, load: the render is bit-identical, plugin state included.
- **Recording lands where it was played.** See criterion 2.
- **The audio path does no heap work.** Asserted in every engine test, and independently under RealtimeSanitizer (which also catches locks and blocking calls).
- **The C ABI is usable from C.** A C99 program compiled with `-Wall -Wextra -Werror -pedantic` against the checked-in header creates an engine, edits, plays, renders, queries and reloads a model.

## Local results

Run on the Linux build machine (x86-64, Rust 1.100.0) at the commit this file was last changed in.

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| the same for `--target aarch64-apple-darwin` | clean |
| `cargo test --workspace` | 158 passed, 0 failed, 2 ignored (the hour-long recording, run separately below, and one documentation example). The macOS-only tests compile for Apple Silicon but cannot run here |
| `scripts/rtsan.sh` (RealtimeSanitizer) | 103 tests passed with the sanitizer live (its self-check passed) |
| third-party conformance run, Surge XT and Surge XT Effects | passed |
| `cargo deny --all-features check` | advisories, bans, licences, sources all ok |
| `long_recording` (ignored by default: one hour, eight tracks, 5.5 GB) | passed in about 90 s; worst disk backlog 1.2 s of a 4 s ring; 0 frames dropped; 0 audio-thread heap operations |
| `riddim session` flow with Surge XT (new, record through simulated loopback, save, reopen, render) | completed; take of the expected length; 0 frames lost; 0 audio-thread heap operations |

## Findings

- **Surge XT is not deterministic.** Two renders of the same project differ (peak 0.45 against 0.42 in one pair). Its default patch has free-running oscillator phase. Bit-identical checks therefore use the fixture plugins (decision D38). Expect the same of Kontakt-style samplers with round-robins.
- **A "green" sanitizer run can be an empty one.** The first RealtimeSanitizer run passed because the runtime had not been initialised and was checking nothing. The self-check in `riddim-rt/tests/rtsan_active.rs` exists because of that.
- **Fast-forwarded recording is harsher than real time.** At sixty times real time the four-second capture ring is 70 ms of wall clock, so a busy machine can overflow it in a way real time never would. The engine now reports the recording backlog (`Status::capture_backlog`), which is also the number a disk-load meter wants.
- **The control thread does not know the playhead exactly.** An early version computed the recording start on the control thread from the last published position, which is stale by up to a block and wrong after a locate. The audio thread now fixes the start and reports it (decision D31).

## Known gaps

- **No read-ahead streaming**: media is played from memory (D29).
- **No tempo ramps** (D24).
- **No aggregate devices**: input and output must be the same CoreAudio device (D37).
- **Live MIDI is placed at block boundaries**, for playing and for recording (D33).
- **MIDI takes are not crash-safe** (D31).
- **Looping is off while recording** (D32); loop recording and takes are phase 4.
- **A bypassed plugin with latency is misaligned** (D40); fixed in phase 3.
- **Plugins are never told they are rendering offline** (D39); phase 3.
- **The sample rate and buffer size are fixed for the life of an engine**; changing them at run time is phase 4.

## Mac verification checklist

For Phil's Mac. Needs Surge XT installed and, for steps 2 and 7, an audio interface. Please paste back the output of anything that fails.

**0. Build and test**

```sh
git checkout feat/p2-engine-core
cargo build --workspace --release
cargo test --workspace -- --nocapture 2>&1 | tee p2-mac-tests.log
scripts/rtsan.sh 2>&1 | tee p2-mac-rtsan.log
```

Good: all green. (The phase 1 checklist's step 0 is included in this; if phase 1 has not been run on the Mac yet, expect its Audio Unit and editor tests to be the interesting part.)

**1. Devices** — `./target/release/riddim devices`. Good: the interface is listed with inputs and outputs.

**2. Recording alignment (the phase's ±1 sample criterion)**

Connect a cable from the interface's output 1 to its input 1, set the interface as the system's default output device, turn monitoring off on the interface, then:

```sh
./target/release/riddim loopback --input 1 --buffer 128
```

Good: `offset from where they were played: 0 to 0 samples` (anything within ±1 passes). If the offset is a constant other than zero, the interface under-reports or over-reports its latency by that much; the number is what we need. Try `--buffer 64` and `--buffer 512` too.

**3. Play through the engine**

```sh
./target/release/riddim play "Surge XT" --sequence
```

Good: the arpeggio loops, the editor opens and works, knob changes are heard, a MIDI keyboard plays along, and on Enter the summary shows `xruns 0` and `audio-thread heap operations 0`.

**4. Record a take over the instrument**

```sh
mkdir -p ~/riddim-test
./target/release/riddim session new ~/riddim-test/song.json --instrument "Surge XT" --sequence --audio-input 1 --count-in 1
./target/release/riddim session record ~/riddim-test/song.json
```

Sing or play into input 1 after the one-bar count-in; press Enter to stop. Good: `take on track 4: N frames`, `saved ... with 1 new take(s)`, and `frames lost while recording 0`.

**5. Reopen and listen**

```sh
./target/release/riddim session info ~/riddim-test/song.json
./target/release/riddim session play ~/riddim-test/song.json
```

Good: the info shows the take and the instrument's saved state; playback has the take in time with the arpeggio, and the instrument sounds as it did (same patch, same knob positions).

**6. Render**

```sh
./target/release/riddim session render ~/riddim-test/song.json -o ~/riddim-test/song.wav --seconds 10
```

Good: the WAV sounds like step 5. (Two renders will print different fingerprints with Surge XT; see Findings.)

**7. An hour on eight tracks, and a hard kill**

```sh
./target/release/riddim session new ~/riddim-test/long.json --audio-input 1 --audio-tracks 8
./target/release/riddim session record ~/riddim-test/long.json --seconds 3600
```

Good after the hour: eight takes of about 172.8 million frames each, `xruns 0`, `frames lost while recording 0`.

Then start the same command again and, a minute in, kill it from another terminal with `kill -9 $(pgrep -f "riddim session record")`. Good: the eight `take-*.wav` files in `~/riddim-test/long.media/` open in QuickTime or `afinfo` and are about a minute long.

**8. From Swift or C** (optional, when Whiplash work starts): `crates/riddim-ffi/include/riddim.h` and `crates/riddim-ffi/tests/smoke.c` show the whole surface in use.
