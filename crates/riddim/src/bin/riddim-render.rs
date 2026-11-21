//! `riddim-render`: bounce a project to a WAV file, with no device, no window and no terminal.
//!
//! The input is an engine model as JSON (what `Engine::model().to_json()` or
//! `riddim_model_json` gives, with plugin states captured). The output is the file, and one
//! line of JSON on standard output describing it. Meant for build machines and render
//! queues; it is the offline bounce and nothing else.
//!
//! ```text
//! riddim-render <model.json|-> --out <file.wav> [options]
//!   --media-dir <dir>     where relative media paths are relative to (default: the model's directory)
//!   --start <beats>       default 0
//!   --end <beats>         default: the end of the last clip
//!   --seconds <seconds>   length from the start, instead of --end
//!   --tail <seconds>      extra time for reverb and release tails (default 0)
//!   --rate <hz>           default 48000
//!   --block <frames>      default 512
//!   --format <f>          float32 (default), pcm24 or pcm16
//!   --stem <track id>     render only this track; repeatable
//!   --threads <n>         extra render threads (the result is the same whatever the number)
//! ```
//!
//! Exit status: 0 on success, 1 if the bounce was made but a plugin could not be loaded
//! (the file exists; the JSON lists what was missing), 2 on any error.

use std::path::PathBuf;
use std::process::ExitCode;

use riddim::files::wav::SampleFormat;
use riddim::model::{ClipContent, Model, TrackId};
use riddim::time::{samples_to_superclock, Position, Ticks, TICKS_PER_BEAT};
use riddim::{bounce, BounceRequest};

struct Options {
    model: String,
    out: PathBuf,
    media_dir: Option<PathBuf>,
    start: f64,
    end: Option<f64>,
    seconds: Option<f64>,
    tail: f64,
    rate: u32,
    block: u32,
    format: SampleFormat,
    stems: Vec<u64>,
    threads: usize,
}

const USAGE: &str = "usage: riddim-render <model.json|-> --out <file.wav> [--media-dir DIR] \
[--start BEATS] [--end BEATS | --seconds S] [--tail S] [--rate HZ] [--block FRAMES] \
[--format float32|pcm24|pcm16] [--stem TRACK]... [--threads N]";

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        model: String::new(),
        out: PathBuf::new(),
        media_dir: None,
        start: 0.0,
        end: None,
        seconds: None,
        tail: 0.0,
        rate: 48_000,
        block: 512,
        format: SampleFormat::Float32,
        stems: Vec::new(),
        threads: 0,
    };
    fn number<T: std::str::FromStr>(flag: &str, value: Option<String>) -> Result<T, String> {
        let value = value.ok_or_else(|| format!("{flag} needs a value"))?;
        value
            .parse()
            .map_err(|_| format!("'{value}' is not a valid value for {flag}"))
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" | "-o" => options.out = number::<String>(&arg, args.next())?.into(),
            "--media-dir" => options.media_dir = Some(number::<String>(&arg, args.next())?.into()),
            "--start" => options.start = number(&arg, args.next())?,
            "--end" => options.end = Some(number(&arg, args.next())?),
            "--seconds" => options.seconds = Some(number(&arg, args.next())?),
            "--tail" => options.tail = number(&arg, args.next())?,
            "--rate" => options.rate = number(&arg, args.next())?,
            "--block" => options.block = number(&arg, args.next())?,
            "--threads" => options.threads = number(&arg, args.next())?,
            "--stem" => options.stems.push(number(&arg, args.next())?),
            "--format" => {
                options.format = match number::<String>(&arg, args.next())?.as_str() {
                    "float32" => SampleFormat::Float32,
                    "pcm24" => SampleFormat::Pcm24,
                    "pcm16" => SampleFormat::Pcm16,
                    other => return Err(format!("unknown format '{other}'")),
                }
            }
            "--help" | "-h" => return Err(USAGE.to_string()),
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ if options.model.is_empty() => options.model = arg,
            _ => return Err(format!("unexpected argument '{arg}'")),
        }
    }
    if options.model.is_empty() || options.out.as_os_str().is_empty() {
        return Err(USAGE.to_string());
    }
    if options.end.is_some() && options.seconds.is_some() {
        return Err("give --end or --seconds, not both".into());
    }
    Ok(options)
}

/// Where the last clip of the project ends, in samples at `rate`.
fn project_end(model: &Model, rate: u32) -> i64 {
    let mut end = 0i64;
    for track in model.all_tracks() {
        for clip in &track.clips {
            let start = model.tempo_map.position_to_samples(clip.start, rate);
            let clip_end = match &clip.content {
                ClipContent::Audio { media, length, .. } => {
                    let media_rate = model.media(*media).map_or(rate, |m| m.sample_rate.max(1));
                    // (Rounded up: a clip that ends part-way into a sample still occupies it.)
                    start + (*length * rate as u64).div_ceil(media_rate as u64) as i64
                }
                ClipContent::Midi { length, .. } => {
                    let start_tick = model.tempo_map.position_to_ticks(clip.start);
                    model
                        .tempo_map
                        .ticks_to_samples(Ticks(start_tick.0 + length.0), rate)
                }
            };
            end = end.max(clip_end);
        }
    }
    end
}

fn run(options: Options) -> Result<bool, String> {
    let json = if options.model == "-" {
        std::io::read_to_string(std::io::stdin()).map_err(|e| e.to_string())?
    } else {
        std::fs::read_to_string(&options.model).map_err(|e| format!("{}: {e}", options.model))?
    };
    let model = Model::from_json(&json)?;
    for id in &options.stems {
        if model.track(TrackId(*id)).is_none() {
            return Err(format!("the project has no track {id}"));
        }
    }
    let beats = |beats: f64| Position::Beats(Ticks((beats * TICKS_PER_BEAT as f64).round() as i64));
    let start = beats(options.start.max(0.0));
    let start_sample = model.tempo_map.position_to_samples(start, options.rate);
    let end = match (options.end, options.seconds) {
        (Some(end), _) => beats(end),
        (None, Some(seconds)) => Position::Time(samples_to_superclock(
            start_sample + (seconds * options.rate as f64).round() as i64,
            options.rate,
        )),
        (None, None) => Position::Time(samples_to_superclock(
            project_end(&model, options.rate),
            options.rate,
        )),
    };
    let media_dir = options.media_dir.clone().unwrap_or_else(|| {
        std::path::Path::new(&options.model)
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
    });
    let result = bounce(
        &model,
        &BounceRequest {
            path: options.out.clone(),
            start,
            end,
            tail_seconds: options.tail.max(0.0),
            sample_rate: options.rate,
            block_frames: options.block,
            format: options.format,
            media_dir,
            tracks: (!options.stems.is_empty())
                .then(|| options.stems.iter().map(|id| TrackId(*id)).collect()),
            render_threads: options.threads,
        },
    )?;
    println!(
        "{}",
        serde_json::json!({
            "path": result.path,
            "frames": result.frames,
            "seconds": result.frames as f64 / options.rate as f64,
            "sample_rate": options.rate,
            "peak": result.peak,
            "latency_trimmed": result.latency_trimmed,
            "missing_plugins": result.missing_plugins,
        })
    );
    Ok(result.missing_plugins.is_empty())
}

fn main() -> ExitCode {
    match parse(std::env::args().skip(1)).and_then(run) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("riddim-render: {message}");
            ExitCode::from(2)
        }
    }
}
