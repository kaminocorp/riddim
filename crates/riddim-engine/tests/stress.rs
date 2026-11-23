//! The stress run: a busy project on a running device at a 64-frame buffer, with render
//! threads, while the graph is churned, the buffer size changed, the transport thrown about,
//! a long file streamed from a disk made slow on purpose and a recording made to a disk
//! made slower.
//!
//! What it checks is what the engine promises under pressure: the audio callback keeps being
//! served at its pace, nothing on the audio path touches the heap or blocks, no message is
//! lost, and the project is still a valid one at the end. It runs for a few seconds in the
//! ordinary test run; `scripts/stress.sh` runs it for minutes, under RealtimeSanitizer.

mod common;

use std::time::{Duration, Instant};

use common::*;
use riddim_disk::capture::ButlerConfig;
use riddim_engine::{
    DeviceConfig, Engine, EngineConfig, Event, NullConfig, StreamingConfig, TransportCommand,
};
use riddim_model::time::Position;
use riddim_model::{
    LoopRange, Op, PluginOutput, Send, SendId, Sidechain, SlotId, TrackChange, TrackId, TrackInput,
    TrackKind,
};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// xorshift: the same sequence of edits every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn seconds() -> u64 {
    std::env::var("RIDDIM_STRESS_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
}

#[test]
fn the_engine_holds_up_under_churn() {
    let dir = temp_dir("stress");
    let mut model = wide_project(&dir);
    // Five more plugins (34 in all), a long file to stream, and a track to record on.
    let mut extra = Vec::new();
    for n in 0..5u64 {
        extra.push(Op::InsertPlugin {
            track: TrackId(900 + n % 3),
            index: 0,
            slot: fixture_slot(9500 + n, ids::GAIN_UID),
        });
    }
    extra.push(media(&dir, 7000, RATE, vec![seeded_noise(99, 960_000)]));
    extra.push(add_track(model.tracks.len(), 700, TrackKind::Audio));
    extra.push(Op::AddClip {
        track: TrackId(700),
        clip: audio_clip(7001, 7000, 0, 960_000),
    });
    extra.push(add_track(model.tracks.len() + 1, 710, TrackKind::Audio));
    extra.push(Op::SetTrack {
        id: TrackId(710),
        change: TrackChange::Input(TrackInput::Audio {
            first_channel: 0,
            channels: 2,
        }),
    });
    model.apply_all(&extra).unwrap();
    let plugins = model.all_tracks().map(|t| t.plugins.len()).sum::<usize>();
    assert!(
        plugins >= 30,
        "the stress project has only {plugins} plugins"
    );

    let mut config = EngineConfig::manual(&dir);
    config.device = DeviceConfig::Null(NullConfig {
        buffer_frames: 64,
        // The output comes back on the input, so the recording has something in it.
        loopback_delay: Some(256),
        ..NullConfig::default()
    });
    config.render_threads = Some(3);
    config.parallel_from = 1;
    // A slow disk for playback: each page takes longer to read than it lasts.
    config.streaming = StreamingConfig {
        threshold_frames: 200_000,
        page_frames: 2048,
        ahead_seconds: 0.3,
        behind_seconds: 0.05,
        read_delay: Some(Duration::from_millis(60)),
    };
    // And a slower one for recording: a ring of a tenth of a second, emptied twice a second.
    config.butler = ButlerConfig {
        poll_interval: Duration::from_millis(500),
        ring_seconds: 0.1,
        ..ButlerConfig::default()
    };
    let mut engine = Engine::start(config).unwrap();
    let events = engine.subscribe();
    engine.load(model).unwrap();
    engine.sync();
    engine.transport(TransportCommand::Play);
    engine.sync();

    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    let started = Instant::now();
    let duration = Duration::from_secs(seconds());
    let before = engine.status();
    let (mut edits, mut rejected, mut buffer_changes, mut takes) = (0u64, 0u64, 0u64, 0u64);
    let mut longest_gap = Duration::ZERO;
    let mut last_progress = (Instant::now(), before.blocks);
    let mut recording = false;
    let mut next_id = 100_000u64;

    while started.elapsed() < duration {
        let audio_track = TrackId(100 + rng.below(16) * 10);
        let bus = TrackId(900 + rng.below(3));
        let set = |id: TrackId, change: TrackChange| Op::SetTrack { id, change };
        next_id += 1;
        let ops: Vec<Op> = match rng.below(16) {
            0 => vec![set(audio_track, TrackChange::Mute(rng.below(2) == 0))],
            1 => vec![set(audio_track, TrackChange::Solo(rng.below(4) == 0))],
            2 => vec![set(
                audio_track,
                TrackChange::Gain(rng.below(100) as f32 / 100.0),
            )],
            3 => vec![set(
                audio_track,
                TrackChange::Pan(rng.below(200) as f32 / 100.0 - 1.0),
            )],
            4 => vec![Op::SetPluginBypass {
                track: audio_track,
                slot: SlotId(audio_track.0 + 2),
                bypass: rng.below(2) == 0,
            }],
            // A plugin taken out and put straight back, and one added for good.
            5 => vec![Op::InsertPlugin {
                track: bus,
                index: 0,
                slot: fixture_slot(next_id, ids::GAIN_UID),
            }],
            6 => {
                let track = engine.model().track(bus).cloned();
                match track.and_then(|t| t.plugins.first().map(|p| p.id)) {
                    Some(slot) if slot != SlotId(9010) => {
                        vec![Op::RemovePlugin { track: bus, slot }]
                    }
                    _ => vec![],
                }
            }
            7 => vec![Op::AddSend {
                track: audio_track,
                index: 0,
                send: Send {
                    id: SendId(next_id),
                    target: bus,
                    gain: 0.3,
                    pre_fader: rng.below(2) == 0,
                    mute: false,
                },
            }],
            8 => {
                let track = engine.model().track(audio_track).cloned();
                match track.and_then(|t| t.sends.first().map(|s| s.id)) {
                    Some(send) => vec![Op::RemoveSend {
                        track: audio_track,
                        send,
                    }],
                    None => vec![],
                }
            }
            // Routing that restarts plugins: the multi-out instrument's second output on
            // and off, and the sidechain's source moved.
            9 => vec![Op::SetPluginRouting {
                track: TrackId(500),
                slot: SlotId(502),
                outputs: (rng.below(2) == 0)
                    .then_some(PluginOutput {
                        bus: 1,
                        target: TrackId(902),
                    })
                    .into_iter()
                    .collect(),
                sidechain: None,
            }],
            10 => vec![Op::SetPluginRouting {
                track: TrackId(901),
                slot: SlotId(9010),
                outputs: vec![],
                sidechain: (rng.below(3) != 0).then_some(Sidechain {
                    source: audio_track,
                    pre_fader: rng.below(2) == 0,
                }),
            }],
            11 => vec![Op::SetLoop {
                range: Some(LoopRange {
                    start: at_sample(rng.below(40_000) as i64),
                    end: at_sample(48_000 + rng.below(40_000) as i64),
                }),
                enabled: rng.below(2) == 0,
            }],
            // A whole track with a plugin on it, and (sometimes) gone again.
            12 => vec![
                add_track(0, next_id, TrackKind::Audio),
                Op::InsertPlugin {
                    track: TrackId(next_id),
                    index: 0,
                    slot: fixture_slot(next_id + 500_000, ids::DELAY_UID),
                },
            ],
            13 => {
                let model = engine.model();
                let spare = model.tracks.iter().find(|t| t.id.0 > 100_000).map(|t| t.id);
                spare
                    .map(|id| vec![Op::RemoveTrack { id }])
                    .unwrap_or_default()
            }
            // Not edits: the transport, the device, recording.
            14 => {
                match rng.below(4) {
                    0 => engine.transport(TransportCommand::Locate(at_sample(
                        rng.below(900_000) as i64
                    ))),
                    1 => engine.transport(TransportCommand::Stop),
                    _ => engine.transport(TransportCommand::Play),
                }
                vec![]
            }
            _ => {
                if recording {
                    engine.transport(TransportCommand::Stop);
                    engine.transport(TransportCommand::Play);
                    recording = false;
                } else if rng.below(3) == 0 {
                    // (Refused while recording, which is one more thing to survive.)
                    let frames = [32u32, 64, 128, 256][rng.below(4) as usize];
                    if engine.set_buffer_size(frames).is_ok() {
                        buffer_changes += 1;
                    }
                } else {
                    engine
                        .apply_wait(vec![set(TrackId(710), TrackChange::RecordArmed(true))])
                        .ok();
                    engine.transport(TransportCommand::Record);
                    recording = true;
                }
                vec![]
            }
        };
        if !ops.is_empty() {
            match engine.apply_wait(ops) {
                Ok(_) => edits += 1,
                // Some of these cannot apply to the model as it happens to be (a send that
                // would loop, a track something listens to): refusing is the right answer.
                Err(_) => rejected += 1,
            }
        }
        while let Ok(event) = events.try_recv() {
            match event {
                Event::RecordingFinished { .. } => takes += 1,
                Event::PluginFailed { error, .. } => panic!("a plugin failed: {error}"),
                _ => {}
            }
        }
        let blocks = engine.status().blocks;
        if blocks != last_progress.1 {
            last_progress = (Instant::now(), blocks);
        }
        longest_gap = longest_gap.max(last_progress.0.elapsed());
        std::thread::sleep(Duration::from_millis(1 + rng.below(4)));
    }

    // One recording long enough to overrun the starved disk: the ring holds a tenth of a
    // second and is emptied twice a second. (First let whatever take the churn left
    // unfinished be put away.)
    engine.transport(TransportCommand::Stop);
    engine.sync();
    let dropped_before = engine.status().capture_dropped;
    engine
        .apply_wait(vec![Op::SetTrack {
            id: TrackId(710),
            change: TrackChange::RecordArmed(true),
        }])
        .unwrap();
    while events.try_recv().is_ok() {}
    engine.transport(TransportCommand::Record);
    wait_for(&events, "the last recording to start", |e| {
        matches!(e, Event::RecordingStarted { .. }).then_some(())
    });
    std::thread::sleep(Duration::from_millis(700));
    let recording_blocks = engine.status().blocks;
    std::thread::sleep(Duration::from_millis(100));
    // (A tenth of a second is 18 blocks at the largest buffer size the churn may have left.)
    assert!(
        engine.status().blocks > recording_blocks + 8,
        "the audio callback waited for the disk while recording"
    );
    engine.transport(TransportCommand::Stop);
    let mut overflow_reported = false;
    wait_for(&events, "the last take to be finished", |e| match e {
        Event::RecordingOverflow { .. } => {
            overflow_reported = true;
            None
        }
        Event::RecordingFinished { .. } => Some(()),
        _ => None,
    });
    takes += 1;
    assert!(overflow_reported, "the overrun was not reported");
    assert!(
        engine.status().capture_dropped > dropped_before,
        "the starved disk dropped nothing"
    );

    engine.sync();
    std::thread::sleep(Duration::from_millis(200));
    let after = engine.status();
    assert!(after.playback_starved > 0, "the slow disk starved nothing");
    eprintln!(
        "stress: {} s, {edits} edits applied, {rejected} refused, {buffer_changes} buffer-size changes, \
         {takes} takes; {} blocks, longest gap between blocks {longest_gap:?}, {} late blocks, \
         {} frames starved on playback, {} frames dropped on capture, dsp load {:.2}",
        duration.as_secs(),
        after.blocks - before.blocks,
        after.xruns,
        after.playback_starved,
        after.capture_dropped,
        after.dsp_load,
    );

    // The promises.
    assert_eq!(after.rt_violations, 0, "the audio path touched the heap");
    assert_eq!(
        after.events_lost, 0,
        "messages from the audio thread were lost"
    );
    assert!(edits > 20, "the churn did not get going: {edits} edits");
    // The device was served throughout. (Each buffer-size change pauses the device for a
    // moment by design; nothing else may.)
    assert!(
        longest_gap < Duration::from_millis(500),
        "the audio callback went unserved for {longest_gap:?}"
    );
    let served = (after.blocks - before.blocks) as f64;
    // At least a third of the blocks a 256-frame buffer (the largest used) would have run.
    let floor = duration.as_secs_f64() * 48_000.0 / 256.0 / 3.0;
    assert!(served > floor, "only {served} blocks ran in {duration:?}");
    // The project came through as a project.
    engine.model().validate().unwrap();
    // And the engine still does what it is told.
    engine
        .apply_wait(vec![Op::SetLoop {
            range: None,
            enabled: false,
        }])
        .unwrap();
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    std::thread::sleep(Duration::from_millis(200));
    assert!(engine.status().position_samples > 0);
}
