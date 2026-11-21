//! WAV reading and writing, including the properties recording depends on.

use std::path::PathBuf;

use riddim_disk::wav::{self, SampleFormat, WavSpec, WavWriter};
use riddim_disk::AudioData;

/// Length of the fixed header our writer produces.
const HEADER: usize = 104;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("riddim-disk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn ramp(channels: usize, frames: usize) -> AudioData {
    AudioData {
        sample_rate: 48_000,
        channels: (0..channels)
            .map(|c| {
                (0..frames)
                    .map(|f| ((f * 7 + c * 13) % 2001) as f32 / 1000.0 - 1.0)
                    .collect()
            })
            .collect(),
    }
}

#[test]
fn float_files_round_trip_exactly() {
    let path = temp("float.wav");
    let data = ramp(2, 10_001);
    wav::write(&path, &data, SampleFormat::Float32).unwrap();
    let info = wav::probe(&path).unwrap();
    assert_eq!(
        (info.sample_rate, info.channels, info.frames, info.float),
        (48_000, 2, 10_001, true)
    );
    assert_eq!(wav::read(&path).unwrap(), data);
    // The fixed header is what a plain RIFF reader expects.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
        bytes.len() - 8
    );
    assert_eq!(&bytes[12..16], b"JUNK");
}

#[test]
fn pcm_files_round_trip_within_their_resolution() {
    for (format, step) in [
        (SampleFormat::Pcm16, 1.0 / 32_768.0),
        (SampleFormat::Pcm24, 1.0 / 8_388_608.0),
    ] {
        let path = temp(&format!("{format:?}.wav"));
        let data = ramp(3, 5000);
        wav::write(&path, &data, format).unwrap();
        let back = wav::read(&path).unwrap();
        assert_eq!(back.channels.len(), 3);
        for (a, b) in data
            .channels
            .iter()
            .flatten()
            .zip(back.channels.iter().flatten())
        {
            assert!((a - b).abs() <= step, "{format:?}: {a} vs {b}");
        }
    }
    // An odd number of 24-bit mono frames needs a pad byte to stay word-aligned.
    let path = temp("odd24.wav");
    wav::write(&path, &ramp(1, 3), SampleFormat::Pcm24).unwrap();
    assert_eq!(std::fs::read(&path).unwrap().len() % 2, 0);
    assert_eq!(wav::probe(&path).unwrap().frames, 3);
}

#[test]
fn ranges_can_be_read_without_the_rest() {
    let path = temp("range.wav");
    let data = ramp(2, 9000);
    wav::write(&path, &data, SampleFormat::Float32).unwrap();
    let part = wav::read_range(&path, 4000, 1000).unwrap();
    assert_eq!(part.channels[1], data.channels[1][4000..5000]);
    assert_eq!(wav::read_range(&path, 8990, 100).unwrap().frames(), 10);
    assert_eq!(wav::read_range(&path, 20_000, 100).unwrap().frames(), 0);
}

#[test]
fn a_file_is_valid_after_every_flush_while_still_being_written() {
    let path = temp("growing.wav");
    let data = ramp(2, 3000);
    let spec = WavSpec {
        sample_rate: 48_000,
        channels: 2,
        format: SampleFormat::Float32,
    };
    let mut writer = WavWriter::create(&path, spec).unwrap();
    // Even before any audio, it is a valid empty file.
    assert_eq!(wav::probe(&path).unwrap().frames, 0);

    let channels: Vec<&[f32]> = data.channels.iter().map(|c| &c[..1000]).collect();
    writer.write_planar(&channels).unwrap();
    writer.flush(false).unwrap();
    assert_eq!(
        wav::read(&path).unwrap().channels[0],
        data.channels[0][..1000]
    );

    let channels: Vec<&[f32]> = data.channels.iter().map(|c| &c[1000..]).collect();
    writer.write_planar(&channels).unwrap();
    writer.flush(false).unwrap();
    assert_eq!(wav::read(&path).unwrap(), data);
    assert_eq!(writer.finish().unwrap(), 3000);
}

#[test]
fn a_recording_cut_off_mid_take_is_recovered_in_full() {
    // Simulates the process dying after samples reached the file but before the header caught
    // up: the header still says "1000 frames" while 2500 are on disk.
    let path = temp("killed.wav");
    let data = ramp(2, 2500);
    let spec = WavSpec {
        sample_rate: 48_000,
        channels: 2,
        format: SampleFormat::Float32,
    };
    let mut writer = WavWriter::create(&path, spec).unwrap();
    let head: Vec<&[f32]> = data.channels.iter().map(|c| &c[..1000]).collect();
    writer.write_planar(&head).unwrap();
    writer.flush(false).unwrap();
    let stale_header = std::fs::read(&path).unwrap()[..HEADER].to_vec();
    let tail: Vec<&[f32]> = data.channels.iter().map(|c| &c[1000..]).collect();
    writer.write_planar(&tail).unwrap();
    writer.flush(false).unwrap();
    drop(writer);
    // Put the stale header back, as if the last header update never happened.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[..HEADER].copy_from_slice(&stale_header);
    // And chop a partial frame off the end, as an interrupted write would leave.
    bytes.truncate(bytes.len() - 3);
    std::fs::write(&path, &bytes).unwrap();

    let recovered = wav::read(&path).unwrap();
    assert_eq!(
        recovered.frames(),
        2499,
        "everything but the torn last frame must be readable"
    );
    assert_eq!(recovered.channels[0], data.channels[0][..2499]);
}

#[test]
fn foreign_headers_are_understood_and_nonsense_is_refused() {
    // 16-bit PCM written by hand with an extra chunk before the data and no JUNK chunk.
    let path = temp("foreign.wav");
    let samples: [i16; 4] = [0, 16_384, -16_384, 32_767];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&44_100u32.to_le_bytes());
    bytes.extend_from_slice(&88_200u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"LIST");
    bytes.extend_from_slice(&5u32.to_le_bytes());
    bytes.extend_from_slice(b"hello\0"); // odd-sized chunk plus its pad byte
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&8u32.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();
    let data = wav::read(&path).unwrap();
    assert_eq!(data.sample_rate, 44_100);
    assert_eq!(
        data.channels,
        vec![vec![0.0, 0.5, -0.5, 32_767.0 / 32_768.0]]
    );

    let bad = temp("bad.wav");
    std::fs::write(&bad, b"RIFF\0\0\0\0AVI ").unwrap();
    assert!(wav::read(&bad).is_err());
    std::fs::write(&bad, b"short").unwrap();
    assert!(wav::probe(&bad).is_err());
    assert!(wav::read(&temp("missing.wav")).is_err());
}
