//! Rendering with worker threads: the result must be the one a single thread gives, bit for
//! bit, whatever the number of threads and however the steps happen to be shared out.

mod common;

use common::*;
use riddim_disk::wav::SampleFormat;
use riddim_engine::{
    bounce, BounceRequest, DeviceConfig, Engine, EngineConfig, Event, NullConfig, TransportCommand,
    TransportState,
};
use riddim_model::time::Position;
use riddim_model::{Model, Op, SlotId, TrackChange, TrackId};
use riddim_rt::guard::RtCheckAllocator;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

fn request(dir: &std::path::Path, name: &str, render_threads: usize) -> BounceRequest {
    BounceRequest {
        path: dir.join(name),
        start: Position::START,
        end: at_sample(72_000),
        tail_seconds: 0.25,
        sample_rate: RATE,
        block_frames: 64,
        format: SampleFormat::Float32,
        media_dir: dir.to_path_buf(),
        tracks: None,
        render_threads,
    }
}

#[test]
fn parallel_and_single_threaded_bounces_are_bit_identical() {
    // Phase 4 exit criterion 3.
    let dir = temp_dir("parallel-bounce");
    let model = wide_project(&dir);
    let single = bounce(&model, &request(&dir, "one.wav", 0)).unwrap();
    assert!(
        single.missing_plugins.is_empty(),
        "{:?}",
        single.missing_plugins
    );
    assert!(
        single.peak > 0.2,
        "the project is too quiet to prove anything: {}",
        single.peak
    );
    assert_eq!(
        single.latency_trimmed, 64,
        "the project has compensated latency in it"
    );
    let reference = std::fs::read(&single.path).unwrap();

    // Several thread counts, each several times: which thread runs which step differs from
    // run to run, and none of it may show.
    for threads in [1usize, 2, 3, 7] {
        for run in 0..4 {
            let name = format!("threads-{threads}-{run}.wav");
            let result = bounce(&model, &request(&dir, &name, threads)).unwrap();
            assert_eq!((result.frames, result.peak), (single.frames, single.peak));
            assert!(
                std::fs::read(&result.path).unwrap() == reference,
                "the bounce with {threads} render thread(s) (run {run}) differs from the single-threaded one"
            );
            std::fs::remove_file(&result.path).ok();
        }
    }
    // A stem, too: solo changes which paths are open, not how they are computed.
    let stem = |threads: usize, name: &str| {
        let mut request = request(&dir, name, threads);
        request.tracks = Some(vec![TrackId(901), TrackId(500)]);
        std::fs::read(bounce(&model, &request).unwrap().path).unwrap()
    };
    assert!(stem(0, "stem-one.wav") == stem(5, "stem-five.wav"));
}

fn threaded_engine(dir: &std::path::Path, threads: usize, device: DeviceConfig) -> Engine {
    let mut config = EngineConfig::manual(dir);
    config.device = device;
    config.render_threads = Some(threads);
    config.parallel_from = 1;
    Engine::start(config).unwrap()
}

fn loaded(engine: &Engine, model: &Model) {
    engine.load(model.clone()).unwrap();
    engine.sync();
}

#[test]
fn a_live_engine_with_render_threads_plays_what_one_thread_plays() {
    let dir = temp_dir("parallel-live");
    let model = wide_project(&dir);
    let manual = || {
        DeviceConfig::Manual(NullConfig {
            buffer_frames: 64,
            ..NullConfig::default()
        })
    };
    let play = |engine: &mut Engine| {
        engine.transport(TransportCommand::Locate(Position::START));
        engine.transport(TransportCommand::Play);
        engine.sync();
        let mut out = engine.run(20_000);
        // Edits while playing swap the plan under the workers: a fader, a mute, a bypass, and
        // a track removed and put back.
        let removed = engine
            .apply_wait(vec![
                Op::SetTrack {
                    id: TrackId(120),
                    change: TrackChange::Gain(0.75),
                },
                Op::SetTrack {
                    id: TrackId(900),
                    change: TrackChange::Mute(true),
                },
                Op::SetPluginBypass {
                    track: TrackId(100),
                    slot: SlotId(103),
                    bypass: true,
                },
                Op::RemoveTrack { id: TrackId(250) },
            ])
            .unwrap();
        for channel in engine.run(20_000) {
            out.push(channel);
        }
        engine.apply_wait(removed.inverses).unwrap();
        engine.sync();
        for channel in engine.run(20_000) {
            out.push(channel);
        }
        engine.transport(TransportCommand::Stop);
        engine.sync();
        out
    };

    let mut one = threaded_engine(&dir, 0, manual());
    loaded(&one, &model);
    let reference = play(&mut one);
    assert!(peak(&reference[0]) > 0.1);
    assert_eq!(one.status().rt_violations, 0);
    drop(one);

    for threads in [1usize, 4] {
        let mut engine = threaded_engine(&dir, threads, manual());
        loaded(&engine, &model);
        let out = play(&mut engine);
        assert!(
            out == reference,
            "with {threads} render thread(s) the engine plays differently"
        );
        // The workers' share of the work is checked for heap traffic like the audio thread's.
        assert_eq!(engine.status().rt_violations, 0);
    }
}

#[test]
fn render_threads_keep_up_with_a_running_device_and_shut_down_cleanly() {
    // The timer-driven device, small blocks, workers that park between blocks and are woken
    // for each one. This is about liveness: nothing hangs, the transport runs, and dropping
    // the engine joins every thread.
    let dir = temp_dir("parallel-null");
    let model = wide_project(&dir);
    let mut engine = threaded_engine(
        &dir,
        3,
        DeviceConfig::Null(NullConfig {
            buffer_frames: 64,
            ..NullConfig::default()
        }),
    );
    let events = engine.subscribe();
    loaded(&engine, &model);
    engine.transport(TransportCommand::Play);
    engine.sync();
    let before = engine.status();
    // Churn the graph while it runs.
    for round in 0..20 {
        engine
            .apply_wait(vec![Op::SetTrack {
                id: TrackId(100 + (round % 16) * 10),
                change: TrackChange::Mute(round % 2 == 0),
            }])
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // A buffer-size change restarts every plugin under the workers' feet.
    engine.set_buffer_size(128).unwrap();
    let mut restarted = 0;
    while restarted < 29 {
        wait_for(&events, "the plugins to come back", |e| match e {
            Event::PluginRestarted { .. } => Some(()),
            Event::PluginFailed { error, .. } => panic!("a plugin did not restart: {error}"),
            _ => None,
        });
        restarted += 1;
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let after = engine.status();
    assert_eq!(after.transport, TransportState::Playing);
    assert!(
        after.blocks > before.blocks + 50,
        "the device stopped being served"
    );
    assert!(after.position_samples > before.position_samples);
    assert_eq!(after.rt_violations, 0);
    drop(engine);
}
