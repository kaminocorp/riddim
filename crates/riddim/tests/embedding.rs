//! The engine embedded the way an in-process UI host embeds it. Whiplash is to be a Tauri 2
//! application (decision D83): a Rust core that links this crate, and a web UI that talks to
//! it in JSON. Nothing here depends on Tauri; it is the same shape built from threads and
//! channels, so that the shape keeps working:
//!
//! * the engine sits in shared state (`Mutex<Engine>`), reached from whichever thread a
//!   command arrives on;
//! * commands come in as JSON and their results go back as JSON;
//! * events are forwarded, as JSON, by a thread that does nothing else;
//! * plugins live on the application's main thread, which pumps the engine's `MainThread`
//!   from its own event loop.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use riddim::model::{
    Clip, ClipContent, ClipId, Model, Note, Op, PluginRef, PluginSlot, SlotId, Track, TrackId,
    TrackKind,
};
use riddim::time::{Position, Ticks};
use riddim::{Applied, Engine, EngineConfig, Event, MainThreadMode, Status, TransportCommand};
use riddim_test_support::ids;

fn sendable<T: Send>() {}
fn shareable<T: Send + Sync>() {}
fn to_json<T: serde::Serialize>() {}
fn from_json<T: serde::de::DeserializeOwned>() {}

#[test]
fn the_engine_and_what_it_exchanges_fit_an_applications_shared_state() {
    // The engine handle moves between threads and sits behind a lock...
    sendable::<Engine>();
    shareable::<Mutex<Engine>>();
    // ...its event stream can be handed to a forwarding thread...
    sendable::<mpsc::Receiver<Event>>();
    // ...and a model snapshot can be read from anywhere.
    shareable::<Arc<Model>>();
    // Everything a web UI sends has a JSON form, and so has everything it is sent.
    from_json::<Vec<Op>>();
    from_json::<TransportCommand>();
    from_json::<Model>();
    to_json::<Event>();
    to_json::<Applied>();
    to_json::<riddim::Rejected>();
    to_json::<Status>();
    to_json::<Model>();
}

#[test]
fn transport_commands_have_a_stable_json_form() {
    let text = |command: TransportCommand| serde_json::to_string(&command).unwrap();
    assert_eq!(text(TransportCommand::Play), r#"{"command":"play"}"#);
    assert_eq!(text(TransportCommand::Stop), r#"{"command":"stop"}"#);
    assert_eq!(text(TransportCommand::Record), r#"{"command":"record"}"#);
    let locate = TransportCommand::Locate(Position::Beats(Ticks::from_beats(2)));
    let json = text(locate);
    assert!(
        json.starts_with(r#"{"command":"locate","position":{"#),
        "{json}"
    );
    assert_eq!(
        serde_json::from_str::<TransportCommand>(&json).unwrap(),
        locate
    );
    assert_eq!(
        serde_json::from_str::<TransportCommand>(r#"{"command":"play"}"#).unwrap(),
        TransportCommand::Play
    );
}

/// What the UI would send to set up one instrument track with a clip on it.
fn ops_as_the_ui_sends_them() -> String {
    let slot = PluginSlot {
        id: SlotId(2),
        plugin: PluginRef {
            format: "vst3".into(),
            uid: ids::SYNTH_UID.into(),
            name: String::new(),
            path: Some(
                riddim_test_support::testplug_bundle()
                    .to_string_lossy()
                    .into_owned(),
            ),
        },
        state: None,
        bypass: false,
        outputs: Vec::new(),
        sidechain: None,
        bridged: false,
    };
    let clip = Clip {
        id: ClipId(3),
        name: "Notes".into(),
        start: Position::Beats(Ticks::ZERO),
        muted: false,
        take: None,
        content: ClipContent::Midi {
            length: Ticks::from_beats(4),
            notes: Arc::new(vec![Note {
                start: Ticks::ZERO,
                length: Ticks::from_beats(2),
                key: 69,
                velocity: 100,
                channel: 0,
            }]),
            events: Arc::new(Vec::new()),
        },
    };
    serde_json::to_string(&vec![
        Op::AddTrack {
            index: 0,
            track: Track::new(TrackId(1), "Synth", TrackKind::Instrument),
        },
        Op::InsertPlugin {
            track: TrackId(1),
            index: 0,
            slot,
        },
        Op::AddClip {
            track: TrackId(1),
            clip,
        },
    ])
    .unwrap()
}

#[test]
fn an_in_process_ui_host_drives_the_engine_in_json_from_several_threads() {
    let dir = std::env::temp_dir().join(format!("riddim-embedding-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // The application's main thread (this one) starts the engine and keeps the half of it
    // that holds the plugins; the rest goes into shared state.
    let mut config = EngineConfig::manual(&dir);
    config.main_thread = MainThreadMode::External;
    let mut engine = Engine::start(config).unwrap();
    let mut main_thread = engine
        .take_main_thread()
        .expect("an external main thread was asked for");
    let events = engine.subscribe();
    let engine = Arc::new(Mutex::new(engine));

    // A thread that forwards every event to the UI as JSON (in Tauri: a channel or `emit`).
    let (to_ui, ui_inbox) = mpsc::channel::<String>();
    let forwarder = std::thread::spawn(move || {
        for event in events {
            if to_ui.send(serde_json::to_string(&event).unwrap()).is_err() {
                break;
            }
        }
    });

    // A command handler on a thread of its own, as an IPC call arrives: JSON in, JSON out.
    let handler = {
        let engine = engine.clone();
        let ops = ops_as_the_ui_sends_them();
        std::thread::spawn(move || {
            let ops: Vec<Op> = serde_json::from_str(&ops).unwrap();
            let engine = engine.lock().unwrap();
            let applied = engine.apply_wait(ops).unwrap();
            // A second batch that must be refused: the track is already there.
            let refused = engine
                .apply_wait(vec![Op::AddTrack {
                    index: 0,
                    track: Track::new(TrackId(1), "Again", TrackKind::Audio),
                }])
                .unwrap_err();
            (
                serde_json::to_value(&applied).unwrap(),
                serde_json::to_value(&refused).unwrap(),
            )
        })
    };
    let (applied, refused) = handler.join().unwrap();
    assert_eq!(applied["revision"], 1);
    assert_eq!(applied["inverses"].as_array().unwrap().len(), 3);
    assert_eq!(refused["index"], 0);
    assert!(refused["error"].is_string(), "{refused}");

    // The main thread's event loop: pump the plugin side until the UI has been told the
    // plugin is in. The plugin is created here, on the thread that will keep it.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while !seen
        .iter()
        .any(|e: &String| e.contains(r#""event":"plugin_loaded""#))
    {
        assert!(
            Instant::now() < deadline,
            "the plugin never loaded: {seen:?}"
        );
        main_thread.pump();
        seen.extend(ui_inbox.try_iter());
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(main_thread.loaded_slots(), vec![SlotId(2)]);
    assert!(
        seen.iter().any(|e| e.contains(r#""event":"applied""#)),
        "{seen:?}"
    );

    // Transport from JSON, on yet another thread; then audio, and a status the UI can show.
    {
        let engine = engine.clone();
        std::thread::spawn(move || {
            let play: TransportCommand = serde_json::from_str(r#"{"command":"play"}"#).unwrap();
            let engine = engine.lock().unwrap();
            engine.transport(play);
            engine.sync();
        })
        .join()
        .unwrap();
    }
    let audio = engine.lock().unwrap().run(4_800);
    assert!(
        audio[0].iter().any(|&sample| sample != 0.0),
        "the synth should be sounding"
    );
    let status = serde_json::to_value(engine.lock().unwrap().status()).unwrap();
    assert_eq!(status["transport"], "playing");
    assert_eq!(status["rt_violations"], 0);
    assert!(status["position_samples"].as_i64().unwrap() > 0);

    // Shutting down: the engine goes first, then the main thread's half lets go of the
    // plugins, on its own thread.
    let engine = Arc::try_unwrap(engine)
        .unwrap_or_else(|_| panic!("the engine is still shared"))
        .into_inner()
        .unwrap();
    drop(engine);
    while main_thread.pump() {
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(main_thread);
    forwarder.join().unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
