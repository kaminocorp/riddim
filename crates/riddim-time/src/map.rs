//! The tempo and meter map.

use std::fmt;

use crate::units::{
    mul_div_ceil, mul_div_floor, samples_to_superclock, superclock_to_samples, Position,
    Superclock, Tempo, Ticks, TICKS_PER_BEAT,
};

/// A tempo that takes effect at a musical position and holds until the next point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TempoPoint {
    pub at: Ticks,
    pub tempo: Tempo,
}

/// A time signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Meter {
    pub numerator: u16,
    /// A power of two from 1 to 64.
    pub denominator: u16,
}

impl Meter {
    pub const COMMON: Meter = Meter {
        numerator: 4,
        denominator: 4,
    };

    pub fn new(numerator: u16, denominator: u16) -> Option<Meter> {
        let valid =
            (1..=64).contains(&numerator) && denominator.is_power_of_two() && denominator <= 64;
        valid.then_some(Meter { numerator, denominator })
    }

    /// Length of one beat of this meter (one denominator note).
    #[inline]
    pub fn ticks_per_beat(self) -> i64 {
        TICKS_PER_BEAT * 4 / self.denominator as i64
    }

    #[inline]
    pub fn ticks_per_bar(self) -> i64 {
        self.ticks_per_beat() * self.numerator as i64
    }
}

impl Default for Meter {
    fn default() -> Self {
        Meter::COMMON
    }
}

/// A meter that takes effect at a bar line and holds until the next point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MeterPoint {
    pub at: Ticks,
    pub meter: Meter,
}

/// A position as bar, beat and tick. Bars and beats count from 1; bars before the start of the
/// timeline (a count-in) are 0, -1 and so on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bbt {
    pub bar: i64,
    pub beat: u32,
    pub tick: i64,
}

/// Why a set of points is not a valid map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TempoError {
    /// There must be a tempo and a meter at tick 0.
    MissingOrigin,
    /// Points must be strictly increasing in position.
    Unordered,
    InvalidMeter,
    /// A meter change must fall on a bar line of the meter before it.
    MeterOffBarLine {
        at: Ticks,
    },
}

impl fmt::Display for TempoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TempoError::MissingOrigin => {
                write!(f, "the map needs a tempo and a meter at the start")
            }
            TempoError::Unordered => {
                write!(f, "tempo and meter points must be in increasing order")
            }
            TempoError::InvalidMeter => write!(f, "invalid time signature"),
            TempoError::MeterOffBarLine { at } => {
                write!(f, "the meter change at tick {} is not on a bar line", at.0)
            }
        }
    }
}

impl std::error::Error for TempoError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TempoSegment {
    start: Ticks,
    start_time: Superclock,
    tempo: Tempo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MeterSegment {
    start: Ticks,
    /// Zero-based index of the bar that starts here.
    first_bar: i64,
    meter: Meter,
}

/// The relation between musical time and audio time, and where the bar lines are.
///
/// Tempos are constant between points (no ramps yet). The map is an immutable value: the
/// editing methods return a new map. Conversions are pure integer arithmetic and never
/// allocate, so the audio thread can use a shared snapshot freely.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "MapPoints", into = "MapPoints"))]
pub struct TempoMap {
    tempos: Vec<TempoSegment>,
    meters: Vec<MeterSegment>,
}

/// The serialised form: just the points. Segment starts are derived.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct MapPoints {
    tempos: Vec<TempoPoint>,
    meters: Vec<MeterPoint>,
}

impl From<TempoMap> for MapPoints {
    fn from(map: TempoMap) -> Self {
        MapPoints {
            tempos: map.tempo_points(),
            meters: map.meter_points(),
        }
    }
}

impl TryFrom<MapPoints> for TempoMap {
    type Error = TempoError;
    fn try_from(points: MapPoints) -> Result<Self, TempoError> {
        TempoMap::from_points(&points.tempos, &points.meters)
    }
}

impl Default for TempoMap {
    fn default() -> Self {
        TempoMap::new(Tempo::default(), Meter::COMMON)
    }
}

impl TempoMap {
    /// A map with one tempo and one meter throughout.
    pub fn new(tempo: Tempo, meter: Meter) -> TempoMap {
        TempoMap {
            tempos: vec![TempoSegment {
                start: Ticks::ZERO,
                start_time: Superclock::ZERO,
                tempo,
            }],
            meters: vec![MeterSegment {
                start: Ticks::ZERO,
                first_bar: 0,
                meter,
            }],
        }
    }

    /// Builds a map from points, checking they are well formed.
    pub fn from_points(
        tempos: &[TempoPoint],
        meters: &[MeterPoint],
    ) -> Result<TempoMap, TempoError> {
        if tempos.first().map(|p| p.at) != Some(Ticks::ZERO)
            || meters.first().map(|p| p.at) != Some(Ticks::ZERO)
        {
            return Err(TempoError::MissingOrigin);
        }
        if tempos.windows(2).any(|w| w[0].at >= w[1].at)
            || meters.windows(2).any(|w| w[0].at >= w[1].at)
        {
            return Err(TempoError::Unordered);
        }

        let mut tempo_segments: Vec<TempoSegment> = Vec::with_capacity(tempos.len());
        for point in tempos {
            let start_time = match tempo_segments.last() {
                None => Superclock::ZERO,
                Some(previous) => Superclock(
                    previous.start_time.0
                        + mul_div_floor(
                            point.at.0 - previous.start.0,
                            previous.tempo.superclock_per_beat(),
                            TICKS_PER_BEAT,
                        ),
                ),
            };
            tempo_segments.push(TempoSegment {
                start: point.at,
                start_time,
                tempo: point.tempo,
            });
        }

        let mut meter_segments: Vec<MeterSegment> = Vec::with_capacity(meters.len());
        for point in meters {
            if Meter::new(point.meter.numerator, point.meter.denominator).is_none() {
                return Err(TempoError::InvalidMeter);
            }
            let first_bar = match meter_segments.last() {
                None => 0,
                Some(previous) => {
                    let span = point.at.0 - previous.start.0;
                    let bar = previous.meter.ticks_per_bar();
                    if span % bar != 0 {
                        return Err(TempoError::MeterOffBarLine { at: point.at });
                    }
                    previous.first_bar + span / bar
                }
            };
            meter_segments.push(MeterSegment {
                start: point.at,
                first_bar,
                meter: point.meter,
            });
        }
        Ok(TempoMap {
            tempos: tempo_segments,
            meters: meter_segments,
        })
    }

    pub fn tempo_points(&self) -> Vec<TempoPoint> {
        self.tempos
            .iter()
            .map(|s| TempoPoint {
                at: s.start,
                tempo: s.tempo,
            })
            .collect()
    }

    pub fn meter_points(&self) -> Vec<MeterPoint> {
        self.meters
            .iter()
            .map(|s| MeterPoint {
                at: s.start,
                meter: s.meter,
            })
            .collect()
    }

    /// A copy of this map with `tempo` taking effect at `at` (replacing a point already there).
    /// Positions before the start are treated as the start.
    pub fn with_tempo(&self, at: Ticks, tempo: Tempo) -> TempoMap {
        let at = at.max(Ticks::ZERO);
        let mut points = self.tempo_points();
        match points.binary_search_by_key(&at, |p| p.at) {
            Ok(index) => points[index].tempo = tempo,
            Err(index) => points.insert(index, TempoPoint { at, tempo }),
        }
        TempoMap::from_points(&points, &self.meter_points())
            .expect("inserting a tempo keeps the map valid")
    }

    /// A copy of this map without the tempo point at `at`. The point at the start cannot be
    /// removed; asking to is a no-op.
    pub fn without_tempo(&self, at: Ticks) -> TempoMap {
        let mut points = self.tempo_points();
        points.retain(|p| p.at != at || p.at == Ticks::ZERO);
        TempoMap::from_points(&points, &self.meter_points())
            .expect("removing a tempo keeps the map valid")
    }

    /// A copy of this map with `meter` taking effect at `at`, which must be a bar line.
    pub fn with_meter(&self, at: Ticks, meter: Meter) -> Result<TempoMap, TempoError> {
        let at = at.max(Ticks::ZERO);
        let mut points = self.meter_points();
        match points.binary_search_by_key(&at, |p| p.at) {
            Ok(index) => points[index].meter = meter,
            Err(index) => points.insert(index, MeterPoint { at, meter }),
        }
        TempoMap::from_points(&self.tempo_points(), &points)
    }

    #[inline]
    fn tempo_segment_at_tick(&self, at: Ticks) -> &TempoSegment {
        let index = self.tempos.partition_point(|s| s.start <= at);
        &self.tempos[index.saturating_sub(1)]
    }

    #[inline]
    fn tempo_segment_at_time(&self, at: Superclock) -> &TempoSegment {
        let index = self.tempos.partition_point(|s| s.start_time <= at);
        &self.tempos[index.saturating_sub(1)]
    }

    #[inline]
    fn meter_segment_at(&self, at: Ticks) -> &MeterSegment {
        let index = self.meters.partition_point(|s| s.start <= at);
        &self.meters[index.saturating_sub(1)]
    }

    /// The tempo in effect at `at`.
    #[inline]
    pub fn tempo_at(&self, at: Ticks) -> Tempo {
        self.tempo_segment_at_tick(at).tempo
    }

    /// The meter in effect at `at`.
    #[inline]
    pub fn meter_at(&self, at: Ticks) -> Meter {
        self.meter_segment_at(at).meter
    }

    /// Musical time to audio time (rounding down to the superclock unit). Positions before the
    /// start extend the first tempo backwards.
    #[inline]
    pub fn ticks_to_superclock(&self, at: Ticks) -> Superclock {
        let segment = self.tempo_segment_at_tick(at);
        Superclock(
            segment.start_time.0
                + mul_div_floor(
                    at.0 - segment.start.0,
                    segment.tempo.superclock_per_beat(),
                    TICKS_PER_BEAT,
                ),
        )
    }

    /// Audio time to musical time, rounding down: the last tick that starts at or before `at`.
    #[inline]
    pub fn superclock_to_ticks(&self, at: Superclock) -> Ticks {
        let segment = self.tempo_segment_at_time(at);
        Ticks(
            segment.start.0
                + mul_div_floor(
                    at.0 - segment.start_time.0,
                    TICKS_PER_BEAT,
                    segment.tempo.superclock_per_beat(),
                ),
        )
    }

    /// Audio time to musical time, rounding up: the first tick that starts at or after `at`.
    #[inline]
    pub fn superclock_to_ticks_ceil(&self, at: Superclock) -> Ticks {
        let segment = self.tempo_segment_at_time(at);
        Ticks(
            segment.start.0
                + mul_div_ceil(
                    at.0 - segment.start_time.0,
                    TICKS_PER_BEAT,
                    segment.tempo.superclock_per_beat(),
                ),
        )
    }

    /// The sample (at `rate`) in which the tick `at` begins.
    #[inline]
    pub fn ticks_to_samples(&self, at: Ticks, rate: u32) -> i64 {
        superclock_to_samples(self.ticks_to_superclock(at), rate)
    }

    /// The first tick that falls in sample `sample` or later.
    ///
    /// For tempos of 4 bpm and up and rates up to 192 kHz a tick is shorter than a sample, so
    /// `ticks_to_samples(samples_to_ticks(s)) == s`: an event recorded at a sample plays back at
    /// that same sample.
    #[inline]
    pub fn samples_to_ticks(&self, sample: i64, rate: u32) -> Ticks {
        self.superclock_to_ticks_ceil(samples_to_superclock(sample, rate))
    }

    /// Any position to audio time.
    #[inline]
    pub fn position_to_superclock(&self, position: Position) -> Superclock {
        match position {
            Position::Beats(ticks) => self.ticks_to_superclock(ticks),
            Position::Time(time) => time,
        }
    }

    /// Any position to musical time (rounding down).
    #[inline]
    pub fn position_to_ticks(&self, position: Position) -> Ticks {
        match position {
            Position::Beats(ticks) => ticks,
            Position::Time(time) => self.superclock_to_ticks(time),
        }
    }

    /// Any position to a sample index at `rate`.
    #[inline]
    pub fn position_to_samples(&self, position: Position, rate: u32) -> i64 {
        superclock_to_samples(self.position_to_superclock(position), rate)
    }

    /// Start of the bar containing `at`.
    pub fn bar_start(&self, at: Ticks) -> Ticks {
        let segment = self.meter_segment_at(at);
        let bar = segment.meter.ticks_per_bar();
        Ticks(segment.start.0 + (at.0 - segment.start.0).div_euclid(bar) * bar)
    }

    /// Bar, beat and tick for a position.
    pub fn bbt_at(&self, at: Ticks) -> Bbt {
        let segment = self.meter_segment_at(at);
        let offset = at.0 - segment.start.0;
        let per_bar = segment.meter.ticks_per_bar();
        let per_beat = segment.meter.ticks_per_beat();
        let in_bar = offset.rem_euclid(per_bar);
        Bbt {
            bar: segment.first_bar + offset.div_euclid(per_bar) + 1,
            beat: (in_bar / per_beat) as u32 + 1,
            tick: in_bar % per_beat,
        }
    }

    /// The position of a bar, beat and tick. Beats and ticks beyond the bar's length carry over.
    pub fn bbt_to_ticks(&self, bbt: Bbt) -> Ticks {
        let bar_index = bbt.bar - 1;
        let index = self.meters.partition_point(|s| s.first_bar <= bar_index);
        let segment = &self.meters[index.saturating_sub(1)];
        Ticks(
            segment.start.0
                + (bar_index - segment.first_bar) * segment.meter.ticks_per_bar()
                + (bbt.beat as i64 - 1) * segment.meter.ticks_per_beat()
                + bbt.tick,
        )
    }

    /// The first beat line at or after `at`, and whether it is a bar line.
    ///
    /// "Beat" here means the meter's own beat (the denominator note), which is what a metronome
    /// clicks on.
    pub fn next_beat(&self, at: Ticks) -> (Ticks, bool) {
        let segment = self.meter_segment_at(at);
        let per_beat = segment.meter.ticks_per_beat();
        let offset = at.0 - segment.start.0;
        let beats = offset.div_euclid(per_beat) + (offset.rem_euclid(per_beat) != 0) as i64;
        let mut candidate = Ticks(segment.start.0 + beats * per_beat);
        // A meter change before that beat line cuts the bar short; the change itself is a beat
        // (and bar) line.
        let next_index = self.meters.partition_point(|s| s.start <= at);
        if let Some(next) = self.meters.get(next_index) {
            if next.start <= candidate {
                candidate = next.start;
                return (candidate, true);
            }
        }
        let downbeat =
            (candidate.0 - segment.start.0).rem_euclid(segment.meter.ticks_per_bar()) == 0;
        (candidate, downbeat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beats(n: i64) -> Ticks {
        Ticks::from_beats(n)
    }

    #[test]
    fn constant_tempo_conversions() {
        let map = TempoMap::default(); // 120 bpm
        assert_eq!(
            map.ticks_to_superclock(beats(2)),
            Superclock::from_seconds(1)
        );
        assert_eq!(
            map.superclock_to_ticks(Superclock::from_seconds(1)),
            beats(2)
        );
        assert_eq!(map.ticks_to_samples(beats(4), 48_000), 96_000);
        assert_eq!(map.samples_to_ticks(96_000, 48_000), beats(4));
        // Before the start, the first tempo extends backwards.
        assert_eq!(map.ticks_to_samples(beats(-2), 48_000), -48_000);
        assert_eq!(map.samples_to_ticks(-48_000, 48_000), beats(-2));
    }

    #[test]
    fn tempo_changes_accumulate() {
        // 120 bpm for 4 beats (2 s), then 60 bpm.
        let map = TempoMap::default().with_tempo(beats(4), Tempo::from_bpm(60.0));
        assert_eq!(map.tempo_at(beats(3)).bpm(), 120.0);
        assert_eq!(map.tempo_at(beats(4)).bpm(), 60.0);
        assert_eq!(
            map.ticks_to_superclock(beats(6)),
            Superclock::from_seconds(4)
        );
        assert_eq!(
            map.superclock_to_ticks(Superclock::from_seconds(3)),
            beats(5)
        );
        assert_eq!(map.ticks_to_samples(beats(6), 44_100), 4 * 44_100);

        let replaced = map.with_tempo(beats(4), Tempo::from_bpm(240.0));
        assert_eq!(replaced.tempo_points().len(), 2);
        assert_eq!(
            replaced.ticks_to_superclock(beats(8)),
            Superclock::from_seconds(3)
        );
        assert_eq!(map.without_tempo(beats(4)), TempoMap::default());
        assert_eq!(
            map.without_tempo(beats(0)),
            map,
            "the origin cannot be removed"
        );
    }

    #[test]
    fn invalid_maps_are_rejected() {
        let tempo = TempoPoint {
            at: beats(0),
            tempo: Tempo::default(),
        };
        let meter = MeterPoint {
            at: beats(0),
            meter: Meter::COMMON,
        };
        assert_eq!(
            TempoMap::from_points(&[], &[meter]),
            Err(TempoError::MissingOrigin)
        );
        let late = TempoPoint {
            at: beats(1),
            tempo: Tempo::default(),
        };
        assert_eq!(
            TempoMap::from_points(&[late], &[meter]),
            Err(TempoError::MissingOrigin)
        );
        assert_eq!(
            TempoMap::from_points(&[tempo, late, late], &[meter]),
            Err(TempoError::Unordered)
        );
        let off_bar = MeterPoint {
            at: beats(3),
            meter: Meter::new(3, 4).unwrap(),
        };
        assert_eq!(
            TempoMap::from_points(&[tempo], &[meter, off_bar]),
            Err(TempoError::MeterOffBarLine { at: beats(3) })
        );
        assert!(Meter::new(4, 3).is_none());
        assert!(Meter::new(0, 4).is_none());
    }

    #[test]
    fn bars_and_beats() {
        // Two bars of 4/4, then 6/8.
        let map = TempoMap::default()
            .with_meter(beats(8), Meter::new(6, 8).unwrap())
            .unwrap();
        assert_eq!(
            map.bbt_at(beats(0)),
            Bbt {
                bar: 1,
                beat: 1,
                tick: 0
            }
        );
        assert_eq!(
            map.bbt_at(beats(5)),
            Bbt {
                bar: 2,
                beat: 2,
                tick: 0
            }
        );
        assert_eq!(
            map.bbt_at(beats(8)),
            Bbt {
                bar: 3,
                beat: 1,
                tick: 0
            }
        );
        // An eighth note into the 6/8 bar plus ten ticks.
        let eighth = TICKS_PER_BEAT / 2;
        assert_eq!(
            map.bbt_at(Ticks(beats(8).0 + eighth + 10)),
            Bbt {
                bar: 3,
                beat: 2,
                tick: 10
            }
        );
        // A 6/8 bar is three quarter notes long.
        assert_eq!(
            map.bbt_at(beats(11)),
            Bbt {
                bar: 4,
                beat: 1,
                tick: 0
            }
        );
        assert_eq!(map.bar_start(beats(10)), beats(8));
        assert_eq!(map.bar_start(beats(7)), beats(4));
        // Count-in territory.
        assert_eq!(
            map.bbt_at(beats(-1)),
            Bbt {
                bar: 0,
                beat: 4,
                tick: 0
            }
        );
        assert_eq!(map.bar_start(beats(-1)), beats(-4));

        for tick in [
            beats(0),
            beats(5),
            Ticks(beats(8).0 + eighth + 10),
            beats(11),
            beats(-3),
        ] {
            assert_eq!(map.bbt_to_ticks(map.bbt_at(tick)), tick);
        }
        assert!(map.with_meter(beats(9), Meter::COMMON).is_err());
    }

    #[test]
    fn metronome_beats() {
        let map = TempoMap::default()
            .with_meter(beats(4), Meter::new(6, 8).unwrap())
            .unwrap();
        assert_eq!(map.next_beat(beats(0)), (beats(0), true));
        assert_eq!(map.next_beat(Ticks(1)), (beats(1), false));
        assert_eq!(map.next_beat(beats(3)), (beats(3), false));
        assert_eq!(map.next_beat(Ticks(beats(3).0 + 1)), (beats(4), true));
        // In 6/8 the click is on eighth notes.
        let eighth = TICKS_PER_BEAT / 2;
        assert_eq!(
            map.next_beat(Ticks(beats(4).0 + 1)),
            (Ticks(beats(4).0 + eighth), false)
        );
        assert_eq!(
            map.next_beat(Ticks(beats(4).0 + 5 * eighth + 1)),
            (beats(7), true)
        );
        assert_eq!(map.next_beat(Ticks(-1)), (beats(0), true));
    }
}
