//! Turning files into model content.

use std::path::Path;
use std::sync::Arc;

use riddim_disk::{smf, wav, DiskError};
use riddim_model::{Clip, ClipContent, ClipId, Media, MediaId, MidiEvent, Note};
use riddim_time::{Meter, MeterPoint, Position, TempoMap, TempoPoint, Ticks};

/// What a Standard MIDI File contains, in model terms.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedMidi {
    /// One clip holding every note and controller in the file, starting at the beginning of
    /// the timeline.
    pub clip: Clip,
    /// The file's tempo and time-signature changes, if it has any.
    pub tempo_map: Option<TempoMap>,
}

/// Reads a MIDI file into a clip (with id `clip_id`) and, if the file has one, a tempo map.
pub fn midi_file(path: &Path, clip_id: ClipId) -> Result<ImportedMidi, DiskError> {
    let bytes = std::fs::read(path)?;
    midi_bytes(
        &bytes,
        clip_id,
        path.file_stem().map(|s| s.to_string_lossy().into_owned()),
    )
}

/// As [`midi_file`], from bytes already in memory.
pub fn midi_bytes(
    bytes: &[u8],
    clip_id: ClipId,
    name: Option<String>,
) -> Result<ImportedMidi, DiskError> {
    let data = smf::read(bytes)?;
    let notes: Vec<Note> = data
        .notes
        .iter()
        .map(|n| Note {
            start: n.start,
            length: n.length,
            key: n.key,
            velocity: n.velocity,
            channel: n.channel,
        })
        .collect();
    let events: Vec<MidiEvent> = data
        .events
        .iter()
        .map(|e| MidiEvent {
            at: e.at,
            data: e.data,
            len: e.len,
        })
        .collect();

    let tempo_map = if data.tempos.is_empty() && data.meters.is_empty() {
        None
    } else {
        let mut tempos: Vec<TempoPoint> = data
            .tempos
            .iter()
            .map(|&(at, tempo)| TempoPoint { at, tempo })
            .collect();
        if tempos.first().map(|t| t.at) != Some(Ticks::ZERO) {
            tempos.insert(
                0,
                TempoPoint {
                    at: Ticks::ZERO,
                    tempo: Default::default(),
                },
            );
        }
        let mut meters: Vec<MeterPoint> = data
            .meters
            .iter()
            .filter_map(|&(at, n, d)| {
                Meter::new(n as u16, d as u16).map(|meter| MeterPoint { at, meter })
            })
            .collect();
        if meters.first().map(|m| m.at) != Some(Ticks::ZERO) {
            meters.insert(
                0,
                MeterPoint {
                    at: Ticks::ZERO,
                    meter: Meter::COMMON,
                },
            );
        }
        // A file may put a meter change off a bar line; keep the tempos and fall back to the
        // file's first meter rather than refuse the import.
        TempoMap::from_points(&tempos, &meters)
            .or_else(|_| TempoMap::from_points(&tempos, &meters[..1]))
            .ok()
    };

    // Whole beats, so the clip ends on a beat line.
    let beats = (data.length.0 + riddim_time::TICKS_PER_BEAT - 1) / riddim_time::TICKS_PER_BEAT;
    Ok(ImportedMidi {
        clip: Clip {
            id: clip_id,
            name: name.unwrap_or_default(),
            start: Position::START,
            muted: false,
            content: ClipContent::Midi {
                length: Ticks::from_beats(beats.max(1)),
                notes: Arc::new(notes),
                events: Arc::new(events),
            },
        },
        tempo_map,
    })
}

/// Describes a WAV file as media (with id `media_id`), without loading its audio.
pub fn audio_file(path: &Path, media_id: MediaId) -> Result<Media, DiskError> {
    let info = wav::probe(path)?;
    Ok(Media {
        id: media_id,
        path: path.to_string_lossy().into_owned(),
        sample_rate: info.sample_rate,
        channels: info.channels,
        frames: info.frames,
    })
}

/// A clip that plays the whole of `media` from `start`.
pub fn clip_for_media(media: &Media, clip_id: ClipId, start: Position) -> Clip {
    Clip {
        id: clip_id,
        name: Path::new(&media.path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        start,
        muted: false,
        content: ClipContent::Audio {
            media: media.id,
            offset: 0,
            length: media.frames,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use riddim_disk::smf::{SmfData, SmfNote};
    use riddim_time::Tempo;

    #[test]
    fn a_midi_file_becomes_a_clip_and_a_tempo_map() {
        let data = SmfData {
            notes: vec![SmfNote {
                start: Ticks::from_beats(1),
                length: Ticks::from_beat_fraction(1, 2),
                key: 64,
                velocity: 90,
                channel: 1,
            }],
            tempos: vec![(Ticks::from_beats(2), Tempo::from_bpm(90.0))],
            meters: vec![(Ticks::ZERO, 3, 4)],
            length: Ticks::from_beat_fraction(7, 2),
            ..SmfData::default()
        };
        let imported = midi_bytes(&smf::write(&data), ClipId(7), Some("riff".into())).unwrap();
        assert_eq!(imported.clip.id, ClipId(7));
        assert_eq!(imported.clip.name, "riff");
        let ClipContent::Midi { length, notes, .. } = &imported.clip.content else {
            panic!()
        };
        assert_eq!(*length, Ticks::from_beats(4), "rounded up to a whole beat");
        assert_eq!(notes.len(), 1);
        assert_eq!(
            (notes[0].key, notes[0].velocity, notes[0].channel),
            (64, 90, 1)
        );
        let map = imported.tempo_map.unwrap();
        assert_eq!(map.meter_at(Ticks::ZERO).numerator, 3);
        // The file starts without a tempo, so the default holds until the change at beat 2.
        assert_eq!(map.tempo_at(Ticks::from_beats(1)).bpm(), 120.0);
        assert!((map.tempo_at(Ticks::from_beats(2)).bpm() - 90.0).abs() < 1e-3);
    }

    #[test]
    fn a_file_without_tempo_information_leaves_the_project_tempo_alone() {
        let data = SmfData {
            length: Ticks::from_beats(1),
            ..SmfData::default()
        };
        assert!(midi_bytes(&smf::write(&data), ClipId(1), None)
            .unwrap()
            .tempo_map
            .is_none());
        assert!(midi_bytes(b"junk", ClipId(1), None).is_err());
    }
}
