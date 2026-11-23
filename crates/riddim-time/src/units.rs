//! The clocks and the arithmetic between them.

use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

/// Superclock units in one second: 282,240,000 = 2^9 · 3^2 · 5^4 · 7^2.
///
/// Divisible by 8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
/// 352800 and 384000.
pub const SUPERCLOCK_PER_SECOND: i64 = 282_240_000;

/// Ticks in one quarter note: 3,628,800 = 10! = 2^8 · 3^4 · 5^2 · 7.
///
/// Divisible by 96, 120, 192, 240, 384, 480, 960 and 1920 (MIDI file resolutions) and by every
/// tuplet up to 10.
pub const TICKS_PER_BEAT: i64 = 3_628_800;

/// `a * b / c`, rounded towards negative infinity, without intermediate overflow.
#[inline]
pub fn mul_div_floor(a: i64, b: i64, c: i64) -> i64 {
    debug_assert!(c > 0);
    (a as i128 * b as i128).div_euclid(c as i128) as i64
}

/// `a * b / c`, rounded towards positive infinity, without intermediate overflow.
#[inline]
pub fn mul_div_ceil(a: i64, b: i64, c: i64) -> i64 {
    debug_assert!(c > 0);
    let product = a as i128 * b as i128;
    let c = c as i128;
    (product.div_euclid(c) + (product.rem_euclid(c) != 0) as i128) as i64
}

/// `a * b / c`, rounded to nearest (ties away from negative infinity).
#[inline]
pub fn mul_div_round(a: i64, b: i64, c: i64) -> i64 {
    debug_assert!(c > 0);
    let c = c as i128;
    (a as i128 * b as i128 * 2 + c).div_euclid(2 * c) as i64
}

macro_rules! clock_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        #[cfg_attr(feature = "serde", serde(transparent))]
        pub struct $name(pub i64);

        impl $name {
            pub const ZERO: $name = $name(0);
        }

        impl Add for $name {
            type Output = $name;
            #[inline]
            fn add(self, rhs: $name) -> $name {
                $name(self.0 + rhs.0)
            }
        }

        impl Sub for $name {
            type Output = $name;
            #[inline]
            fn sub(self, rhs: $name) -> $name {
                $name(self.0 - rhs.0)
            }
        }

        impl AddAssign for $name {
            #[inline]
            fn add_assign(&mut self, rhs: $name) {
                self.0 += rhs.0;
            }
        }

        impl SubAssign for $name {
            #[inline]
            fn sub_assign(&mut self, rhs: $name) {
                self.0 -= rhs.0;
            }
        }

        impl Neg for $name {
            type Output = $name;
            #[inline]
            fn neg(self) -> $name {
                $name(-self.0)
            }
        }
    };
}

clock_type!(
    /// A point or span of audio time, in superclock units.
    Superclock
);

clock_type!(
    /// A point or span of musical time, in ticks.
    Ticks
);

impl Superclock {
    /// Whole seconds (for tests and display; real positions come from samples or ticks).
    pub const fn from_seconds(seconds: i64) -> Superclock {
        Superclock(seconds * SUPERCLOCK_PER_SECOND)
    }

    /// Seconds as a float, for display only.
    pub fn as_seconds_f64(self) -> f64 {
        self.0 as f64 / SUPERCLOCK_PER_SECOND as f64
    }
}

impl Ticks {
    pub const fn from_beats(beats: i64) -> Ticks {
        Ticks(beats * TICKS_PER_BEAT)
    }

    /// `numerator / denominator` beats, exactly if the denominator divides [`TICKS_PER_BEAT`].
    pub const fn from_beat_fraction(numerator: i64, denominator: i64) -> Ticks {
        Ticks(numerator * TICKS_PER_BEAT / denominator)
    }

    /// Converts a position at another resolution (a MIDI file's ticks per quarter note).
    pub fn from_ppq(ticks: i64, ppq: u32) -> Ticks {
        Ticks(mul_div_round(ticks, TICKS_PER_BEAT, ppq.max(1) as i64))
    }

    /// Beats as a float: for plugin transport reports and display only.
    pub fn as_beats_f64(self) -> f64 {
        self.0 as f64 / TICKS_PER_BEAT as f64
    }
}

/// Samples at `rate` to superclock. Exact for every rate that divides
/// [`SUPERCLOCK_PER_SECOND`]; rounded down otherwise.
#[inline]
pub fn samples_to_superclock(samples: i64, rate: u32) -> Superclock {
    Superclock(mul_div_floor(samples, SUPERCLOCK_PER_SECOND, rate as i64))
}

/// Superclock to the sample that contains it at `rate` (rounds down).
#[inline]
pub fn superclock_to_samples(time: Superclock, rate: u32) -> i64 {
    mul_div_floor(time.0, rate as i64, SUPERCLOCK_PER_SECOND)
}

/// A tempo, stored as the exact superclock length of one beat.
///
/// Storing the period rather than beats-per-minute keeps every conversion in integers. 120 bpm
/// is exactly 141,120,000 units per beat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Tempo {
    superclock_per_beat: i64,
}

impl Tempo {
    /// Slowest and fastest tempos accepted, in beats per minute.
    pub const MIN_BPM: f64 = 1.0;
    pub const MAX_BPM: f64 = 2000.0;

    /// The tempo closest to `bpm` quarter notes per minute. Out-of-range values are clamped.
    pub fn from_bpm(bpm: f64) -> Tempo {
        let bpm = if bpm.is_finite() {
            bpm.clamp(Self::MIN_BPM, Self::MAX_BPM)
        } else {
            120.0
        };
        Tempo {
            superclock_per_beat: (SUPERCLOCK_PER_SECOND as f64 * 60.0 / bpm).round() as i64,
        }
    }

    /// A tempo from the exact length of a beat. `None` if the length is not positive.
    pub fn from_superclock_per_beat(superclock_per_beat: i64) -> Option<Tempo> {
        (superclock_per_beat > 0).then_some(Tempo {
            superclock_per_beat,
        })
    }

    #[inline]
    pub fn superclock_per_beat(self) -> i64 {
        self.superclock_per_beat
    }

    /// Beats per minute as a float: for plugin transport reports and display only.
    pub fn bpm(self) -> f64 {
        SUPERCLOCK_PER_SECOND as f64 * 60.0 / self.superclock_per_beat as f64
    }
}

impl Default for Tempo {
    fn default() -> Self {
        Tempo::from_bpm(120.0)
    }
}

/// A place on the timeline, in whichever clock it is anchored to.
///
/// Musical material (MIDI clips, notes, tempo changes) is anchored to beats and moves when the
/// tempo changes. Recorded audio is anchored to audio time and does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Position {
    Beats(Ticks),
    Time(Superclock),
}

impl Position {
    pub const START: Position = Position::Beats(Ticks(0));
}

impl Default for Position {
    fn default() -> Self {
        Position::START
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_rates_divide_the_superclock() {
        for rate in [
            8000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
            352_800, 384_000,
        ] {
            assert_eq!(SUPERCLOCK_PER_SECOND % rate, 0, "{rate} Hz");
        }
    }

    #[test]
    fn midi_resolutions_and_tuplets_divide_the_beat() {
        for ppq in [96, 120, 192, 240, 384, 480, 960, 1920] {
            assert_eq!(TICKS_PER_BEAT % ppq, 0, "{ppq} ppq");
        }
        for tuplet in 1..=10 {
            assert_eq!(TICKS_PER_BEAT % tuplet, 0, "{tuplet}-tuplet");
        }
    }

    #[test]
    fn samples_round_trip_exactly_at_standard_rates() {
        for rate in [44_100u32, 48_000, 96_000, 192_000] {
            for samples in [0i64, 1, 127, 48_000, 1_234_567_891, -1, -48_001] {
                let clock = samples_to_superclock(samples, rate);
                assert_eq!(
                    superclock_to_samples(clock, rate),
                    samples,
                    "{samples} @ {rate}"
                );
            }
        }
    }

    #[test]
    fn rounding_helpers_agree_with_exact_arithmetic() {
        assert_eq!(mul_div_floor(7, 3, 2), 10);
        assert_eq!(mul_div_ceil(7, 3, 2), 11);
        assert_eq!(mul_div_round(7, 3, 2), 11);
        assert_eq!(mul_div_floor(-7, 3, 2), -11);
        assert_eq!(mul_div_ceil(-7, 3, 2), -10);
        assert_eq!(mul_div_round(5, 1, 3), 2);
        assert_eq!(mul_div_round(4, 1, 3), 1);
        // No overflow where a 64-bit product would.
        assert_eq!(mul_div_floor(i64::MAX / 2, 4, 4), i64::MAX / 2);
    }

    #[test]
    fn tempo_is_exact_for_round_values_and_clamped() {
        assert_eq!(Tempo::from_bpm(120.0).superclock_per_beat(), 141_120_000);
        assert_eq!(
            Tempo::from_bpm(60.0).superclock_per_beat(),
            SUPERCLOCK_PER_SECOND
        );
        assert!((Tempo::from_bpm(97.3).bpm() - 97.3).abs() < 1e-6);
        assert_eq!(Tempo::from_bpm(0.0), Tempo::from_bpm(Tempo::MIN_BPM));
        assert_eq!(Tempo::from_bpm(f64::NAN), Tempo::default());
        assert!(Tempo::from_superclock_per_beat(0).is_none());
    }

    #[test]
    fn ppq_conversion() {
        assert_eq!(Ticks::from_ppq(480, 480), Ticks::from_beats(1));
        assert_eq!(Ticks::from_ppq(240, 960), Ticks::from_beat_fraction(1, 4));
        assert_eq!(Ticks::from_beat_fraction(1, 3).0 * 3, TICKS_PER_BEAT);
    }
}
