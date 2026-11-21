//! The phase-1 "spike" player: one plugin, straight to the audio device.
//!
//! This is deliberately not an engine. It exists to answer phase 1's questions (does the plugin
//! load, sound, show its editor, keep its state) with the smallest amount of code between the
//! plugin and the device. Phase 2's engine replaces it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use riddim_io::midi::MidiMessage;
use riddim_io::{AudioCallback, IoBlock};
use riddim_plugin::{
    Event, EventBody, EventBuffer, PluginProcessor, ProcessContext, ProcessStatus, TransportInfo,
};
use riddim_rt::atomic::AtomicF32;
use riddim_rt::planar::PlanarScratch;
use riddim_rt::spsc::{Consumer, Producer};

/// Main thread to audio thread.
pub enum ToAudio {
    Install(Box<dyn PluginProcessor>),
    Param {
        id: u32,
        value: f64,
    },
    /// Hand the processor back and go quiet.
    Release,
}

/// Audio thread to main thread.
pub enum FromAudio {
    /// The plugin changed one of its own parameters while processing.
    Param {
        id: u32,
        value: f64,
    },
    Released(Box<dyn PluginProcessor>),
}

/// Counters the audio thread publishes for the main thread to print.
#[derive(Default)]
pub struct Stats {
    pub blocks: AtomicU64,
    pub failed_blocks: AtomicU64,
    pub rt_violations: AtomicU64,
    pub midi_messages: AtomicU64,
    pub peak: AtomicF32,
}

/// A fixed arpeggio, generated sample-accurately from the running sample position.
pub struct Sequencer {
    step_frames: u64,
    gate_frames: u64,
    position: u64,
}

const ARPEGGIO: [u8; 8] = [60, 64, 67, 72, 76, 72, 67, 64];

impl Sequencer {
    pub fn new(sample_rate: f64) -> Self {
        let step_frames = (sample_rate * 0.25) as u64;
        Self {
            step_frames,
            gate_frames: step_frames * 4 / 5,
            position: 0,
        }
    }

    /// Appends the note events that fall inside the next `frames` frames.
    pub fn emit(&mut self, frames: usize, out: &mut EventBuffer) {
        let start = self.position;
        let end = start + frames as u64;
        let first_step = start / self.step_frames;
        for step in first_step.saturating_sub(1)..=end / self.step_frames {
            let key = ARPEGGIO[(step % ARPEGGIO.len() as u64) as usize];
            let on = step * self.step_frames;
            let off = on + self.gate_frames;
            if (start..end).contains(&off) {
                out.push(Event::note_off((off - start) as u32, 0, key));
            }
            if (start..end).contains(&on) {
                out.push(Event::note_on((on - start) as u32, 0, key, 0.8));
            }
        }
        out.sort();
        self.position = end;
    }
}

/// The audio callback: drains control and MIDI queues, runs the plugin, copies to the device.
pub struct Player {
    processor: Option<Box<dyn PluginProcessor>>,
    control: Consumer<ToAudio>,
    replies: Producer<FromAudio>,
    midi: Consumer<MidiMessage>,
    sequencer: Option<Sequencer>,
    events_in: EventBuffer,
    events_out: EventBuffer,
    plugin_inputs: Vec<Vec<f32>>,
    plugin_outputs: Vec<Vec<f32>>,
    scratch: PlanarScratch,
    transport: TransportInfo,
    max_block: usize,
    stats: Arc<Stats>,
}

impl Player {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        control: Consumer<ToAudio>,
        replies: Producer<FromAudio>,
        midi: Consumer<MidiMessage>,
        sequencer: Option<Sequencer>,
        plugin_input_channels: usize,
        plugin_output_channels: usize,
        sample_rate: f64,
        max_block: usize,
        stats: Arc<Stats>,
    ) -> Self {
        let mut transport = TransportInfo::stopped(sample_rate);
        transport.playing = true;
        Self {
            processor: None,
            control,
            replies,
            midi,
            sequencer,
            events_in: EventBuffer::with_capacity(1024),
            events_out: EventBuffer::with_capacity(1024),
            plugin_inputs: vec![vec![0.0; max_block]; plugin_input_channels],
            plugin_outputs: vec![vec![0.0; max_block]; plugin_output_channels],
            scratch: PlanarScratch::new(plugin_input_channels, plugin_output_channels),
            transport,
            max_block,
            stats,
        }
    }

    /// Runs the plugin for up to `max_block` frames and returns how the block went.
    fn run_plugin(&mut self, frames: usize) -> ProcessStatus {
        let Some(processor) = self.processor.as_mut() else {
            return ProcessStatus::Ok;
        };
        self.events_out.clear();
        let (inputs, outputs) = self.scratch.fill(
            self.plugin_inputs.iter().map(|c| c.as_slice()),
            self.plugin_outputs.iter_mut().map(|c| c.as_mut_slice()),
            frames,
        );
        let mut context = ProcessContext {
            frames,
            inputs,
            outputs,
            events_in: self.events_in.as_slice(),
            events_out: &mut self.events_out,
            transport: &self.transport,
        };
        processor.process(&mut context)
    }
}

impl AudioCallback for Player {
    fn process(&mut self, block: &mut IoBlock<'_>) {
        self.events_in.clear();
        while let Ok(message) = self.control.pop() {
            match message {
                ToAudio::Install(processor) => {
                    // Replacing a processor here would drop the old one on this thread; the
                    // main thread only ever installs once, after a Release if at all.
                    if self.processor.is_none() {
                        self.processor = Some(processor);
                    } else {
                        let _ = self.replies.push(FromAudio::Released(processor));
                    }
                }
                ToAudio::Param { id, value } => {
                    self.events_in.push(Event::param(0, id, value));
                }
                ToAudio::Release => {
                    if let Some(processor) = self.processor.take() {
                        let _ = self.replies.push(FromAudio::Released(processor));
                    }
                }
            }
        }
        // Live MIDI plays at the start of the block it arrived before: within one buffer.
        while let Ok(message) = self.midi.pop() {
            if let Some(event) = Event::from_midi(0, &message.bytes[..message.len as usize]) {
                self.events_in.push(event);
                self.stats.midi_messages.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut done = 0;
        let mut peak = 0.0f32;
        while done < block.frames {
            let frames = (block.frames - done).min(self.max_block);
            if let Some(sequencer) = self.sequencer.as_mut() {
                sequencer.emit(frames, &mut self.events_in);
            }
            let status = self.run_plugin(frames);
            if status == ProcessStatus::Failed {
                self.stats.failed_blocks.fetch_add(1, Ordering::Relaxed);
            }
            let playing = self.processor.is_some() && status == ProcessStatus::Ok;

            // Plugin main output to the device: mono fans out, extra channels are dropped.
            let sources = self.plugin_outputs.len();
            for (c, output) in block.outputs.iter_mut().enumerate() {
                let dst = &mut output[done..done + frames];
                if playing && sources > 0 && c < 2 {
                    let src = &self.plugin_outputs[c.min(sources - 1)][..frames];
                    dst.copy_from_slice(src);
                    for sample in src {
                        peak = peak.max(sample.abs());
                    }
                } else {
                    dst.fill(0.0);
                }
            }
            for event in self.events_out.as_slice() {
                if let EventBody::Param { id, value } = event.body {
                    let _ = self.replies.push(FromAudio::Param { id, value });
                }
            }
            self.events_in.clear();
            self.transport.position_samples += frames as i64;
            self.transport.continuous_samples += frames as i64;
            self.transport.position_quarters =
                self.transport.position_samples as f64 / self.transport.sample_rate * 2.0;
            done += frames;
        }

        self.stats.blocks.fetch_add(1, Ordering::Relaxed);
        if peak > self.stats.peak.load(Ordering::Relaxed) {
            self.stats.peak.store(peak, Ordering::Relaxed);
        }
        self.stats
            .rt_violations
            .store(riddim_rt::guard::thread_violations(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequencer_is_sample_accurate_across_block_boundaries() {
        // Collect absolute note-on positions using awkward block sizes.
        let mut sequencer = Sequencer::new(48_000.0);
        let mut events = EventBuffer::with_capacity(64);
        let mut ons = Vec::new();
        let mut offs = Vec::new();
        let mut position = 0u64;
        for frames in [100usize, 7, 4096, 333, 12_000, 12_000, 12_000]
            .into_iter()
            .cycle()
            .take(40)
        {
            events.clear();
            sequencer.emit(frames, &mut events);
            for event in events.as_slice() {
                assert!((event.offset as usize) < frames);
                match event.body {
                    EventBody::NoteOn { .. } => ons.push(position + event.offset as u64),
                    EventBody::NoteOff { .. } => offs.push(position + event.offset as u64),
                    _ => {}
                }
            }
            position += frames as u64;
        }
        assert!(ons.len() > 8);
        for (i, on) in ons.iter().enumerate() {
            assert_eq!(
                *on,
                i as u64 * 12_000,
                "note {i} must start on its step exactly"
            );
        }
        for (i, off) in offs.iter().enumerate() {
            assert_eq!(*off, i as u64 * 12_000 + 9_600);
        }
    }
}
