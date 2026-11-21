//! The Audio Unit host against the Audio Units Apple ships with macOS.
//!
//! These only build and run on macOS. They are real hosting of real Audio Units, but Apple's
//! own: passing here says nothing yet about third-party units.

#![cfg(target_os = "macos")]

use riddim_plugin::check::{self, Options, Verdict};
use riddim_plugin::format::au;
use riddim_plugin::host::instantiate;
use riddim_plugin::scan::{ScanOutcome, ScanTarget, Scanner};
use riddim_plugin::{
    ActivateConfig, Event, EventBuffer, PluginDescriptor, PluginFormat, PluginId, PluginKind,
    ProcessContext, ProcessMode, ProcessStatus, TransportInfo,
};
use riddim_rt::guard::RtCheckAllocator;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

/// Apple's DLS/SoundFont synth: an instrument present on every Mac.
const DLS_SYNTH: &str = "aumu:dls :appl";
const AU_DELAY: &str = "aufx:dely:appl";

fn apple(uid: &str) -> PluginDescriptor {
    au::scan()
        .expect("AU scan")
        .into_iter()
        .find(|d| d.id.uid == uid)
        .unwrap_or_else(|| panic!("Apple Audio Unit {uid} is not registered on this Mac"))
}

#[test]
fn scan_lists_apple_units_in_and_out_of_process() {
    let found = au::scan().unwrap();
    assert!(
        found.len() > 10,
        "expected Apple's units, found {}",
        found.len()
    );
    let synth = found
        .iter()
        .find(|d| d.id.uid == DLS_SYNTH)
        .expect("DLSMusicDevice");
    assert_eq!(synth.kind, PluginKind::Instrument);
    assert_eq!(synth.vendor, "Apple");
    assert_eq!(synth.id, PluginId::new(PluginFormat::AudioUnit, DLS_SYNTH));
    assert!(found
        .iter()
        .any(|d| d.id.uid == AU_DELAY && d.kind == PluginKind::Effect));

    let scanner = Scanner::new(env!("CARGO_BIN_EXE_riddim-scan"));
    match scanner.scan(&ScanTarget {
        format: PluginFormat::AudioUnit,
        path: None,
    }) {
        ScanOutcome::Ok(plugins) => assert_eq!(plugins.len(), found.len()),
        other => panic!("out-of-process AU scan failed: {other:?}"),
    }
}

#[test]
fn apple_synth_passes_the_conformance_run() {
    let report = check::run(&apple(DLS_SYNTH), &Options::default());
    eprintln!("{report}");
    assert!(report.passed(), "{report}");
    assert_eq!(report.step("sound").unwrap().verdict, Verdict::Pass);
    assert_eq!(
        report.step("host rt-safety").unwrap().verdict,
        Verdict::Pass
    );
}

#[test]
fn apple_effects_pass_the_conformance_run() {
    for uid in [
        AU_DELAY,
        "aufx:lpas:appl",
        "aufx:lmtr:appl",
        "aufx:dcmp:appl",
    ] {
        let Some(descriptor) = au::scan().unwrap().into_iter().find(|d| d.id.uid == uid) else {
            eprintln!("{uid} is not present on this Mac; skipped");
            continue;
        };
        let report = check::run(&descriptor, &Options::default());
        eprintln!("{report}");
        assert!(report.passed(), "{report}");
    }
}

#[test]
fn effect_input_reaches_the_output_and_parameters_apply() {
    // A low-pass filter passes DC, so a constant input must come out (almost) unchanged.
    let Some(descriptor) = au::scan()
        .unwrap()
        .into_iter()
        .find(|d| d.id.uid == "aufx:lpas:appl")
    else {
        eprintln!("AULowpass is not present on this Mac; skipped");
        return;
    };
    let mut instance = instantiate(&descriptor).unwrap();
    let layout = instance.bus_layout();
    assert_eq!(layout.audio_inputs.len(), 1);
    let channels = layout.active_output_channels();
    assert!(channels >= 1);
    let params = instance.params();
    assert!(!params.is_empty());
    for param in &params {
        let value = instance.param_value(param.id).expect("readable parameter");
        assert!((0.0..=1.0).contains(&value), "{} = {value}", param.name);
        assert!(instance.param_text(param.id, value).is_some());
    }

    let config = ActivateConfig {
        sample_rate: 48_000.0,
        max_block: 256,
        mode: ProcessMode::Realtime,
    };
    let mut processor = instance.activate(&config).unwrap();
    let inputs = vec![vec![0.5f32; 256]; layout.active_input_channels()];
    let mut outputs = vec![vec![0.0f32; 256]; channels];
    let transport = TransportInfo::stopped(48_000.0);
    let mut events_out = EventBuffer::with_capacity(64);
    let mut last = 0.0;
    for block in 0..40 {
        let events = if block == 0 {
            vec![Event::param(0, params[0].id, 1.0)]
        } else {
            vec![]
        };
        let input_refs: Vec<&[f32]> = inputs.iter().map(|c| c.as_slice()).collect();
        let mut output_refs: Vec<&mut [f32]> =
            outputs.iter_mut().map(|c| c.as_mut_slice()).collect();
        let mut context = ProcessContext {
            frames: 256,
            inputs: &input_refs,
            outputs: &mut output_refs,
            events_in: &events,
            events_out: &mut events_out,
            transport: &transport,
        };
        let (status, violations) =
            riddim_rt::guard::rt_scope_counted(|| processor.process(&mut context));
        assert_eq!(status, ProcessStatus::Ok);
        assert_eq!(
            violations, 0,
            "the host side of the AU render path touched the heap"
        );
        last = outputs[0][255];
    }
    assert!(
        (last - 0.5).abs() < 0.05,
        "DC should pass a low-pass filter, got {last}"
    );
    instance.deactivate(processor);
}

#[test]
fn unknown_component_is_a_clean_error() {
    let descriptor = PluginDescriptor {
        id: PluginId::new(PluginFormat::AudioUnit, "aumu:zzzz:zzzz"),
        name: "Nope".into(),
        vendor: String::new(),
        version: String::new(),
        kind: PluginKind::Instrument,
        categories: vec![],
        path: None,
    };
    assert!(instantiate(&descriptor).is_err());
    let malformed = PluginDescriptor {
        id: PluginId::new(PluginFormat::AudioUnit, "x"),
        ..descriptor
    };
    assert!(instantiate(&malformed).is_err());
}
