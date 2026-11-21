//! The events the engine sends its client: every kind is produced here at least once, and
//! each serialises to the JSON object a binding in another language would see.

mod common;

use std::collections::BTreeSet;
use std::sync::mpsc::Receiver;

use common::*;
use riddim_engine::{DeviceConfig, Engine, EngineConfig, Event, NullConfig, TransportCommand};
use riddim_model::time::{Position, Ticks};
use riddim_model::{LoopRange, Op, SlotId, TrackChange, TrackId, TrackInput, TrackKind};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// The `"event"` tag an event serialises with.
fn tag(event: &Event) -> String {
    let json = serde_json::to_value(event).expect("every event serialises");
    json["event"]
        .as_str()
        .expect("events are tagged")
        .to_string()
}

/// Waits for an event, remembering the tag of everything that goes by.
fn expect<T>(
    events: &Receiver<Event>,
    seen: &mut BTreeSet<String>,
    what: &str,
    mut pick: impl FnMut(&Event) -> Option<T>,
) -> T {
    wait_for(events, what, |event| {
        seen.insert(tag(event));
        pick(event)
    })
}

#[test]
fn every_kind_of_event_is_sent_and_serialises() {
    let dir = temp_dir("events");
    // A timer-driven device, so that the periodic events (position, meters, load) arrive by
    // themselves.
    let mut config = EngineConfig::manual(&dir);
    config.device = DeviceConfig::Null(NullConfig {
        buffer_frames: 256,
        ..NullConfig::default()
    });
    let mut engine = Engine::start(config).unwrap();
    let events = engine.subscribe();
    let mut seen = BTreeSet::new();

    // applied, rejected
    let ticket = engine.apply(vec![
        media(&dir, 1, RATE, vec![vec![0.25; 96_000]]),
        add_track(0, 10, TrackKind::Audio),
        add_track(1, 20, TrackKind::Instrument),
        add_track(2, 30, TrackKind::Audio),
        Op::AddClip {
            track: TrackId(10),
            clip: audio_clip(11, 1, 0, 96_000),
        },
        Op::InsertPlugin {
            track: TrackId(20),
            index: 0,
            slot: fixture_slot(21, ids::SYNTH_UID),
        },
        Op::InsertPlugin {
            track: TrackId(10),
            index: 0,
            slot: fixture_slot(12, ids::DELAY_UID),
        },
        // A plugin that is not there, and a clip whose file is not there.
        Op::InsertPlugin {
            track: TrackId(30),
            index: 0,
            slot: {
                let mut ghost = fixture_slot(31, "00000000000000000000000000000000");
                ghost.plugin.path = Some("/nonexistent/Ghost.vst3".into());
                ghost
            },
        },
        Op::AddMedia {
            media: riddim_model::Media {
                id: riddim_model::MediaId(2),
                path: "missing.wav".into(),
                sample_rate: RATE,
                channels: 1,
                frames: 1000,
            },
        },
        Op::AddClip {
            track: TrackId(30),
            clip: audio_clip(32, 2, 0, 1000),
        },
    ]);
    engine.apply(vec![Op::RemoveTrack { id: TrackId(99) }]);
    // (In whatever order they come: a missing file is noticed while the edit is being
    // applied, plugins load afterwards.)
    let (mut applied, mut loaded, mut failed, mut missing, mut rejected) =
        (false, 0, false, false, false);
    while !applied || loaded < 2 || !failed || !missing || !rejected {
        expect(
            &events,
            &mut seen,
            "the edit, the plugins and the media",
            |e| {
                match e {
                    Event::Applied { ticket: t, .. } => applied |= *t == ticket,
                    Event::PluginLoaded { .. } => loaded += 1,
                    Event::PluginFailed { slot, .. } => failed |= *slot == SlotId(31),
                    Event::MediaMissing { .. } => missing = true,
                    Event::Rejected { .. } => rejected = true,
                    _ => return None,
                }
                Some(())
            },
        );
    }
    engine.sync();

    // transport, position, meters, jumped (a loop wrap), load
    engine
        .apply_wait(vec![Op::SetLoop {
            range: Some(LoopRange {
                start: Position::START,
                end: Position::Beats(Ticks::from_beat_fraction(1, 4)),
            }),
            enabled: true,
        }])
        .unwrap();
    engine.transport(TransportCommand::Play);
    let mut wanted: BTreeSet<&str> = ["transport", "position", "meters", "jumped", "load"]
        .into_iter()
        .collect();
    while !wanted.is_empty() {
        expect(&events, &mut seen, "the periodic events", |e| {
            match e {
                Event::Jumped {
                    from_samples,
                    to_samples,
                } => assert_eq!((*from_samples, *to_samples), (6_000, 0)),
                Event::Load { dsp } => assert!(dsp.is_finite() && *dsp >= 0.0, "{dsp}"),
                Event::Meters { tracks } => {
                    assert!(tracks.iter().any(|(track, _)| *track == TrackId::MASTER))
                }
                _ => {}
            }
            wanted.remove(tag(e).as_str()).then_some(())
        });
    }
    let status = engine.status();
    assert!(status.dsp_load.is_finite());
    assert_eq!(status.events_lost, 0);

    // What a plugin tells its host: a knob dragged in its editor (gesture, value, gesture),
    // unsaved state, a preset's worth of values, a changed parameter list, a restart request.
    let command = |engine: &Engine, what: f64| {
        engine.set_param(SlotId(21), ids::COMMAND, what);
    };
    command(&engine, ids::COMMAND_EDIT);
    let mut gestures = Vec::new();
    while gestures.len() < 3 {
        expect(
            &events,
            &mut seen,
            "the edit made in the plugin",
            |e| match e {
                Event::PluginGesture { slot, id, down } if *slot == SlotId(21) => {
                    assert_eq!(*id, ids::SYNTH_GAIN);
                    gestures.push(if *down { "down" } else { "up" });
                    Some(())
                }
                Event::PluginParam { slot, id, value }
                    if *slot == SlotId(21) && *id == ids::SYNTH_GAIN =>
                {
                    assert_eq!(*value, 0.75);
                    gestures.push("value");
                    Some(())
                }
                _ => None,
            },
        );
    }
    assert_eq!(gestures, vec!["down", "value", "up"]);
    command(&engine, ids::COMMAND_DIRTY);
    expect(&events, &mut seen, "the dirty flag", |e| {
        matches!(e, Event::PluginStateDirty { slot } if *slot == SlotId(21)).then_some(())
    });
    command(&engine, ids::COMMAND_VALUES_CHANGED);
    expect(&events, &mut seen, "the values-changed notice", |e| {
        matches!(e, Event::PluginParamsChanged { slot, info: false } if *slot == SlotId(21))
            .then_some(())
    });
    command(&engine, ids::COMMAND_INFO_CHANGED);
    expect(&events, &mut seen, "the list-changed notice", |e| {
        matches!(e, Event::PluginParamsChanged { slot, info: true } if *slot == SlotId(21))
            .then_some(())
    });
    command(&engine, ids::COMMAND_RESTART);
    expect(
        &events,
        &mut seen,
        "the restart the plugin asked for",
        |e| matches!(e, Event::PluginRestarted { slot, .. } if *slot == SlotId(21)).then_some(()),
    );
    // A latency change: the fixture delay reports its delay parameter as latency.
    engine.set_param(SlotId(12), ids::DELAY_DELAY, 128.0 / ids::DELAY_MAX as f64);
    let latency = expect(&events, &mut seen, "the latency change", |e| match e {
        Event::PluginLatencyChanged { slot, latency } if *slot == SlotId(12) => Some(*latency),
        _ => None,
    });
    assert_eq!(latency, 128);

    // stream_changed
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.set_buffer_size(128).unwrap();
    expect(&events, &mut seen, "the stream change", |e| {
        matches!(
            e,
            Event::StreamChanged {
                buffer_frames: 128,
                ..
            }
        )
        .then_some(())
    });
    for _ in 0..2 {
        expect(&events, &mut seen, "the plugins to come back", |e| {
            matches!(e, Event::PluginRestarted { .. }).then_some(())
        });
    }

    // recording_started, recording_finished, model_changed; and an error for a locate
    // attempted mid-take.
    engine
        .apply_wait(vec![
            Op::SetLoop {
                range: None,
                enabled: false,
            },
            Op::SetTrack {
                id: TrackId(30),
                change: TrackChange::Input(TrackInput::Audio {
                    first_channel: 0,
                    channels: 1,
                }),
            },
            Op::SetTrack {
                id: TrackId(30),
                change: TrackChange::RecordArmed(true),
            },
        ])
        .unwrap();
    engine.transport(TransportCommand::Record);
    expect(&events, &mut seen, "recording to start", |e| {
        matches!(e, Event::RecordingStarted { .. }).then_some(())
    });
    engine.transport(TransportCommand::Locate(Position::START));
    expect(&events, &mut seen, "the refusal", |e| {
        matches!(e, Event::Error { .. }).then_some(())
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    engine.transport(TransportCommand::Stop);
    let mut wanted: BTreeSet<&str> = ["model_changed", "recording_finished"]
        .into_iter()
        .collect();
    while !wanted.is_empty() {
        expect(&events, &mut seen, "the take", |e| {
            wanted.remove(tag(e).as_str()).then_some(())
        });
    }

    // Every kind the engine has, bar two that need a failure the test cannot stage: a device
    // missing its deadline, and a disk too slow to take a recording.
    let all: BTreeSet<String> = [
        "applied",
        "rejected",
        "model_changed",
        "transport",
        "position",
        "jumped",
        "meters",
        "plugin_loaded",
        "plugin_failed",
        "plugin_restarted",
        "plugin_param",
        "plugin_latency_changed",
        "plugin_gesture",
        "plugin_params_changed",
        "plugin_state_dirty",
        "recording_started",
        "recording_finished",
        "media_missing",
        "stream_changed",
        "load",
        "error",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let unseen: Vec<&String> = all.difference(&seen).collect();
    assert!(unseen.is_empty(), "never sent: {unseen:?}");

    // The remaining kinds at least serialise to what the reference says.
    for (event, expected) in [
        (Event::Xrun { total: 3 }, r#"{"event":"xrun","total":3}"#),
        (
            Event::RecordingOverflow {
                track: TrackId(4),
                frames_dropped: 9,
            },
            r#"{"event":"recording_overflow","track":4,"frames_dropped":9}"#,
        ),
        (
            Event::PluginEditorResize {
                slot: SlotId(2),
                width: 800,
                height: 600,
            },
            r#"{"event":"plugin_editor_resize","slot":2,"width":800,"height":600}"#,
        ),
    ] {
        assert_eq!(serde_json::to_string(&event).unwrap(), expected);
    }
    assert_eq!(engine.status().rt_violations, 0);
}
