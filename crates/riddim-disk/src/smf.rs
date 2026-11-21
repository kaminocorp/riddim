//! Standard MIDI Files.
//!
//! Reading merges every track of a file into one list of notes and events in Riddim's own tick
//! resolution, along with the file's tempo and time-signature changes. Writing produces a
//! single-track (format 0) file at 960 ticks per quarter note.

use std::collections::HashMap;

use midly::num::{u15, u24, u28, u4, u7};
use midly::{
    Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, Timing, TrackEvent, TrackEventKind,
};
use riddim_time::{mul_div_round, Tempo, Ticks, SUPERCLOCK_PER_SECOND, TICKS_PER_BEAT};

use crate::DiskError;

/// Resolution of the files we write.
pub const WRITE_PPQ: u16 = 960;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmfNote {
    pub start: Ticks,
    pub length: Ticks,
    pub key: u8,
    /// 1..=127.
    pub velocity: u8,
    pub channel: u8,
}

/// A channel-voice message other than a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmfEvent {
    pub at: Ticks,
    pub data: [u8; 3],
    pub len: u8,
}

/// The musical content of a MIDI file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SmfData {
    /// Sorted by start.
    pub notes: Vec<SmfNote>,
    /// Sorted by time.
    pub events: Vec<SmfEvent>,
    pub tempos: Vec<(Ticks, Tempo)>,
    /// (position, numerator, denominator).
    pub meters: Vec<(Ticks, u8, u8)>,
    /// Time of the last event or note end.
    pub length: Ticks,
}

fn tempo_from_micros(micros_per_quarter: u32) -> Option<Tempo> {
    Tempo::from_superclock_per_beat(mul_div_round(
        micros_per_quarter as i64,
        SUPERCLOCK_PER_SECOND,
        1_000_000,
    ))
}

fn micros_from_tempo(tempo: Tempo) -> u32 {
    mul_div_round(
        tempo.superclock_per_beat(),
        1_000_000,
        SUPERCLOCK_PER_SECOND,
    )
    .clamp(1, 0xFF_FFFF) as u32
}

/// Parses a Standard MIDI File.
pub fn read(bytes: &[u8]) -> Result<SmfData, DiskError> {
    let smf = Smf::parse(bytes)
        .map_err(|e| DiskError::Format(format!("not a readable MIDI file: {e}")))?;
    let ppq = match smf.header.timing {
        Timing::Metrical(ticks) => ticks.as_int() as u32,
        Timing::Timecode(..) => {
            return Err(DiskError::Format(
                "MIDI files with SMPTE timing are not supported".into(),
            ))
        }
    };
    if ppq == 0 {
        return Err(DiskError::Format(
            "the MIDI file has a zero time division".into(),
        ));
    }

    let mut data = SmfData::default();
    for track in &smf.tracks {
        let mut now = 0i64;
        // Notes currently held: (channel, key) -> (start, velocity).
        let mut held: HashMap<(u8, u8), (Ticks, u8)> = HashMap::new();
        let close = |held: &mut HashMap<(u8, u8), (Ticks, u8)>,
                     notes: &mut Vec<SmfNote>,
                     channel: u8,
                     key: u8,
                     at: Ticks| {
            if let Some((start, velocity)) = held.remove(&(channel, key)) {
                // A zero-length note would be inaudible and is not valid in the model.
                let length = Ticks((at.0 - start.0).max(1));
                notes.push(SmfNote {
                    start,
                    length,
                    key,
                    velocity,
                    channel,
                });
            }
        };
        for event in track {
            now += event.delta.as_int() as i64;
            let at = Ticks::from_ppq(now, ppq);
            data.length = data.length.max(at);
            match event.kind {
                TrackEventKind::Midi { channel, message } => {
                    let channel = channel.as_int();
                    let mut push = |data3: [u8; 3], len: u8| {
                        data.events.push(SmfEvent {
                            at,
                            data: data3,
                            len,
                        });
                    };
                    match message {
                        MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                            // A second note-on for a held key retriggers it.
                            close(&mut held, &mut data.notes, channel, key.as_int(), at);
                            held.insert((channel, key.as_int()), (at, vel.as_int()));
                        }
                        MidiMessage::NoteOn { key, .. } | MidiMessage::NoteOff { key, .. } => {
                            close(&mut held, &mut data.notes, channel, key.as_int(), at);
                        }
                        MidiMessage::Aftertouch { key, vel } => {
                            push([0xA0 | channel, key.as_int(), vel.as_int()], 3)
                        }
                        MidiMessage::Controller { controller, value } => {
                            push([0xB0 | channel, controller.as_int(), value.as_int()], 3)
                        }
                        MidiMessage::ProgramChange { program } => {
                            push([0xC0 | channel, program.as_int(), 0], 2)
                        }
                        MidiMessage::ChannelAftertouch { vel } => {
                            push([0xD0 | channel, vel.as_int(), 0], 2)
                        }
                        MidiMessage::PitchBend { bend } => {
                            let value = bend.0.as_int();
                            push(
                                [0xE0 | channel, (value & 0x7F) as u8, (value >> 7) as u8],
                                3,
                            )
                        }
                    }
                }
                TrackEventKind::Meta(MetaMessage::Tempo(micros)) => {
                    if let Some(tempo) = tempo_from_micros(micros.as_int()) {
                        data.tempos.push((at, tempo));
                    }
                }
                TrackEventKind::Meta(MetaMessage::TimeSignature(
                    numerator,
                    denominator_power,
                    _,
                    _,
                )) => {
                    if numerator > 0 && denominator_power <= 6 {
                        data.meters.push((at, numerator, 1 << denominator_power));
                    }
                }
                _ => {}
            }
        }
        // Notes never released end with the track.
        let end = Ticks::from_ppq(now, ppq);
        let dangling: Vec<(u8, u8)> = held.keys().copied().collect();
        for (channel, key) in dangling {
            close(&mut held, &mut data.notes, channel, key, end);
        }
    }

    data.notes.sort_by_key(|n| (n.start, n.channel, n.key));
    data.events.sort_by_key(|e| e.at);
    data.tempos.sort_by_key(|t| t.0);
    data.tempos.dedup_by_key(|t| t.0);
    data.meters.sort_by_key(|m| m.0);
    data.meters.dedup_by_key(|m| m.0);
    for note in &data.notes {
        data.length = data.length.max(note.start + note.length);
    }
    Ok(data)
}

fn to_ppq(ticks: Ticks) -> u64 {
    mul_div_round(ticks.0.max(0), WRITE_PPQ as i64, TICKS_PER_BEAT) as u64
}

/// Serialises `data` as a format 0 MIDI file.
pub fn write(data: &SmfData) -> Vec<u8> {
    // (time, rank, event): at equal times tempo and meter come first, then releases, then
    // everything else, so a note that ends where the next begins is released first.
    let mut timed: Vec<(u64, u8, TrackEventKind<'static>)> = Vec::new();
    for &(at, tempo) in &data.tempos {
        timed.push((
            to_ppq(at),
            0,
            TrackEventKind::Meta(MetaMessage::Tempo(u24::new(micros_from_tempo(tempo)))),
        ));
    }
    for &(at, numerator, denominator) in &data.meters {
        let power = denominator.max(1).trailing_zeros() as u8;
        timed.push((
            to_ppq(at),
            0,
            TrackEventKind::Meta(MetaMessage::TimeSignature(numerator, power, 24, 8)),
        ));
    }
    for note in &data.notes {
        let channel = u4::new(note.channel & 0x0F);
        let key = u7::new(note.key & 0x7F);
        let start = to_ppq(note.start);
        let end = to_ppq(note.start + note.length).max(start + 1);
        timed.push((
            start,
            2,
            TrackEventKind::Midi {
                channel,
                message: MidiMessage::NoteOn {
                    key,
                    vel: u7::new(note.velocity.clamp(1, 127)),
                },
            },
        ));
        timed.push((
            end,
            1,
            TrackEventKind::Midi {
                channel,
                message: MidiMessage::NoteOff {
                    key,
                    vel: u7::new(0),
                },
            },
        ));
    }
    for event in &data.events {
        let channel = u4::new(event.data[0] & 0x0F);
        let (d1, d2) = (u7::new(event.data[1] & 0x7F), u7::new(event.data[2] & 0x7F));
        let message = match event.data[0] & 0xF0 {
            0xA0 => MidiMessage::Aftertouch { key: d1, vel: d2 },
            0xB0 => MidiMessage::Controller {
                controller: d1,
                value: d2,
            },
            0xC0 => MidiMessage::ProgramChange { program: d1 },
            0xD0 => MidiMessage::ChannelAftertouch { vel: d1 },
            0xE0 => MidiMessage::PitchBend {
                bend: PitchBend(midly::num::u14::new(
                    ((event.data[2] as u16 & 0x7F) << 7) | (event.data[1] as u16 & 0x7F),
                )),
            },
            _ => continue,
        };
        timed.push((
            to_ppq(event.at),
            2,
            TrackEventKind::Midi { channel, message },
        ));
    }
    timed.sort_by_key(|(at, rank, _)| (*at, *rank));

    let mut track: Vec<TrackEvent<'static>> = Vec::with_capacity(timed.len() + 1);
    let mut previous = 0u64;
    for (at, _, kind) in timed {
        let delta = (at - previous).min(0x0FFF_FFFF) as u32;
        previous = at;
        track.push(TrackEvent {
            delta: u28::new(delta),
            kind,
        });
    }
    let tail = to_ppq(data.length)
        .saturating_sub(previous)
        .min(0x0FFF_FFFF) as u32;
    track.push(TrackEvent {
        delta: u28::new(tail),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });

    let mut smf = Smf::new(Header::new(
        Format::SingleTrack,
        Timing::Metrical(u15::new(WRITE_PPQ)),
    ));
    smf.tracks.push(track);
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes)
        .expect("writing to memory cannot fail");
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beats(numerator: i64, denominator: i64) -> Ticks {
        Ticks::from_beat_fraction(numerator, denominator)
    }

    fn song() -> SmfData {
        SmfData {
            notes: vec![
                SmfNote {
                    start: beats(0, 1),
                    length: beats(1, 1),
                    key: 60,
                    velocity: 100,
                    channel: 0,
                },
                SmfNote {
                    start: beats(1, 1),
                    length: beats(1, 2),
                    key: 60,
                    velocity: 80,
                    channel: 0,
                },
                SmfNote {
                    start: beats(1, 1),
                    length: beats(2, 1),
                    key: 67,
                    velocity: 1,
                    channel: 9,
                },
                SmfNote {
                    start: beats(7, 4),
                    length: beats(1, 4),
                    key: 72,
                    velocity: 127,
                    channel: 0,
                },
            ],
            events: vec![
                SmfEvent {
                    at: beats(0, 1),
                    data: [0xC0, 5, 0],
                    len: 2,
                },
                SmfEvent {
                    at: beats(1, 2),
                    data: [0xB0, 64, 127],
                    len: 3,
                },
                SmfEvent {
                    at: beats(1, 1),
                    data: [0xE0, 0x7F, 0x7F],
                    len: 3,
                },
                SmfEvent {
                    at: beats(3, 2),
                    data: [0xD3, 40, 0],
                    len: 2,
                },
            ],
            tempos: vec![
                (beats(0, 1), Tempo::from_bpm(120.0)),
                (beats(2, 1), Tempo::from_bpm(90.0)),
            ],
            meters: vec![(beats(0, 1), 6, 8)],
            length: beats(4, 1),
        }
    }

    #[test]
    fn a_song_survives_write_then_read() {
        let original = song();
        let bytes = write(&original);
        assert_eq!(&bytes[..4], b"MThd");
        let read_back = read(&bytes).unwrap();
        assert_eq!(read_back.notes, original.notes);
        assert_eq!(read_back.events, original.events);
        assert_eq!(read_back.meters, original.meters);
        assert_eq!(read_back.length, original.length);
        assert_eq!(read_back.tempos.len(), 2);
        for (got, want) in read_back.tempos.iter().zip(&original.tempos) {
            assert_eq!(got.0, want.0);
            assert!(
                (got.1.bpm() - want.1.bpm()).abs() < 1e-3,
                "{} vs {}",
                got.1.bpm(),
                want.1.bpm()
            );
        }
        // And the bytes are stable: writing what we read gives the same file.
        assert_eq!(write(&read_back), bytes);
    }

    #[test]
    fn a_note_that_ends_where_the_next_begins_is_not_swallowed() {
        let data = SmfData {
            notes: vec![
                SmfNote {
                    start: beats(0, 1),
                    length: beats(1, 1),
                    key: 60,
                    velocity: 90,
                    channel: 0,
                },
                SmfNote {
                    start: beats(1, 1),
                    length: beats(1, 1),
                    key: 60,
                    velocity: 91,
                    channel: 0,
                },
            ],
            length: beats(2, 1),
            ..SmfData::default()
        };
        assert_eq!(read(&write(&data)).unwrap().notes, data.notes);
    }

    #[test]
    fn other_resolutions_and_unreleased_notes() {
        // Hand-built file: 96 ppq, a note-on with no note-off, then end of track 96 ticks later.
        let mut smf = Smf::new(Header::new(
            Format::SingleTrack,
            Timing::Metrical(u15::new(96)),
        ));
        smf.tracks.push(vec![
            TrackEvent {
                delta: u28::new(48),
                kind: TrackEventKind::Midi {
                    channel: u4::new(2),
                    message: MidiMessage::NoteOn {
                        key: u7::new(64),
                        vel: u7::new(70),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(96),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            },
        ]);
        let mut bytes = Vec::new();
        smf.write_std(&mut bytes).unwrap();
        let data = read(&bytes).unwrap();
        assert_eq!(
            data.notes,
            vec![SmfNote {
                start: beats(1, 2),
                length: beats(1, 1),
                key: 64,
                velocity: 70,
                channel: 2
            }]
        );
        assert_eq!(data.length, beats(3, 2));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(read(b"not a midi file").is_err());
        assert!(read(&[]).is_err());
    }
}
