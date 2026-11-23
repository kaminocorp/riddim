# Embedding the engine in an application

Whiplash is to be a Tauri 2 application (decision D83): a Rust core that links Riddim, a TypeScript web UI, WebGPU for waveforms and meters. This page is how the engine is meant to sit in such a host.

**What is verified and what is not.** The shape described here is exercised by `crates/riddim/tests/embedding.rs`, built from plain threads and channels with no Tauri in it. **No Tauri application has been built or run against the engine**, and nothing here has run on macOS. The Tauri specifics below (which hook the pump goes in, how the web view is fed) are the design, to be confirmed when Whiplash's shell exists.

## The parts, and which thread has which

| Part | Lives on | Why |
|---|---|---|
| `Engine` | Shared state: `tauri::State<Mutex<Engine>>` | It is `Send`. Its methods are quick and never wait on audio, so a plain lock is enough |
| `MainThread` (from `Engine::take_main_thread`) | **The application's main thread**, for good | It holds the plugin instances. On macOS, plugins, their editors and Audio Units must be created and called on the process's main thread. It is not `Send`, on purpose |
| The event stream (`Engine::subscribe`) | A thread of its own that forwards to the web view | So that a slow UI never backs up into the engine |
| The audio thread, render threads, disk threads | The engine's own | Not the application's concern |

Use the `riddim` crate directly from the Rust core. The C ABI (`riddim-ffi`) is for clients that are not Rust; a Tauri core does not need it.

## Starting up

```rust,ignore
// In the Tauri setup hook, which runs on the main thread.
let mut config = EngineConfig::manual(project_dir);   // then set the real device:
config.device = DeviceConfig::System(/* the chosen device, rate and buffer size */);
config.main_thread = MainThreadMode::External;        // the application pumps the plugin side
let mut engine = Engine::start(config)?;
let main_thread = engine.take_main_thread().unwrap(); // made here, stays here
let events = engine.subscribe();
app.manage(Mutex::new(engine));
```

`take_main_thread` must be called on the thread that will pump it. Keep the `MainThread` in a main-thread-only place (a `thread_local!`, or a field of whatever the event loop owns), never in `State`.

## Pumping the plugin side

`MainThread::pump()` does what plugins need of the main thread: it loads and unloads them, hands their processors to the engine, and collects what they report. Call it a few dozen times a second from the main thread's own loop: in Tauri, from the run loop's idle or "about to wait" event, or from a repeating timer that calls back on the main thread.

It loads **at most one plugin per call**, and loading a big sampler can take seconds, so the UI thread is held for one plugin at a time, never for a whole project (decision D50). Audio does not stop while it loads.

`pump()` returns `false` once the engine has shut down. On quit: drop the `Engine`, keep pumping until it says so, then drop the `MainThread`, so that every plugin is destroyed on the thread it was made on.

## Commands: JSON in, JSON out

Everything a web UI sends and is sent has a JSON form, so a Tauri command is a few lines:

| From the UI | Rust | Back to the UI |
|---|---|---|
| A list of ops: `[{"op":"add_track",...}]` | `engine.apply_wait(ops)` | `Applied`: `{"revision":7,"inverses":[...]}`, or `Rejected`: `{"index":0,"error":"..."}` |
| `{"command":"play"}`, `{"command":"stop"}`, `{"command":"record"}`, `{"command":"locate","position":{"beats":0}}` | `engine.transport(command)` | nothing; `transport` and `position` events follow |
| a slot, a parameter id, a value from 0 to 1 | `engine.set_param(...)` | nothing; `plugin_param` events come from the plugin's side |
| | `engine.model()` | The whole model, as the project format |
| | `engine.status()` | `Status`: transport, position, rates, latencies, load, counters |

`apply_wait` returns when the edit has been validated and applied to the model, which is at once; anything slow it sets off (a plugin loading) is reported by events. `inverses` is the undo for that edit: the UI's undo stack is a list of them (vision §1.1). Use `apply` and match the `applied` or `rejected` event by its ticket if a command must not wait even that long.

The ops and the model are the project format and are covered by the stability policy ([`stability.md`](stability.md)). The JSON of commands, results and status is new with this decision and not yet pinned by golden files.

## Events: one forwarding thread

```rust,ignore
std::thread::spawn(move || {
    for event in events {                       // ends when the engine is dropped
        let _ = app_handle.emit("riddim", &event);   // or a tauri::ipc::Channel
    }
});
```

Every event is `{"event":"<name>", ...}`; the list is in [`events.md`](events.md). The UI must ignore names and fields it does not know.

**Meters and the playhead** arrive as `meters` and `position` events about thirty times a second. That is enough to drive a meter that is smoothed and peak-held in the UI. If the web view wants them at display rate, or as a binary buffer for a WebGPU pass, the core can keep the latest values and answer a per-frame request with raw bytes; the engine does not need to change for that. Sample-accurate metering (true peak, loudness) is not in the engine yet.

**Waveforms** are the application's (vision §1.1: the media pool and drawing belong to Whiplash). The core reads the media files and computes the overviews; the engine is not involved, and recorded files are ordinary WAVs: `recording_finished` names the media entry, and the model has its path.

## Plugin editors

A plugin's editor is a **native view**, not web content, and cannot be put inside the web view. `MainThread::instance_mut(slot)` gives the plugin; `open_editor(ParentWindow::AppKit(ns_view))` puts its editor in a view the application supplies. The application makes a native window (or panel) per editor, on the main thread, beside its web view window, and owns where it goes and when it closes. `plugin_editor_resize` says when the plugin wants a different size. The `riddim` command-line tool does exactly this, and its window code (`riddim-plugin`'s editor window) can be used as it is or as a model.

Bridged plugins (in a process of their own) have no editor yet.

## Things to know

- **Do not hold the engine lock across a wait.** Every `Engine` method returns promptly except `sync`, `apply_wait` (brief) and `capture_plugin_states` (as long as the plugins take, which need the main thread to be pumping: never call it *from* the main thread while holding up the pump).
- **`set_buffer_size` and the manual-device methods take `&mut self`**; behind a `Mutex` that is not a problem.
- **Licence.** Riddim is AGPL-3.0-only. Whiplash has the same owner (decision D79), so it can link the engine on whatever terms it ships under. Anyone else's application linking the engine in process is covered by the AGPL, or by a commercial licence when there is one.
- **Windows.** The engine's core builds for Windows and has no audio device backend there yet (decision D82). A Tauri shell is portable; the engine under it is, for now, macOS (unverified) and Linux (deviceless).
