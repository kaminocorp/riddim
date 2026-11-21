//! WAV files.
//!
//! The writer is built for recording: it can be asked at any time to bring the header up to
//! date, after which the file on disk is a complete, valid WAV holding everything written so
//! far. A process that is killed mid-take therefore loses nothing that had been flushed, and
//! the reader recovers even the tail written after the last header update.
//!
//! Files that outgrow 4 GB are switched to RF64 in place: the header always reserves room for
//! the 64-bit size chunk.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::{AudioData, DiskError};

/// How samples are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    Float32,
    Pcm24,
    Pcm16,
}

impl SampleFormat {
    fn bytes(self) -> u32 {
        match self {
            SampleFormat::Float32 => 4,
            SampleFormat::Pcm24 => 3,
            SampleFormat::Pcm16 => 2,
        }
    }

    fn tag(self) -> u16 {
        match self {
            SampleFormat::Float32 => 3, // WAVE_FORMAT_IEEE_FLOAT
            _ => 1,                     // WAVE_FORMAT_PCM
        }
    }
}

/// The shape of a WAV file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

// Fixed header layout: RIFF(12) + JUNK/ds64(8+28) + fmt(8+16) + rdim(8+4) + fact(8+4) + data(8).
const DS64_BODY: u32 = 28;
const HEADER_LEN: u64 = 12 + 8 + DS64_BODY as u64 + 8 + 16 + 8 + 4 + 8 + 4 + 8;
/// A private chunk that marks a file as written by this writer. Its presence tells the reader
/// that the data chunk is the last thing in the file, which is what makes it safe to trust the
/// file's length over a stale header when recovering an interrupted recording.
const MARKER_CHUNK: &[u8; 4] = b"rdim";
const MARKER_VERSION: u32 = 1;
/// Beyond this many data bytes the 32-bit size fields no longer fit and the file becomes RF64.
const RIFF_LIMIT: u64 = u32::MAX as u64 - HEADER_LEN;

/// An incremental WAV writer.
pub struct WavWriter {
    file: BufWriter<File>,
    spec: WavSpec,
    data_bytes: u64,
    scratch: Vec<u8>,
}

impl WavWriter {
    /// Creates (or truncates) `path` and writes a header for an empty file.
    pub fn create(path: &Path, spec: WavSpec) -> Result<WavWriter, DiskError> {
        if spec.channels == 0 || spec.sample_rate == 0 {
            return Err(DiskError::Format(
                "a WAV file needs channels and a sample rate".into(),
            ));
        }
        let file = File::create(path)?;
        let mut writer = WavWriter {
            file: BufWriter::with_capacity(256 * 1024, file),
            spec,
            data_bytes: 0,
            scratch: Vec::new(),
        };
        let header = writer.header();
        writer.file.write_all(&header)?;
        // The empty file is already a valid WAV on disk.
        writer.file.flush()?;
        Ok(writer)
    }

    pub fn spec(&self) -> WavSpec {
        self.spec
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.data_bytes / (self.spec.channels as u64 * self.spec.format.bytes() as u64)
    }

    fn header(&self) -> Vec<u8> {
        let frame_bytes = self.spec.channels as u32 * self.spec.format.bytes();
        let frames = self.frames();
        let rf64 = self.data_bytes > RIFF_LIMIT;
        let mut h = Vec::with_capacity(HEADER_LEN as usize);
        let riff_size = HEADER_LEN - 8 + self.data_bytes;

        h.extend_from_slice(if rf64 { b"RF64" } else { b"RIFF" });
        h.extend_from_slice(&(if rf64 { u32::MAX } else { riff_size as u32 }).to_le_bytes());
        h.extend_from_slice(b"WAVE");

        // The 64-bit sizes live here once needed; until then the space is a JUNK chunk.
        h.extend_from_slice(if rf64 { b"ds64" } else { b"JUNK" });
        h.extend_from_slice(&DS64_BODY.to_le_bytes());
        if rf64 {
            h.extend_from_slice(&riff_size.to_le_bytes());
            h.extend_from_slice(&self.data_bytes.to_le_bytes());
            h.extend_from_slice(&frames.to_le_bytes());
            h.extend_from_slice(&0u32.to_le_bytes()); // no chunk-size table
        } else {
            h.extend_from_slice(&[0u8; DS64_BODY as usize]);
        }

        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&self.spec.format.tag().to_le_bytes());
        h.extend_from_slice(&self.spec.channels.to_le_bytes());
        h.extend_from_slice(&self.spec.sample_rate.to_le_bytes());
        h.extend_from_slice(&(self.spec.sample_rate * frame_bytes).to_le_bytes());
        h.extend_from_slice(&(frame_bytes as u16).to_le_bytes());
        h.extend_from_slice(&((self.spec.format.bytes() * 8) as u16).to_le_bytes());

        h.extend_from_slice(MARKER_CHUNK);
        h.extend_from_slice(&4u32.to_le_bytes());
        h.extend_from_slice(&MARKER_VERSION.to_le_bytes());

        h.extend_from_slice(b"fact");
        h.extend_from_slice(&4u32.to_le_bytes());
        h.extend_from_slice(&(if rf64 { u32::MAX } else { frames as u32 }).to_le_bytes());

        h.extend_from_slice(b"data");
        h.extend_from_slice(
            &(if rf64 {
                u32::MAX
            } else {
                self.data_bytes as u32
            })
            .to_le_bytes(),
        );
        debug_assert_eq!(h.len() as u64, HEADER_LEN);
        h
    }

    /// Appends interleaved samples. `samples.len()` must be a multiple of the channel count.
    pub fn write_interleaved(&mut self, samples: &[f32]) -> Result<(), DiskError> {
        debug_assert_eq!(samples.len() % self.spec.channels as usize, 0);
        self.scratch.clear();
        match self.spec.format {
            SampleFormat::Float32 => {
                for sample in samples {
                    self.scratch.extend_from_slice(&sample.to_le_bytes());
                }
            }
            SampleFormat::Pcm24 => {
                for sample in samples {
                    let value = (*sample as f64 * 8_388_608.0)
                        .round()
                        .clamp(-8_388_608.0, 8_388_607.0) as i32;
                    self.scratch.extend_from_slice(&value.to_le_bytes()[..3]);
                }
            }
            SampleFormat::Pcm16 => {
                for sample in samples {
                    let value = (*sample as f64 * 32_768.0)
                        .round()
                        .clamp(-32_768.0, 32_767.0) as i16;
                    self.scratch.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        self.file.write_all(&self.scratch)?;
        self.data_bytes += self.scratch.len() as u64;
        Ok(())
    }

    /// Appends planar samples (one slice per channel, all the same length).
    pub fn write_planar(&mut self, channels: &[&[f32]]) -> Result<(), DiskError> {
        let frames = channels.first().map_or(0, |c| c.len());
        let mut interleaved = Vec::with_capacity(frames * channels.len());
        for frame in 0..frames {
            for channel in channels {
                interleaved.push(channel[frame]);
            }
        }
        self.write_interleaved(&interleaved)
    }

    /// Makes the file on disk valid as of now: pushes buffered samples to the operating
    /// system and rewrites the header with the current sizes.
    ///
    /// After this returns, the data survives the process being killed. With `sync` it also
    /// survives the machine losing power, at the cost of waiting for the disk.
    pub fn flush(&mut self, sync: bool) -> Result<(), DiskError> {
        self.file.flush()?;
        let header = self.header();
        let file = self.file.get_mut();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.seek(SeekFrom::End(0))?;
        if sync {
            file.sync_data()?;
        }
        Ok(())
    }

    /// Flushes and closes the file. Returns the number of frames it holds.
    pub fn finish(mut self) -> Result<u64, DiskError> {
        // RIFF chunks are word-aligned; pad odd-length data (only possible for odd byte widths).
        if self.data_bytes % 2 == 1 {
            self.file.write_all(&[0])?;
        }
        self.flush(true)?;
        Ok(self.frames())
    }
}

/// What a WAV file's header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub bits_per_sample: u16,
    pub float: bool,
}

struct Layout {
    info: WavInfo,
    data_start: u64,
}

fn bad(why: impl Into<String>) -> DiskError {
    DiskError::Format(why.into())
}

fn read_layout(reader: &mut (impl Read + Seek), file_len: u64) -> Result<Layout, DiskError> {
    let mut riff = [0u8; 12];
    reader
        .read_exact(&mut riff)
        .map_err(|_| bad("not a WAV file (too short)"))?;
    let rf64 = &riff[..4] == b"RF64";
    if (&riff[..4] != b"RIFF" && !rf64) || &riff[8..] != b"WAVE" {
        return Err(bad("not a WAV file"));
    }

    let mut format: Option<(u16, u16, u32, u16)> = None; // tag, channels, rate, bits
    let mut data64: Option<u64> = None;
    let mut ours = false;
    let mut position = 12u64;
    loop {
        let mut head = [0u8; 8];
        if reader.read_exact(&mut head).is_err() {
            return Err(bad("the file has no audio data chunk"));
        }
        position += 8;
        let id = &head[..4];
        let size = u32::from_le_bytes(head[4..].try_into().unwrap()) as u64;
        match id {
            b"ds64" => {
                let mut body = [0u8; 24];
                reader
                    .read_exact(&mut body)
                    .map_err(|_| bad("truncated ds64 chunk"))?;
                data64 = Some(u64::from_le_bytes(body[8..16].try_into().unwrap()));
                reader.seek(SeekFrom::Start(position + size + (size & 1)))?;
            }
            id if id == MARKER_CHUNK => {
                ours = true;
                reader.seek(SeekFrom::Start(position + size + (size & 1)))?;
            }
            b"fmt " => {
                if size < 16 {
                    return Err(bad("the format chunk is too short"));
                }
                let mut body = vec![0u8; size.min(64) as usize];
                reader
                    .read_exact(&mut body)
                    .map_err(|_| bad("truncated format chunk"))?;
                let mut tag = u16::from_le_bytes([body[0], body[1]]);
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes([body[14], body[15]]);
                // WAVE_FORMAT_EXTENSIBLE keeps the real format in the first two bytes of a GUID.
                if tag == 0xFFFE && body.len() >= 26 {
                    tag = u16::from_le_bytes([body[24], body[25]]);
                }
                format = Some((tag, channels, rate, bits));
                reader.seek(SeekFrom::Start(position + size + (size & 1)))?;
            }
            b"data" => {
                let (tag, channels, sample_rate, bits) =
                    format.ok_or_else(|| bad("the data chunk comes before the format chunk"))?;
                let float = match (tag, bits) {
                    (1, 8 | 16 | 24 | 32) => false,
                    (3, 32 | 64) => true,
                    _ => {
                        return Err(bad(format!(
                            "unsupported WAV encoding (format {tag}, {bits} bits)"
                        )))
                    }
                };
                if channels == 0 || sample_rate == 0 {
                    return Err(bad("the file claims zero channels or a zero sample rate"));
                }
                let available = file_len.saturating_sub(position);
                let declared = match data64 {
                    Some(size64) if rf64 && size == u32::MAX as u64 => size64,
                    _ => size,
                };
                // A recording interrupted before its final header update declares less than it
                // holds. Our own files end with their data chunk, so for those the file's
                // length is the truth; for anyone else's, only a size that cannot be right
                // (zero, or past the end of the file) is overridden.
                let data_bytes = if ours || declared == 0 || declared > available {
                    available
                } else {
                    declared
                };
                let frame_bytes = channels as u64 * (bits as u64 / 8);
                return Ok(Layout {
                    info: WavInfo {
                        sample_rate,
                        channels,
                        frames: data_bytes / frame_bytes,
                        bits_per_sample: bits,
                        float,
                    },
                    data_start: position,
                });
            }
            _ => {
                reader.seek(SeekFrom::Start(position + size + (size & 1)))?;
            }
        }
        position += size + (size & 1);
    }
}

/// Reads a WAV file's header without loading its audio.
pub fn probe(path: &Path) -> Result<WavInfo, DiskError> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    Ok(read_layout(&mut BufReader::new(file), len)?.info)
}

/// Loads a whole WAV file as planar `f32`.
pub fn read(path: &Path) -> Result<AudioData, DiskError> {
    read_range(path, 0, u64::MAX)
}

/// Loads `frames` frames starting at `first_frame` (clamped to the file).
pub fn read_range(path: &Path, first_frame: u64, frames: u64) -> Result<AudioData, DiskError> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let layout = read_layout(&mut reader, len)?;
    let info = layout.info;
    let first = first_frame.min(info.frames);
    let count = frames.min(info.frames - first) as usize;
    let channels = info.channels as usize;
    let sample_bytes = info.bits_per_sample as usize / 8;
    let frame_bytes = channels * sample_bytes;
    reader.seek(SeekFrom::Start(
        layout.data_start + first * frame_bytes as u64,
    ))?;

    let mut out = vec![Vec::with_capacity(count); channels];
    let mut chunk = vec![0u8; frame_bytes * 4096];
    let mut remaining = count;
    while remaining > 0 {
        let take = remaining.min(4096);
        let bytes = &mut chunk[..take * frame_bytes];
        reader
            .read_exact(bytes)
            .map_err(|_| bad("the file ends before its audio does"))?;
        for frame in bytes.chunks_exact(frame_bytes) {
            for (channel, sample) in out.iter_mut().zip(frame.chunks_exact(sample_bytes)) {
                channel.push(decode_sample(sample, info.float));
            }
        }
        remaining -= take;
    }
    Ok(AudioData {
        sample_rate: info.sample_rate,
        channels: out,
    })
}

#[inline]
fn decode_sample(bytes: &[u8], float: bool) -> f32 {
    match (bytes.len(), float) {
        (4, true) => f32::from_le_bytes(bytes.try_into().unwrap()),
        (8, true) => f64::from_le_bytes(bytes.try_into().unwrap()) as f32,
        (1, false) => (bytes[0] as f32 - 128.0) / 128.0,
        (2, false) => i16::from_le_bytes(bytes.try_into().unwrap()) as f32 / 32_768.0,
        (3, false) => {
            let value = i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8;
            value as f32 / 8_388_608.0
        }
        (4, false) => i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2_147_483_648.0,
        _ => 0.0,
    }
}

/// Writes planar audio to a new WAV file in one go.
pub fn write(path: &Path, data: &AudioData, format: SampleFormat) -> Result<(), DiskError> {
    let spec = WavSpec {
        sample_rate: data.sample_rate,
        channels: data.channels.len() as u16,
        format,
    };
    let mut writer = WavWriter::create(path, spec)?;
    let channels: Vec<&[f32]> = data.channels.iter().map(|c| c.as_slice()).collect();
    writer.write_planar(&channels)?;
    writer.finish()?;
    Ok(())
}
