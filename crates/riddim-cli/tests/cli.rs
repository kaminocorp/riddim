//! The `riddim` binary driven as a user would, against the fixture plugins.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use riddim_test_support::{crashplug_bundle, ids, scanner_exe, testplug_bundle};

fn riddim(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_riddim"))
        .arg("--scanner")
        .arg(scanner_exe())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("running riddim")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn plugin(name: &str) -> String {
    format!("{}#{name}", testplug_bundle().display())
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("riddim-cli-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn scan_lists_plugins_and_survives_a_crashing_bundle() {
    // Both fixture bundles live in the same directory: the good one and the one that aborts.
    let _ = crashplug_bundle();
    let dir = testplug_bundle().parent().unwrap().to_path_buf();
    let output = riddim(&["--vst3-dir", dir.to_str().unwrap(), "scan", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plugins: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let uids: Vec<&str> = plugins
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"]["uid"].as_str())
        .collect();
    for uid in [ids::SYNTH_UID, ids::DELAY_UID, ids::GAIN_UID] {
        assert!(uids.contains(&uid), "missing {uid} in {uids:?}");
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("RiddimCrash.vst3 did not scan"),
        "stderr was: {stderr}"
    );
}

#[test]
fn info_and_check_report_on_a_plugin() {
    let output = riddim(&["info", &plugin("synth")]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("Riddim Test Synth"));
    assert!(text.contains("audio out 1: Aux (2 ch, Aux, inactive)"));
    assert!(text.contains("parameters: 5"));

    let output = riddim(&["check", &plugin("synth"), &plugin("delay"), &plugin("gain")]);
    let text = stdout(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(text.matches("=> passed").count(), 3, "{text}");
    assert!(text.contains("0 heap operation(s) on the host side of process"));
}

/// The `fingerprint` the tool printed for a render.
fn fingerprint(output: &Output) -> String {
    let text = stdout(output);
    let at = text
        .find("fingerprint ")
        .unwrap_or_else(|| panic!("no fingerprint in: {text}"));
    text[at + 12..at + 28].to_string()
}

#[test]
fn render_writes_an_audible_wav_and_is_repeatable() {
    let dir = temp_dir("render");
    let out = dir.join("synth.wav");
    let render = || {
        riddim(&[
            "render",
            &plugin("synth"),
            "-o",
            out.to_str().unwrap(),
            "--seconds",
            "1",
        ])
    };
    let first = render();
    assert!(
        first.status.success(),
        "{}\n{}",
        stdout(&first),
        String::from_utf8_lossy(&first.stderr)
    );
    let data = riddim::files::wav::read(&out).unwrap();
    assert_eq!(
        (data.sample_rate, data.channels.len(), data.frames()),
        (48_000, 2, 48_000)
    );
    assert!(
        data.channels[0].iter().any(|s| s.abs() > 0.05),
        "the rendered file is silent"
    );
    // The same render again gives the same audio, bit for bit.
    let second = render();
    assert_eq!(fingerprint(&first), fingerprint(&second));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_midi_file_can_be_rendered_through_a_plugin() {
    use riddim::files::smf::{self, SmfData, SmfNote};
    use riddim::time::Ticks;
    let dir = temp_dir("midi");
    // One A4 on beat 2 of a file at 240 bpm: a quarter of a second in.
    let song = SmfData {
        notes: vec![SmfNote {
            start: Ticks::from_beats(1),
            length: Ticks::from_beats(1),
            key: 69,
            velocity: 127,
            channel: 0,
        }],
        tempos: vec![(Ticks::ZERO, riddim::time::Tempo::from_bpm(240.0))],
        length: Ticks::from_beats(4),
        ..SmfData::default()
    };
    let midi = dir.join("song.mid");
    std::fs::write(&midi, smf::write(&song)).unwrap();
    let out = dir.join("song.wav");
    let output = riddim(&[
        "render",
        &plugin("synth"),
        "--midi",
        midi.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
        "--seconds",
        "1",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data = riddim::files::wav::read(&out).unwrap();
    let first = data.channels[0].iter().position(|&s| s != 0.0);
    assert_eq!(
        first,
        Some(12_001),
        "the note must start a quarter second in, at the file's tempo"
    );
    let last = data.channels[0].iter().rposition(|&s| s != 0.0).unwrap();
    assert!(last < 24_000, "the note must end after one beat");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn state_files_round_trip() {
    let dir = temp_dir("state");
    let file = dir.join("delay.rstate");
    let saved = riddim(&[
        "state",
        "save",
        &plugin("delay"),
        "-o",
        file.to_str().unwrap(),
    ]);
    assert!(saved.status.success());
    let output = riddim(&[
        "state",
        "roundtrip",
        &plugin("delay"),
        "--state",
        file.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("identical"));

    // A state file for one plugin is refused by another.
    let wrong = riddim(&[
        "state",
        "roundtrip",
        &plugin("gain"),
        "--state",
        file.to_str().unwrap(),
    ]);
    assert_eq!(wrong.status.code(), Some(2));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn play_runs_the_engine_on_a_device_without_heap_traffic_and_saves_state() {
    let dir = temp_dir("play");
    let file = dir.join("after.rstate");
    let output = riddim(&[
        "play",
        &plugin("synth"),
        "--sequence",
        "--seconds",
        "0.6",
        "--buffer",
        "64",
        "--no-editor",
        "--null-device",
        "--save-state",
        file.to_str().unwrap(),
    ]);
    let text = stdout(&output);
    assert!(
        output.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("audio-thread heap operations 0"), "{text}");
    assert!(
        text.contains("plugin loaded in slot 2: 5 parameters"),
        "{text}"
    );
    // The saved file is the same container `state roundtrip` reads.
    let check = riddim(&[
        "state",
        "roundtrip",
        &plugin("synth"),
        "--state",
        file.to_str().unwrap(),
    ]);
    assert!(check.status.success(), "{}", stdout(&check));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_session_can_be_recorded_saved_reopened_and_rendered_identically() {
    let dir = temp_dir("session");
    let project = dir.join("song.json");
    let project_arg = project.to_str().unwrap();
    let created = riddim(&[
        "session",
        "new",
        project_arg,
        "--instrument",
        &plugin("synth"),
        "--sequence",
        "--audio-input",
        "1-2",
    ]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    // Record the instrument through a simulated cable from output to input.
    let recorded = riddim(&[
        "session",
        "record",
        project_arg,
        "--null-device",
        "--loopback",
        "512",
        "--seconds",
        "1.5",
    ]);
    let text = stdout(&recorded);
    assert!(
        recorded.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    assert!(text.contains("take on track 4"), "{text}");
    assert!(text.contains("frames lost while recording 0"), "{text}");

    let info = stdout(&riddim(&["session", "info", project_arg]));
    assert!(info.contains("audio clip"), "{info}");
    assert!(
        info.contains("bytes of state"),
        "the instrument's state must be saved with the project: {info}"
    );

    // The take holds what the instrument played: it is not silent, and it starts where the
    // arpeggio's first note does (one sample after the note-on, as the fixture synth does).
    let take = std::fs::read_dir(dir.join("song.media"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let audio = riddim::files::wav::read(&take).unwrap();
    assert_eq!(
        audio.channels[0].iter().position(|&s| s != 0.0),
        Some(1),
        "the take is misaligned"
    );

    // Reopen and render, twice: identical audio both times.
    let out = dir.join("mix.wav");
    let render = || {
        riddim(&[
            "session",
            "render",
            project_arg,
            "-o",
            out.to_str().unwrap(),
            "--seconds",
            "2",
        ])
    };
    let first = render();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(fingerprint(&first), fingerprint(&render()));
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn killing_the_process_mid_take_keeps_what_was_recorded() {
    // Phase 2 exit criterion 5, second half: kill -9 during a take loses nothing that had
    // reached the disk.
    use std::time::{Duration, Instant};
    let dir = temp_dir("kill");
    let project = dir.join("song.json");
    let project_arg = project.to_str().unwrap();
    let created = riddim(&[
        "session",
        "new",
        project_arg,
        "--instrument",
        &plugin("synth"),
        "--sequence",
        "--audio-input",
        "1",
    ]);
    assert!(created.status.success());

    let mut child = Command::new(env!("CARGO_BIN_EXE_riddim"))
        .arg("--scanner")
        .arg(scanner_exe())
        .args([
            "session",
            "record",
            project_arg,
            "--null-device",
            "--loopback",
            "256",
            "--seconds",
            "60",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Wait until the take has been growing on disk for a while, then kill without warning.
    let media = dir.join("song.media");
    let take = || {
        std::fs::read_dir(&media)
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .map(|e| e.path())
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let path = loop {
        if let Some(path) = take() {
            if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 4 * 48_000 * 2 {
                break path;
            }
        }
        assert!(Instant::now() < deadline, "the take never appeared on disk");
        std::thread::sleep(Duration::from_millis(50));
    };
    child.kill().unwrap(); // SIGKILL
    child.wait().unwrap();

    let size = std::fs::metadata(&path).unwrap().len();
    let audio =
        riddim::files::wav::read(&path).expect("the interrupted take must still be a readable WAV");
    // Everything that reached the file is there: its size is header plus whole mono frames.
    assert!(
        audio.frames() as u64 >= (size - 104) / 4 - 1,
        "frames were lost: {} of {size} bytes",
        audio.frames()
    );
    assert!(
        audio.frames() > 48_000,
        "expected well over a second of audio, got {}",
        audio.frames()
    );
    assert!(
        audio.channels[0].iter().any(|&s| s != 0.0),
        "the take should hold the instrument's sound"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_plugins_and_bad_arguments_fail_cleanly() {
    let output = riddim(&["info", &plugin("no-such-plugin")]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no plugin matches"));
    let output = riddim(&["check"]);
    assert_eq!(output.status.code(), Some(2));
}
