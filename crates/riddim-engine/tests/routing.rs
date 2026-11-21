//! Routing between tracks beyond outputs and sends: a plugin's extra outputs, sidechains, and
//! notes from one track's plugins into another track's instrument.

mod common;

use common::*;
use riddim_engine::{Engine, Event, TransportCommand};
use riddim_model::time::Position;
use riddim_model::{Note, Op, PluginOutput, Sidechain, SlotId, TrackChange, TrackId, TrackKind};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

fn set(id: u64, change: TrackChange) -> Op {
    Op::SetTrack {
        id: TrackId(id),
        change,
    }
}

fn on_channel(mut note: Note, channel: u8) -> Note {
    note.channel = channel;
    note
}

fn play_from_start(engine: &mut Engine, frames: usize) -> Vec<Vec<f32>> {
    engine.transport(TransportCommand::Stop);
    engine.sync();
    engine.run(2048);
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    engine.run(frames)
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

/// Waits for a plugin to come back from a restart.
///
/// A restart needs the audio thread to have let go of the plugin first, which it does when it
/// next runs a block. A real device does that by itself; the manual one has to be turned.
fn restarted(engine: &mut Engine, events: &std::sync::mpsc::Receiver<Event>, slot: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        engine.run(BLOCK);
        while let Ok(event) = events.try_recv() {
            match event {
                Event::PluginRestarted { slot: s, .. } if s == SlotId(slot) => {
                    engine.sync();
                    return;
                }
                Event::PluginFailed { error, .. } => {
                    panic!("the plugin failed to restart: {error}")
                }
                _ => {}
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the plugin to restart"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// One instrument track (10, synth in slot 11) and a bus (20). The clip has a low note on MIDI
/// channel 1 and a high note on channel 2, which the fixture synth sends to its second output
/// when that is switched on.
fn multi_out_engine(tag: &str) -> (Engine, std::sync::mpsc::Receiver<Event>) {
    let dir = temp_dir(tag);
    let engine = engine(&dir);
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            add_track(0, 10, TrackKind::Instrument),
            add_track(1, 20, TrackKind::Bus),
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(11, ids::SYNTH_UID),
            },
            Op::AddClip {
                track: TrackId(10),
                clip: midi_clip(
                    12,
                    0,
                    4,
                    vec![
                        note((0, 1), (1, 1), 48),
                        on_channel(note((0, 1), (1, 1), 72), 1),
                    ],
                ),
            },
        ])
        .unwrap();
    loaded(&events, 1);
    engine.sync();
    (engine, events)
}

fn route_second_output(target: Option<u64>) -> Op {
    Op::SetPluginRouting {
        track: TrackId(10),
        slot: SlotId(11),
        outputs: target
            .map(|id| PluginOutput {
                bus: 1,
                target: TrackId(id),
            })
            .into_iter()
            .collect(),
        sidechain: None,
    }
}

#[test]
fn a_second_plugin_output_plays_through_its_own_bus() {
    const FRAMES: usize = 12_000;
    let (mut engine, events) = multi_out_engine("multiout");

    // Nothing routed: both notes come out of the main output, through the track.
    let both = play_from_start(&mut engine, FRAMES);
    assert!(peak(&both[0]) > 0.1);
    // The track alone, for reference: mute it and the project is silent.
    engine
        .apply_wait(vec![set(10, TrackChange::Mute(true))])
        .unwrap();
    assert_eq!(peak(&play_from_start(&mut engine, FRAMES)[0]), 0.0);
    engine
        .apply_wait(vec![set(10, TrackChange::Mute(false))])
        .unwrap();

    // Route the second output to the bus. The plugin is restarted with that output switched
    // on, and the sum is what it was: the high note has only changed its way to the master.
    // (A knob turned beforehand stays turned through the restart.)
    engine.set_param(SlotId(11), ids::SYNTH_GAIN, 0.75);
    engine.sync();
    let both = play_from_start(&mut engine, FRAMES);
    let undo = engine
        .apply_wait(vec![route_second_output(Some(20))])
        .unwrap();
    restarted(&mut engine, &events, 11);
    let routed = play_from_start(&mut engine, FRAMES);
    assert_eq!(routed, both, "routing an output must not change the sum");

    // Now the two notes have a fader each. Muting the bus leaves the low note...
    engine
        .apply_wait(vec![set(20, TrackChange::Mute(true))])
        .unwrap();
    let low = play_from_start(&mut engine, FRAMES);
    // ...muting the track instead leaves the high one...
    engine
        .apply_wait(vec![
            set(20, TrackChange::Mute(false)),
            set(10, TrackChange::Mute(true)),
        ])
        .unwrap();
    let high = play_from_start(&mut engine, FRAMES);
    assert!(peak(&low[0]) > 0.05 && peak(&high[0]) > 0.05);
    assert_ne!(low, high);
    // ...and they are the two halves of the whole, sample for sample.
    let worst = (0..FRAMES)
        .map(|n| (both[0][n] - low[0][n] - high[0][n]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        worst < 1e-6,
        "the two outputs do not add up to the sum (off by {worst})"
    );
    // A different pitch on each: the low note crosses zero less often.
    let crossings = |signal: &[f32]| {
        signal
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count()
    };
    assert!(crossings(&high[0]) > 3 * crossings(&low[0]));

    // The bus fader works on the second output only.
    engine
        .apply_wait(vec![
            set(10, TrackChange::Mute(false)),
            set(20, TrackChange::Gain(0.5)),
        ])
        .unwrap();
    let mixed = play_from_start(&mut engine, FRAMES);
    let worst = (0..FRAMES)
        .map(|n| (mixed[0][n] - low[0][n] - 0.5 * high[0][n]).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-6, "off by {worst}");

    // Undo: the output is switched off again, and everything is back on the track.
    engine
        .apply_wait(vec![set(20, TrackChange::Gain(1.0))])
        .unwrap();
    engine.apply_wait(undo.inverses).unwrap();
    restarted(&mut engine, &events, 11);
    assert_eq!(play_from_start(&mut engine, FRAMES), both);
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn an_output_the_plugin_does_not_have_is_left_unconnected() {
    let (mut engine, events) = multi_out_engine("nooutput");
    let before = play_from_start(&mut engine, 6000);
    // The fixture synth has two outputs; the fourth does not exist.
    engine
        .apply_wait(vec![Op::SetPluginRouting {
            track: TrackId(10),
            slot: SlotId(11),
            outputs: vec![PluginOutput {
                bus: 3,
                target: TrackId(20),
            }],
            sidechain: None,
        }])
        .unwrap();
    restarted(&mut engine, &events, 11);
    // Its second output is now on (everything up to the highest routed is), but goes nowhere,
    // so the note that plays there is no longer heard; nothing else is disturbed.
    let after = play_from_start(&mut engine, 6000);
    assert!(peak(&after[0]) > 0.05);
    assert!(peak(&after[0]) < peak(&before[0]));
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn a_sidechain_reaches_the_plugins_second_input() {
    // Track 10 carries an impulse through the fixture delay (64 samples, reported as latency).
    // The delay adds whatever is on its sidechain to its output, undelayed. Track 20 carries
    // another impulse and is the sidechain source.
    let dir = temp_dir("sidechain");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    let impulse = |level: f32| {
        let mut audio = vec![0.0; 4000];
        audio[1000] = level;
        vec![audio]
    };
    engine
        .apply_wait(vec![
            media(&dir, 1, RATE, impulse(0.25)),
            media(&dir, 2, RATE, impulse(0.125)),
            add_track(0, 10, TrackKind::Audio),
            add_track(1, 20, TrackKind::Audio),
            Op::AddClip {
                track: TrackId(10),
                clip: audio_clip(11, 1, 0, 4000),
            },
            Op::AddClip {
                track: TrackId(20),
                clip: audio_clip(21, 2, 0, 4000),
            },
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(12, ids::DELAY_UID),
            },
            // The key track is not heard itself.
            set(20, TrackChange::Mute(true)),
        ])
        .unwrap();
    loaded(&events, 1);
    engine.sync();
    let key = |source: Option<(u64, bool)>| Op::SetPluginRouting {
        track: TrackId(10),
        slot: SlotId(12),
        outputs: vec![],
        sidechain: source.map(|(track, pre_fader)| Sidechain {
            source: TrackId(track),
            pre_fader,
        }),
    };
    let spikes = |out: &[Vec<f32>]| -> Vec<(usize, f32)> {
        out[0]
            .iter()
            .enumerate()
            .filter(|(_, s)| **s != 0.0)
            .map(|(n, s)| (n, *s))
            .collect()
    };

    // No sidechain: just the delayed impulse. (The engine output is the plan's 64 samples
    // late, and the plugin's own delay is what that compensates: 1000 + 64.)
    assert_eq!(
        spikes(&play_from_start(&mut engine, 4000)),
        vec![(1064, 0.25)]
    );

    // Keyed from before track 20's fader: its impulse arrives although the track is muted. It
    // reaches the plugin at the same moment as the main input does, and the fixture passes it
    // without the delay, so it is 64 samples ahead of the main signal.
    engine.apply_wait(vec![key(Some((20, true)))]).unwrap();
    restarted(&mut engine, &events, 12);
    assert_eq!(engine.status().plan_latency, 64);
    assert_eq!(
        spikes(&play_from_start(&mut engine, 4000)),
        vec![(1000, 0.125), (1064, 0.25)]
    );

    // Keyed from after the fader: a muted track has nothing to say, and its fader counts.
    engine.apply_wait(vec![key(Some((20, false)))]).unwrap();
    engine.sync();
    assert_eq!(
        spikes(&play_from_start(&mut engine, 4000)),
        vec![(1064, 0.25)]
    );
    engine
        .apply_wait(vec![
            set(20, TrackChange::Mute(false)),
            set(20, TrackChange::Gain(0.5)),
        ])
        .unwrap();
    let out = play_from_start(&mut engine, 4000);
    // Direct path: 0.125 x 0.5, delayed 64 by compensation. Sidechain path: the same level,
    // undelayed by the plugin. Main path: 0.25 at 1064, where it meets the direct impulse.
    assert_eq!(spikes(&out), vec![(1000, 0.0625), (1064, 0.25 + 0.0625)]);

    // Bypassing the plugin passes the main input (delayed to stay aligned) and nothing of the
    // sidechain.
    engine
        .apply_wait(vec![
            set(20, TrackChange::Mute(true)),
            Op::SetPluginBypass {
                track: TrackId(10),
                slot: SlotId(12),
                bypass: true,
            },
        ])
        .unwrap();
    assert_eq!(
        spikes(&play_from_start(&mut engine, 4000)),
        vec![(1064, 0.25)]
    );

    // Taking the sidechain away switches the input off again.
    engine
        .apply_wait(vec![
            key(None),
            Op::SetPluginBypass {
                track: TrackId(10),
                slot: SlotId(12),
                bypass: false,
            },
        ])
        .unwrap();
    restarted(&mut engine, &events, 12);
    assert_eq!(
        spikes(&play_from_start(&mut engine, 4000)),
        vec![(1064, 0.25)]
    );
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn notes_from_one_tracks_plugin_play_another_tracks_instrument() {
    // Two notes of a quarter of a second each, the second ending at sample 36,000.
    const FRAMES: usize = 40_000;
    let dir = temp_dir("midiout");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            add_track(0, 10, TrackKind::Instrument),
            add_track(1, 30, TrackKind::Instrument),
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(11, ids::SYNTH_UID),
            },
            Op::InsertPlugin {
                track: TrackId(30),
                index: 0,
                slot: fixture_slot(31, ids::SYNTH_UID),
            },
            Op::AddClip {
                track: TrackId(10),
                clip: midi_clip(
                    12,
                    0,
                    2,
                    vec![note((0, 1), (1, 2), 60), note((1, 1), (1, 2), 67)],
                ),
            },
        ])
        .unwrap();
    loaded(&events, 2);
    engine.sync();

    // Track 30 has no notes of its own: one synth is heard.
    let one = play_from_start(&mut engine, FRAMES);
    assert!(peak(&one[0]) > 0.1);

    // The fixture synth passes the notes it plays to its event output. Feed those to track
    // 30: its synth now plays the same notes on the same samples, so the sum is exactly double.
    let undo = engine
        .apply_wait(vec![set(30, TrackChange::EventsFrom(Some(TrackId(10))))])
        .unwrap();
    let two = play_from_start(&mut engine, FRAMES);
    let doubled: Vec<f32> = one[0].iter().map(|s| s * 2.0).collect();
    assert_eq!(two[0], doubled);
    // Note-offs travel too: after the second note ends, both synths are silent.
    assert!(peak(&two[0][35_000..36_000]) > 0.1);
    let tail = &two[0][36_100..];
    assert_eq!(peak(tail), 0.0, "a note is stuck on the second instrument");

    // Muting the first track silences its own sound but not the notes it passes on.
    engine
        .apply_wait(vec![set(10, TrackChange::Mute(true))])
        .unwrap();
    assert_eq!(play_from_start(&mut engine, FRAMES), one);

    // Undo: one synth again.
    engine
        .apply_wait(vec![set(10, TrackChange::Mute(false))])
        .unwrap();
    engine.apply_wait(undo.inverses).unwrap();
    assert_eq!(play_from_start(&mut engine, FRAMES), one);
    assert_eq!(engine.status().rt_violations, 0);
}

#[test]
fn a_slot_emptied_and_filled_again_at_once_gets_a_working_plugin() {
    // Remove a plugin and put it back before the audio thread has run a single block: the old
    // instance is still held by the plan when the new one is asked for. Each must be loaded
    // and let go of in its own right.
    let dir = temp_dir("refill");
    let mut engine = engine(&dir);
    let events = engine.subscribe();
    engine
        .apply_wait(vec![
            add_track(0, 10, TrackKind::Instrument),
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: fixture_slot(11, ids::SYNTH_UID),
            },
            Op::AddClip {
                track: TrackId(10),
                clip: midi_clip(12, 0, 2, vec![note((0, 1), (1, 1), 60)]),
            },
        ])
        .unwrap();
    loaded(&events, 1);
    engine.sync();
    let before = play_from_start(&mut engine, 8000);
    engine.transport(TransportCommand::Stop);
    engine.sync();

    for _ in 0..3 {
        let removed = engine
            .apply_wait(vec![Op::RemovePlugin {
                track: TrackId(10),
                slot: SlotId(11),
            }])
            .unwrap();
        engine.apply_wait(removed.inverses).unwrap();
    }
    // (Not three "loaded" events: an instance whose slot was emptied again before it
    // finished loading is let go of without ever being announced.)
    engine.sync();
    engine.run(1024);
    engine.sync();
    assert_eq!(play_from_start(&mut engine, 8000), before);
    assert_eq!(engine.status().rt_violations, 0);
}
