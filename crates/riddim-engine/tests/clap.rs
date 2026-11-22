//! CLAP plugins in the engine, with Riddim's own CLAP fixtures (`fixtures/clapplug`): to the
//! engine a CLAP plugin is a plugin like any other. This is what loads one in every ordinary
//! test run; the real ones are in `third_party.rs`, behind `RIDDIM_TEST_CLAP`.

mod common;

use common::*;
use riddim_disk::wav::{self, SampleFormat};
use riddim_engine::{bounce, BounceRequest, Engine, EngineConfig, Event, TransportCommand};
use riddim_model::time::Position;
use riddim_model::{Model, Op, PluginRef, PluginSlot, SlotId, TrackChange, TrackId, TrackKind};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

const LATENCY: u32 = ids::CLAP_DELAY_LATENCY;

fn clap_slot(id: u64, plugin: &str, bridged: bool) -> PluginSlot {
    PluginSlot {
        id: SlotId(id),
        plugin: PluginRef {
            format: "clap".into(),
            uid: plugin.into(),
            name: String::new(),
            path: Some(
                riddim_test_support::clapplug_bundle()
                    .to_string_lossy()
                    .into_owned(),
            ),
        },
        state: None,
        bypass: false,
        outputs: Vec::new(),
        sidechain: None,
        bridged,
    }
}

/// Two tracks playing the same two notes through the CLAP synth. The second also goes through
/// the CLAP delay, which is late by its latency and says so; `invert` flips that track's
/// polarity, so that the two cancel if, and only if, the engine lines them up.
fn project(invert: bool, bridged: bool) -> Vec<Op> {
    let mut ops = Vec::new();
    for (index, track) in [10u64, 20].into_iter().enumerate() {
        ops.push(add_track(index, track, TrackKind::Instrument));
        ops.push(Op::InsertPlugin {
            track: TrackId(track),
            index: 0,
            slot: clap_slot(track + 1, ids::CLAP_SYNTH_ID, bridged),
        });
        ops.push(Op::AddClip {
            track: TrackId(track),
            clip: midi_clip(
                track + 2,
                0,
                2,
                vec![note((0, 1), (1, 1), 60), note((1, 1), (1, 2), 67)],
            ),
        });
    }
    ops.push(Op::InsertPlugin {
        track: TrackId(20),
        index: 1,
        slot: clap_slot(23, ids::CLAP_DELAY_ID, bridged),
    });
    if invert {
        ops.push(Op::SetTrack {
            id: TrackId(20),
            change: TrackChange::Invert(true),
        });
    }
    ops
}

fn config(dir: &std::path::Path) -> EngineConfig {
    let mut config = EngineConfig::manual(dir);
    config.bridge_executable = Some(riddim_test_support::bridge_exe());
    config
}

fn loaded(events: &std::sync::mpsc::Receiver<Event>, count: usize) {
    for _ in 0..count {
        wait_for(events, "a CLAP plugin to load", |e| match e {
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

fn start(dir: &std::path::Path, ops: Vec<Op>, plugins: usize) -> Engine {
    let mut engine = Engine::start(config(dir)).unwrap();
    let events = engine.subscribe();
    engine.apply_wait(ops).unwrap();
    loaded(&events, plugins);
    engine.sync();
    engine.run(256);
    engine
}

#[test]
fn clap_plugins_play_and_their_latency_is_compensated_to_the_sample() {
    let dir = temp_dir("clap-plays");

    let mut engine = start(&dir, project(false, false), 3);
    let out = play(&mut engine, 30_000);
    assert_eq!(engine.status().plan_latency, LATENCY);
    assert_eq!(engine.status().rt_violations, 0);
    assert!(peak(&out[0]) > 0.3, "two synths should sound");
    // The sine starts at zero on the note's own sample, and the whole mix is late by the
    // delay's latency: the first sample that is not zero is the one after.
    assert_eq!(first_sound(&out[0]), Some(LATENCY as usize + 1));
    assert_eq!(out[0], out[1]);
    drop(engine);

    // The same with the delayed track's polarity flipped: the track without the delay is
    // held back by exactly the delay's latency, and nothing is left.
    let mut engine = start(&dir, project(true, false), 3);
    let out = play(&mut engine, 30_000);
    assert_eq!(engine.status().plan_latency, LATENCY);
    assert_eq!(
        peak(&out[0]),
        0.0,
        "a CLAP plugin's latency must be compensated exactly"
    );
    assert_eq!(peak(&out[1]), 0.0);
}

#[test]
fn a_clap_parameter_is_set_saved_with_the_project_and_restored() {
    let dir = temp_dir("clap-state");
    let mut ops = project(false, false);
    // One synth is enough here.
    ops.truncate(3);

    let mut engine = start(&dir, ops.clone(), 1);
    let full = play(&mut engine, 12_000);
    // A quarter of the way along the parameter is a plain gain of 0.5: exactly half.
    engine.set_param(SlotId(11), ids::CLAP_SYNTH_GAIN, 0.25);
    engine.sync();
    engine.run(256);
    let half = play(&mut engine, 12_000);
    assert!(peak(&full[0]) > 0.2);
    let halved: Vec<f32> = full[0].iter().map(|s| s * 0.5).collect();
    assert!(half[0] == halved, "the parameter did not reach the plugin");

    engine.transport(TransportCommand::Stop);
    engine.capture_plugin_states().unwrap();
    let saved = engine.model().to_json();
    drop(engine);

    // A new engine, given the saved project, sounds as the old one did when it was saved.
    let model = Model::from_json(&saved).unwrap();
    let slot = model.track(TrackId(10)).unwrap().plugins[0].clone();
    assert!(slot.state.is_some(), "the plugin's state is in the project");
    let reopened_ops = vec![
        ops[0].clone(),
        Op::InsertPlugin {
            track: TrackId(10),
            index: 0,
            slot: (*slot).clone(),
        },
        ops[2].clone(),
    ];
    let mut engine = start(&dir, reopened_ops, 1);
    let reopened = play(&mut engine, 12_000);
    assert!(reopened[0] == half[0], "the saved state was not restored");
}

#[test]
fn a_bounce_through_clap_plugins_is_repeatable_and_trims_their_latency() {
    let dir = temp_dir("clap-bounce");
    let mut model = Model::new();
    model.apply_all(&project(false, false)).unwrap();
    let request = |name: &str, render_threads: usize| BounceRequest {
        path: dir.join(name),
        start: Position::START,
        end: at_sample(48_000),
        tail_seconds: 0.0,
        sample_rate: RATE,
        block_frames: 64,
        format: SampleFormat::Float32,
        media_dir: dir.to_path_buf(),
        tracks: None,
        render_threads,
    };
    let first = bounce(&model, &request("one.wav", 0)).unwrap();
    assert!(
        first.missing_plugins.is_empty(),
        "{:?}",
        first.missing_plugins
    );
    assert_eq!(first.latency_trimmed, LATENCY);
    assert_eq!(first.frames, 48_000);
    assert!(first.peak > 0.3);
    let again = bounce(&model, &request("two.wav", 0)).unwrap();
    let threaded = bounce(&model, &request("three.wav", 2)).unwrap();
    let bytes = |result: &riddim_engine::BounceResult| std::fs::read(&result.path).unwrap();
    assert!(bytes(&first) == bytes(&again), "two bounces differ");
    assert!(
        bytes(&first) == bytes(&threaded),
        "render threads change the bounce"
    );

    // The file is the timeline, not the timeline plus latency: it is what the live engine
    // plays, once the live engine's latency is allowed for.
    let mut engine = start(&dir, project(false, false), 3);
    let live = play(&mut engine, 4_096);
    let wav = wav::read(&first.path).unwrap();
    let latency = LATENCY as usize;
    assert!(wav.channels[0][..4_096 - latency] == live[0][latency..]);
}

#[cfg(unix)]
#[test]
fn clap_plugins_in_processes_of_their_own_sound_the_same() {
    let dir = temp_dir("clap-bridged");
    let render = |bridged: bool| {
        let mut engine = start(&dir, project(false, bridged), 3);
        let out = play(&mut engine, 30_000);
        assert_eq!(engine.status().plan_latency, LATENCY);
        assert_eq!(engine.status().rt_violations, 0);
        out
    };
    let in_process = render(false);
    assert!(peak(&in_process[0]) > 0.3);
    assert!(
        render(true) == in_process,
        "bridged CLAP plugins change the audio"
    );
}
