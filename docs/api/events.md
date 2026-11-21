# Events

Everything the engine tells its client. In Rust these are the variants of `riddim::Event`, received from `Engine::subscribe()`. Across the C ABI each is a JSON object returned by `riddim_poll_event`, tagged with `"event"`; the names and fields below are those JSON names. Positions are in samples at the session's sample rate unless a field says otherwise.

One test (`crates/riddim-engine/tests/events.rs`) produces every event that can be staged without a failing disk or device and checks its tag; the three it cannot stage (`xrun`, `recording_overflow`, `plugin_editor_resize`) are checked for their JSON shape, and the first two are produced by other tests.

Events arrive in the order the engine sent them. The periodic ones are not sent while nobody is subscribed.

## Edits and the model

| Event | Fields | When |
|---|---|---|
| `applied` | `ticket`, `revision`, `inverses` | A batch of ops submitted with `apply` was applied. `inverses`, applied first to last, undo it |
| `rejected` | `ticket`, `index`, `error` | A batch was refused; the model is unchanged. `index` is the op that failed |
| `model_changed` | `revision`, `ops`, `inverses` | The engine edited the model itself: a finished recording, a pass of written automation. Treat it as an edit of your own; `inverses` belong on the undo stack |
| `media_missing` | `media`, `error` | A media file could not be read. Sent once per file; its clips are silent |

## Transport and metering

| Event | Fields | When |
|---|---|---|
| `transport` | `state` (`stopped`, `playing`, `recording`), `position_samples` | The transport state changed |
| `position` | `samples` | About thirty times a second while rolling |
| `jumped` | `from_samples`, `to_samples` | While rolling, the playhead left one place and carried on at another: a loop wrapped, or the transport was located |
| `meters` | `tracks`: list of `[track, [left, right]]` | About thirty times a second: each strip's peak since the last report, master included (track 0) |
| `load` | `dsp` | About once a second: the largest share of a block's duration that rendering one took in that second. 1.0 is the whole of it |
| `xrun` | `total` | The device missed a deadline. `total` counts them since the engine started |
| `playback_starved` | `total` | A file played from disk was not read in time and a stretch played as silence. `total` counts frames since the engine started |
| `stream_changed` | `sample_rate`, `buffer_frames`, `input_latency_frames`, `output_latency_frames` | The device runs with new settings, after a buffer-size change |

## Plugins

| Event | Fields | When |
|---|---|---|
| `plugin_loaded` | `track`, `slot`, `latency`, `parameters` | A plugin finished loading and is in the mix |
| `plugin_failed` | `track`, `slot`, `error` | A plugin could not be loaded (or restarted). Its slot stays in the model, state and all; the track plays without it |
| `plugin_restarted` | `track`, `slot`, `latency` | A loaded plugin was taken out of the mix for a moment and activated again: its outputs or sidechain were routed, the buffer size changed, or it asked for it. Its settings are intact |
| `plugin_param` | `slot`, `id`, `value` | A parameter changed from the plugin's editor or by the plugin itself (normalised 0 to 1) |
| `plugin_gesture` | `slot`, `id`, `down` | A control in the plugin's editor was grabbed or let go. (This is also what starts and ends touch automation for it) |
| `plugin_params_changed` | `slot`, `info` | Many values changed at once (a preset); with `info` true, the parameter list itself changed. Re-read whatever is cached |
| `plugin_state_dirty` | `slot` | The plugin's state changed in a way its parameters do not show: the project has unsaved changes |
| `plugin_latency_changed` | `slot`, `latency` | The plugin's latency changed; compensation has been recomputed |
| `plugin_editor_resize` | `slot`, `width`, `height` | The plugin's editor wants to be this size, in points |

## Recording

| Event | Fields | When |
|---|---|---|
| `recording_started` | `tracks`, `position_samples` | Recording will start at this position (after the count-in, or at the punch-in) |
| `recording_finished` | `track`, `clip`, `media`, `frames`, `take` | A take was written and placed on its track (the `model_changed` that added it comes first). `media` is null for MIDI, where `frames` is the number of notes. Loop recording sends one per time round, numbered from 1 in `take` |
| `recording_overflow` | `track`, `frames_dropped` | The disk could not keep up and this much of a take was lost |

## Anything else

| Event | Fields | When |
|---|---|---|
| `error` | `message` | Something the client asked for could not be done and there is no more specific event: recording with no track armed, a locate while recording, a punch with the playhead past the range, a file that could not be written |

## Status

Not everything is an event. `Engine::status()` (`riddim_status`) is a snapshot that can be read at any time: transport state and position, sample rate and buffer size, model revision, blocks processed, xruns, latencies, DSP load, the recording backlog and frames dropped, frames of streamed playback missed, messages lost between the audio thread and the engine (which should stay at zero), and, in instrumented builds, heap operations seen on the audio path.

## Stability

The C ABI is version 0 (`riddim_abi_version`) and may still change. What is meant to hold from here on: event names and the meaning of existing fields do not change; events and fields may be added, so a client should ignore what it does not know.
