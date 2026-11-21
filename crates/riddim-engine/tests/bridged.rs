//! Plugins hosted in processes of their own, in the engine: they sound the same, and one that
//! crashes takes down nothing but itself.

#![cfg(unix)]

mod common;

use common::*;
use riddim_engine::{DeviceConfig, Engine, EngineConfig, Event, NullConfig, TransportCommand};
use riddim_model::time::Position;
use riddim_model::{Op, PluginSlot, SlotId, TrackId, TrackKind};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// An engine configuration that knows where the test build's bridge executable is.
fn config(dir: &std::path::Path) -> EngineConfig {
    let mut config = EngineConfig::manual(dir);
    config.bridge_executable = Some(riddim_test_support::bridge_exe());
    config
}

fn slot(id: u64, uid: &str, bridged: bool) -> PluginSlot {
    PluginSlot {
        bridged,
        ..fixture_slot(id, uid)
    }
}

/// A synth on track 10 playing two notes, and noise on track 20 through the fixture delay
/// (64 samples of latency). Either plugin, or both, bridged.
fn project(dir: &std::path::Path, bridge_synth: bool, bridge_delay: bool) -> Vec<Op> {
    vec![
        media(dir, 1, RATE, vec![seeded_noise(5, 60_000)]),
        add_track(0, 10, TrackKind::Instrument),
        add_track(1, 20, TrackKind::Audio),
        Op::InsertPlugin {
            track: TrackId(10),
            index: 0,
            slot: slot(11, ids::SYNTH_UID, bridge_synth),
        },
        Op::AddClip {
            track: TrackId(10),
            clip: midi_clip(
                12,
                0,
                2,
                vec![note((0, 1), (1, 1), 60), note((1, 1), (1, 2), 67)],
            ),
        },
        Op::AddClip {
            track: TrackId(20),
            clip: audio_clip(21, 1, 0, 60_000),
        },
        Op::InsertPlugin {
            track: TrackId(20),
            index: 0,
            slot: slot(22, ids::DELAY_UID, bridge_delay),
        },
    ]
}

fn loaded(events: &std::sync::mpsc::Receiver<Event>, count: usize) {
    for _ in 0..count {
        wait_for(events, "a plugin to load", |e| match e {
            Event::PluginLoaded { .. } => Some(()),
            Event::PluginFailed { error, .. } => panic!("a plugin failed to load: {error}"),
            _ => None,
        });
    }
}

fn play(engine: &mut Engine, frames: usize) -> Vec<Vec<f32>> {
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(2048);
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(frames)
}

#[test]
fn a_bridged_project_plays_what_the_same_project_plays_in_process() {
    let dir = temp_dir("bridged-same");
    let render = |bridge_synth: bool, bridge_delay: bool| {
        let mut engine = Engine::start(config(&dir)).unwrap();
        let events = engine.subscribe();
        engine
            .apply_wait(project(&dir, bridge_synth, bridge_delay))
            .unwrap();
        loaded(&events, 2);
        engine.sync();
        // A knob turned, saved with the project, and still there afterwards.
        engine.set_param(SlotId(11), ids::SYNTH_GAIN, 0.75);
        engine.sync();
        engine.run(256);
        let out = play(&mut engine, 50_000);
        assert_eq!(engine.status().plan_latency, 64);
        assert_eq!(engine.status().rt_violations, 0);
        engine.transport(TransportCommand::Stop);
        engine.capture_plugin_states().unwrap();
        (out, engine.model().to_json())
    };
    let (in_process, saved_in_process) = render(false, false);
    assert!(peak(&in_process[0]) > 0.2);
    let (bridged, saved_bridged) = render(true, true);
    assert!(bridged == in_process, "bridged plugins change the audio");
    let (mixed, _) = render(true, false);
    assert!(mixed == in_process);
    // The saved state is the plugin's own, wherever it ran (only the "bridged" flags differ).
    assert_eq!(
        saved_bridged.replace("\"bridged\":true", "\"bridged\":false"),
        saved_in_process
    );
}

#[test]
fn a_crashing_plugin_does_not_take_the_session_down() {
    // The exit criterion of the bridge. A running device this time, so that the audio
    // callback is a real thread that could be left waiting.
    let dir = temp_dir("bridged-crash");
    let mut config = config(&dir);
    config.device = DeviceConfig::Null(NullConfig {
        buffer_frames: 128,
        ..NullConfig::default()
    });
    let engine = Engine::start(config).unwrap();
    let events = engine.subscribe();
    engine.apply_wait(project(&dir, true, false)).unwrap();
    loaded(&events, 2);
    engine.sync();
    engine.transport(TransportCommand::Play);
    engine.sync();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let before = engine.status();

    // The synth's process aborts.
    engine.set_param(SlotId(11), ids::COMMAND, ids::COMMAND_ABORT);
    let crashed = wait_for(&events, "the crash to be noticed", |e| match e {
        Event::PluginCrashed { track, slot } => Some((*track, *slot)),
        _ => None,
    });
    assert_eq!(crashed, (TrackId(10), SlotId(11)));

    // The session is still here: the transport runs, the other track's meter moves, edits
    // are applied, and the audio path stayed clean.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after = engine.status();
    assert!(after.position_samples > before.position_samples + 4_800);
    assert!(
        after.blocks > before.blocks + 50,
        "the audio callback stopped being served"
    );
    assert_eq!(after.rt_violations, 0);
    engine
        .apply_wait(vec![Op::SetTrack {
            id: TrackId(20),
            change: riddim_model::TrackChange::Gain(0.5),
        }])
        .unwrap();
    let heard = wait_for(&events, "the other track to be heard", |e| match e {
        Event::Meters { tracks } => tracks
            .iter()
            .find(|(track, _)| *track == TrackId(20))
            .filter(|(_, peaks)| peaks[0] > 0.0)
            .map(|(_, peaks)| peaks[0]),
        _ => None,
    });
    assert!(heard > 0.0);

    // The slot is still in the project, state and all. Taking it out and putting it back
    // starts a fresh process, which plays.
    let removed = engine
        .apply_wait(vec![Op::RemovePlugin {
            track: TrackId(10),
            slot: SlotId(11),
        }])
        .unwrap();
    engine.apply_wait(removed.inverses).unwrap();
    loaded(&events, 1);
    engine.transport(TransportCommand::Locate(Position::START));
    wait_for(&events, "the reloaded synth to be heard", |e| match e {
        Event::Meters { tracks } => tracks
            .iter()
            .find(|(track, _)| *track == TrackId(10))
            .filter(|(_, peaks)| peaks[0] > 0.0)
            .map(|_| ()),
        _ => None,
    });
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn a_bridge_that_is_not_there_fails_the_plugin_not_the_engine() {
    let dir = temp_dir("bridged-missing");
    let mut config = EngineConfig::manual(&dir);
    config.bridge_executable = Some("/nonexistent/riddim-bridge".into());
    let mut engine = Engine::start(config).unwrap();
    let events = engine.subscribe();
    engine.apply_wait(project(&dir, true, false)).unwrap();
    let error = wait_for(&events, "the failure", |e| match e {
        Event::PluginFailed { slot, error, .. } if *slot == SlotId(11) => Some(error.clone()),
        _ => None,
    });
    assert!(error.contains("riddim-bridge"), "{error}");
    engine.sync();
    // The rest of the project plays.
    assert!(peak(&play(&mut engine, 10_000)[0]) > 0.05);
}
