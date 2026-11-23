//! The VST3 host driven against Riddim's own fixture plugins.
//!
//! This is real hosting (a real module is loaded through the real COM interfaces), but of
//! plugins we wrote. It says nothing about third-party plugins.

mod common;

use common::{config, descriptor, fixture_descriptors, peak, Rig, BLOCK};
use riddim_plugin::host::instantiate;
use riddim_plugin::{
    ActivateConfig, BusDirection, BusKind, Event, EventBody, PluginEvent, PluginFormat, PluginKind,
    ProcessMode, ProcessStatus, StateBlob, Tail,
};
use riddim_rt::guard::RtCheckAllocator;
use riddim_test_support::ids;

#[global_allocator]
static ALLOC: RtCheckAllocator = RtCheckAllocator::system();

#[test]
fn rt_checks_are_live() {
    assert!(riddim_rt::guard::allocator_installed());
}

#[test]
fn scan_lists_the_fixture_plugins() {
    let found = fixture_descriptors();
    assert_eq!(
        found.len(),
        4,
        "controller classes must not be listed: {found:#?}"
    );
    let synth = found.iter().find(|d| d.id.uid == ids::SYNTH_UID).unwrap();
    assert_eq!(synth.name, "Riddim Test Synth");
    assert_eq!(synth.vendor, "Riddim Test");
    assert_eq!(synth.version, "1.0.0");
    assert_eq!(synth.kind, PluginKind::Instrument);
    assert_eq!(synth.id.format, PluginFormat::Vst3);
    assert!(synth.path.as_ref().unwrap().ends_with("RiddimTest.vst3"));
    assert_eq!(
        found
            .iter()
            .find(|d| d.id.uid == ids::DELAY_UID)
            .unwrap()
            .kind,
        PluginKind::Effect
    );
    assert_eq!(
        found
            .iter()
            .find(|d| d.id.uid == ids::GAIN_UID)
            .unwrap()
            .kind,
        PluginKind::Effect
    );
}

#[test]
fn synth_reports_buses_and_parameters() {
    let instance = instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
    let layout = instance.bus_layout();
    assert!(layout.audio_inputs.is_empty());
    assert_eq!(layout.audio_outputs.len(), 2);
    assert_eq!(layout.audio_outputs[0].name, "Main");
    assert_eq!(layout.audio_outputs[0].kind, BusKind::Main);
    assert!(layout.audio_outputs[0].active);
    assert_eq!(layout.audio_outputs[1].kind, BusKind::Aux);
    assert!(!layout.audio_outputs[1].active);
    assert_eq!(layout.event_inputs, 1);
    assert_eq!(layout.event_outputs, 1);
    assert_eq!(layout.active_output_channels(), 2);

    let params = instance.params();
    assert_eq!(params.len(), 5);
    let gain = params.iter().find(|p| p.id == ids::SYNTH_GAIN).unwrap();
    assert_eq!(gain.name, "Gain");
    assert_eq!(gain.default, 0.5);
    assert!(gain.flags.automatable && !gain.flags.read_only);
    assert!(
        params
            .iter()
            .find(|p| p.id == ids::SYNTH_METER)
            .unwrap()
            .flags
            .read_only
    );
    assert_eq!(instance.param_value(ids::SYNTH_GAIN), Some(0.5));
    assert_eq!(
        instance.param_text(ids::SYNTH_GAIN, 0.25).as_deref(),
        Some("0.250")
    );
    assert_eq!(instance.latency_samples(), 0);
    assert_eq!(instance.tail(), Tail::Samples(0));
    assert!(!instance.is_active());
}

#[test]
fn notes_are_sample_accurate_and_processing_is_allocation_free() {
    let mut instance = instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
    let mut rig = Rig::new(instance.as_mut(), &config());
    assert!(instance.is_active());

    let silent = rig.idle(BLOCK);
    assert_eq!(silent.status, ProcessStatus::Ok);
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
    assert!(left[11] != 0.0, "the note must start exactly at its offset");
    assert_eq!(block.outputs[0], block.outputs[1]);
    // A4 at 48 kHz: second sample of the sine, at unity gain and amplitude 0.25.
    let expected = (std::f64::consts::TAU * 440.0 / 48_000.0).sin() as f32 * 0.25;
    assert!((left[11] - expected).abs() < 1e-6);

    let block = rig.run(BLOCK, &[], &[Event::note_off(32, 0, 69)]);
    assert_eq!(block.rt_violations, 0);
    assert!(block.outputs[0][31] != 0.0);
    assert!(block.outputs[0][32..].iter().all(|&s| s == 0.0));

    rig.finish(instance.as_mut());
    assert!(!instance.is_active());
}

#[test]
fn parameter_events_and_mapped_midi_controllers_reach_the_processor() {
    let mut instance = instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
    let mut rig = Rig::new(instance.as_mut(), &config());

    let loud = rig.run(BLOCK, &[], &[Event::note_on(0, 0, 60, 1.0)]);
    let reference = peak(&loud.outputs[0]);
    assert!(reference > 0.1);

    // CC1 is mapped to the Mod parameter through IMidiMapping; fully up halves the level.
    let cc = Event::from_midi(0, &[0xB0, 1, 127]).unwrap();
    let halved = rig.run(BLOCK, &[], &[cc]);
    assert_eq!(halved.rt_violations, 0);
    let ratio = peak(&halved.outputs[0]) / reference;
    assert!(
        (ratio - 0.5).abs() < 0.05,
        "mod wheel should halve the level, got ratio {ratio}"
    );

    // An unmapped controller is dropped, not turned into something else.
    let unmapped = Event::from_midi(0, &[0xB0, 74, 127]).unwrap();
    let same = rig.run(BLOCK, &[], &[unmapped]);
    assert!((peak(&same.outputs[0]) / reference - 0.5).abs() < 0.05);

    let muted = rig.run(BLOCK, &[], &[Event::param(0, ids::SYNTH_GAIN, 0.0)]);
    assert_eq!(muted.rt_violations, 0);
    assert_eq!(peak(&muted.outputs[0]), 0.0);
    rig.finish(instance.as_mut());
}

#[test]
fn plugin_output_events_and_transport_come_back() {
    let mut instance = instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
    let mut rig = Rig::new(instance.as_mut(), &config());
    rig.transport.playing = true;
    rig.transport.tempo_bpm = 97.0;

    let block = rig.run(BLOCK, &[], &[Event::note_on(7, 3, 64, 0.5)]);
    let echoed = block
        .events
        .iter()
        .find(|e| matches!(e.body, EventBody::NoteOn { .. }))
        .expect("the synth echoes note-ons on its event output");
    assert_eq!(echoed.offset, 7);
    assert!(matches!(
        echoed.body,
        EventBody::NoteOn {
            channel: 3,
            key: 64,
            ..
        }
    ));

    let param = |id: u32| {
        block.events.iter().find_map(|e| match e.body {
            EventBody::Param { id: got, value } if got == id => Some(value),
            _ => None,
        })
    };
    assert!(param(ids::SYNTH_METER).unwrap() > 0.0);
    assert!((param(ids::SYNTH_TEMPO_SEEN).unwrap() - 0.097).abs() < 1e-9);
    assert_eq!(param(ids::SYNTH_PLAYING_SEEN), Some(1.0));
    rig.finish(instance.as_mut());
}

#[test]
fn state_survives_save_restore_save_bit_for_bit() {
    let synth = descriptor(ids::SYNTH_UID);
    let mut first = instantiate(&synth).unwrap();
    let mut rig = Rig::new(first.as_mut(), &config());
    rig.run(
        BLOCK,
        &[],
        &[
            Event::param(0, ids::SYNTH_GAIN, 0.8125),
            Event::param(0, ids::SYNTH_MOD, 0.25),
        ],
    );
    rig.finish(first.as_mut());
    let saved = first.save_state().unwrap();
    assert_eq!(saved.plugin, synth.id);

    // Through the file container too, as the CLI does it.
    let from_file = StateBlob::from_bytes(&saved.to_bytes()).unwrap();

    let mut second = instantiate(&synth).unwrap();
    assert_eq!(second.param_value(ids::SYNTH_GAIN), Some(0.5));
    second.restore_state(&from_file).unwrap();
    // The controller learned the component's values through setComponentState.
    assert_eq!(second.param_value(ids::SYNTH_GAIN), Some(0.8125));
    assert_eq!(second.param_value(ids::SYNTH_MOD), Some(0.25));
    let mut events = Vec::new();
    second.poll_events(&mut events);
    assert!(events.contains(&PluginEvent::ParamValuesChanged));

    let resaved = second.save_state().unwrap();
    assert_eq!(
        resaved, saved,
        "save -> restore -> save must give an identical blob"
    );

    // The restored processor really uses the restored value: gain 0.8125 is x1.625.
    let mut rig = Rig::new(second.as_mut(), &config());
    let block = rig.run(BLOCK, &[], &[Event::note_on(0, 0, 69, 1.0)]);
    let expected = (std::f64::consts::TAU * 440.0 / 48_000.0).sin() as f32 * 0.25 * 1.625 * 0.875;
    assert!((block.outputs[0][1] - expected).abs() < 1e-6);
    rig.finish(second.as_mut());
}

#[test]
fn state_for_another_plugin_is_refused() {
    let mut synth = instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
    let mut delay = instantiate(&descriptor(ids::DELAY_UID)).unwrap();
    let delay_state = delay.save_state().unwrap();
    assert!(synth.restore_state(&delay_state).is_err());
    let mut corrupt = synth.save_state().unwrap();
    corrupt.data.truncate(6);
    assert!(synth.restore_state(&corrupt).is_err());
}

#[test]
fn an_auxiliary_output_can_be_activated() {
    let mut instance = instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
    instance
        .set_bus_active(BusDirection::Output, 1, true)
        .unwrap();
    assert!(instance
        .set_bus_active(BusDirection::Output, 5, true)
        .is_err());
    let mut rig = Rig::new(instance.as_mut(), &config());
    assert!(
        instance
            .set_bus_active(BusDirection::Output, 1, false)
            .is_err(),
        "not while active"
    );
    assert_eq!(instance.bus_layout().active_output_channels(), 4);

    // Channel 1 goes to the main bus, channel 2 to the aux bus.
    let block = rig.run(
        BLOCK,
        &[],
        &[Event::note_on(0, 0, 60, 1.0), Event::note_on(0, 1, 72, 1.0)],
    );
    assert_eq!(block.outputs.len(), 4);
    assert!(peak(&block.outputs[0]) > 0.0 && peak(&block.outputs[2]) > 0.0);
    assert_ne!(block.outputs[0], block.outputs[2]);
    rig.finish(instance.as_mut());
}

#[test]
fn delay_reports_latency_and_delays_by_exactly_that() {
    let mut instance = instantiate(&descriptor(ids::DELAY_UID)).unwrap();
    assert_eq!(instance.latency_samples(), 64);
    let layout = instance.bus_layout();
    assert_eq!(layout.audio_inputs.len(), 2);
    assert!(
        !layout.audio_inputs[1].active,
        "the sidechain starts inactive"
    );
    assert_eq!(layout.active_input_channels(), 2);

    let mut rig = Rig::new(instance.as_mut(), &config());
    let mut impulse = vec![0.0f32; BLOCK];
    impulse[3] = 1.0;
    let first = rig.run(BLOCK, &[impulse.clone(), impulse], &[]);
    assert_eq!(first.rt_violations, 0);
    assert_eq!(peak(&first.outputs[0]), 0.0);
    let second = rig.idle(BLOCK);
    assert_eq!(
        second.outputs[0][3], 1.0,
        "the impulse must come out 64 samples later"
    );
    assert_eq!(second.outputs[0].iter().filter(|&&s| s != 0.0).count(), 1);

    // Changing the delay parameter changes the latency, and the plugin says so.
    let delay_value = 200.0 / ids::DELAY_MAX as f64;
    rig.run(
        BLOCK,
        &[vec![0.0; BLOCK], vec![0.0; BLOCK]],
        &[Event::param(0, ids::DELAY_DELAY, delay_value)],
    );
    assert_eq!(instance.latency_samples(), 200);
    instance.set_param_value(ids::DELAY_DELAY, delay_value);
    let mut events = Vec::new();
    instance.poll_events(&mut events);
    assert!(
        events.contains(&PluginEvent::LatencyChanged),
        "got {events:?}"
    );
    rig.finish(instance.as_mut());
}

#[test]
fn delay_sidechain_bus_is_summed_when_active() {
    let mut instance = instantiate(&descriptor(ids::DELAY_UID)).unwrap();
    instance
        .set_bus_active(BusDirection::Input, 1, true)
        .unwrap();
    let mut rig = Rig::new(instance.as_mut(), &config());
    let silence = vec![0.0f32; BLOCK];
    let sidechain = vec![0.25f32; BLOCK];
    let block = rig.run(
        BLOCK,
        &[silence.clone(), silence, sidechain.clone(), sidechain],
        &[],
    );
    assert!(block.outputs[0].iter().all(|&s| s == 0.25));
    rig.finish(instance.as_mut());
}

#[test]
fn single_component_plugin_works_end_to_end() {
    let gain = descriptor(ids::GAIN_UID);
    let mut instance = instantiate(&gain).unwrap();
    assert_eq!(instance.params().len(), 1);
    assert_eq!(
        instance.param_text(ids::GAIN_GAIN, 0.5).as_deref(),
        Some("x1.00")
    );

    let mut rig = Rig::new(instance.as_mut(), &config());
    let input = vec![0.5f32; BLOCK];
    let unity = rig.run(BLOCK, &[input.clone(), input.clone()], &[]);
    assert!(unity.outputs[0].iter().all(|&s| s == 0.5));
    let doubled = rig.run(
        BLOCK,
        &[input.clone(), input],
        &[Event::param(0, ids::GAIN_GAIN, 1.0)],
    );
    assert!(doubled.outputs[1].iter().all(|&s| s == 1.0));
    assert_eq!(doubled.rt_violations, 0);
    rig.finish(instance.as_mut());

    let saved = instance.save_state().unwrap();
    let mut other = instantiate(&gain).unwrap();
    other.restore_state(&saved).unwrap();
    assert_eq!(other.param_value(ids::GAIN_GAIN), Some(1.0));
    assert_eq!(other.save_state().unwrap(), saved);
}

#[test]
fn reactivation_at_another_rate_and_oversized_blocks() {
    let mut instance = instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
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
    let expected = (std::f64::consts::TAU * 440.0 / 96_000.0).sin() as f32 * 0.25;
    assert!(
        (block.outputs[0][1] - expected).abs() < 1e-6,
        "the new sample rate must be in effect"
    );

    // A block larger than promised is refused and silenced rather than passed to the plugin.
    let too_big = rig.run(64, &[], &[]);
    assert_eq!(too_big.status, ProcessStatus::Failed);
    assert_eq!(peak(&too_big.outputs[0]), 0.0);
    rig.finish(instance.as_mut());
}

#[test]
fn many_instances_coexist_and_drop_cleanly() {
    let synth = descriptor(ids::SYNTH_UID);
    let mut instances: Vec<_> = (0..8).map(|_| instantiate(&synth).unwrap()).collect();
    let mut rigs: Vec<Rig> = instances
        .iter_mut()
        .map(|i| Rig::new(i.as_mut(), &config()))
        .collect();
    for (n, rig) in rigs.iter_mut().enumerate() {
        let block = rig.run(BLOCK, &[], &[Event::note_on(0, 0, 60 + n as u8, 1.0)]);
        assert!(peak(&block.outputs[0]) > 0.0);
    }
    // Drop half while still active (no deactivate): teardown must cope.
    for (rig, instance) in rigs.drain(..).zip(instances.iter_mut()).take(4) {
        rig.finish(instance.as_mut());
    }
    drop(instances);
}
