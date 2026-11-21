//! Plugins inside the engine: instruments driven by MIDI clips and live notes, parameters,
//! loading and unloading, latency compensation, and state that survives a save and reopen.

mod common;

use common::*;
use riddim_engine::{Engine, Event, TransportCommand};
use riddim_model::{Model, Op, PluginRef, PluginSlot, SlotId, TrackChange, TrackId, TrackKind};
use riddim_plugin::EventBody;
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// An instrument track (id 10) with the fixture synth (slot 12) and a one-bar clip (id 11).
fn synth_session(engine: &Engine, notes: Vec<riddim_model::Note>) {
    engine
        .apply_wait(vec![
            add_track(0, 10, TrackKind::Instrument),
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(12, ids::SYNTH_UID),
            },
            Op::AddClip {
                track: TrackId(10),
                clip: midi_clip(11, 0, 4, notes),
            },
        ])
        .unwrap();
    engine.sync();
}

/// Second sample of a full-velocity A4 from the fixture synth at unity gain.
fn a4_second_sample() -> f32 {
    (std::f64::consts::TAU * 440.0 / 48_000.0).sin() as f32 * 0.25
}

#[test]
fn a_midi_clip_plays_the_instrument_on_the_sample() {
    let dir = temp_dir("midi");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    // A4 on beat 2 (sample 24000 at 120 bpm), half a beat long.
    synth_session(&engine, vec![note((1, 1), (1, 2), 69)]);
    let (latency, parameters) = wait_for(&events, "the synth to load", |e| match e {
        Event::PluginLoaded {
            slot: SlotId(12),
            latency,
            parameters,
            ..
        } => Some((*latency, *parameters)),
        _ => None,
    });
    assert_eq!((latency, parameters), (0, 5));

    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(48_000);
    let left = &out[0];
    // The synth's phase starts at zero, so its first non-zero sample is one after the note-on.
    assert_eq!(first_sound(left), Some(24_001));
    assert!((left[24_001] - a4_second_sample()).abs() < 1e-6);
    assert_eq!(out[0], out[1]);
    // Half a beat later the note is released, on the sample.
    assert!(left[35_999] != 0.0);
    assert!(left[36_000..].iter().all(|&s| s == 0.0));
    assert_eq!(
        engine.status().rt_violations,
        0,
        "the audio path touched the heap"
    );
}

#[test]
fn notes_are_cut_at_the_clip_end_and_released_on_stop_and_on_loop() {
    let dir = temp_dir("release");
    let mut engine = engine(&dir);
    // A note far longer than its one-bar clip.
    synth_session(&engine, vec![note((0, 1), (64, 1), 60)]);
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(100_000);
    assert!(out[0][95_999] != 0.0);
    assert!(
        out[0][96_000..].iter().all(|&s| s == 0.0),
        "the note must end with its clip"
    );

    // Stop in the middle of a note: it must not hang.
    engine.transport(TransportCommand::Stop);
    engine.transport(TransportCommand::Locate(
        riddim_model::time::Position::START,
    ));
    engine.transport(TransportCommand::Play);
    engine.sync();
    assert!(peak(&engine.run(1000)[0]) > 0.0);
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(128);
    assert!(
        engine.run(1000)[0].iter().all(|&s| s == 0.0),
        "a note was left hanging after stop"
    );
}

#[test]
fn live_notes_parameters_and_bypass() {
    let dir = temp_dir("live");
    let mut engine = engine(&dir);
    synth_session(&engine, vec![]);

    // A note played from the application sounds without the transport running.
    engine.play_note(
        TrackId(10),
        EventBody::NoteOn {
            channel: 0,
            key: 69,
            velocity: 1.0,
            note_id: -1,
        },
    );
    engine.sync();
    let out = engine.run(256);
    assert_eq!(first_sound(&out[0]), Some(1));
    let reference = peak(&engine.run(1024)[0]);
    assert!(reference > 0.2);

    // Gain to zero through the engine: silent from the next block.
    engine.set_param(SlotId(12), ids::SYNTH_GAIN, 0.0);
    engine.sync();
    engine.run(128);
    assert_eq!(peak(&engine.run(512)[0]), 0.0);
    engine.set_param(SlotId(12), ids::SYNTH_GAIN, 0.5);
    engine.sync();
    engine.run(128);
    assert!(peak(&engine.run(512)[0]) > 0.2);

    // Bypassing an instrument silences it; un-bypassing brings it back without reloading.
    engine
        .apply_wait(vec![Op::SetPluginBypass {
            track: TrackId(10),
            slot: SlotId(12),
            bypass: true,
        }])
        .unwrap();
    engine.sync();
    engine.run(128);
    assert_eq!(peak(&engine.run(512)[0]), 0.0);
    engine
        .apply_wait(vec![Op::SetPluginBypass {
            track: TrackId(10),
            slot: SlotId(12),
            bypass: false,
        }])
        .unwrap();
    engine.sync();
    engine.run(128);
    assert!(
        peak(&engine.run(512)[0]) > 0.2,
        "the held note should still be sounding"
    );

    engine.play_note(
        TrackId(10),
        EventBody::NoteOff {
            channel: 0,
            key: 69,
            velocity: 0.0,
            note_id: -1,
        },
    );
    engine.sync();
    engine.run(128);
    assert_eq!(peak(&engine.run(512)[0]), 0.0);
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn plugins_can_be_removed_and_added_while_playing() {
    let dir = temp_dir("churn");
    let mut engine = engine(&dir);
    synth_session(&engine, vec![note((0, 1), (4, 1), 60)]);
    engine
        .apply_wait(vec![Op::SetLoop {
            range: Some(riddim_model::LoopRange {
                start: at_sample(0),
                end: at_sample(96_000),
            }),
            enabled: true,
        }])
        .unwrap();
    engine.transport(TransportCommand::Play);
    engine.sync();
    assert!(peak(&engine.run(2048)[0]) > 0.0);

    for round in 0..5u64 {
        let undo = engine
            .apply_wait(vec![Op::RemovePlugin {
                track: TrackId(10),
                slot: SlotId(12),
            }])
            .unwrap();
        engine.sync();
        engine.run(256);
        assert_eq!(
            peak(&engine.run(1024)[0]),
            0.0,
            "round {round}: the instrument is gone"
        );
        // Undo brings the same slot back; it loads again and plays on the next loop pass.
        engine.apply_wait(undo.inverses).unwrap();
        engine.sync();
        let out = engine.run(96_000);
        assert!(
            peak(&out[0]) > 0.0,
            "round {round}: the instrument did not come back"
        );
    }
    assert_eq!(
        engine.status().rt_violations,
        0,
        "graph churn touched the heap on the audio path"
    );
}

#[test]
fn a_missing_plugin_is_reported_and_the_rest_of_the_project_plays() {
    let dir = temp_dir("missing");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    let ghost = PluginSlot {
        id: SlotId(22),
        plugin: PluginRef {
            format: "vst3".into(),
            uid: "00000000000000000000000000000000".into(),
            name: "Not Installed".into(),
            path: Some("/nonexistent/Ghost.vst3".into()),
        },
        state: Some(std::sync::Arc::new(vec![1, 2, 3])),
        bypass: false,
    };
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![vec![0.5; 4000]]),
            add_track(0, 10, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 4000),
            },
            // An effect that cannot be found, on the audio track.
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: ghost.clone(),
            },
        ])
        .unwrap();
    let error = wait_for(&events, "the load failure", |e| match e {
        Event::PluginFailed {
            slot: SlotId(22),
            error,
            ..
        } => Some(error.clone()),
        _ => None,
    });
    assert!(!error.is_empty());
    engine.transport(TransportCommand::Play);
    engine.sync();
    // The audio passes the missing effect by.
    assert_eq!(engine.run(1000)[0][999], 0.5);
    // And the model still carries the slot and its state, for the machine that has the plugin.
    assert_eq!(
        engine.model().track(TrackId(10)).unwrap().plugins[0].as_ref(),
        &ghost
    );
}

#[test]
fn a_plugin_with_latency_is_compensated_against_a_dry_track() {
    let dir = temp_dir("pdc");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    let mut impulse = vec![0.0f32; 2000];
    impulse[0] = 1.0;
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, vec![impulse]),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 1000, 2000),
            },
            Op::AddClip {
                track: TrackId(20),
                clip: audio_clip(21, 1, 1000, 2000),
            },
            // The fixture delay delays by 64 samples and reports 64 samples of latency.
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(12, ids::DELAY_UID),
            },
        ])
        .unwrap();
    let latency = wait_for(&events, "the delay to load", |e| match e {
        Event::PluginLoaded {
            slot: SlotId(12),
            latency,
            ..
        } => Some(*latency),
        _ => None,
    });
    assert_eq!(latency, 64);
    engine.sync();
    engine.transport(TransportCommand::Play);
    engine.sync();
    let out = engine.run(4000);
    assert_eq!(engine.status().plan_latency, 64);
    // Both impulses arrive together, 64 samples late: one sample of amplitude 2, nothing else.
    let hits: Vec<(usize, f32)> = out[0]
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, s)| *s != 0.0)
        .collect();
    assert_eq!(
        hits,
        vec![(1064, 2.0)],
        "the dry track was not delayed to match the plugin"
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn a_project_sounds_the_same_after_save_and_reopen() {
    let dir = temp_dir("reopen");
    let render = |engine: &mut Engine| {
        engine.transport(TransportCommand::Locate(
            riddim_model::time::Position::START,
        ));
        engine.transport(TransportCommand::Play);
        engine.sync();
        engine.run(96_000)
    };

    let saved_json;
    let original;
    {
        let mut engine = engine(&dir);
        engine
            .apply_wait(vec![
                media(
                    &dir,
                    1,
                    RATE,
                    vec![(0..30_000)
                        .map(|n| ((n % 97) as f32 / 97.0) - 0.5)
                        .collect()],
                ),
                add_track(0, 10, TrackKind::Instrument),
                add_track(1, 20, TrackKind::Audio),
                Op::InsertPlugin {
                    track: TrackId(10),
                    index: 0,
                    slot: fixture_slot(12, ids::SYNTH_UID),
                },
                Op::InsertPlugin {
                    track: TrackId(20),
                    index: 0,
                    slot: fixture_slot(22, ids::GAIN_UID),
                },
                Op::AddClip {
                    track: TrackId(10),
                    clip: midi_clip(
                        11,
                        0,
                        4,
                        vec![note((0, 1), (1, 1), 60), note((1, 1), (2, 1), 67)],
                    ),
                },
                Op::AddClip {
                    track: TrackId(20),
                    clip: audio_clip(21, 1, 5000, 30_000),
                },
                Op::SetTrack {
                    id: TrackId(20),
                    change: TrackChange::Pan(0.5),
                },
            ])
            .unwrap();
        engine.sync();
        // Turn knobs: these live only in the plugins until the states are captured.
        engine.set_param(SlotId(12), ids::SYNTH_GAIN, 0.8125);
        engine.set_param(SlotId(22), ids::GAIN_GAIN, 0.25);
        engine.sync();
        engine.run(256);
        original = render(&mut engine);
        assert!(peak(&original[0]) > 0.0);

        engine.transport(TransportCommand::Stop);
        let applied = engine.capture_plugin_states().unwrap();
        assert_eq!(applied.inverses.len(), 2);
        saved_json = engine.model().to_json();
    }

    // "Quit and relaunch": a new engine, the model from the file.
    let mut reopened = engine(&dir);
    let model = Model::from_json(&saved_json).unwrap();
    assert!(model.track(TrackId(10)).unwrap().plugins[0].state.is_some());
    reopened.load(model).unwrap();
    reopened.sync();
    let again = render(&mut reopened);
    assert_eq!(
        again, original,
        "the reopened project must render bit for bit the same"
    );

    // Capturing state again from the reopened project gives the same blobs: nothing drifts.
    reopened.transport(TransportCommand::Stop);
    reopened.capture_plugin_states().unwrap();
    assert_eq!(reopened.model().to_json(), saved_json);
}

#[test]
fn an_external_main_thread_loads_one_plugin_per_pump() {
    // An application that pumps the plugin host from its UI thread must get a turn between
    // one plugin load and the next, however many were asked for at once.
    let dir = temp_dir("pump");
    let mut config = riddim_engine::EngineConfig::manual(&dir);
    config.main_thread = riddim_engine::MainThreadMode::External;
    let mut engine = Engine::start(config).unwrap();
    let mut main = engine
        .take_main_thread()
        .expect("external mode hands out the main thread");
    let events = engine.subscribe();
    engine.apply(vec![
        add_track(0, 10, TrackKind::Instrument),
        add_track(1, 20, TrackKind::Audio),
        add_track(2, 30, TrackKind::Audio),
        Op::InsertPlugin {
            track: TrackId(10),
            index: 0,
            slot: fixture_slot(11, ids::SYNTH_UID),
        },
        Op::InsertPlugin {
            track: TrackId(20),
            index: 0,
            slot: fixture_slot(21, ids::GAIN_UID),
        },
        Op::InsertPlugin {
            track: TrackId(30),
            index: 0,
            slot: fixture_slot(31, ids::DELAY_UID),
        },
    ]);
    // Wait for the edit itself, without pumping: nothing can load yet.
    wait_for(&events, "the edit", |e| {
        matches!(e, Event::Applied { .. }).then_some(())
    });
    // (No `engine.sync()` here: it waits for plugins to finish loading, which is exactly what
    // an unpumped main thread prevents.)
    let loaded_within = |wait: std::time::Duration| {
        let deadline = std::time::Instant::now() + wait;
        let mut count = 0;
        loop {
            while let Ok(event) = events.try_recv() {
                if matches!(event, Event::PluginLoaded { .. }) {
                    count += 1;
                }
            }
            if std::time::Instant::now() >= deadline {
                return count;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    let settle = std::time::Duration::from_millis(250);
    assert_eq!(
        loaded_within(settle),
        0,
        "nothing loads until the main thread is pumped"
    );
    for turn in 1..=3 {
        assert!(main.pump());
        assert_eq!(
            loaded_within(settle),
            1,
            "pump {turn} should have loaded exactly one plugin"
        );
    }
    assert!(main.pump());
    assert_eq!(loaded_within(settle), 0);
    drop(engine);
    while main.pump() {}
}
