//! A minimal 32-bit float WAV writer for the phase-1 tools. `riddim-disk` replaces it in
//! phase 2.

use std::io::{self, Write};
use std::path::Path;

/// Writes planar `f32` channels as an interleaved IEEE-float WAV file.
pub fn write_f32(path: &Path, sample_rate: u32, channels: &[Vec<f32>]) -> io::Result<()> {
    let channel_count = channels.len() as u16;
    let frames = channels.first().map_or(0, Vec::len);
    let data_bytes = frames as u64 * channel_count as u64 * 4;
    if data_bytes > u32::MAX as u64 - 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too long for a plain WAV file",
        ));
    }
    let mut out = io::BufWriter::new(std::fs::File::create(path)?);
    let block_align = channel_count * 4;
    out.write_all(b"RIFF")?;
    out.write_all(&(4 + 8 + 16 + 8 + 4 + 8 + data_bytes as u32).to_le_bytes())?;
    out.write_all(b"WAVE")?;
    out.write_all(b"fmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&3u16.to_le_bytes())?; // WAVE_FORMAT_IEEE_FLOAT
    out.write_all(&channel_count.to_le_bytes())?;
    out.write_all(&sample_rate.to_le_bytes())?;
    out.write_all(&(sample_rate * block_align as u32).to_le_bytes())?;
    out.write_all(&block_align.to_le_bytes())?;
    out.write_all(&32u16.to_le_bytes())?;
    // Non-PCM formats carry a fact chunk with the frame count.
    out.write_all(b"fact")?;
    out.write_all(&4u32.to_le_bytes())?;
    out.write_all(&(frames as u32).to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&(data_bytes as u32).to_le_bytes())?;
    for frame in 0..frames {
        for channel in channels {
            out.write_all(&channel[frame].to_le_bytes())?;
        }
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_payload_sizes_add_up() {
        let dir = std::env::temp_dir().join(format!("riddim-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        write_f32(&path, 48_000, &[vec![0.5; 10], vec![-0.5; 10]]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        assert_eq!(bytes.len(), 12 + 24 + 12 + 8 + 10 * 2 * 4);
        let first = f32::from_le_bytes(
            bytes[bytes.len() - 80..bytes.len() - 76]
                .try_into()
                .unwrap(),
        );
        assert_eq!(first, 0.5);
        std::fs::remove_dir_all(&dir).ok();
    }
}
