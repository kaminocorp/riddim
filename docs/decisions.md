# Decisions log

Decisions taken while implementing the vision ([`docs/visions/riddim.md`](visions/riddim.md)), for Phil to review. Each entry says what was decided, why, and how to reverse it. Entries are numbered in the order they were taken and are never renumbered.

"Vision Qn" refers to the open questions in §7.2 of the vision.

## Answers to the vision's open questions

| # | Question | Taken | Basis |
|---|---|---|---|
| D1 | Vision Q1: Rust-first, or Tracktion + JUCE? | **Rust-first.** Riddim is being built as the engine. | Phil's go-ahead of 2026-11-21. |
| D2 | Vision Q7: licence | **AGPL-3.0-only.** Every crate declares it and the repository carries the licence text in `LICENSE`. The commercial licence and CLA do not exist yet. | The vision's recommendation (§6.2). To change: edit `license` in the root `Cargo.toml` and replace `LICENSE`. Nothing has been published, so this is still free to change. |
| D3 | Vision Q4: out-of-process hosting | **In process for v1.** The bridge is planned for v1.x. Scanning is already out of process. | The vision's plan (§4.4). |
| D4 | Vision Q3: is the phase 1 exit bar right? | **Kept as written**: both formats, and an identical blob after save, restore, save. | The vision's default. |

These questions have **no recommended default in the vision, so they are not decided**. What the code assumes in the meantime:

| # | Question | Working assumption | What depends on it |
|---|---|---|---|
| D5 | Vision Q2: Whiplash's UI stack | Unknown. Nothing in the engine assumes Rust on the other side; the C ABI arrives in phase 2. | The shape of the main-thread handle and the C ABI. |
| D6 | Vision Q5: Phil's Mac and interface | Unknown. Code targets Apple Silicon and any CoreAudio device. | Performance targets in phase 4. |
| D7 | Vision Q6: Helix Native or Helix Stadium Native | Not needed yet (see D10). | The phase 3 target list. |
| D8 | Vision Q8: copyright owner | Unknown. No copyright-holder line has been added to source files or to `LICENSE` beyond the licence text itself. | The CLA, the commercial licence, and whether an internal licence to Whiplash is needed. |
| D9 | Vision Q9 and Q10: when the repo goes public; Windows | Repo stays private. No Windows code is written, but nothing platform-specific leaks out of `riddim-io`, the format modules and the editor window. | Nothing yet. |

## Phase 1

**D10. Hosting is tested against free plugins, not the four commercial targets.** Phil has not bought Kontakt/Noire, Superior Drummer 3, EZbass or Helix Native yet (his note of 2026-11-21). Phase 1 is therefore exercised against Riddim's own fixture plugins, Surge XT 1.3.4, and Apple's built-in Audio Units. The four commercial plugins become a later verification target on Phil's Mac; nothing in this repository claims they work.

**D11. Development happens on Linux; macOS code is type-checked from Linux.** The build machine is Linux, so everything platform-neutral is built and tested there. macOS-only code (CoreAudio, CoreMIDI, Audio Units, editor windows, CFBundle loading) compiles behind `cfg(target_os = "macos")`, and CI lints the whole workspace for `aarch64-apple-darwin` on a Linux runner. That proves it type-checks, not that it works: every such module is marked "unverified on macOS" in its own documentation until a Mac has run it.

**D12. The macOS CI workflow runs on request, not on every push.** macOS runner minutes cost ten times Linux minutes on a private repository, and a full run could use a large part of a month's free allowance. `.github/workflows/macos.yml` therefore runs only when the `mac-ci` label is added to a pull request (or by hand from the Actions tab). To make it run on every pull request instead, change its trigger to `pull_request:` with no `types` filter.

**D13. Pull requests are stacked.** No pull request is merged by the implementer, and each phase builds on the one before, so each phase's pull request is based on the previous phase's branch (phase 1 on `docs/vision`). Merge them in order. Once `docs/vision` reaches `main`, GitHub retargets the next one automatically if branches are deleted on merge; otherwise retarget by hand.

**D14. Crates live under `crates/`, test fixtures under `fixtures/`.** The crate set is the vision's (§3.1). Fixtures (test plugins, test support) are separate packages so they never ship.

**D15. `riddim-scan` is a binary of the `riddim-plugin` crate**, not of `riddim-cli` as the vision's tree sketched. The scanner is the other half of the scan protocol defined in `riddim-plugin`, and keeping them together lets that crate's tests run the real child process. The executable name and behaviour are as the vision describes.

**D16. A plugin is two objects: `PluginInstance` (control side) and `PluginProcessor` (audio side).** The vision lists one contract (§4.3). In Rust the part that crosses to the audio thread has to be a separate, `Send` value, so `activate` returns the processor and `deactivate` takes it back. The instance is deliberately not `Send`: it stays on the thread that created it, which must be the main thread when an editor will be opened.

**D17. Our own allocation guard instead of the `assert_no_alloc` crate.** `riddim_rt::guard` counts heap operations inside real-time sections (per thread and in total) rather than aborting, so a test can run a scenario and assert the count is zero, and tools can print it. Same idea as `assert_no_alloc`, about 100 lines, no dependency. It detects allocation and free only; locks and system calls are for the RealtimeSanitizer job planned in phase 2.

**D18. VST3 host choices.**
- Inactive buses are given real memory (shared silence for inputs, a shared scratch buffer for outputs) rather than null pointers. Plugins differ on whether they check activation before touching a bus.
- A main bus in first position is always active; every other bus starts as the plugin's "default active" flag says. (Found with Surge XT, whose first input bus is a sidechain.)
- MIDI controllers, pitch bend, aftertouch and program change reach the plugin as parameter changes through `IMidiMapping`, which is how VST3 defines it. Controllers the plugin has not mapped are dropped.
- Editor resize requests are queued and answered on the next poll rather than inside the plugin's `resizeView` call. The spec allows either; this avoids re-entrancy. To be revisited if a plugin misbehaves.
- Plugin class ids are written as 32 hex digits in Steinberg's platform-independent order, so a project means the same plugin on macOS and Windows.
- Modules are never unloaded once loaded.

**D19. Audio Unit host choices.**
- One host on the AudioToolbox C API serves version 2 units natively and version 3 units through Apple's bridge. A version 3 unit's own view controller is not requested yet; such units get Apple's generic parameter view. None of the plugins on the target list is version 3.
- State is the unit's `ClassInfo` property list serialised as XML, not binary, because the XML writer orders dictionary keys and the phase 1 exit bar compares blobs byte for byte.
- Parameters are normalised linearly between the unit's minimum and maximum.
- Notifications from the unit arrive through an `AUEventListener` on the instance's run loop, and `poll_events` gives that run loop a turn so hosts without an application event loop still receive them.

**D20. CoreAudio: one device, one IOProc.** A single device is opened for output, optionally capturing from the same device (which is the duplex case for an audio interface). Using *different* input and output devices, such as a MacBook's built-in microphone and speakers, needs an aggregate device; that is not built yet (see D37).

**D21. The toolchain is pinned** to Rust 1.100.0 in `rust-toolchain.toml`, so CI and every machine build with the same compiler and lints.

**D22. The phase 1 player is a throwaway.** `riddim play` drives one plugin straight to the device from code in `riddim-cli`. It is the vision's "tool, not an engine yet" (§4.5) and is replaced by the engine in phase 2.

**D23. CI is in place but cannot run; checks are run locally and recorded.** As of 2026-11-21 GitHub refuses to start any Actions job on this repository ("recent account payments have failed or your spending limit needs to be increased"). That is an account matter only Phil can resolve. Until then every push is checked with `scripts/ci.sh`, which runs what the workflow runs, and each phase's notes record the result. When billing is fixed, re-run the failed workflow runs on the open pull requests; the macOS run is started by adding the `mac-ci` label (D12).

## Phase 2

**D24. Time is integer, in two clocks.** Audio time is the *superclock*: 282,240,000 units per second (Ardour's constant), which every common sample rate divides, so sample positions convert exactly. Musical time is *ticks*: 3,628,800 per quarter note (10!), fine enough to address single samples at any tempo of 4 bpm or more up to 192 kHz, and divisible by the usual MIDI resolutions and tuplets. A tempo is stored as the exact superclock length of a beat, not as a bpm float. Tempos are constant between points: **there are no tempo ramps yet.**

**D25. Timeline positions carry their clock.** A clip starts at `Position::Beats(ticks)` or `Position::Time(superclock)`. MIDI clips are beat-anchored and move with tempo changes; recorded audio is time-anchored and does not. This is the vision's "superclock for audio time, beat ticks for musical time" applied to clip placement.

**D26. Model conventions.**
- Ids are chosen by the client, checked for uniqueness by the engine. `Model::fresh_id` suggests one.
- The master track always exists, with id 0, and cannot be removed or re-routed.
- Every strip is stereo internally; mono material is spread to both channels.
- Clips within a track and media within the project are kept sorted by id, so a model's contents do not depend on the order edits were made in (which is what lets "apply, inverse, apply" be compared for exact equality).
- Opaque plugin state is base64 inside the JSON form.
- Replacing the whole model (`Engine::load`, opening a project) is not an op and has no inverse.

**D27. Every output port gets its own buffer.** The graph compiler does not reuse buffers between steps. That costs a few megabytes on a large session and buys two things: no aliasing bugs, and steps without a dependency between them can run in parallel (phase 4) with no further analysis. Sources into a port are summed in a fixed order (by upstream node, then port), which is what makes mixes reproducible bit for bit.

**D28. Node state is shared by reference, not held in a slot map.** The vision describes plugin instances, delay lines and meters living "in a slot map keyed by stable IDs, which the plan only references". The implementation keeps the idea and changes the mechanism: each node's persistent state is a reference-counted cell keyed by what the node *is* (`Source(track)`, `Plugin(track, slot)`, `Strip(track)`, ...). The control thread holds one reference; every plan that includes the node holds another. A recompile builds a new plan that points at the same cells. A plugin is unloaded once the last plan that used it has come back from the audio thread. No audio-thread table to size or grow.

**D29. Media is played from memory, not streamed.** Phase 2 decodes each media file once, converts it to the session rate, and plays clips from RAM. The vision's `riddim-disk` includes read-ahead streaming from disk; that is **not built yet**. It matters for long sessions (an hour of stereo at 48 kHz is about 1.4 GB in memory) and is planned alongside the disk-starvation stress tests.

**D30. Small file-format code of our own.** WAV/RF64 reading and incremental writing, and a windowed-sinc resampler for import, are written here rather than taken from `hound`, `symphonia` or `rubato`. The vision anticipated the WAV writer; the resampler is the same call (about 100 lines, deterministic, used only at import). Standard MIDI Files use `midly`, as the vision suggested. `symphonia` can be added when formats beyond WAV are needed.

**D31. Where a recording lands.**
- The audio thread, not the control thread, fixes the timeline sample a recording starts at, because only it knows the position exactly. If the transport is already rolling, the start is two blocks ahead of the command.
- Capture begins one round trip (device input latency + device output latency + the plan's plugin latency) after that start, so the first captured frame is the one that was played at the start, and the take's clip begins exactly there.
- MIDI is corrected by the output-side latency only (a MIDI input has no meaningful latency of its own).
- Audio takes are written incrementally and survive a crash. **MIDI takes are not**: they are held in memory until the transport stops, then added to the model and written out as a `.mid` file.
- Device latency is taken as the device reports it. `riddim loopback` measures the truth with a cable.

**D32. Looping is suspended while recording.** Loop recording with takes is phase 4.

**D33. Live MIDI is applied at the start of the next block.** Message timestamps are captured but not yet used to place events inside the block. This meets phase 1's "within one buffer" and is the same for recording (so recorded MIDI is quantised to the block it arrived in, up to one buffer early or late of where a timestamp would put it).

**D34. The C ABI carries structure as JSON.** Configuration, ops, the model and events cross as JSON strings; transport, parameters and status are plain values. The header is generated by cbindgen, checked in, and a test fails if it is stale. Events are polled, not called back, so the client chooses the thread. The ABI is version 0 and will change.

**D35. RealtimeSanitizer through `rtsan-standalone`.** With the `rtsan` feature and `RTSAN_ENABLE=1`, every real-time section is an RTSan context; `scripts/rtsan.sh` runs the engine, host, I/O and disk tests that way. A self-check re-runs itself with deliberate violations and fails the run if the sanitizer does not stop them, so a green run cannot be a silent no-op. Calls into plugin code are excluded. Known limit: the pre-built runtime (LLVM 20.1.1) does not intercept `clock_nanosleep`, so a `thread::sleep` in a real-time section would go unnoticed.

**D36. Mixer conventions.** Pan is a balance control with unity gain at centre (a default strip is bit-transparent). Solo is solo-in-place: soloing a track keeps the buses it plays through open; soloing a bus keeps the tracks that feed it. Gain changes ramp over 5 ms. The metronome goes straight to the device output, not through the master strip.

**D37. No aggregate devices yet.** Recording on a Mac needs the input and output on one CoreAudio device (true of any audio interface). Built-in microphone plus built-in speakers will not work until the engine can build a private aggregate device; planned for phase 4 with the rest of the device work.

**D38. Bit-identical checks use deterministic plugins.** Surge XT's default patch renders differently from run to run (free-running oscillator phase), which is the plugin's own behaviour and what the vision allows for. "Save, reopen, hear it identically" is therefore asserted bit for bit with the fixture plugins and audio clips, and by ear for real instruments.

**D39. Plugins always run in real-time mode for now.** `ActivateConfig::mode` is `Realtime` even for `riddim render`. The offline mode arrives with bounce in phase 3.

**D40. A bypassed plugin is passed by without its delay.** Bypass copies the input to the output with no latency, while the graph still compensates for the plugin's reported latency. A bypassed plugin with latency is therefore misaligned by that amount until this is fixed in phase 3.

**D41. The CLI's "session" is a stand-in for a project.** `riddim session` saves the engine model as JSON beside a media folder so the whole path (build, record, save, reopen, render) can be run from a terminal before Whiplash exists. It is not a project format.
