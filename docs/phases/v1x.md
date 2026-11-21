# v1.x

Completion notes for the fifth row of the vision's phasing (§5): CLAP, the out-of-process bridge, a headless renderer, and a public API stability promise.

**Where it stands (2026-11-21):** all four are written, and everything that can run without a Mac passes on Linux. CLAP hosting has been run against real plugins (Surge XT's CLAP builds); the bridge against the test plugins, including ones told to crash and to hang. **Nothing here has run on a Mac**, and the bridge's first exit criterion (Kontakt bridged at 128 frames with no xruns) needs Kontakt, which has not been bought. The stability promise is written and guarded but not made: that is Phil's to do, with a 1.0.

The GitHub check is green on this pull request and on every phase branch below it (2026-11-21, 247 tests passed here). Until the repository was made public (decision D81) GitHub Actions could not start jobs on it (an account-level block), and the same job was run in a clean Ubuntu 24.04 container on two cores with `scripts/ci-clean.sh` instead (decision D73). CI is one small Linux job in any case (decision D12), so the fuller checks are run locally and recorded below.

Where every phase's exit criteria stand, on one page: [`README.md`](README.md).

## What was built

| Piece | Where | State |
|---|---|---|
| CLAP host | `crates/riddim-plugin/src/format/clap.rs` | Run on Linux against Surge XT 1.3.4 (CLAP). Editor **never opened**. macOS **never run** |
| CLAP scanning and resolution | `riddim-plugin` (`scan`), `riddim-cli` | Run on Linux |
| CLAP fixture plugins: a synth and a delay with latency, on the C ABI directly (D74) | `fixtures/clapplug` | Loaded by every test run on Linux. macOS **never run** (the `.clap` there is a bundle) |
| Bridge: child process, shared memory, control protocol | `crates/riddim-plugin/src/bridge.rs`, `src/bin/riddim-bridge.rs` | Tested on Linux. macOS **never run** |
| Bridged slots in the model and the engine; crash handling | `riddim-model`, `riddim-engine` | Tested on Linux |
| `riddim-render` | `crates/riddim/src/bin/riddim-render.rs` | Tested on Linux |
| Stability policy and its guards | `docs/api/stability.md`, `crates/riddim-model/tests/compat.rs` and `tests/golden/` | In place |

## Exit criteria

| Criterion | Status |
|---|---|
| CLAP: a CLAP synth passes the P1 checks | **Met on Linux.** Surge XT (CLAP) passes every step of the conformance run: scan, instantiate, buses, 775 parameters, activate, process, sound from a chord, automation with sample offsets, finite output, no host-side heap traffic, state save, restore and round trip, reactivation at another rate and block size. Surge XT Effects (CLAP) likewise. The phase 1 checks that need a person (open the editor, play a keyboard, hear it come back after a relaunch) are in the Mac checklist |
| Bridge: Kontakt bridged at 128 frames with no xruns | **Not met**: needs Kontakt and a Mac. What exists: fixture plugins bridged at 128 frames on a timer-driven device, serving every block; nothing about the CPU cost with a real sampler |
| Bridge: a crashing plugin doesn't take the session down | **Met on Linux.** A bridged plugin whose process aborts, and one whose `process` hangs, are noticed within the bridge's patience (100 ms at this block size), give silence, and are reported (`plugin_crashed`); the transport keeps running, the other track keeps sounding, edits keep applying, nothing touches the heap on the audio path, and removing and re-inserting the slot starts a fresh process that plays |

The headless renderer and the stability promise have no exit criteria in the vision. The renderer is tested against the library bounce byte for byte. The promise is a document and a set of checks (decision D72).

## What the tests actually establish

- **The CLAP host and a plugin written against the bare C ABI agree** (the fixtures share no code with the host): notes and parameter changes land on their sample, a parameter's plain range is mapped to 0 to 1 and back, state survives save, restore and save bit for bit and another plugin's state is refused, latency is read when CLAP allows it, a block larger than promised never reaches the plugin, the plugin is told when a render is offline (D76), and nothing on the host's side of `process` touches the heap.
- **In the engine a CLAP plugin's latency is compensated to the sample**: two tracks of the CLAP synth, one through the CLAP delay with its polarity flipped, cancel to exactly zero. Its state is saved with the project and restored; a bounce through CLAP plugins is the same bytes each time and with render threads; and the same project with its CLAP plugins bridged plays the same samples.
- **A CLAP plugin is a plugin like any other to the engine.** The demo song (an instrument, an effect return fed by a send, fader automation, a second output and both sidechains routed) plays, saves, reopens and bounces through Surge XT's CLAP builds exactly as through its VST3 builds.
- **A bridged plugin is indistinguishable from the same plugin in process**, apart from where it runs: the same buses, parameters, parameter text, state bytes, events and audio, bit for bit; an effect's latency and a change to it cross the bridge; a project renders and bounces to the same bytes with its plugins bridged, in process, or mixed, and the saved project differs only in the `bridged` flags.
- **A bridged plugin that dies costs only itself.** See the exit criteria.
- **A bridge that cannot start fails its plugin, not the engine.**
- **`riddim-render` is the library bounce**, byte for byte, with or without render threads; it finds media beside the model, takes ranges, tails, formats and stems, reads the model from standard input, and reports failures by exit status.
- **A schema-1 project and every kind of op still load with nothing lost**, and a project claiming a newer schema is refused.

## Local results

Run on the Linux build machine (x86-64, 8 cores and 16 threads, Rust 1.100.0) with `scripts/ci.sh`, with Surge XT 1.3.4's VST3 and CLAP builds, on the branch's final tree.

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| the same for `--target aarch64-apple-darwin` | clean |
| the same for `--target x86_64-pc-windows-msvc` (type-check of the portable core, decision D82) | clean |
| `cargo test --workspace` | 247 passed, 0 failed, 2 ignored (the hour-long recording and one documentation example). The macOS-only tests compile for Apple Silicon but cannot run here |
| the CI job in a clean container (`scripts/ci-clean.sh`: fmt, clippy, test, on two cores) | passed: 247 tests; likewise 66, 158, 180 and 209 for the heads of the four phase branches below this one |
| third-party conformance run: Surge XT and Surge XT Effects, VST3 and CLAP | passed, all four |
| third-party demo song, VST3 and CLAP | passed, both |
| `scripts/rtsan.sh` (RealtimeSanitizer) | 171 passed, 0 failed, with the sanitizer live, including the bridge tests, the CLAP fixture tests and the CLAP demo song |
| `cargo deny --all-features check` | advisories, bans, licences, sources all ok (new dependencies: `clack-host`, `clack-extensions`, `clap-sys`, `libc`; MIT or Apache-2.0) |
| `scripts/stress.sh 60` under RealtimeSanitizer | passed, run on two cores while the rest of the machine was busy with the hour-long sessions: 15,175 edits, 199 buffer-size changes and 375 takes in 60 s; 29,943 blocks served; 0 heap operations on the audio path, 0 messages lost. (Five threads on two cores: the device's timer ran late 2,466 times, which is the machine's doing and not something the run fails on) |

## Findings

- **Surge XT's CLAP build returns an error from `process` unless its first output port has one or two channels.** That is what a port-splitting helper of mine was handing it: zero channels per port, because a slice's length was read after the slice had been moved out. The conformance run failed on the first block, which is what it is for.
- **Mutual recursion on the control thread.** Writing automation from parameter changes reported by the audio thread called back into the function that drains the audio thread's messages; with a plugin that reports a parameter every block, the stack ran out. Found by an existing CLI test within a minute of the change.
- **RealtimeSanitizer caught the CLAP host calling the plugin unguarded.** The VST3 and Audio Unit hosts exempt the plugin's own `process` from the real-time checks (its allocations are its own affair); the CLAP host did not, and Surge XT allocates in `process`. The first full sanitizer run with a CLAP plugin stopped on it.
- **The CLAP fixture found a contract breach in the CLAP host on its first run.** A block larger than the plugin was activated for was handed to the plugin, and for an effect the host indexed past its own input buffers first: a panic on the audio thread. The engine never sends such a block (it splits them), so this could not be heard; the VST3 host already refused them. Fixed, with a test for an instrument and for an effect.
- **The scan cache could blocklist a healthy plugin** (D75), a phase 3 bug. Two processes sharing the cache file: one reads it while the other is mid-probe, and later writes its stale copy of the "being probed" mark over the other's result. Found because the new CLAP test in the CLI suite runs its commands alongside fourteen other tests' processes on one cache file; it failed about one run in two until this was fixed.
- **A stand-in for phase 4's reference session was run from this branch** (four Surge XT instruments and twenty Surge XT effects at 64 frames for an hour, on a timer): the engine's share of a block is small, and the dropouts that session has on this machine are Surge XT reading the processor's hardware random number generator on every note. The account is in [`p4-v1.md`](p4-v1.md), under "A stand-in for the reference session".
- **Environment variables are a poor way to configure a test.** The bridge executable was first found through `RIDDIM_BRIDGE` alone, and tests running in parallel set it under each other. It is now a field of the engine configuration, with the variable as a fallback.

## Known gaps

- **CLAP `stop_processing` is called off the audio thread** (D68).
- **CLAP extensions not used**: tail, thread-check, voice-info, note names, presets, timers and file descriptors (which some Linux plugin editors need).
- **A bridged plugin has no editor** (D69). On macOS this needs a window owned by the child.
- **One plugin per bridge process** (D69); no grouping by manufacturer.
- **The bridge's patience is a constant** (D70).
- **The bridge is Unix only.**
- **`riddim-render` and `bounce` instantiate every plugin afresh**, as before (D48).

## Mac verification checklist

For Phil's Mac. Needs Surge XT installed, which brings its CLAP builds (`/Library/Audio/Plug-Ins/CLAP/Surge XT.clap`). Please paste back the output of anything that fails.

**0. Build and test**

```sh
git checkout feat/v1x
cargo build --workspace --release
RIDDIM_TEST_VST3="/Library/Audio/Plug-Ins/VST3/Surge XT.vst3;/Library/Audio/Plug-Ins/VST3/Surge XT Effects.vst3" \
RIDDIM_TEST_CLAP="/Library/Audio/Plug-Ins/CLAP/Surge XT.clap;/Library/Audio/Plug-Ins/CLAP/Surge XT Effects.clap" \
  scripts/ci.sh 2>&1 | tee v1x-mac-ci.log
```

Good: `All checks passed.`, four plugins ending `=> passed`, and two lines starting `demo song (`. The bridge tests (`bridge` and `bridged`) are part of this and have never run on macOS: shared memory, Unix sockets and child processes behave a little differently there, so they are the ones to watch.

**1. CLAP: the phase 1 checks, by hand**

```sh
R=./target/release/riddim
$R scan | grep -i clap
$R check "/Library/Audio/Plug-Ins/CLAP/Surge XT.clap"
$R play "/Library/Audio/Plug-Ins/CLAP/Surge XT.clap" --sequence --save-state /tmp/surge-clap.state
```

Good: Surge XT is listed with a `clap:` id; `check` ends `=> passed`; `play` opens Surge XT's editor in a window, the arpeggio sounds, knob changes are heard, a MIDI keyboard plays along, and Enter ends with `xruns 0`. Then `$R play "/Library/Audio/Plug-Ins/CLAP/Surge XT.clap" --sequence --state /tmp/surge-clap.state` should come back sounding as it was left. The editor is the part that has never run anywhere.

**2. The bridge**

```sh
mkdir -p ~/riddim-test && cd ~/riddim-test
R=~/path/to/riddim/target/release/riddim
$R session new bridged.json --instrument "Surge XT" --sequence --send-effect "Surge XT Effects" --bridged
$R session info bridged.json
$R session play bridged.json --seconds 20 --buffer 128
```

Good: both plugins are shown `in a process of its own`; it plays as the unbridged project does; `xruns 0`; and `ps aux | grep riddim-bridge` in another terminal shows two child processes while it plays and none after. No editor opens for a bridged plugin: that is a known gap, not a failure.

**3. Kill a bridged plugin while it plays**

While step 2's `session play` is running (use `--seconds 60`), in another terminal: `kill -9 $(pgrep -f riddim-bridge | head -1)`. Good: that plugin goes silent, the other carries on, the run ends normally with `audio-thread heap operations 0`, and nothing crashes.

**4. The headless renderer**

```sh
$R session render bridged.json -o cli.wav --seconds 8
~/path/to/riddim/target/release/riddim-render bridged.json --out render.wav --media-dir bridged.media --seconds 8
```

Good: `riddim-render` prints one line of JSON with `"frames":384000` and an empty `missing_plugins`, and `render.wav` sounds like `cli.wav`. (The CLI's project file is the engine's model JSON, so `riddim-render` reads it directly.)

**5. Kontakt, bridged**, when it is installed: step 2 with `--instrument` naming Kontakt and `--buffer 128`, a full instrument loaded, for several minutes. Good: `xruns 0`. That is the bridge's first exit criterion.
