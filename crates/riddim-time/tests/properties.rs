//! Property tests for the clocks: conversions are monotonic and round-trip where promised.

use proptest::prelude::*;
use riddim_time::{Meter, Tempo, TempoMap, Ticks, TICKS_PER_BEAT};

/// Maps with up to six tempo changes between 20 and 400 bpm.
fn maps() -> impl Strategy<Value = TempoMap> {
    (
        20.0f64..400.0,
        prop::collection::vec((1i64..64, 20.0f64..400.0), 0..6),
    )
        .prop_map(|(first, changes)| {
            let mut map = TempoMap::new(Tempo::from_bpm(first), Meter::COMMON);
            let mut at = 0i64;
            for (gap_sixteenths, bpm) in changes {
                at += gap_sixteenths * TICKS_PER_BEAT / 4;
                map = map.with_tempo(Ticks(at), Tempo::from_bpm(bpm));
            }
            map
        })
}

fn rates() -> impl Strategy<Value = u32> {
    prop::sample::select(vec![44_100u32, 48_000, 88_200, 96_000, 192_000])
}

proptest! {
    #[test]
    fn a_sample_survives_the_trip_through_ticks(
        map in maps(),
        rate in rates(),
        sample in -10_000_000i64..500_000_000,
    ) {
        let ticks = map.samples_to_ticks(sample, rate);
        prop_assert_eq!(map.ticks_to_samples(ticks, rate), sample);
    }

    #[test]
    fn ticks_to_time_is_monotonic(map in maps(), a in -1_000i64..100_000, b in 0i64..1_000_000) {
        let first = Ticks(a * 1000);
        let second = Ticks(first.0 + b);
        prop_assert!(map.ticks_to_superclock(first) <= map.ticks_to_superclock(second));
        prop_assert!(map.ticks_to_samples(first, 48_000) <= map.ticks_to_samples(second, 48_000));
    }

    #[test]
    fn time_to_ticks_is_monotonic_and_brackets(map in maps(), rate in rates(), s in -1_000_000i64..100_000_000, d in 0i64..100_000) {
        let a = map.samples_to_ticks(s, rate);
        let b = map.samples_to_ticks(s + d, rate);
        prop_assert!(a <= b);
        // Floor and ceiling conversions bracket the true value within one tick.
        let clock = riddim_time::samples_to_superclock(s, rate);
        let floor = map.superclock_to_ticks(clock);
        let ceil = map.superclock_to_ticks_ceil(clock);
        prop_assert!(floor <= ceil && ceil.0 - floor.0 <= 1);
    }

    #[test]
    fn a_tick_survives_the_trip_through_time_within_one_tick(map in maps(), t in -1_000_000i64..10_000_000) {
        // Ticks are finer than superclock units only at slow tempos; either way the floor
        // conversion never lands after the tick and is at most one tick early.
        let tick = Ticks(t * 97);
        let back = map.superclock_to_ticks(map.ticks_to_superclock(tick));
        prop_assert!(back <= tick && tick.0 - back.0 <= 1, "{:?} -> {:?}", tick, back);
    }

    #[test]
    fn bbt_round_trips(numerator in 1u16..13, denominator_power in 0u32..5, t in -100_000i64..10_000_000) {
        let meter = Meter::new(numerator, 1 << denominator_power).unwrap();
        let map = TempoMap::new(Tempo::default(), meter);
        let tick = Ticks(t * 131);
        let bbt = map.bbt_at(tick);
        prop_assert!(bbt.beat >= 1 && bbt.beat <= numerator as u32);
        prop_assert!(bbt.tick >= 0 && bbt.tick < meter.ticks_per_beat());
        prop_assert_eq!(map.bbt_to_ticks(bbt), tick);
        prop_assert!(map.bar_start(tick) <= tick);
    }

    #[test]
    fn next_beat_is_at_or_after_and_on_the_grid(t in -1_000_000i64..10_000_000) {
        let map = TempoMap::default();
        let tick = Ticks(t * 53);
        let (beat, downbeat) = map.next_beat(tick);
        prop_assert!(beat >= tick && beat.0 - tick.0 < TICKS_PER_BEAT);
        prop_assert_eq!(beat.0.rem_euclid(TICKS_PER_BEAT), 0);
        prop_assert_eq!(downbeat, beat.0.rem_euclid(4 * TICKS_PER_BEAT) == 0);
    }
}
