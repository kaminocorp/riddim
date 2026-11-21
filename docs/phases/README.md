# Where the phases stand

One page across the five phase notes in this folder: every exit criterion of the vision's phasing (§5), whether it is met, and what the unmet ones are waiting for. The detail, the evidence and the Mac checklists are in the notes themselves.

**In one line (2026-11-21):** every phase's code is written and passes on Linux; 9 of the 22 criteria are met there, 5 are met only in a weaker form (in simulation, fast-forwarded, or with stand-in plugins), and 8 are not met. None of the 13 that fall short can be finished from the Linux build machine: they need Phil's Mac, the four commercial plugins, Whiplash, or four weeks of calendar time. Nothing has run on macOS at all.

## Every criterion

| Phase | # | Criterion (short) | Status | Waiting for |
|---|---|---|---|---|
| P1 | 1 | Target instrument loads as VST3 and AU; editor opens, resizes, closes 10 times | Not met | Mac (editors and Audio Units exist only there) |
| P1 | 2 | A note sounds within one buffer; no glitches over 10 minutes at 128 frames | Not met | Mac (a real audio device) |
| P1 | 3 | State survives save, quit, relaunch; save, restore, save is identical | Met on Linux for VST3 and CLAP | Mac, for AU |
| P1 | 4 | A crashing plugin kills only the scanner | Met on Linux | Mac, to confirm |
| P1 | 5 | A write-up of per-format surprises and thread requirements | Partly (Linux findings written) | Mac, for the questions that need one |
| P2 | 1 | Whiplash v0: record a take over the instrument, save, reopen, hear it identically | Not met (the same flow runs from the CLI on Linux) | Whiplash, Mac |
| P2 | 2 | Recorded audio within ±1 sample of a loopback-calibrated position | Met in simulation, exactly | Mac and an interface, for real hardware |
| P2 | 3 | Zero allocations on the audio thread under RealtimeSanitizer | Met (run locally, not in CI: D12) | |
| P2 | 4 | Ops round-trip: apply, inverse, apply restores the model | Met | |
| P2 | 5 | 60 minutes on 8 tracks with no dropped buffers; `kill -9` keeps what was flushed | Met fast-forwarded | Mac, for a real-time hour on real disks |
| P3 | 1 | Whiplash v0.5: a demo song with all four plugins, bounced | Not met (stand-in: Surge XT demo song) | Whiplash, the four plugins, Mac |
| P3 | 2 | PDC null test to −120 dBFS through sends and buses | Met (exactly zero) | |
| P3 | 3 | Two offline bounces are bit-identical | Met | |
| P3 | 4 | SD3 and Kontakt load without stalling audio or UI | Met with a stand-in (a slow fixture plugin) | The plugins, Mac |
| P3 | 5 | Every target plugin's state survives save and restore | Met for fixtures and Surge XT | The four plugins |
| P4 | 1 | Phil makes a track from recording to bounce in Whiplash | Not met | Whiplash, Mac |
| P4 | 2 | Reference session (4 targets, 20 effects) at 64 frames, no xruns for an hour, on Phil's Mac | Not met (stand-ins run on Linux: see the P4 notes) | Mac, the four plugins |
| P4 | 3 | Parallel and single-threaded bounces are bit-identical | Met | |
| P4 | 4 | RT-safety runs (rtsan and stress) green in CI for 4 weeks | Not met as written (runs locally before every push: D67) | Four weeks; a decision on CI minutes |
| v1.x | 1 | A CLAP synth passes the P1 checks | Met on Linux (Surge XT's CLAP builds, and the CLAP fixtures in every test run) | Mac, for the by-hand checks and the editor |
| v1.x | 2 | Kontakt bridged at 128 frames with no xruns | Not met | Kontakt, Mac |
| v1.x | 3 | A crashing bridged plugin doesn't take the session down | Met on Linux | Mac, to confirm |

## What is waiting for what

- **Phil's Mac, with Surge XT only** (free): everything marked "Mac" above that does not name a plugin. Each phase note ends with a checklist; they are cumulative, so running the v1.x branch's `scripts/ci.sh` and then the checklists in order covers all of it. The code that has never run anywhere is listed at the top of each note: Core Audio, Audio Units, plugin editor windows, MIDI input, audio workgroups, aggregate devices, and the bridge and CLAP host on macOS.
- **The four commercial plugins** (Kontakt with Noire, Superior Drummer 3, EZbass, Helix Native), not bought yet: P3 criteria 1, 4 and 5, P4 criterion 2, v1.x criterion 2. Nothing in the engine has been tried with any of them, and nothing here claims they work.
- **Whiplash**: P2 criterion 1, P3 criterion 1, P4 criterion 1 are Whiplash's own exit criteria. The engine's side of each runs from the `riddim` command line.
- **Calendar time**: P4 criterion 4.
- **GitHub Actions**: no job has ever started for this repository (the account's billing; decision D23), so the check on every pull request is red for that reason. `scripts/ci-clean.sh` runs the same job in a clean container and passes for every phase branch (D73). Making the check itself green needs the billing sorted, and then a re-run.
