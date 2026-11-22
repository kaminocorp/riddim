//! A plugin hosted in a child process: it must behave as it does in process, and when its
//! process dies or hangs, the host must be told and left standing.

#![cfg(unix)]

use std::path::Path;
use std::time::{Duration, Instant};

use riddim_plugin::bridge;
use riddim_plugin::{
    ActivateConfig, BusDirection, Event, EventBuffer, PluginDescriptor, PluginEvent, PluginFormat,
    PluginId, PluginInstance, PluginKind, PluginProcessor, ProcessContext, ProcessMode,
    ProcessStatus, TransportInfo,
};
use riddim_test_support::{ids, testplug_bundle};

const BLOCK: usize = 128;

fn descriptor(uid: &str) -> PluginDescriptor {
    PluginDescriptor {
        id: PluginId::new(PluginFormat::Vst3, uid),
        name: String::new(),
        vendor: String::new(),
        version: String::new(),
        kind: PluginKind::Other,
        categories: Vec::new(),
        path: Some(testplug_bundle()),
    }
}

fn bridged(uid: &str) -> bridge::BridgedInstance {
    bridge::instantiate(
        &descriptor(uid),
        Path::new(env!("CARGO_BIN_EXE_riddim-bridge")),
    )
    .expect("the bridge starts and loads the plugin")
}

fn config() -> ActivateConfig {
    ActivateConfig {
        sample_rate: 48_000.0,
        max_block: BLOCK as u32,
        mode: ProcessMode::Realtime,
    }
}

/// Runs `blocks` blocks through a processor, with `events` in the first one and `input` on
/// every input channel, and returns the output channels end to end plus the events it sent.
fn run(
    processor: &mut dyn PluginProcessor,
    channels: (usize, usize),
    blocks: usize,
    events: &[Event],
    input: f32,
) -> (Vec<Vec<f32>>, Vec<Event>, ProcessStatus) {
    let mut output = vec![Vec::new(); channels.1];
    let mut sent = Vec::new();
    let mut events_out = EventBuffer::with_capacity(256);
    let transport = TransportInfo::stopped(48_000.0);
    let mut status = ProcessStatus::Ok;
    for block in 0..blocks {
        let ins = vec![vec![input; BLOCK]; channels.0];
        let mut outs = vec![vec![9.0f32; BLOCK]; channels.1];
        let in_refs: Vec<&[f32]> = ins.iter().map(|c| c.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|c| c.as_mut_slice()).collect();
        events_out.clear();
        status = processor.process(&mut ProcessContext {
            frames: BLOCK,
            inputs: &in_refs,
            outputs: &mut out_refs,
            events_in: if block == 0 { events } else { &[] },
            events_out: &mut events_out,
            transport: &transport,
        });
        sent.extend_from_slice(events_out.as_slice());
        for (all, this) in output.iter_mut().zip(&outs) {
            all.extend_from_slice(this);
        }
    }
    (output, sent, status)
}

#[test]
fn a_bridged_plugin_behaves_as_it_does_in_process() {
    let mut local = riddim_plugin::host::instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
    let mut remote = bridged(ids::SYNTH_UID);

    // The same buses and the same parameters, described across the process boundary.
    assert_eq!(remote.bus_layout(), local.bus_layout());
    assert_eq!(remote.params(), local.params());
    assert_eq!(
        remote.param_value(ids::SYNTH_GAIN),
        local.param_value(ids::SYNTH_GAIN)
    );
    assert_eq!(
        remote.param_text(ids::SYNTH_GAIN, 0.5),
        local.param_text(ids::SYNTH_GAIN, 0.5)
    );
    // Switching the second output on works from here too.
    for instance in [&mut *local, &mut remote as &mut dyn PluginInstance] {
        instance
            .set_bus_active(BusDirection::Output, 1, true)
            .unwrap();
    }
    assert_eq!(remote.bus_layout(), local.bus_layout());
    assert_eq!(remote.bus_layout().active_output_channels(), 4);

    // A note on each output, a parameter change with an offset, and the same audio comes
    // back, bit for bit, with the same events.
    let events = [
        Event::note_on(0, 0, 60, 1.0),
        Event::note_on(17, 1, 67, 0.5),
        Event::param(64, ids::SYNTH_GAIN, 0.25),
        Event::note_off(100, 0, 60),
    ];
    let mut local_audio = local.activate(&config()).unwrap();
    let mut remote_audio = remote.activate(&config()).unwrap();
    assert!(remote.is_active());
    let here = run(&mut *local_audio, (0, 4), 8, &events, 0.0);
    let there = run(&mut *remote_audio, (0, 4), 8, &events, 0.0);
    assert!(here.0[0].iter().any(|s| *s != 0.0) && here.0[2].iter().any(|s| *s != 0.0));
    assert!(here.0 == there.0, "the bridged plugin's audio differs");
    assert_eq!(here.1, there.1, "the bridged plugin's events differ");
    assert!(!there.1.is_empty(), "the synth echoes its notes");
    assert_eq!(there.2, ProcessStatus::Ok);

    // State crosses the boundary and comes back the same.
    remote.set_param_value(ids::SYNTH_MOD, 0.875);
    local.set_param_value(ids::SYNTH_MOD, 0.875);
    let saved = remote.save_state().unwrap();
    assert_eq!(saved.to_bytes(), local.save_state().unwrap().to_bytes());
    remote.deactivate(remote_audio);
    local.deactivate(local_audio);
    assert!(!remote.is_active());

    // Restored into a fresh bridged instance, it is what it is when restored in process.
    let mut fresh = bridged(ids::SYNTH_UID);
    fresh.restore_state(&saved).unwrap();
    let mut fresh_local = riddim_plugin::host::instantiate(&descriptor(ids::SYNTH_UID)).unwrap();
    fresh_local.restore_state(&saved).unwrap();
    for id in [ids::SYNTH_GAIN, ids::SYNTH_MOD] {
        assert_eq!(fresh.param_value(id), fresh_local.param_value(id));
    }
    assert_eq!(
        fresh.save_state().unwrap().to_bytes(),
        fresh_local.save_state().unwrap().to_bytes()
    );

    // It can be activated again, at another block size.
    let mut again = remote
        .activate(&ActivateConfig {
            max_block: 512,
            ..config()
        })
        .unwrap();
    let (audio, _, status) = run(
        &mut *again,
        (0, 4),
        2,
        &[Event::note_on(0, 0, 72, 1.0)],
        0.0,
    );
    assert_eq!(status, ProcessStatus::Ok);
    assert!(audio[0].iter().any(|s| *s != 0.0));
    remote.deactivate(again);
}

#[test]
fn an_effect_with_latency_and_notifications_works_across_the_bridge() {
    let mut remote = bridged(ids::DELAY_UID);
    let mut audio = remote.activate(&config()).unwrap();
    assert_eq!(remote.latency_samples(), 64);
    // An impulse train in, the same delayed by 64 samples out.
    let (out, _, _) = run(&mut *audio, (2, 2), 2, &[], 0.5);
    assert!(out[0][..64].iter().all(|s| *s == 0.0));
    assert!(out[0][64..].iter().all(|s| *s == 0.5));

    // The plugin announces a latency change; the host hears of it and of the new figure.
    remote.set_param_value(ids::DELAY_DELAY, 200.0 / ids::DELAY_MAX as f64);
    run(
        &mut *audio,
        (2, 2),
        1,
        &[Event::param(
            0,
            ids::DELAY_DELAY,
            200.0 / ids::DELAY_MAX as f64,
        )],
        0.5,
    );
    let mut events = Vec::new();
    remote.poll_events(&mut events);
    assert!(events.contains(&PluginEvent::LatencyChanged), "{events:?}");
    assert_eq!(remote.latency_samples(), 200);
    remote.deactivate(audio);
}

fn dies(command: f64, what: &str) {
    let mut remote = bridged(ids::SYNTH_UID);
    let mut audio = remote.activate(&config()).unwrap();
    let (out, _, status) = run(
        &mut *audio,
        (0, 2),
        2,
        &[Event::note_on(0, 0, 60, 1.0)],
        0.0,
    );
    assert_eq!(status, ProcessStatus::Ok);
    assert!(out[0].iter().any(|s| *s != 0.0));

    // The plugin goes wrong. (For the abort the call itself cannot be answered.)
    remote.set_param_value(ids::COMMAND, command);

    // The audio side notices within its patience, gives silence, and says the block failed;
    // it does not wait forever, and it does not panic.
    let started = Instant::now();
    let (out, _, status) = run(&mut *audio, (0, 2), 3, &[], 0.0);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{what}: the audio side waited {:?}",
        started.elapsed()
    );
    assert_eq!(status, ProcessStatus::Failed, "{what}");
    assert!(
        out[0][BLOCK..].iter().all(|s| *s == 0.0),
        "{what}: a dead plugin must be silent"
    );
    // The control side reports it, once.
    let mut events = Vec::new();
    remote.poll_events(&mut events);
    assert_eq!(events, vec![PluginEvent::Crashed], "{what}");
    events.clear();
    remote.poll_events(&mut events);
    assert!(events.is_empty(), "{what}");
    // Everything else it is asked fails politely, and letting go of it works.
    assert!(remote.save_state().is_err());
    assert!(remote.params().is_empty());
    remote.deactivate(audio);
    drop(remote);
}

#[test]
fn a_plugin_that_aborts_takes_only_its_own_process_down() {
    dies(ids::COMMAND_ABORT, "abort");
}

#[test]
fn a_plugin_that_hangs_is_given_up_on() {
    dies(ids::COMMAND_HANG, "hang");
}

#[test]
fn a_bridge_that_cannot_load_its_plugin_says_so() {
    let mut missing = descriptor("00000000000000000000000000000000");
    missing.path = Some("/nonexistent/Ghost.vst3".into());
    let error = bridge::instantiate(&missing, Path::new(env!("CARGO_BIN_EXE_riddim-bridge")))
        .err()
        .expect("a missing plugin cannot load");
    assert!(!error.to_string().is_empty());
    assert!(bridge::instantiate(
        &descriptor(ids::SYNTH_UID),
        Path::new("/nonexistent/riddim-bridge")
    )
    .is_err());
}
