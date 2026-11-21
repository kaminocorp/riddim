//! Offline bounce: render a model to a file, faster (or slower) than real time.
//!
//! A bounce runs in an engine of its own, on the manual device, with a fixed block size and no
//! clock. Nothing about it depends on timing, thread scheduling or what the live engine is
//! doing, which is what makes two bounces of the same model identical bit for bit (whenever
//! the plugins themselves are deterministic).
//!
//! The model must carry the plugin state to render with: capture it from the live engine first
//! ([`Engine::capture_plugin_states`](crate::Engine::capture_plugin_states)).

use std::path::PathBuf;

use riddim_disk::wav::{SampleFormat, WavSpec, WavWriter};
use riddim_io::null::NullConfig;
use riddim_model::{Metronome, Model, Op, TrackChange, TrackId};
use riddim_plugin::ProcessMode;
use riddim_time::Position;

use crate::engine::{DeviceConfig, Engine, EngineConfig, MainThreadMode};
use crate::events::Event;
use crate::TransportCommand;

/// What to render and where to.
#[derive(Clone, Debug)]
pub struct BounceRequest {
    pub path: PathBuf,
    pub start: Position,
    pub end: Position,
    /// Extra time after `end`, for reverb and release tails.
    pub tail_seconds: f64,
    pub sample_rate: u32,
    /// Block size to render with. The result does not depend on it for sample-accurate
    /// material, but plugins that work per block will; keep it fixed for repeatable bounces.
    pub block_frames: u32,
    pub format: SampleFormat,
    /// Where relative media paths in the model are relative to.
    pub media_dir: PathBuf,
    /// Render only these tracks (a stem): they are soloed for the bounce. `None` is the mix.
    pub tracks: Option<Vec<TrackId>>,
    /// Threads to render with besides the calling one. The file is the same, bit for bit,
    /// whatever the number; more only makes a large project bounce faster.
    pub render_threads: usize,
}

/// What a bounce produced.
#[derive(Clone, Debug, PartialEq)]
pub struct BounceResult {
    pub path: PathBuf,
    pub frames: u64,
    pub peak: f32,
    /// Plugin latency that was rendered and discarded so the file starts exactly at `start`.
    pub latency_trimmed: u32,
    /// Plugins in the model that could not be loaded; their tracks were rendered without them.
    pub missing_plugins: Vec<String>,
}

/// Renders `model` as `request` describes.
pub fn bounce(model: &Model, request: &BounceRequest) -> Result<BounceResult, String> {
    let rate = request.sample_rate;
    let start = model.tempo_map.position_to_samples(request.start, rate);
    let end = model.tempo_map.position_to_samples(request.end, rate);
    if end <= start {
        return Err("a bounce must end after it starts".into());
    }
    let tail = (request.tail_seconds.max(0.0) * rate as f64) as i64;

    // A copy of the model set up for rendering: no loop, no click, nothing armed, and the
    // requested stem soloed.
    let mut model = model.clone();
    let mut ops = vec![
        Op::SetLoop {
            range: model.loop_range,
            enabled: false,
        },
        Op::SetMetronome {
            metronome: Metronome {
                enabled: false,
                ..model.metronome
            },
        },
    ];
    for track in &model.tracks {
        if track.record_armed {
            ops.push(Op::SetTrack {
                id: track.id,
                change: TrackChange::RecordArmed(false),
            });
        }
        if track.monitor {
            ops.push(Op::SetTrack {
                id: track.id,
                change: TrackChange::Monitor(false),
            });
        }
        if let Some(stem) = &request.tracks {
            ops.push(Op::SetTrack {
                id: track.id,
                change: TrackChange::Solo(stem.contains(&track.id)),
            });
        }
    }
    model
        .apply_all(&ops)
        .map_err(|(_, error)| error.to_string())?;

    let directory = request.path.parent().map(PathBuf::from).unwrap_or_default();
    let mut engine = Engine::start(EngineConfig {
        device: DeviceConfig::Manual(NullConfig {
            sample_rate: rate as f64,
            buffer_frames: request.block_frames,
            input_channels: 0,
            output_channels: 2,
            ..NullConfig::default()
        }),
        capture_dir: if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory
        },
        media_dir: request.media_dir.clone(),
        main_thread: MainThreadMode::Internal,
        butler: Default::default(),
        midi_input: false,
        process_mode: ProcessMode::Offline,
        streaming: Default::default(),
        bridge_executable: None,
        render_threads: Some(request.render_threads),
        // Nothing is waiting for a bounce to make its deadline, so even a small plan is
        // shared out when threads were asked for.
        parallel_from: 1,
    })
    .map_err(|e| e.to_string())?;
    let events = engine.subscribe();
    engine.load(model)?;
    engine.sync();
    let mut missing_plugins = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::PluginFailed { slot, error, .. } = event {
            missing_plugins.push(format!("slot {}: {error}", slot.0));
        }
    }

    engine.transport(TransportCommand::Locate(Position::Time(
        riddim_time::samples_to_superclock(start, rate),
    )));
    engine.transport(TransportCommand::Play);
    engine.sync();

    let spec = WavSpec {
        sample_rate: rate,
        channels: 2,
        format: request.format,
    };
    let mut writer = WavWriter::create(&request.path, spec).map_err(|e| e.to_string())?;
    // The first block tells us how much latency compensation the plan has; that much is
    // rendered beyond the end and dropped from the front, so the file lines up with the
    // timeline.
    let block = request.block_frames.max(1) as usize;
    let mut latency = None;
    let mut to_skip = 0usize;
    let mut remaining = (end - start + tail) as usize;
    let mut peak = 0.0f32;
    let mut frames = 0u64;
    while remaining > 0 {
        let audio = engine.run(block);
        if latency.is_none() {
            let plan_latency = engine.status().plan_latency;
            latency = Some(plan_latency);
            to_skip = plan_latency as usize;
        }
        let rendered = audio.first().map_or(0, Vec::len);
        let discard = to_skip.min(rendered);
        to_skip -= discard;
        let take = (rendered - discard).min(remaining);
        if take == 0 {
            continue;
        }
        let channels: Vec<&[f32]> = audio.iter().map(|c| &c[discard..discard + take]).collect();
        for channel in &channels {
            for sample in channel.iter() {
                peak = peak.max(sample.abs());
            }
        }
        writer.write_planar(&channels).map_err(|e| e.to_string())?;
        frames += take as u64;
        remaining -= take;
    }
    writer.finish().map_err(|e| e.to_string())?;
    Ok(BounceResult {
        path: request.path.clone(),
        frames,
        peak,
        latency_trimmed: latency.unwrap_or(0),
        missing_plugins,
    })
}
