//! A small "demo song" through plugins that are not part of this repository.
//!
//! Set `RIDDIM_TEST_VST3` to one or more `.vst3` bundle paths (separated by `;`). The first
//! instrument found plays a part, the first effect found sits on a bus fed by a send, the
//! instrument's fader is automated, and the whole thing is bounced. `scripts/ci.sh` points this
//! at Surge XT on the build machine. Without the variable the test passes vacuously and says so.
//!
//! This is the stand-in for the vision's "a demo song with all four plugins, bounced": the four
//! commercial plugins it names have not been run by anyone yet.

mod common;

use std::sync::Arc;

use common::*;
use riddim_disk::wav::{self, SampleFormat};
use riddim_engine::{bounce, BounceRequest, Event, TransportCommand};
use riddim_model::time::{Position, Ticks};
use riddim_model::{
    AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, CurveShape, LaneId, Model,
    Op, PluginRef, PluginSlot, Send, SendId, SlotId, TrackId, TrackKind,
};
use riddim_plugin::scan::{ScanOutcome, ScanTarget, Scanner};
use riddim_plugin::{PluginDescriptor, PluginKind};
use riddim_rt::guard::RtCheckAllocator;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

fn descriptors(format: &str) -> Vec<PluginDescriptor> {
    // CLAP plugins are preferred when both kinds are named: the VST3 builds of the same
    // plugins are exercised by the conformance run in any case.
    let named = |variable: &str| -> Vec<std::path::PathBuf> {
        std::env::var(variable)
            .map(|v| {
                v.split(';')
                    .filter(|s| !s.is_empty())
                    .map(Into::into)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut bundles = named(&format!("RIDDIM_TEST_{}", format.to_uppercase()));
    bundles.retain(|path: &std::path::PathBuf| path.exists());
    let scanner = Scanner::new(riddim_test_support::scanner_exe());
    let mut found = Vec::new();
    for bundle in bundles {
        let target = ScanTarget::for_path(bundle.clone());
        match scanner.scan(&target) {
            ScanOutcome::Ok(plugins) => found.extend(plugins),
            other => panic!("{} did not scan: {other:?}", bundle.display()),
        }
    }
    found
}

fn slot(id: u64, descriptor: &PluginDescriptor) -> PluginSlot {
    PluginSlot {
        id: SlotId(id),
        plugin: PluginRef {
            format: descriptor.id.format.as_str().to_string(),
            uid: descriptor.id.uid.clone(),
            name: descriptor.name.clone(),
            path: descriptor
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        },
        state: None,
        bypass: false,
        outputs: Vec::new(),
        sidechain: None,
        bridged: false,
    }
}

fn point(beat: i64, value: f32) -> AutomationPoint {
    AutomationPoint {
        at: Ticks::from_beats(beat),
        value,
        shape: CurveShape::Linear,
    }
}

#[test]
fn a_demo_song_through_third_party_plugins_bounces() {
    demo_song("vst3");
}

#[test]
fn a_demo_song_through_third_party_clap_plugins_bounces() {
    demo_song("clap");
}

fn demo_song(format: &str) {
    let found = descriptors(format);
    let instrument = found.iter().find(|d| d.kind == PluginKind::Instrument);
    let effect = found.iter().find(|d| d.kind == PluginKind::Effect);
    let (Some(instrument), Some(effect)) = (instrument, effect) else {
        eprintln!(
            "RIDDIM_TEST_{} does not name an instrument and an effect: no demo song was bounced",
            format.to_uppercase()
        );
        return;
    };
    eprintln!(
        "demo song ({format}): {} into {}",
        instrument.name, effect.name
    );

    // Build the song in a live engine, as an application would.
    let dir = temp_dir(&format!("demo-song-{format}"));
    let mut engine = engine_with(
        &dir,
        riddim_engine::NullConfig {
            buffer_frames: 256,
            ..Default::default()
        },
    );
    let events = engine.subscribe();
    let notes = (0..8)
        .map(|i| note((i, 1), (1, 2), [48, 55, 60, 63, 67, 63, 60, 55][i as usize]))
        .collect();
    engine
        .apply_wait(vec![
            add_track(0, 10, TrackKind::Instrument),
            Op::InsertPlugin {
                track: TrackId(10),
                index: 0,
                slot: slot(11, instrument),
            },
            Op::AddClip {
                track: TrackId(10),
                clip: midi_clip(12, 0, 8, notes),
            },
            add_track(1, 20, TrackKind::Bus),
            Op::InsertPlugin {
                track: TrackId(20),
                index: 0,
                slot: slot(21, effect),
            },
            Op::AddSend {
                track: TrackId(10),
                index: 0,
                send: Send {
                    id: SendId(30),
                    target: TrackId(20),
                    gain: 0.5,
                    pre_fader: false,
                    mute: false,
                },
            },
            Op::AddLane {
                track: TrackId(10),
                lane: AutomationLane {
                    id: LaneId(40),
                    target: AutomationTarget::TrackGain,
                    mode: AutomationMode::Read,
                    points: Arc::new(vec![point(0, 0.25), point(4, 1.0), point(8, 0.5)]),
                },
            },
        ])
        .unwrap();
    for _ in 0..2 {
        wait_for(&events, "both plugins to load", |e| match e {
            Event::PluginLoaded { .. } => Some(()),
            Event::PluginFailed { error, .. } => panic!("a plugin failed to load: {error}"),
            _ => None,
        });
    }
    engine.sync();

    // It plays, and nothing of ours touches the heap on the audio path while it does.
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    let live = engine.run(2 * RATE as usize);
    assert!(
        peak(&live[0]) > 0.001,
        "the song is silent when played live"
    );
    assert_eq!(engine.status().rt_violations, 0);
    engine.transport(TransportCommand::Stop);

    // Routing that makes the engine switch the plugins' optional buses on, which means
    // restarting them: the instrument's second output to the effect bus, and each plugin's
    // sidechain input (if it has one) fed from a third track. Whatever the plugins make of
    // it, they must come back, and the song must still play.
    let instrument_layout = riddim_plugin::host::instantiate(instrument)
        .map(|plugin| plugin.bus_layout())
        .unwrap();
    let effect_layout = riddim_plugin::host::instantiate(effect)
        .map(|plugin| plugin.bus_layout())
        .unwrap();
    let has_aux_in = |layout: &riddim_plugin::BusLayout| {
        layout
            .audio_inputs
            .iter()
            .any(|bus| bus.kind == riddim_plugin::BusKind::Aux)
    };
    let mut restarts = 0;
    let mut routing = vec![add_track(2, 50, TrackKind::Audio)];
    let second_output = instrument_layout.audio_outputs.len() > 1;
    if second_output || has_aux_in(&instrument_layout) {
        restarts += 1;
        routing.push(Op::SetPluginRouting {
            track: TrackId(10),
            slot: SlotId(11),
            outputs: second_output
                .then_some(riddim_model::PluginOutput {
                    bus: 1,
                    target: TrackId(20),
                })
                .into_iter()
                .collect(),
            sidechain: has_aux_in(&instrument_layout).then_some(riddim_model::Sidechain {
                source: TrackId(50),
                pre_fader: true,
            }),
        });
    }
    if has_aux_in(&effect_layout) {
        restarts += 1;
        routing.push(Op::SetPluginRouting {
            track: TrackId(20),
            slot: SlotId(21),
            outputs: vec![],
            sidechain: Some(riddim_model::Sidechain {
                source: TrackId(50),
                pre_fader: false,
            }),
        });
    }
    engine.apply_wait(routing).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while restarts > 0 {
        engine.run(256);
        while let Ok(event) = events.try_recv() {
            match event {
                Event::PluginRestarted { .. } => restarts -= 1,
                Event::PluginFailed { error, .. } => panic!("a plugin did not restart: {error}"),
                _ => {}
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the plugins did not come back"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    engine.sync();
    engine.transport(TransportCommand::Locate(Position::START));
    engine.transport(TransportCommand::Play);
    engine.sync();
    let rerouted = engine.run(2 * RATE as usize);
    assert!(
        peak(&rerouted[0]) > 0.001,
        "the song is silent after rerouting"
    );
    assert!(rerouted.iter().flatten().all(|s| s.is_finite()));
    assert_eq!(engine.status().rt_violations, 0);
    engine.transport(TransportCommand::Stop);
    eprintln!(
        "demo song: rerouted (second output: {second_output}, instrument sidechain: {}, effect sidechain: {}), peak {:.4}",
        has_aux_in(&instrument_layout),
        has_aux_in(&effect_layout),
        peak(&rerouted[0])
    );

    // Its plugins' states go into the model, and the model survives the trip through a file.
    let applied = engine.capture_plugin_states().unwrap();
    assert_eq!(
        applied.inverses.len(),
        2,
        "both plugins must hand over a state"
    );
    let saved = engine.model().to_json();
    drop(engine);
    let model = Model::from_json(&saved).unwrap();
    for (track, name) in [(10, &instrument.name), (20, &effect.name)] {
        let state = &model.track(TrackId(track)).unwrap().plugins[0].state;
        assert!(
            state.as_ref().is_some_and(|s| !s.is_empty()),
            "{name} saved no state"
        );
    }

    // Bounce the saved project, twice. (Bit-identity is tested with our own plugins: Surge XT,
    // for one, does not render the same audio twice. See decision D38.)
    let request = |name: &str, tracks: Option<Vec<TrackId>>| BounceRequest {
        path: dir.join(name),
        start: Position::START,
        end: Position::Beats(Ticks::from_beats(8)),
        tail_seconds: 1.0,
        sample_rate: RATE,
        block_frames: 256,
        format: SampleFormat::Float32,
        media_dir: dir.clone(),
        tracks,
        render_threads: 0,
    };
    let first = bounce(&model, &request("mix.wav", None)).unwrap();
    let second = bounce(&model, &request("mix-again.wav", None)).unwrap();
    for result in [&first, &second] {
        assert!(
            result.missing_plugins.is_empty(),
            "{:?}",
            result.missing_plugins
        );
        assert!(result.peak > 0.001, "the bounce is silent");
        assert!(
            result.peak.is_finite() && result.peak < 8.0,
            "peak {}",
            result.peak
        );
    }
    // 8 beats at 120 bpm is 4 s, plus the 1 s tail.
    assert_eq!(first.frames, 5 * RATE as u64);
    assert_eq!(second.frames, first.frames);
    let audio = wav::read(&first.path).unwrap();
    assert_eq!(audio.channels.len(), 2);
    assert_eq!(audio.channels[0].len() as u64, first.frames);
    assert!(audio.channels.iter().flatten().all(|s| s.is_finite()));

    // The effect bus alone, as a stem: fed only by the send, so quieter than the mix but there.
    let stem = bounce(&model, &request("bus.wav", Some(vec![TrackId(20)]))).unwrap();
    assert!(
        stem.peak > 0.0,
        "nothing reached the effect bus through the send"
    );
    eprintln!(
        "demo song: mix peak {:.4}, second bounce peak {:.4}, bus stem peak {:.4}, {} frames, {} frames of latency trimmed",
        first.peak, second.peak, stem.peak, first.frames, first.latency_trimmed
    );
}
