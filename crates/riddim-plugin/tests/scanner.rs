//! The out-of-process scanner against a healthy bundle and against one that takes its process
//! down. This is exit criterion 4 of phase 1: a crashing plugin kills only the scanner.

use std::time::{Duration, Instant};

use riddim_plugin::scan::{ScanOutcome, ScanTarget, Scanner};
use riddim_plugin::PluginFormat;
use riddim_test_support::{crashplug_bundle, ids, testplug_bundle};

fn scanner() -> Scanner {
    Scanner::new(env!("CARGO_BIN_EXE_riddim-scan"))
}

fn vst3(path: std::path::PathBuf) -> ScanTarget {
    ScanTarget {
        format: PluginFormat::Vst3,
        path: Some(path),
    }
}

#[test]
fn healthy_bundle_scans_out_of_process() {
    match scanner().scan(&vst3(testplug_bundle())) {
        ScanOutcome::Ok(plugins) => {
            assert_eq!(plugins.len(), 4);
            assert!(plugins.iter().any(|p| p.id.uid == ids::SYNTH_UID));
        }
        other => panic!("expected a clean scan, got {other:?}"),
    }
}

#[test]
fn aborting_plugin_kills_only_the_scanner() {
    let outcome = scanner().scan(&vst3(crashplug_bundle()));
    assert!(
        matches!(outcome, ScanOutcome::Crashed(_)),
        "got {outcome:?}"
    );
    // We are still here, and the scanner still works for the next bundle.
    assert!(scanner().scan(&vst3(testplug_bundle())).is_ok());
}

#[test]
fn segfaulting_plugin_kills_only_the_scanner() {
    let scanner = scanner().with_env("RIDDIM_CRASHPLUG_MODE", "segv");
    match scanner.scan(&vst3(crashplug_bundle())) {
        ScanOutcome::Crashed(detail) => {
            assert!(
                detail.contains("signal"),
                "expected a signal death, got: {detail}"
            )
        }
        other => panic!("expected a crash, got {other:?}"),
    }
}

#[test]
fn hanging_plugin_is_killed_at_the_timeout() {
    let scanner = scanner()
        .with_env("RIDDIM_CRASHPLUG_MODE", "hang")
        .with_timeout(Duration::from_millis(400));
    let started = Instant::now();
    assert_eq!(
        scanner.scan(&vst3(crashplug_bundle())),
        ScanOutcome::TimedOut
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn missing_bundle_is_a_reported_failure() {
    let outcome = scanner().scan(&vst3("/nonexistent/Nope.vst3".into()));
    assert!(matches!(outcome, ScanOutcome::Failed(_)), "got {outcome:?}");
}

#[test]
fn scan_all_collects_plugins_and_failures() {
    let targets = [vst3(crashplug_bundle()), vst3(testplug_bundle())];
    let mut seen = 0;
    let report = scanner().scan_all(&targets, |_, _| seen += 1);
    assert_eq!(seen, 2);
    assert_eq!(report.plugins.len(), 4);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].0, targets[0]);
}
