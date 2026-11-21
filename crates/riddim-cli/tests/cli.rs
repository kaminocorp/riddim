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

#[test]
fn render_writes_an_audible_wav() {
    let dir = temp_dir("render");
    let out = dir.join("synth.wav");
    let output = riddim(&[
        "render",
        &plugin("synth"),
        "-o",
        out.to_str().unwrap(),
        "--seconds",
        "1",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let bytes = std::fs::read(&out).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    // 1 s of 48 kHz stereo float.
    assert_eq!(bytes.len(), 56 + 48_000 * 2 * 4);
    let (samples, _) = bytes[56..].as_chunks::<4>();
    let loud = samples.iter().any(|s| f32::from_le_bytes(*s).abs() > 0.05);
    assert!(loud, "the rendered file is silent");
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
fn play_runs_the_plugin_through_a_device_callback_without_heap_traffic() {
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
    assert!(text.contains("failed 0"), "{text}");
    assert!(
        !text.contains("peak 0.0000"),
        "the plugin should have made sound: {text}"
    );
    assert!(file.is_file());
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
