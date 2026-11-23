//! The per-block processing contract.

use crate::event::{Event, EventBuffer};

/// Transport state for one block, in the shape plugin formats want it.
///
/// This is the one place floating-point time is allowed: it is a per-block *report* derived from
/// the engine's integer clocks, never accumulated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportInfo {
    pub playing: bool,
    pub recording: bool,
    pub looping: bool,
    pub sample_rate: f64,
    /// Position of the first frame of this block on the project timeline, in samples.
    pub position_samples: i64,
    /// Samples processed since the stream started; keeps counting while stopped.
    pub continuous_samples: i64,
    /// Position in quarter notes.
    pub position_quarters: f64,
    /// Start of the current bar in quarter notes.
    pub bar_start_quarters: f64,
    pub loop_start_quarters: f64,
    pub loop_end_quarters: f64,
    pub tempo_bpm: f64,
    pub time_sig_numerator: u16,
    pub time_sig_denominator: u16,
}

impl TransportInfo {
    /// A stopped transport at 120 bpm, 4/4.
    pub fn stopped(sample_rate: f64) -> Self {
        Self {
            playing: false,
            recording: false,
            looping: false,
            sample_rate,
            position_samples: 0,
            continuous_samples: 0,
            position_quarters: 0.0,
            bar_start_quarters: 0.0,
            loop_start_quarters: 0.0,
            loop_end_quarters: 0.0,
            tempo_bpm: 120.0,
            time_sig_numerator: 4,
            time_sig_denominator: 4,
        }
    }
}

/// Everything a processor needs for one block.
///
/// `inputs` and `outputs` are flat lists of channels: the channels of the first *active* bus,
/// then those of the next, in the order of [`BusLayout`](crate::BusLayout). Every channel slice
/// is exactly `frames` long. Inputs and outputs never alias.
pub struct ProcessContext<'a> {
    pub frames: usize,
    pub inputs: &'a [&'a [f32]],
    pub outputs: &'a mut [&'a mut [f32]],
    /// Events for the plugin, sorted by offset.
    pub events_in: &'a [Event],
    /// Events from the plugin (MIDI out, parameter changes it made itself).
    pub events_out: &'a mut EventBuffer,
    pub transport: &'a TransportInfo,
}

/// What the processor reports after a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStatus {
    /// Output was produced normally.
    Ok,
    /// The plugin returned an error; outputs have been silenced by the host wrapper.
    Failed,
}
