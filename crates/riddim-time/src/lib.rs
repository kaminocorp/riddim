//! Integer time.
//!
//! Riddim never accumulates time in floating point. There are three clocks, all integers:
//!
//! * [`Superclock`]: audio time, independent of sample rate. One second is
//!   [`SUPERCLOCK_PER_SECOND`] units, a number every common sample rate divides exactly, so a
//!   position converts to and from samples at 44.1, 48, 88.2, 96, 176.4 or 192 kHz without
//!   error. (The idea, and the constant, are Ardour's.)
//! * [`Ticks`]: musical time. One quarter-note beat is [`TICKS_PER_BEAT`] ticks, fine enough to
//!   address single samples at any sane tempo and divisible by every common subdivision and by
//!   the usual MIDI file resolutions.
//! * Samples (`i64`) at the session rate: what the audio thread counts in.
//!
//! A [`TempoMap`] relates the first two; [`samples_to_superclock`] and
//! [`superclock_to_samples`] relate the first and third. Maps are immutable values: editing one
//! makes a new one, which is what lets the audio thread read a snapshot with no locking.

mod map;
mod units;

pub use map::{Bbt, Meter, MeterPoint, TempoError, TempoMap, TempoPoint};
pub use units::{
    mul_div_ceil, mul_div_floor, mul_div_round, samples_to_superclock, superclock_to_samples,
    Position, Superclock, Tempo, Ticks, SUPERCLOCK_PER_SECOND, TICKS_PER_BEAT,
};
