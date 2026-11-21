//! Conformance runs against plugins that are not part of this repository.
//!
//! Set `RIDDIM_TEST_VST3` to one or more `.vst3` bundle paths (separated by `;`), and
//! `RIDDIM_TEST_CLAP` to `.clap` files likewise, to run them.
//! CI points it at Surge XT; on a developer machine it can point at anything installed. Without
//! the variable the tests pass vacuously and say so.

use riddim_plugin::check::{self, Options, Verdict};
use riddim_plugin::scan::{ScanOutcome, ScanTarget, Scanner};
use riddim_plugin::PluginFormat;
use riddim_rt::guard::RtCheckAllocator;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// The bundles named by `RIDDIM_TEST_VST3` and `RIDDIM_TEST_CLAP`.
fn bundles() -> Vec<std::path::PathBuf> {
    ["RIDDIM_TEST_VST3", "RIDDIM_TEST_CLAP"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .flat_map(|list| {
            list.split(';')
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn third_party_vst3_plugins_pass_the_conformance_run() {
    let bundles = bundles();
    if bundles.is_empty() {
        eprintln!(
            "neither RIDDIM_TEST_VST3 nor RIDDIM_TEST_CLAP is set: no third-party plugins were tested"
        );
        return;
    }
    let scanner = Scanner::new(env!("CARGO_BIN_EXE_riddim-scan"));
    for bundle in bundles {
        let target = ScanTarget::for_path(bundle.clone());
        let plugins = match scanner.scan(&target) {
            ScanOutcome::Ok(plugins) => plugins,
            other => panic!("{} did not scan: {other:?}", bundle.display()),
        };
        assert!(
            !plugins.is_empty(),
            "{} contains no plugins",
            bundle.display()
        );
        for descriptor in plugins {
            let report = check::run(&descriptor, &Options::default());
            eprintln!("{report}");
            assert!(
                report.passed(),
                "{} failed its conformance run",
                descriptor.name
            );
            assert_eq!(
                report.step("host rt-safety").unwrap().verdict,
                Verdict::Pass
            );
        }
    }
}

#[test]
fn fixture_plugins_pass_the_conformance_run() {
    let target = ScanTarget {
        format: PluginFormat::Vst3,
        path: Some(riddim_test_support::testplug_bundle()),
    };
    for descriptor in riddim_plugin::scan::scan_in_process(&target).unwrap() {
        let report = check::run(&descriptor, &Options::default());
        assert!(report.passed(), "{report}");
        assert_eq!(
            report.step("state round trip").unwrap().verdict,
            Verdict::Pass,
            "{report}"
        );
        assert_eq!(
            report.step("host rt-safety").unwrap().verdict,
            Verdict::Pass,
            "{report}"
        );
    }
}
