# Phase 3: mix

Completion notes for phase 3 of the vision (the third row of §5): sends and buses, latency compensation through them, automation, offline bounce, loading that does not stall, and the scan cache.

**Where it stands (2026-11-21):** the mixing engine is written and everything that can run without a Mac passes, on a deterministic simulated device and with Surge XT standing in for the commercial plugins. **Nothing here has run on a Mac or on real audio hardware, and nothing has been run with Kontakt, Superior Drummer 3, EZbass or Helix Native**, which have not been bought. The phase's first exit criterion (a demo song with all four, bounced, in Whiplash) is therefore not met and cannot be from here.

Still true from earlier phases: GitHub Actions could not start jobs on this repository (an account-level block), and CI is deliberately one small Linux job in any case (decision D12), so the fuller checks are run locally and recorded below.

## What was built

| Piece | Where | State |
|---|---|---|
| Sends, polarity, automation lanes and their edit ops | `crates/riddim-model` | Tested, including by property |
| Send and output-switch nodes in the graph | `crates/riddim-graph` | Tested |
| Automation read (per sample for strips and sends, per block with offsets for plugins) | `crates/riddim-engine` | Tested on the manual device |
| Automation write: touch, latch, write | `crates/riddim-engine` | Tested on the manual device |
| Solo that follows the signal through sends | `crates/riddim-engine` | Tested |
| Bypass that keeps a plugin's latency | `crates/riddim-engine` | Tested |
| Offline bounce: mix, stems, ranges, tails | `crates/riddim-engine` (`bounce`) | Tested; bit-identical |
| Offline process mode told to plugins | `crates/riddim-plugin`, both hosts | VST3 tested on Linux; Audio Unit **never run** |
| Scan cache and blocklist | `crates/riddim-plugin` (`scan`) | Tested on Linux |
| `riddim_touch`, `riddim_bounce` | `crates/riddim-ffi` | A C program calls both in the tests |
| CLI: cached `scan`, mix options for `session new`, stems and tails for `session render`, `session play --save` | `crates/riddim-cli` | Tested on Linux |

## Exit criteria

| # | Criterion | Status |
|---|---|---|
| 1 | Whiplash v0.5's exit: a demo song with all four plugins, bounced | **Not met.** The four plugins are not owned and Whiplash is not built. Stand-in (decision D52): a demo song through Surge XT and Surge XT Effects (instrument, effect return fed by a send, fader automation) is played, saved, reopened and bounced by the test suite on Linux, mix and return stem. The same from the CLI on a Mac is steps 3 to 5 of the checklist |
| 2 | PDC null test: a dry and a latency-compensated path cancel to −120 dBFS or below at the master, through sends and buses | **Met, on Linux.** With the compensated path through a bus, through a send, and with its plugin bypassed, the residual is exactly zero in all three. Each path alone is loud, so the null is not a vacuous one. The latency is the fixture plugin's reported 64 samples; a real plugin that reports its latency wrongly will not null, and that is the plugin's doing |
| 3 | Two offline bounces of a test-node project are bit-identical | **Met.** Two bounces of a project with an instrument, an effect with latency, a bus, a send and automation are the same file byte for byte, start exactly at the timeline's start, and equal what the live engine plays, sample for sample. Also asserted through the CLI |
| 4 | SD3 and Kontakt load without stalling audio or the UI (no xruns during load) | **Met with a stand-in, for audio and for the engine.** A fixture plugin that takes 400 ms to initialise is inserted while a timer-driven device plays: the callback never pauses, the block count keeps pace with the clock, and an edit made during the load is applied at once. Not met for an application that pumps the plugin main thread from its UI thread: the load holds that thread (decision D50). Neither SD3 nor Kontakt has been loaded by anyone |
| 5 | Every target plugin's state survives save/restore | **Met for what is available.** The fixtures round-trip bit for bit through save, reopen and bounce; Surge XT and Surge XT Effects hand over state that is stored in the model, survives the trip through JSON, and loads into the bounce. The four target plugins: not tried |

## What the tests actually establish

All on Linux. Everything except the load test runs on a simulated device that advances only when told to, so results are exact and repeatable.

- **Sends work both ways round.** A post-fader send follows the fader and the mute; a pre-fader send follows neither. Send gain, send mute, the bus's own fader and polarity all behave.
- **Latency compensation reaches every meeting point.** See criterion 2. Bypassing a plugin with latency does not shift its track.
- **Automation is exact.** A fader ramp has the right value on every sample checked, to within float rounding; a step lands on its sample although that sample is in the middle of a block; before the first point and after the last the lane holds. A plugin parameter lane reaches the plugin in the block of each breakpoint. Removing a plugin that has lanes is refused rather than leaving them dangling.
- **Automation can be written.** Holding a fader and moving it during playback writes those moves into the lane and nothing else; what was there before and after the pass is intact; the pass arrives as one edit with its inverse and can be undone. Latch keeps writing after release until stop; write records everything played over. A pass does not span a loop wrap: each time round is its own pass.
- **Solo follows the signal.** Soloing an effect return gives the return alone; soloing a track gives it and its share of the return; mute wins.
- **Bounce is the timeline, repeatably.** See criterion 3. Stems of complementary tracks add up to the mix to within float rounding. A tail extends the file; a range picks a section, sample for sample. A project with a plugin that is not installed still bounces and says what was missing.
- **A slow plugin does not stall anything.** See criterion 4.
- **The scan cache does its job.** A second scan loads no plugin (checked with a scanner that cannot run). A crashing bundle is blocklisted and not probed again; an updated bundle is; `--rescan` and `--unblock` give a blocked bundle another go. If the host dies while a plugin has the scanner hung, the cache file already names the culprit. Two processes saving the same cache keep each other's results.
- **The audio path still does no heap work**, with all of the above on it: asserted in every engine test, and under RealtimeSanitizer.

## Local results

Run on the Linux build machine (x86-64, Rust 1.100.0) with `scripts/ci.sh`, Surge XT 1.3.4 as the third-party plugin, at the commit this file was added in.

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| the same for `--target aarch64-apple-darwin` | clean |
| `cargo test --workspace` | 178 passed, 0 failed, 2 ignored (the hour-long recording and one documentation example). The macOS-only tests compile for Apple Silicon but cannot run here |
| third-party conformance run, Surge XT and Surge XT Effects | passed |
| third-party demo song, Surge XT into Surge XT Effects | passed: mix peak about 0.32 on both bounces and return stem peak about 0.09, varying a little from run to run (Surge XT is not deterministic); 240,000 frames each |
| `scripts/rtsan.sh` (RealtimeSanitizer) | 118 tests passed with the sanitizer live |
| `cargo deny --all-features check` | advisories, bans, licences, sources all ok |
| The CLI flow in the checklist below (steps 2 to 5), on the null device with Surge XT | completed; second scan served from the cache; 0 xruns, 0 audio-thread heap operations |

Not re-run for this phase: the hour-long eight-track recording from phase 2.

## Findings

- **Per-block automation breaks bit-identity between buffer sizes.** The first version ramped a fader to the curve's value at the end of each block. It sounded right and failed two tests: a value was wrong just before a breakpoint that fell mid-block, and a bounce at 256 frames did not equal playback at 128. Strips and sends now read the curve per sample from its breakpoints (D45).
- **"Solo the tracks that feed a soloed bus" is wrong for sends.** With that rule a stem of an effect return contained the dry signal of everything sending to it: the Surge XT return stem peaked at 0.32, the same as the mix. Following the signal instead (D43), it is about 0.09: the effect alone.
- **A scan cache with a crash mark needs to know who made the mark.** The first version marked a bundle as blocked before probing it and cleared the mark after. Two CLI processes sharing a cache then blocked each other's healthy plugins mid-scan (a test caught it). The mark now names the probing process and only counts once that process is gone (D51).
- **A fader that sat still and then moved was recorded as a slow slide.** Joining recorded values with straight lines is right within a gesture and wrong across a pause. Values more than 100 ms apart are now a hold and a step (D49).
- **Surge XT is still not deterministic**, so the demo song's two bounces differ slightly (as in phase 2, D38).

## Known gaps

- **Plugin parameter ramps are stepped per block** (D46); breakpoints are sample-accurate only if the plugin honours event offsets.
- **Written automation is placed to within a block**, not to the sample (D49), and is not thinned beyond dropping repeated values.
- **A bypassed plugin still runs** (D47).
- **A bounce instantiates every plugin a second time** (D48): slow and memory-hungry with big samplers. Sharing instances with the live engine is not planned; freezing tracks would be the remedy.
- **Loading a slow plugin holds the thread that pumps the plugin main thread** (D50). Fixing that for a UI-thread pump needs the out-of-process bridge (v1.x).
- **No lock on the scan cache file** (D51): two hosts scanning the same bundle at the same moment cost a repeated scan, nothing worse.
- **Audio Unit scans are not cached** (D51).
- **One approximate solo case** (D43): a bus that both carries a soloed track and feeds another soloed bus.
- Still open from phase 2: no read-ahead streaming, no tempo ramps, no aggregate devices, live MIDI at block boundaries, looping off while recording, fixed sample rate and buffer size.

## Mac verification checklist

For Phil's Mac. Needs Surge XT installed (it brings Surge XT Effects with it). Nothing here needs an audio interface, but step 4 is better on one. Please paste back the output of anything that fails. The four commercial plugins are step 8, for when they exist.

**0. Build and test**

```sh
git checkout feat/p3-mix
cargo build --workspace --release
cargo test --workspace -- --nocapture 2>&1 | tee p3-mac-tests.log
RIDDIM_TEST_VST3="/Library/Audio/Plug-Ins/VST3/Surge XT.vst3;/Library/Audio/Plug-Ins/VST3/Surge XT Effects.vst3" \
  scripts/ci.sh 2>&1 | tee p3-mac-ci.log
```

Good: all green, ending in `All checks passed.` The lines starting `demo song:` should show non-zero peaks. (This includes the phase 1 and 2 test suites; if those have not been run on the Mac yet, their Audio Unit, editor and CoreAudio tests are the part that has never run anywhere.)

**1. The scan cache**

```sh
time ./target/release/riddim scan
time ./target/release/riddim scan
./target/release/riddim scan --blocklist
```

Good: the same list both times, the second in a fraction of the time of the first; the blocklist is empty or names plugins that really do misbehave, each with a reason. If a plugin you use is listed, `riddim scan --unblock "/path/to/It.vst3"` and scan again, and send me what the first scan printed about it. The cache is `~/Library/Caches/riddim/plugin-scan.json`.

**2. Build a small mix**

```sh
mkdir -p ~/riddim-test
./target/release/riddim session new ~/riddim-test/mix.json \
  --instrument "Surge XT" --sequence --send-effect "Surge XT Effects" --fade-in 4
./target/release/riddim session info ~/riddim-test/mix.json
```

Good: an instrument track with a send to track 1000 and an automation lane, and an "Effect return" bus carrying Surge XT Effects. To try Audio Units instead, name them the way `riddim scan` lists them (for example `--send-effect "AUMatrixReverb"`).

**3. Play it**

```sh
./target/release/riddim session play ~/riddim-test/mix.json --editor
```

Good: the arpeggio fades in over two seconds with the effect on it, the editor opens, and on Enter the summary shows `xruns 0` and `audio-thread heap operations 0`.

**4. Load something heavy with the device running** (criterion 4, with whatever is installed)

```sh
./target/release/riddim session new ~/riddim-test/heavy.json \
  --instrument "<the slowest-loading instrument you have>" --sequence
./target/release/riddim session play ~/riddim-test/heavy.json --seconds 20
```

Good: `xruns 0` in the summary however long the instrument took to load (the audio device is already running, playing silence, while the plugin loads, and the count covers that time). This is the test that wants Kontakt or Superior Drummer 3; until then any sampler will do.

**5. Bounce: mix, stem, tail**

```sh
cd ~/riddim-test
~/path/to/riddim session render mix.json -o mix.wav --seconds 8 --tail 2
~/path/to/riddim session render mix.json -o return.wav --seconds 8 --tail 2 --stem 1000
afinfo mix.wav
```

Good: `mix.wav` is 10 seconds, starts with the fade-in, and sounds like step 3; `return.wav` is the effect alone (clearly quieter, no dry synth). Two renders print different fingerprints with Surge XT; with plugins that are deterministic they should match.

**6. State survives**

```sh
./target/release/riddim session play ~/riddim-test/mix.json --editor --save
```

Change the patch and a few knobs in the editor before pressing Enter. Good: `saved ... with the plugins' current settings`; `session info` now shows bytes of state for both plugins; `session play` and `session render` afterwards both have the changes.

**7. Automation by hand** is not reachable from the CLI (touch, latch and write need a fader to hold). It is covered by the tests in step 0; from Whiplash it is `riddim_touch` plus ordinary gain ops.

**8. The four target plugins**, when they are installed: repeat steps 2 to 6 with each as the instrument or the effect (`--instrument "Kontakt 7"`, `--instrument "Superior Drummer 3"`, `--instrument "EZbass"`, `--insert "Helix Native"`), and step 4 with Kontakt and Superior Drummer 3 loading a full library. That is exit criteria 1, 4 and 5.
