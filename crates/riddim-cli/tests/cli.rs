//! The `riddim` binary driven as a user would, against the fixture plugins.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use riddim_test_support::{crashplug_bundle, ids, scanner_exe, testplug_bundle};

fn riddim(args: &[&str]) -> Output {
    riddim_with_cache(args, &scan_cache("shared"))
}

/// Runs the tool with its plugin scan cache in `cache` (never the user's own).
fn riddim_with_cache(args: &[&str], cache: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_riddim"))
        .arg("--scanner")
        .arg(scanner_exe())
        .args(args)
        .env("RIDDIM_SCAN_CACHE", cache)
        .stdin(Stdio::null())
        .output()
        .expect("running riddim")
}

/// A scan cache file for this test run only.
fn scan_cache(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("riddim-cli-scan-{tag}-{}.json", std::process::id()))
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
    // A cache of its own: the other tests run at the same time and share theirs.
    let cache = scan_cache("blocklist");
    let _ = std::fs::remove_file(&cache);
    let riddim = |args: &[&str]| riddim_with_cache(args, &cache);
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

    // The crasher is now on the blocklist: the next scan lists the same plugins without
    // loading it, and says why it was skipped.
    let blocked = riddim(&["scan", "--blocklist"]);
    assert!(
        stdout(&blocked).contains("RiddimCrash.vst3"),
        "{}",
        stdout(&blocked)
    );
    assert!(stdout(&blocked).contains("crashed"), "{}", stdout(&blocked));
    let second = riddim(&["--vst3-dir", dir.to_str().unwrap(), "scan", "--json"]);
    assert_eq!(
        stdout(&second),
        stdout(&output),
        "the cached scan lists the same plugins"
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("on the blocklist"));

    // A rescan gives it another chance (it crashes again), and it can be taken off by hand.
    let again = riddim(&[
        "--vst3-dir",
        dir.to_str().unwrap(),
        "scan",
        "--rescan",
        "--json",
    ]);
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("the scanner crashed"));
    let crasher = crashplug_bundle();
    let unblock = riddim(&["scan", "--unblock", crasher.to_str().unwrap()]);
    assert!(unblock.status.success(), "{}", stdout(&unblock));
    assert!(!stdout(&riddim(&["scan", "--blocklist"])).contains("RiddimCrash.vst3"));
    let _ = std::fs::remove_file(&cache);
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
        .env("RIDDIM_SCAN_CACHE", scan_cache("shared"))
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
fn the_loopback_measurement_reports_zero_offset_on_an_honest_device() {
    let output = riddim(&[
        "loopback",
        "--null-device",
        "--loopback",
        "777",
        "--seconds",
        "2",
    ]);
    let text = stdout(&output);
    assert!(
        output.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("reported latency 388 in + 389 out = 777 frames"),
        "{text}"
    );
    assert!(
        text.contains("offset from where they were played: 0 to 0 samples"),
        "{text}"
    );

    // With no cable (a silent input) it says so instead of inventing a number.
    let silent = riddim(&["loopback", "--null-device", "--seconds", "1"]);
    assert_eq!(silent.status.code(), Some(1));
    assert!(stdout(&silent).contains("no clicks came back"));
}

#[test]
fn unknown_plugins_and_bad_arguments_fail_cleanly() {
    let output = riddim(&["info", &plugin("no-such-plugin")]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no plugin matches"));
    let output = riddim(&["check"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn a_mix_with_an_insert_a_send_and_automation_bounces_to_stems() {
    let dir = temp_dir("mix");
    let project = dir.join("song.json");
    let project_arg = project.to_str().unwrap();
    let created = riddim(&[
        "session",
        "new",
        project_arg,
        "--instrument",
        &plugin("synth"),
        "--sequence",
        "--insert",
        &plugin("gain"),
        "--send-effect",
        &plugin("delay"),
        "--fade-in",
        "2",
    ]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let info = stdout(&riddim(&["session", "info", project_arg]));
    assert!(info.contains("Effect return"), "{info}");
    assert!(info.contains("send 1002 to track 1000"), "{info}");
    assert!(info.contains("automation 1003"), "{info}");

    let render = |name: &str, extra: &[&str]| {
        let out = dir.join(name);
        let mut args = vec![
            "session",
            "render",
            project_arg,
            "--out",
            out.to_str().unwrap(),
            "--seconds",
            "2",
        ];
        args.extend_from_slice(extra);
        let output = riddim(&args);
        assert!(
            output.status.success(),
            "{}{}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
        (stdout(&output), std::fs::read(out).unwrap())
    };
    // The mix, twice: the same file, byte for byte.
    let (text, mix) = render("mix.wav", &[]);
    assert!(text.contains("of the mix"), "{text}");
    let (_, again) = render("mix-again.wav", &[]);
    assert!(mix == again, "two bounces of the same project differ");

    // The instrument as a stem (it carries its share of the return with it) is the whole
    // song here; the return alone is something else and not silent; a tail makes it longer.
    let (text, stem) = render("synth.wav", &["--stem", "1"]);
    assert!(text.contains("a stem of 1 track(s)"), "{text}");
    assert!(
        stem == mix,
        "the only source track, soloed, should be the mix"
    );
    let (text, wet) = render("return.wav", &["--stem", "1000"]);
    assert!(wet != mix);
    assert!(!text.contains("peak 0.0000"), "{text}");
    let (text, tail) = render("tail.wav", &["--tail", "0.5"]);
    assert!(text.contains("rendered 2.50 s"), "{text}");
    assert!(tail.len() > mix.len());

    // Playing with --save writes the plugins' states into the project, and the project
    // still bounces to the same file afterwards (the fixtures' default state is their state).
    assert!(!info.contains("bytes of state"), "{info}");
    let played = riddim(&[
        "session",
        "play",
        project_arg,
        "--null-device",
        "--seconds",
        "0.2",
        "--save",
    ]);
    assert!(
        played.status.success(),
        "{}{}",
        stdout(&played),
        String::from_utf8_lossy(&played.stderr)
    );
    let info = stdout(&riddim(&["session", "info", project_arg]));
    assert_eq!(info.matches("bytes of state").count(), 3, "{info}");
    let (_, saved) = render("mix-saved.wav", &[]);
    assert!(
        saved == mix,
        "saving the plugins' states changed the bounce"
    );

    // A stem of a track that does not exist is an error, not a silent file.
    let bad = riddim(&[
        "session",
        "render",
        project_arg,
        "--out",
        dir.join("bad.wav").to_str().unwrap(),
        "--stem",
        "77",
    ]);
    assert_eq!(bad.status.code(), Some(2));
    // Mixing options without an instrument to hang them on are refused.
    let lonely = riddim(&[
        "session",
        "new",
        dir.join("lonely.json").to_str().unwrap(),
        "--fade-in",
        "4",
    ]);
    assert_eq!(lonely.status.code(), Some(2));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn loop_takes_punch_routing_threads_and_a_live_buffer_change() {
    let dir = temp_dir("v1");
    let project = dir.join("song.json");
    let project_arg = project.to_str().unwrap();
    let ok = |output: Output| {
        assert!(
            output.status.success(),
            "{}{}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
        stdout(&output)
    };
    ok(riddim(&[
        "session",
        "new",
        project_arg,
        "--instrument",
        &plugin("synth"),
        "--sequence",
        "--audio-input",
        "1",
    ]));

    // Loop recording: the first two beats, round and round for two and a half seconds
    // (a second a time at 120 bpm), through a simulated cable.
    let text = ok(riddim(&[
        "session",
        "record",
        project_arg,
        "--null-device",
        "--loopback",
        "256",
        "--seconds",
        "2.5",
        "--loop",
        "0:2",
    ]));
    assert!(text.contains("take 1 on track 4: 48000 frames"), "{text}");
    assert!(text.contains("take 2 on track 4: 48000 frames"), "{text}");
    assert!(text.contains("take 3 on track 4"), "{text}");
    assert!(text.contains("frames lost while recording 0"), "{text}");
    let info = ok(riddim(&["session", "info", project_arg]));
    assert!(info.contains("loop beats 0.00 to 2.00 (on)"), "{info}");
    assert!(info.contains("take 1, muted"), "{info}");
    assert!(
        info.contains("take 2)"),
        "the newest whole take should be the one heard: {info}"
    );
    assert!(info.contains("take 3, muted"), "{info}");

    // Punch recording into the same track: one beat, and only that.
    let text = ok(riddim(&[
        "session",
        "record",
        project_arg,
        "--null-device",
        "--loopback",
        "256",
        "--seconds",
        "2",
        "--punch",
        "1:2",
    ]));
    assert!(
        text.contains("recording 1 track(s) from sample 24000"),
        "{text}"
    );
    assert!(text.contains("take on track 4: 24000 frames"), "{text}");
    let info = ok(riddim(&["session", "info", project_arg]));
    assert!(info.contains("punch beats 1.00 to 2.00 (on)"), "{info}");

    // Anything the model can express, from a file of ops: a bus, and the synth's second
    // output routed to it.
    let ops = dir.join("ops.json");
    std::fs::write(
        &ops,
        r#"[
          {"op":"set_punch","range":null,"enabled":false},
          {"op":"set_loop","range":null,"enabled":false},
          {"op":"add_track","index":0,"track":{"id":50,"name":"Second output","kind":"bus",
            "gain":1.0,"pan":0.0,"mute":false,"solo":false,"record_armed":false,"monitor":false,
            "input":{"type":"none"},"output":{"type":"master"}}},
          {"op":"set_plugin_routing","track":1,"slot":2,"outputs":[{"bus":1,"target":50}]}
        ]"#,
    )
    .unwrap();
    let text = ok(riddim(&[
        "session",
        "apply",
        project_arg,
        ops.to_str().unwrap(),
    ]));
    assert!(text.contains("applied 4 op(s)"), "{text}");
    let info = ok(riddim(&["session", "info", project_arg]));
    assert!(info.contains("output 2 to track 50"), "{info}");
    assert!(
        info.contains("plugin Riddim Test Synth in slot 2"),
        "{info}"
    );
    // An op the model refuses is reported with its position, and nothing is saved.
    std::fs::write(
        &ops,
        r#"[{"op":"set_metronome","metronome":{"enabled":true,"gain":0.5,"count_in_bars":0}},
            {"op":"remove_track","id":77}]"#,
    )
    .unwrap();
    let refused = riddim(&["session", "apply", project_arg, ops.to_str().unwrap()]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("op 1 was refused"));
    // (And the bus cannot be removed while the synth's output is routed to it.)
    std::fs::write(&ops, r#"[{"op":"remove_track","id":50}]"#).unwrap();
    let refused = riddim(&["session", "apply", project_arg, ops.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("op 0 was refused"));
    assert!(ok(riddim(&["session", "info", project_arg])).contains("Second output"));

    // Bounced with and without render threads: the same audio, to the bit.
    let fingerprint = |threads: &str| {
        let out = dir.join(format!("mix-{threads}.wav"));
        let text = ok(riddim(&[
            "session",
            "render",
            project_arg,
            "--out",
            out.to_str().unwrap(),
            "--seconds",
            "3",
            "--threads",
            threads,
        ]));
        let print = text
            .split("fingerprint ")
            .nth(1)
            .expect("a fingerprint")
            .trim()
            .to_string();
        (print, std::fs::read(out).unwrap())
    };
    let (one, one_bytes) = fingerprint("0");
    let (four, four_bytes) = fingerprint("4");
    assert_eq!(one, four);
    assert!(one_bytes == four_bytes);
    assert!(!one.starts_with("cbf29ce484222325"), "the bounce is empty");

    // Played on a running device with render threads, changing the buffer size on the way.
    let text = ok(riddim(&[
        "session",
        "play",
        project_arg,
        "--null-device",
        "--seconds",
        "1.5",
        "--render-threads",
        "2",
        "--switch-buffer",
        "256",
    ]));
    assert!(
        text.contains("buffer size changed to 256 frames while running"),
        "{text}"
    );
    assert!(text.contains("audio-thread heap operations 0"), "{text}");
    assert!(text.contains("messages lost 0"), "{text}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_project_with_bridged_plugins_renders_what_it_renders_in_process() {
    let dir = temp_dir("bridged");
    let render = |name: &str, extra: &[&str]| {
        let project = dir.join(format!("{name}.json"));
        let mut args = vec!["session", "new", project.to_str().unwrap(), "--instrument"];
        let synth = plugin("synth");
        let delay = plugin("delay");
        args.extend_from_slice(&[&synth, "--sequence", "--insert", &delay]);
        args.extend_from_slice(extra);
        let created = riddim(&args);
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let out = dir.join(format!("{name}.wav"));
        let rendered = riddim(&[
            "session",
            "render",
            project.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--seconds",
            "2",
        ]);
        assert!(
            rendered.status.success(),
            "{}{}",
            stdout(&rendered),
            String::from_utf8_lossy(&rendered.stderr)
        );
        (
            std::fs::read(out).unwrap(),
            stdout(&riddim(&["session", "info", project.to_str().unwrap()])),
        )
    };
    let (in_process, info) = render("in-process", &[]);
    assert!(!info.contains("in a process of its own"), "{info}");
    // The bridge executable is found beside the tool, as it would be when installed.
    let (bridged, info) = render("bridged", &["--bridged"]);
    assert_eq!(info.matches("in a process of its own").count(), 2, "{info}");
    assert!(
        bridged == in_process,
        "bridging the plugins changed the bounce"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_reference_session_is_one_command() {
    let dir = temp_dir("reference");
    let project = dir.join("reference.json");
    let project_arg = project.to_str().unwrap();
    let ok = |output: Output| {
        assert!(
            output.status.success(),
            "{}{}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
        stdout(&output)
    };
    // Four instruments, five effects after each: the shape of the vision's reference session.
    let (gain, delay) = (plugin("gain"), plugin("delay"));
    let mut args = vec![
        "session",
        "new",
        project_arg,
        "--instrument",
        "synth",
        "--sequence",
        "--copies",
        "4",
        "--looped",
    ];
    let synth = plugin("synth");
    args[4] = &synth;
    for effect in [&gain, &delay, &gain, &delay, &gain] {
        args.extend(["--insert", effect.as_str()]);
    }
    let text = ok(riddim(&args));
    assert!(text.contains("4 track(s)"), "{text}");

    let info = ok(riddim(&["session", "info", project_arg]));
    assert_eq!(
        info.matches("  instrument Riddim Test Synth").count(),
        4,
        "{info}"
    );
    assert_eq!(info.matches("      plugin ").count(), 24, "{info}");
    assert_eq!(info.matches("Riddim Test Synth").count(), 8, "{info}");
    assert!(info.contains("loop beats 0.00 to 4.00 (on)"), "{info}");
    // Every slot and clip has an id of its own, or the project would not have been accepted;
    // the copies are turned down so that together they are as loud as one.
    assert_eq!(info.matches("gain 0.25").count(), 4, "{info}");

    // It plays, looping, at 64 frames: longer than the material is, and still sounding. The
    // summary's load figure covers the whole run.
    let text = ok(riddim(&[
        "session",
        "play",
        project_arg,
        "--null-device",
        "--buffer",
        "64",
        "--seconds",
        "3",
    ]));
    assert!(text.contains("audio-thread heap operations 0"), "{text}");
    assert!(text.contains("seconds had a block over its time"), "{text}");
    assert_eq!(text.matches("plugin loaded").count(), 24, "{text}");

    // The options need an instrument to copy.
    let lonely = riddim(&[
        "session",
        "new",
        dir.join("lonely.json").to_str().unwrap(),
        "--copies",
        "2",
    ]);
    assert_eq!(lonely.status.code(), Some(2));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_clap_plugin_is_checked_played_and_rendered_like_any_other() {
    let dir = temp_dir("clap");
    let project = dir.join("clap.json");
    let project_arg = project.to_str().unwrap();
    let ok = |output: Output| {
        assert!(
            output.status.success(),
            "{}{}",
            stdout(&output),
            String::from_utf8_lossy(&output.stderr)
        );
        stdout(&output)
    };
    let bundle = riddim_test_support::clapplug_bundle();
    let synth = format!("{}#Riddim CLAP Synth", bundle.display());
    let delay = format!("{}#Riddim CLAP Delay", bundle.display());

    // The conformance run, on a `.clap` named by its path.
    let checked = ok(riddim(&["check", &synth]));
    assert!(checked.contains("=> passed"), "{checked}");

    ok(riddim(&[
        "session",
        "new",
        project_arg,
        "--instrument",
        &synth,
        "--sequence",
        "--insert",
        &delay,
    ]));
    let info = ok(riddim(&["session", "info", project_arg]));
    assert!(info.contains("(clap:dev.riddim.test.clap-synth"), "{info}");
    assert!(info.contains("(clap:dev.riddim.test.clap-delay"), "{info}");

    // It renders, the same bytes each time, and it plays on a device.
    let render = |name: &str| {
        let out = dir.join(name);
        let text = ok(riddim(&[
            "session",
            "render",
            project_arg,
            "--out",
            out.to_str().unwrap(),
            "--seconds",
            "2",
        ]));
        assert!(text.contains("rendered 2.00 s"), "{text}");
        std::fs::read(out).unwrap()
    };
    let first = render("one.wav");
    assert!(first == render("two.wav"), "two renders differ");
    assert!(
        first[44..].iter().any(|&byte| byte != 0),
        "the render is silent"
    );
    let text = ok(riddim(&[
        "session",
        "play",
        project_arg,
        "--null-device",
        "--seconds",
        "1",
    ]));
    assert_eq!(text.matches("plugin loaded").count(), 2, "{text}");
    assert!(text.contains("audio-thread heap operations 0"), "{text}");
    std::fs::remove_dir_all(&dir).ok();
}
