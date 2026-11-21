//! Changing the audio buffer size while the engine runs.

mod common;

use common::*;
use riddim_engine::{
    DeviceConfig, Engine, EngineConfig, EngineError, Event, NullConfig, TransportCommand,
    TransportState,
};
use riddim_model::time::Position;
use riddim_model::{Op, SlotId, TrackChange, TrackId, TrackInput, TrackKind};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// Deterministic noise in -0.5..0.5.
fn noise(frames: usize) -> Vec<f32> {
    let mut state = 0x2545_F491u32;
    (0..frames)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect()
}

/// Noise on an audio track through the fixture delay (64 samples of latency, compensated),
/// and a synth playing a long note: a project whose output depends on the position sample
/// for sample and on plugin state.
fn project(dir: &std::path::Path, engine: &Engine) {
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            media(dir, 1, RATE, vec![noise(200_000)]),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Instrument),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 200_000),
            },
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(12, ids::DELAY_UID),
            },
            Op::InsertPlugin {
                track: TrackId(20),
                index: 0,
                slot: fixture_slot(21, ids::SYNTH_UID),
            },
            Op::AddClip {
                track: TrackId(20),
                clip: midi_clip(22, 0, 8, vec![note((0, 1), (8, 1), 57)]),
            },
        ])
        .unwrap();
    for _ in 0..2 {
        wait_for(&events, "the plugins to load", |e| {
            matches!(e, Event::PluginLoaded { .. }).then_some(())
        });
    }
    engine.sync();
    // A knob turned by hand: it has to survive the plugins being restarted.
    engine.set_param(SlotId(21), ids::SYNTH_GAIN, 0.625);
    engine.sync();
}

fn play_from_start(engine: &mut Engine, frames: usize) -> Vec<Vec<f32>> {
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(4096);
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(frames)
}

/// Turns the manual device until every plugin is back from its restart.
fn settle(engine: &mut Engine, events: &std::sync::mpsc::Receiver<Event>, plugins: usize) -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut back = 0;
    let mut frames = 0;
    while back < plugins {
        frames += engine.run(engine.stream_info().max_frames as usize)[0].len();
        while let Ok(event) = events.try_recv() {
            match event {
                Event::PluginRestarted { .. } => back += 1,
                Event::PluginFailed { error, .. } => panic!("a plugin did not restart: {error}"),
                _ => {}
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the plugins did not come back"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    engine.sync();
    frames
}

#[test]
fn the_buffer_size_changes_without_reopening_the_project() {
    const FRAMES: usize = 40_000;
    let dir = temp_dir("buffersize");
    let mut engine = engine(&dir);
    project(&dir, &engine);
    let reference = play_from_start(&mut engine, FRAMES);
    assert!(peak(&reference[0]) > 0.2);
    let events = engine.subscribe();

    // Across the whole range the vision asks for, changed while stopped: the project plays
    // exactly as before, sample for sample, at every size.
    for frames in [32u32, 2048, 64, 512, 128] {
        engine.transport(TransportCommand::Stop);
        engine.sync();
        let info = engine.set_buffer_size(frames).unwrap();
        assert_eq!(info.max_frames, frames);
        assert_eq!(engine.stream_info().max_frames, frames);
        let changed = wait_for(&events, "the stream change", |e| match e {
            Event::StreamChanged { buffer_frames, .. } => Some(*buffer_frames),
            _ => None,
        });
        assert_eq!(changed, frames);
        settle(&mut engine, &events, 2);
        assert_eq!(
            play_from_start(&mut engine, FRAMES),
            reference,
            "at {frames} frames the project plays differently"
        );
    }
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn the_buffer_size_changes_while_playing() {
    let dir = temp_dir("buffersize-live");
    let mut engine = engine(&dir);
    project(&dir, &engine);
    let reference = play_from_start(&mut engine, 120_000);
    let events = engine.subscribe();

    // Play, change the size mid-flight, and keep going.
    let mut played = play_from_start(&mut engine, 20_000)[0].clone();
    engine.set_buffer_size(512).unwrap();
    // While the plugins are being restarted they are out of the mix; the transport carries
    // on regardless.
    let gap = settle(&mut engine, &events, 2);
    played.extend(vec![f32::NAN; gap]);
    played.extend(engine.run(60_000)[0].iter());
    assert_eq!(engine.status().position_samples, played.len() as i64);

    // Before the change: exactly the reference. After the plugins are back and the delay has
    // refilled (64 samples) and the synth's note... the synth was restarted mid-note, so it
    // is silent until its next note-on; compare the audio track alone from here on.
    assert_eq!(played[..20_000], reference[0][..20_000]);
    engine.transport(TransportCommand::Stop);
    engine
        .apply_wait(vec![Op::SetTrack {
            id: TrackId(20),
            change: TrackChange::Mute(true),
        }])
        .unwrap();
    let noise_only = play_from_start(&mut engine, 120_000);
    let resumed = 20_000 + gap + 2048;
    assert_eq!(
        played[resumed..],
        noise_only[0][resumed..played.len()],
        "after the change the audio track is not where the timeline says it should be"
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn what_a_buffer_size_change_refuses() {
    let dir = temp_dir("buffersize-refused");
    let mut engine = engine(&dir);
    assert!(matches!(
        engine.set_buffer_size(8),
        Err(EngineError::Refused(_))
    ));
    assert!(matches!(
        engine.set_buffer_size(100_000),
        Err(EngineError::Refused(_))
    ));
    assert_eq!(engine.stream_info().max_frames, BLOCK as u32);

    // Not while recording.
    engine
        .apply_wait(vec![
            add_track(0, 10, TrackKind::Audio),
            Op::SetTrack {
                id: TrackId(10),
                change: TrackChange::Input(TrackInput::Audio {
                    first_channel: 0,
                    channels: 1,
                }),
            },
            Op::SetTrack {
                id: TrackId(10),
                change: TrackChange::RecordArmed(true),
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Record);
    engine.sync();
    engine.run(1024);
    engine.sync();
    assert_eq!(
        engine.status().transport,
        TransportState::Recording,
        "the test needs a recording in progress"
    );
    assert!(matches!(
        engine.set_buffer_size(256),
        Err(EngineError::Refused(_))
    ));
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(1024);
    engine.sync();
    assert!(engine.set_buffer_size(256).is_ok());
}

#[test]
fn a_running_device_carries_on_after_a_buffer_size_change() {
    // The timer-driven device: a thread of its own calls the engine, as hardware would.
    let dir = temp_dir("buffersize-null");
    let mut config = EngineConfig::manual(&dir);
    config.device = DeviceConfig::Null(NullConfig {
        buffer_frames: 64,
        ..NullConfig::default()
    });
    let mut engine = Engine::start(config).unwrap();
    project(&dir, &engine);
    let events = engine.subscribe();
    engine.transport(TransportCommand::Play);
    engine.sync();
    std::thread::sleep(std::time::Duration::from_millis(50));

    let before = engine.status();
    engine.set_buffer_size(256).unwrap();
    for _ in 0..2 {
        wait_for(&events, "the plugins to come back", |e| {
            matches!(e, Event::PluginRestarted { .. }).then_some(())
        });
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let after = engine.status();
    assert!(after.blocks > before.blocks, "the device stopped calling");
    assert!(
        after.position_samples > before.position_samples,
        "the transport stopped"
    );
    assert_eq!(after.transport, TransportState::Playing);
    assert_eq!(after.rt_violations, 0);
}
