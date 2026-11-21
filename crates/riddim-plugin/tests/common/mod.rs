//! A small harness for driving a plugin processor block by block from tests.

#![allow(dead_code)]

use riddim_plugin::scan::{scan_in_process, ScanTarget};
use riddim_plugin::{
    ActivateConfig, Event, EventBuffer, PluginDescriptor, PluginFormat, PluginInstance,
    PluginProcessor, ProcessContext, ProcessMode, ProcessStatus, TransportInfo,
};

pub const RATE: f64 = 48_000.0;
pub const BLOCK: usize = 64;

/// Descriptors of the fixture plugins, scanned in process (they are ours, they do not crash).
pub fn fixture_descriptors() -> Vec<PluginDescriptor> {
    let target = ScanTarget {
        format: PluginFormat::Vst3,
        path: Some(riddim_test_support::testplug_bundle()),
    };
    scan_in_process(&target).expect("scanning the fixture bundle")
}

pub fn descriptor(uid: &str) -> PluginDescriptor {
    fixture_descriptors()
        .into_iter()
        .find(|d| d.id.uid == uid)
        .unwrap_or_else(|| panic!("fixture plugin {uid} not found"))
}

pub fn config() -> ActivateConfig {
    ActivateConfig {
        sample_rate: RATE,
        max_block: BLOCK as u32,
        mode: ProcessMode::Realtime,
    }
}

/// The result of one processed block.
pub struct Block {
    pub status: ProcessStatus,
    pub outputs: Vec<Vec<f32>>,
    pub events: Vec<Event>,
    /// Allocations and frees our side of the host made during the block.
    pub rt_violations: u64,
}

/// Owns the buffers a processor needs and runs it under the real-time guard.
pub struct Rig {
    pub processor: Box<dyn PluginProcessor>,
    in_channels: usize,
    out_channels: usize,
    pub transport: TransportInfo,
}

impl Rig {
    pub fn new(instance: &mut dyn PluginInstance, config: &ActivateConfig) -> Rig {
        let processor = instance.activate(config).expect("activate");
        let layout = instance.bus_layout();
        Rig {
            processor,
            in_channels: layout.active_input_channels(),
            out_channels: layout.active_output_channels(),
            transport: TransportInfo::stopped(config.sample_rate),
        }
    }

    pub fn run(&mut self, frames: usize, inputs: &[Vec<f32>], events: &[Event]) -> Block {
        assert_eq!(
            inputs.len(),
            self.in_channels,
            "wrong number of input channels"
        );
        let mut outputs = vec![vec![f32::NAN; frames]; self.out_channels];
        let mut events_out = EventBuffer::with_capacity(256);
        let input_refs: Vec<&[f32]> = inputs.iter().map(|c| &c[..frames]).collect();
        let status;
        let rt_violations;
        {
            let mut output_refs: Vec<&mut [f32]> =
                outputs.iter_mut().map(|c| c.as_mut_slice()).collect();
            let mut context = ProcessContext {
                frames,
                inputs: &input_refs,
                outputs: &mut output_refs,
                events_in: events,
                events_out: &mut events_out,
                transport: &self.transport,
            };
            let processor = &mut self.processor;
            let (s, v) = riddim_rt::guard::rt_scope_counted(|| processor.process(&mut context));
            status = s;
            rt_violations = v;
        }
        self.transport.continuous_samples += frames as i64;
        Block {
            status,
            outputs,
            events: events_out.as_slice().to_vec(),
            rt_violations,
        }
    }

    /// Runs a silent-input block with no events.
    pub fn idle(&mut self, frames: usize) -> Block {
        let inputs = vec![vec![0.0; frames]; self.in_channels];
        self.run(frames, &inputs, &[])
    }

    pub fn finish(self, instance: &mut dyn PluginInstance) {
        instance.deactivate(self.processor);
    }
}

pub fn peak(buffer: &[f32]) -> f32 {
    buffer.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}
