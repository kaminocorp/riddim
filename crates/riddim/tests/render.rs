//! `riddim-render`, the headless renderer, driven as a build machine would drive it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use riddim::files::wav::{self, SampleFormat};
use riddim::files::AudioData;
use riddim::model::{
    Clip, ClipContent, ClipId, Media, MediaId, Model, Note, Op, PluginRef, PluginSlot, SlotId,
    Track, TrackId, TrackKind,
};
use riddim::time::{Position, Ticks};
use riddim::{bounce, BounceRequest};
use riddim_test_support::{ids, testplug_bundle};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("riddim-render-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture(id: u64, uid: &str, path: &Path) -> PluginSlot {
    PluginSlot::new(
        SlotId(id),
        PluginRef {
            format: "vst3".into(),
            uid: uid.into(),
            name: String::new(),
            path: Some(path.to_string_lossy().into_owned()),
        },
    )
}

/// One second of a steady 0.25 on an audio track, and the fixture synth playing a note over
/// the first two beats: three beats (1.5 s) of project in all.
fn project(dir: &Path) -> Model {
    let data = AudioData {
        sample_rate: 48_000,
        channels: vec![vec![0.25; 48_000]],
    };
    wav::write(&dir.join("steady.wav"), &data, SampleFormat::Float32).unwrap();
    let mut model = Model::new();
    let ops = vec![
        Op::AddMedia {
            media: Media {
                id: MediaId(1),
                path: "steady.wav".into(),
                sample_rate: 48_000,
                channels: 1,
                frames: 48_000,
            },
        },
        Op::AddTrack {
            index: 0,
            track: Track::new(TrackId(10), "Audio", TrackKind::Audio),
        },
        Op::AddTrack {
            index: 1,
            track: Track::new(TrackId(20), "Synth", TrackKind::Instrument),
        },
        Op::AddClip {
            track: TrackId(10),
            clip: Clip {
                id: ClipId(11),
                name: String::new(),
                start: Position::START,
                muted: false,
                take: None,
                content: ClipContent::Audio {
                    media: MediaId(1),
                    offset: 0,
                    length: 48_000,
                    gain: 1.0,
                    fade_in: 0,
                    fade_out: 0,
                },
            },
        },
        Op::InsertPlugin {
            track: TrackId(20),
            index: 0,
            slot: fixture(21, ids::SYNTH_UID, &testplug_bundle()),
        },
        Op::AddClip {
            track: TrackId(20),
            clip: Clip {
                id: ClipId(22),
                name: String::new(),
                start: Position::START,
                muted: false,
                take: None,
                content: ClipContent::Midi {
                    length: Ticks::from_beats(3),
                    notes: std::sync::Arc::new(vec![Note {
                        start: Ticks::ZERO,
                        length: Ticks::from_beats(2),
                        key: 57,
                        velocity: 100,
                        channel: 0,
                    }]),
                    events: Default::default(),
                },
            },
        },
    ];
    model.apply_all(&ops).unwrap();
    model
}

fn render(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_riddim-render"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("running riddim-render")
}

fn report(output: &Output) -> serde_json::Value {
    assert!(
        output.status.code().is_some_and(|code| code < 2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one line of JSON on stdout")
}

#[test]
fn the_headless_renderer_bounces_a_project_file() {
    let dir = temp_dir("basic");
    let model = project(&dir);
    let model_file = dir.join("song.json");
    std::fs::write(&model_file, model.to_json()).unwrap();
    let model_arg = model_file.to_str().unwrap();
    let out = dir.join("song.wav");

    // With nothing but a model and an output: the whole project, from the start to the end
    // of its last clip, media found beside the model.
    let done = render(&[model_arg, "--out", out.to_str().unwrap()]);
    assert_eq!(done.status.code(), Some(0));
    let told = report(&done);
    assert_eq!(told["frames"], 72_000);
    assert_eq!(told["seconds"], 1.5);
    assert_eq!(told["missing_plugins"], serde_json::json!([]));
    assert!(told["peak"].as_f64().unwrap() > 0.25);
    let audio = wav::read(&out).unwrap();
    assert_eq!(audio.channels[0].len(), 72_000);

    // It is the library's bounce, byte for byte, threads or no threads.
    let reference = bounce(
        &model,
        &BounceRequest {
            path: dir.join("reference.wav"),
            start: Position::START,
            end: Position::Beats(Ticks::from_beats(3)),
            tail_seconds: 0.0,
            sample_rate: 48_000,
            block_frames: 512,
            format: SampleFormat::Float32,
            media_dir: dir.clone(),
            tracks: None,
            render_threads: 0,
        },
    )
    .unwrap();
    let bytes = std::fs::read(&out).unwrap();
    assert!(bytes == std::fs::read(&reference.path).unwrap());
    let threaded = dir.join("threaded.wav");
    report(&render(&[
        model_arg,
        "-o",
        threaded.to_str().unwrap(),
        "--threads",
        "3",
    ]));
    assert!(std::fs::read(&threaded).unwrap() == bytes);

    // A range, a tail, another format, a stem; and the model on standard input.
    let part = dir.join("part.wav");
    let mut child = Command::new(env!("CARGO_BIN_EXE_riddim-render"))
        .args([
            "-",
            "--out",
            part.to_str().unwrap(),
            "--media-dir",
            dir.to_str().unwrap(),
            "--start",
            "1",
            "--end",
            "2",
            "--tail",
            "0.25",
            "--format",
            "pcm16",
            "--stem",
            "10",
            "--block",
            "64",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(model.to_json().as_bytes())
        .unwrap();
    let told = report(&child.wait_with_output().unwrap());
    assert_eq!(told["frames"], 24_000 + 12_000);
    let audio = wav::read(&part).unwrap();
    // The audio track alone: 0.25 from beat 1 (half a second) to the clip's end at one
    // second, then silence. No synth.
    assert!((audio.channels[0][100] - 0.25).abs() < 1e-3);
    assert!((audio.channels[0][23_999] - 0.25).abs() < 1e-3);
    assert_eq!(audio.channels[0][24_100], 0.0);
}

#[test]
fn the_headless_renderer_says_what_went_wrong() {
    let dir = temp_dir("errors");
    let mut model = project(&dir);
    let out = dir.join("out.wav");
    let out_arg = out.to_str().unwrap();

    // Nothing to render, nonsense to render, nowhere to put it: status 2 and a message.
    for args in [
        vec![],
        vec!["/nonexistent/model.json", "--out", out_arg],
        vec!["--out", out_arg],
        vec!["x.json", "--out", out_arg, "--format", "mp3"],
        vec!["x.json", "--out", out_arg, "--end", "4", "--seconds", "2"],
        vec!["x.json", "--out", out_arg, "--frobnicate"],
    ] {
        let failed = render(&args);
        assert_eq!(failed.status.code(), Some(2), "{args:?}");
        assert!(!failed.stderr.is_empty(), "{args:?}");
        assert!(failed.stdout.is_empty(), "{args:?}");
    }
    let not_a_model = dir.join("not-a-model.json");
    std::fs::write(&not_a_model, "{\"hello\": 1}").unwrap();
    assert_eq!(
        render(&[not_a_model.to_str().unwrap(), "--out", out_arg])
            .status
            .code(),
        Some(2)
    );
    let model_file = dir.join("song.json");
    std::fs::write(&model_file, model.to_json()).unwrap();
    let no_track = render(&[
        model_file.to_str().unwrap(),
        "--out",
        out_arg,
        "--stem",
        "99",
    ]);
    assert_eq!(no_track.status.code(), Some(2));

    // A plugin that is not installed: the bounce is made without it, the report names it,
    // and the status says the file is not the whole story.
    model
        .apply(&Op::InsertPlugin {
            track: TrackId(10),
            index: 0,
            slot: fixture(
                99,
                "00000000000000000000000000000000",
                Path::new("/nonexistent/Ghost.vst3"),
            ),
        })
        .unwrap();
    std::fs::write(&model_file, model.to_json()).unwrap();
    let partial = render(&[model_file.to_str().unwrap(), "--out", out_arg]);
    assert_eq!(partial.status.code(), Some(1));
    let told = report(&partial);
    assert_eq!(told["missing_plugins"].as_array().unwrap().len(), 1);
    assert!(out.exists());
}
