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

**D20. CoreAudio: one device, one IOProc.** Phase 1 opens a single device for output (optionally capturing from the same device). Using different input and output devices needs an aggregate device, which is left to phase 2 along with duplex I/O.

**D21. The toolchain is pinned** to Rust 1.100.0 in `rust-toolchain.toml`, so CI and every machine build with the same compiler and lints.

**D22. The phase 1 player is a throwaway.** `riddim play` drives one plugin straight to the device from code in `riddim-cli`. It is the vision's "tool, not an engine yet" (§4.5) and is replaced by the engine in phase 2.

**D23. CI is in place but cannot run; checks are run locally and recorded.** As of 2026-11-21 GitHub refuses to start any Actions job on this repository ("recent account payments have failed or your spending limit needs to be increased"). That is an account matter only Phil can resolve. Until then every push is checked with `scripts/ci.sh`, which runs what the workflow runs, and each phase's notes record the result. When billing is fixed, re-run the failed workflow runs on the open pull requests; the macOS run is started by adding the `mac-ci` label (D12).
