//! The out-of-process scanner against a healthy bundle and against one that takes its process
//! down. This is exit criterion 4 of phase 1: a crashing plugin kills only the scanner.

use std::time::{Duration, Instant};

use riddim_plugin::scan::{ScanCache, ScanOutcome, ScanTarget, Scanner};
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

/// A private copy of a fixture bundle, so a test can change it without disturbing the others.
fn copy_bundle(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_bundle(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn the_cache_spares_good_bundles_and_blocklists_bad_ones() {
    let dir = std::env::temp_dir().join(format!("riddim-scan-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let good = dir.join("Good.vst3");
    let bad = dir.join("Bad.vst3");
    copy_bundle(&testplug_bundle(), &good);
    copy_bundle(&crashplug_bundle(), &bad);
    // The copies keep the binary names of the originals inside; the loader looks for
    // "<bundle name>.so", so rename them to match.
    for (bundle, old, new) in [(&good, "RiddimTest", "Good"), (&bad, "RiddimCrash", "Bad")] {
        fn rename(dir: &std::path::Path, old: &str, new: &str) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rename(&path, old, new);
                } else if path.file_stem().is_some_and(|s| s == old) {
                    let mut renamed = path.with_file_name(new);
                    if let Some(extension) = path.extension() {
                        renamed.set_extension(extension);
                    }
                    std::fs::rename(&path, renamed).unwrap();
                }
            }
        }
        rename(bundle, old, new);
    }
    let cache_file = dir.join("cache.json");
    let targets = [vst3(good.clone()), vst3(bad.clone())];

    // First run: both are probed. The good one is cached, the bad one blocklisted.
    let mut cache = ScanCache::load(&cache_file);
    assert!(cache.is_empty());
    let mut from_cache = Vec::new();
    let report = scanner().scan_cached(&targets, &mut cache, Some(&cache_file), |_, _, cached| {
        from_cache.push(cached)
    });
    assert_eq!(from_cache, vec![false, false]);
    assert_eq!(report.plugins.len(), 4);
    assert_eq!(report.failures.len(), 1);
    let blocked = cache.blocked();
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].0, bad);
    assert!(blocked[0].1.contains("crashed"), "{}", blocked[0].1);

    // Second run, from the file, with a scanner that cannot possibly run: nothing is probed,
    // and the answers are the same. That is the cache doing all the work.
    let mut reloaded = ScanCache::load(&cache_file);
    assert_eq!(reloaded.len(), cache.len());
    assert_eq!(reloaded.blocked(), cache.blocked());
    let no_scanner = Scanner::new("/nonexistent/riddim-scan");
    let mut from_cache = Vec::new();
    let report = no_scanner.scan_cached(&targets, &mut reloaded, None, |_, _, cached| {
        from_cache.push(cached)
    });
    assert_eq!(from_cache, vec![true, true]);
    assert_eq!(report.plugins.len(), 4);
    assert!(matches!(&report.failures[0].1, ScanOutcome::Failed(why) if why.contains("blocklist")));

    // An updated bundle is not trusted from the cache: it is probed again.
    let binary = std::fs::read_dir(good.join("Contents"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let library = std::fs::read_dir(&binary)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let mut bytes = std::fs::read(&library).unwrap();
    bytes.push(0);
    std::fs::write(&library, bytes).unwrap();
    assert!(
        reloaded.lookup(&good).is_none(),
        "a changed bundle must miss the cache"
    );
    let mut from_cache = Vec::new();
    scanner().scan_cached(&targets[..1], &mut reloaded, None, |_, _, cached| {
        from_cache.push(cached)
    });
    assert_eq!(from_cache, vec![false]);

    // Taking a bundle off the blocklist gets it probed again (and blocked again: it still crashes).
    assert!(reloaded.forget(&bad));
    assert!(reloaded.blocked().is_empty());
    scanner().scan_cached(&targets[1..], &mut reloaded, None, |_, _, _| {});
    assert_eq!(reloaded.blocked().len(), 1);

    // A damaged or foreign cache file is ignored, not trusted.
    std::fs::write(&cache_file, "{ not json").unwrap();
    assert!(ScanCache::load(&cache_file).is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_host_that_dies_mid_scan_leaves_the_culprit_blocked() {
    let dir = std::env::temp_dir().join(format!("riddim-scan-mark-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache_file = dir.join("cache.json");
    let hang = scanner()
        .with_env("RIDDIM_CRASHPLUG_MODE", "hang")
        .with_timeout(Duration::from_millis(1500));
    let bundle = crashplug_bundle();
    let targets = [vst3(bundle.clone())];

    // While the scanner hangs, the file on disk already says who is probing the bundle.
    let file = cache_file.clone();
    let watcher = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            if text.contains("\"probing\"") {
                return text;
            }
            assert!(Instant::now() < deadline, "the mark was never written");
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let mut cache = ScanCache::new();
    hang.scan_cached(&targets, &mut cache, Some(&cache_file), |_, _, _| {});
    let during = watcher.join().unwrap();
    let mine = format!("\"pid\": {}", std::process::id());
    assert!(during.contains(&mine), "{during}");

    // The mark of a live process (this one) blocks nothing: another host running at the same
    // time just scans the bundle itself.
    let marked = dir.join("during.json");
    std::fs::write(&marked, &during).unwrap();
    let live = ScanCache::load(&marked);
    assert!(live.lookup(&bundle).is_none());
    assert!(live.blocked().is_empty());

    // The same mark from a process that no longer exists is a blocked bundle. (Process ids
    // stop well short of this one on every system we run on.)
    std::fs::write(&marked, during.replace(&mine, "\"pid\": 2147483646")).unwrap();
    let mut dead = ScanCache::load(&marked);
    let blocked = dead.blocked();
    assert_eq!(blocked.len(), 1);
    assert!(blocked[0].1.contains("did not finish"), "{}", blocked[0].1);
    let no_scanner = Scanner::new("/nonexistent/riddim-scan");
    let mut from_cache = Vec::new();
    let report = no_scanner.scan_cached(&targets, &mut dead, None, |_, _, cached| {
        from_cache.push(cached)
    });
    assert_eq!(
        from_cache,
        vec![true],
        "the orphaned bundle must not be probed"
    );
    assert_eq!(report.failures.len(), 1);

    // Once the scan does come back (timed out), the reason on disk is the real one.
    assert!(ScanCache::load(&cache_file).blocked()[0].1.contains("hung"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn two_hosts_saving_the_same_cache_keep_each_others_results() {
    let dir = std::env::temp_dir().join(format!("riddim-scan-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cache_file = dir.join("cache.json");
    let good = [vst3(testplug_bundle())];
    let bad = [vst3(crashplug_bundle())];

    // Both start from the same (empty) file, then each scans something different and saves.
    let mut first = ScanCache::load(&cache_file);
    let mut second = ScanCache::load(&cache_file);
    scanner().scan_cached(&good, &mut first, Some(&cache_file), |_, _, _| {});
    scanner().scan_cached(&bad, &mut second, Some(&cache_file), |_, _, _| {});
    let merged = ScanCache::load(&cache_file);
    assert_eq!(
        merged.len(),
        2,
        "the second save must not drop what the first one wrote"
    );
    assert!(merged.lookup(&testplug_bundle()).is_some());

    // But something deliberately forgotten stays forgotten through a save.
    let mut third = ScanCache::load(&cache_file);
    assert!(third.forget(&crashplug_bundle()));
    third.save(&cache_file).unwrap();
    assert_eq!(ScanCache::load(&cache_file).len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}
