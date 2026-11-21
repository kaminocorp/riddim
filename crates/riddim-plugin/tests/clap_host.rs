//! The CLAP host driven against Riddim's own CLAP fixture plugins (`fixtures/clapplug`).
//!
//! Real hosting, of plugins we wrote: a real `.clap` is loaded and driven through the real
//! C ABI. The fixtures are written straight against that ABI and share no code with the host
//! (which is built on `clack-host`), so the two sides have to agree on the specification, not
//! on a library. This is what loads a CLAP plugin in every ordinary test run; third-party
//! CLAP plugins are in `third_party.rs`, behind `RIDDIM_TEST_CLAP`.

mod common;

use common::{config, peak, Rig, BLOCK};
use riddim_plugin::check::{self, Options, Verdict};
use riddim_plugin::host::instantiate;
use riddim_plugin::scan::{scan_in_process, ScanOutcome, ScanTarget, Scanner};
use riddim_plugin::{
    ActivateConfig, BusKind, Event, PluginDescriptor, PluginFormat, PluginKind, ProcessMode,
    ProcessStatus, StateBlob,
};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

fn target() -> ScanTarget {
    ScanTarget::for_path(riddim_test_support::clapplug_bundle())
}

fn descriptors() -> Vec<PluginDescriptor> {
    scan_in_process(&target()).expect("scanning the CLAP fixture")
}

fn descriptor(id: &str) -> PluginDescriptor {
    descriptors()
        .into_iter()
        .find(|d| d.id.uid == id)
        .unwrap_or_else(|| panic!("CLAP fixture plugin {id} not found"))
}

/// The synth's first sample after a note-on of A4 at full velocity and unity gain: the sine
/// starts at phase zero on the note's own sample, so this is the sample after it.
fn second_sample(rate: f64) -> f32 {
    ((std::f64::consts::TAU * 440.0 / rate).sin() * ids::CLAP_SYNTH_AMPLITUDE) as f32
}

#[test]
fn rt_checks_are_live() {
    assert!(riddim_rt::guard::allocator_installed());
}

#[test]
fn a_clap_file_is_recognised_scanned_and_described() {
    let target = target();
    assert_eq!(target.format, PluginFormat::Clap);

    let found = descriptors();
    assert_eq!(found.len(), 2, "{found:#?}");
    let synth = found
        .iter()
        .find(|d| d.id.uid == ids::CLAP_SYNTH_ID)
        .unwrap();
    assert_eq!(synth.name, "Riddim CLAP Synth");
    assert_eq!(synth.vendor, "Riddim tests");
    assert_eq!(synth.version, "1.0.0");
    assert_eq!(synth.kind, PluginKind::Instrument);
    assert_eq!(synth.id.format, PluginFormat::Clap);
    assert!(synth
        .path
        .as_ref()
        .unwrap()
        .ends_with("RiddimTestClap.clap"));
    let delay = found
        .iter()
        .find(|d| d.id.uid == ids::CLAP_DELAY_ID)
        .unwrap();
    assert_eq!(delay.kind, PluginKind::Effect);

    // The out-of-process scanner, which is how plugins are scanned outside tests, agrees.
    let scanner = Scanner::new(env!("CARGO_BIN_EXE_riddim-scan"));
    match scanner.scan(&target) {
        ScanOutcome::Ok(plugins) => assert_eq!(plugins, found),
        other => panic!("the CLAP fixture did not scan out of process: {other:?}"),
    }
}

#[test]
fn the_clap_fixtures_pass_the_conformance_run() {
    for descriptor in descriptors() {
        let report = check::run(&descriptor, &Options::default());
        assert!(report.passed(), "{report}");
        for step in ["state round trip", "host rt-safety"] {
            assert_eq!(
                report.step(step).unwrap().verdict,
                Verdict::Pass,
                "{report}"
            );
        }
    }
}

#[test]
fn the_synth_reports_its_ports_and_its_parameter_in_normalised_terms() {
    let instance = instantiate(&descriptor(ids::CLAP_SYNTH_ID)).unwrap();
    let layout = instance.bus_layout();
    assert!(layout.audio_inputs.is_empty());
    assert_eq!(layout.audio_outputs.len(), 1);
    assert_eq!(layout.audio_outputs[0].name, "Output");
    assert_eq!(layout.audio_outputs[0].kind, BusKind::Main);
    assert_eq!(layout.audio_outputs[0].channels, 2);
    assert!(layout.audio_outputs[0].active);
    assert_eq!(layout.event_inputs, 1);
    assert_eq!(layout.active_output_channels(), 2);

    // The plugin's range is 0 to 2 with a default of 1; the contract's is 0 to 1.
    let params = instance.params();
    assert_eq!(params.len(), 1);
    let gain = &params[0];
    assert_eq!(gain.id, ids::CLAP_SYNTH_GAIN);
    assert_eq!(gain.name, "Gain");
    assert_eq!(gain.default, 1.0 / ids::CLAP_GAIN_MAX);
    assert_eq!(gain.steps, 0);
    assert!(gain.flags.automatable && !gain.flags.read_only && !gain.flags.hidden);
    assert_eq!(instance.param_value(ids::CLAP_SYNTH_GAIN), Some(0.5));
    // A quarter of the way along is a plain 0.5, which the plugin words itself.
    assert_eq!(
        instance.param_text(ids::CLAP_SYNTH_GAIN, 0.25).as_deref(),
        Some("0.50 x")
    );
    assert_eq!(instance.param_value(ids::CLAP_SYNTH_GAIN + 1), None);
    assert_eq!(instance.latency_samples(), 0);
    assert!(!instance.is_active());
}

#[test]
fn notes_are_sample_accurate_and_processing_is_allocation_free() {
    let mut instance = instantiate(&descriptor(ids::CLAP_SYNTH_ID)).unwrap();
    let mut rig = Rig::new(instance.as_mut(), &config());
    assert!(instance.is_active());

    let silent = rig.idle(BLOCK);
    assert_eq!(silent.status, ProcessStatus::Ok);
    assert_eq!(silent.rt_violations, 0);
    assert_eq!(peak(&silent.outputs[0]), 0.0);

    let block = rig.run(BLOCK, &[], &[Event::note_on(10, 0, 69, 1.0)]);
    assert_eq!(
        block.rt_violations, 0,
        "the host side of process must not touch the heap"
    );
    let left = &block.outputs[0];
    assert!(
        left[..=10].iter().all(|&s| s == 0.0),
        "nothing may sound before the note's offset"
    );
    assert_eq!(
        left[11],
        second_sample(48_000.0),
        "the note must start exactly at its offset"
    );
    assert_eq!(block.outputs[0], block.outputs[1]);

    let block = rig.run(BLOCK, &[], &[Event::note_off(32, 0, 69)]);
    assert_eq!(block.rt_violations, 0);
    assert!(block.outputs[0][31] != 0.0);
    assert!(block.outputs[0][32..].iter().all(|&s| s == 0.0));

    // Two notes in one block, each on its own sample, and a note-off for one of them.
    let block = rig.run(
        BLOCK,
        &[],
        &[
            Event::note_on(4, 0, 69, 1.0),
            Event::note_on(20, 0, 81, 0.5),
            Event::note_off(40, 0, 69),
        ],
    );
    assert_eq!(block.rt_violations, 0);
    let left = &block.outputs[0];
    assert!(left[..=4].iter().all(|&s| s == 0.0));
    assert_eq!(left[5], second_sample(48_000.0));
    // From sample 40 only the octave above is left, at half the level, 20 samples in.
    let octave =
        (std::f64::consts::TAU * 880.0 / 48_000.0 * 20.0).sin() * ids::CLAP_SYNTH_AMPLITUDE * 0.5;
    assert!(
        (left[40] as f64 - octave).abs() < 1e-6,
        "{} vs {octave}",
        left[40]
    );

    rig.finish(instance.as_mut());
    assert!(!instance.is_active());
}

#[test]
fn a_parameter_event_lands_on_its_sample_in_the_plugins_own_range() {
    let mut instance = instantiate(&descriptor(ids::CLAP_SYNTH_ID)).unwrap();
    let mut rig = Rig::new(instance.as_mut(), &config());
    let before = rig.run(BLOCK, &[], &[Event::note_on(0, 0, 69, 1.0)]);
    assert_eq!(before.outputs[0][1], second_sample(48_000.0));

    // Fully up is a plain 2: from sample 16 on, exactly twice what it would have been.
    let block = rig.run(BLOCK, &[], &[Event::param(16, ids::CLAP_SYNTH_GAIN, 1.0)]);
    assert_eq!(block.rt_violations, 0);
    let at = |n: usize| {
        (std::f64::consts::TAU * 440.0 / 48_000.0 * (BLOCK + n) as f64).sin()
            * ids::CLAP_SYNTH_AMPLITUDE
    };
    let left = &block.outputs[0];
    assert!((left[15] as f64 - at(15)).abs() < 1e-6);
    assert!((left[16] as f64 - 2.0 * at(16)).abs() < 1e-6);
    assert!((left[63] as f64 - 2.0 * at(63)).abs() < 1e-6);
    // The plugin's own idea of the value, read back through the main-thread side.
    assert_eq!(instance.param_value(ids::CLAP_SYNTH_GAIN), Some(1.0));

    let muted = rig.run(BLOCK, &[], &[Event::param(0, ids::CLAP_SYNTH_GAIN, 0.0)]);
    assert_eq!(peak(&muted.outputs[0]), 0.0);
    rig.finish(instance.as_mut());
}

#[test]
fn state_survives_save_restore_save_and_a_foreign_state_is_refused() {
    let synth = descriptor(ids::CLAP_SYNTH_ID);
    let mut first = instantiate(&synth).unwrap();
    let mut rig = Rig::new(first.as_mut(), &config());
    rig.run(BLOCK, &[], &[Event::param(0, ids::CLAP_SYNTH_GAIN, 0.8125)]);
    rig.finish(first.as_mut());
    let saved = first.save_state().unwrap();
    assert_eq!(saved.plugin, synth.id);

    // Through the file container too, as the CLI does it.
    let from_file = StateBlob::from_bytes(&saved.to_bytes()).unwrap();
    let mut second = instantiate(&synth).unwrap();
    assert_eq!(second.param_value(ids::CLAP_SYNTH_GAIN), Some(0.5));
    second.restore_state(&from_file).unwrap();
    assert_eq!(second.param_value(ids::CLAP_SYNTH_GAIN), Some(0.8125));
    assert_eq!(
        second.save_state().unwrap(),
        saved,
        "save -> restore -> save must give an identical blob"
    );

    // The restored value is the one that sounds: 0.8125 of the way along is a gain of 1.625.
    let mut rig = Rig::new(second.as_mut(), &config());
    let block = rig.run(BLOCK, &[], &[Event::note_on(0, 0, 69, 1.0)]);
    let expected =
        (std::f64::consts::TAU * 440.0 / 48_000.0).sin() * ids::CLAP_SYNTH_AMPLITUDE * 1.625;
    assert!((block.outputs[0][1] as f64 - expected).abs() < 1e-6);
    rig.finish(second.as_mut());

    // Another plugin's state, and a truncated one, are refused and change nothing.
    let mut delay = instantiate(&descriptor(ids::CLAP_DELAY_ID)).unwrap();
    assert!(delay.restore_state(&saved).is_err());
    let mut wrong_plugin = saved.clone();
    wrong_plugin.plugin = delay.descriptor().id.clone();
    assert!(
        delay.restore_state(&wrong_plugin).is_err(),
        "the plugin itself must refuse bytes that are not its own"
    );
    assert_eq!(delay.param_value(ids::CLAP_DELAY_GAIN), Some(0.5));
    let mut corrupt = saved.clone();
    corrupt.data.truncate(6);
    assert!(second.restore_state(&corrupt).is_err());
}

#[test]
fn the_delay_reports_its_latency_and_delays_by_exactly_that() {
    let mut instance = instantiate(&descriptor(ids::CLAP_DELAY_ID)).unwrap();
    // CLAP lets a host ask for the latency only of a plugin that is active (or being
    // activated), so before that it is not known.
    assert_eq!(instance.latency_samples(), 0);
    let layout = instance.bus_layout();
    assert_eq!(layout.audio_inputs.len(), 1);
    assert_eq!(layout.audio_outputs.len(), 1);
    assert_eq!(layout.event_inputs, 0);
    assert_eq!(layout.active_input_channels(), 2);

    let mut rig = Rig::new(instance.as_mut(), &config());
    assert_eq!(instance.latency_samples(), ids::CLAP_DELAY_LATENCY);
    let mut left = vec![0.0f32; BLOCK];
    let mut right = vec![0.0f32; BLOCK];
    left[3] = 1.0;
    right[5] = -0.5;
    let block = rig.run(BLOCK, &[left, right], &[]);
    assert_eq!(block.rt_violations, 0);
    let latency = ids::CLAP_DELAY_LATENCY as usize;
    assert_eq!(block.outputs[0][3 + latency], 1.0);
    assert_eq!(block.outputs[1][5 + latency], -0.5);
    assert_eq!(block.outputs[0].iter().filter(|&&s| s != 0.0).count(), 1);
    assert_eq!(block.outputs[1].iter().filter(|&&s| s != 0.0).count(), 1);

    // What is still in the line comes out in the next block, at the gain set in that block.
    let mut late = vec![0.0f32; BLOCK];
    late[BLOCK - 1] = 0.25;
    rig.run(BLOCK, &[late.clone(), late], &[]);
    let block = rig.run(
        BLOCK,
        &[vec![0.0; BLOCK], vec![0.0; BLOCK]],
        &[Event::param(0, ids::CLAP_DELAY_GAIN, 1.0)],
    );
    assert_eq!(block.outputs[0][latency - 1], 0.5);
    rig.finish(instance.as_mut());
}

#[test]
fn reactivation_at_another_rate_and_oversized_blocks() {
    let mut instance = instantiate(&descriptor(ids::CLAP_SYNTH_ID)).unwrap();
    let rig = Rig::new(instance.as_mut(), &config());
    assert!(
        instance.activate(&config()).is_err(),
        "double activation must be refused"
    );
    rig.finish(instance.as_mut());

    let offline = ActivateConfig {
        sample_rate: 96_000.0,
        max_block: 32,
        mode: ProcessMode::Offline,
    };
    let mut rig = Rig::new(instance.as_mut(), &offline);
    let block = rig.run(32, &[], &[Event::note_on(0, 0, 69, 1.0)]);
    assert_eq!(
        block.outputs[0][1],
        second_sample(96_000.0),
        "the new sample rate must be in effect"
    );

    // A block larger than promised is refused and silenced rather than passed to the plugin.
    let too_big = rig.run(64, &[], &[]);
    assert_eq!(too_big.status, ProcessStatus::Failed);
    assert_eq!(peak(&too_big.outputs[0]), 0.0);
    rig.finish(instance.as_mut());

    // The same for an effect, whose input the host copies into buffers of the promised size.
    let mut delay = instantiate(&descriptor(ids::CLAP_DELAY_ID)).unwrap();
    let mut rig = Rig::new(delay.as_mut(), &offline);
    let too_big = rig.run(64, &[vec![1.0; 64], vec![1.0; 64]], &[]);
    assert_eq!(too_big.status, ProcessStatus::Failed);
    assert_eq!(peak(&too_big.outputs[0]), 0.0);
    let fits = rig.run(32, &[vec![1.0; 32], vec![1.0; 32]], &[]);
    assert_eq!(fits.status, ProcessStatus::Ok);
    rig.finish(delay.as_mut());
}

#[test]
fn many_instances_coexist_and_drop_cleanly() {
    let synth = descriptor(ids::CLAP_SYNTH_ID);
    let mut instances: Vec<_> = (0..8).map(|_| instantiate(&synth).unwrap()).collect();
    let mut rigs: Vec<_> = instances
        .iter_mut()
        .map(|instance| Rig::new(instance.as_mut(), &config()))
        .collect();
    // Each gets a different gain, and keeps its own.
    for (n, rig) in rigs.iter_mut().enumerate() {
        let gain = n as f64 / 8.0;
        let block = rig.run(
            BLOCK,
            &[],
            &[
                Event::param(0, ids::CLAP_SYNTH_GAIN, gain),
                Event::note_on(0, 0, 69, 1.0),
            ],
        );
        let expected = second_sample(48_000.0) as f64 * gain * ids::CLAP_GAIN_MAX;
        assert!((block.outputs[0][1] as f64 - expected).abs() < 1e-6);
    }
    for (rig, instance) in rigs.into_iter().zip(instances.iter_mut()) {
        rig.finish(instance.as_mut());
    }
}
