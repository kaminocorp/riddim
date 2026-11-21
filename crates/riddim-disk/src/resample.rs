//! Sample-rate conversion for imported media.
//!
//! A windowed-sinc resampler: simple, deterministic, and good enough for bringing a file to the
//! session rate once, at import. It is not a real-time resampler and is never run on the audio
//! thread.

use crate::AudioData;

/// Zero crossings of the sinc on each side of the centre, before widening for down-sampling.
const HALF_WIDTH: usize = 32;
/// Kaiser window shape; about 100 dB of stop-band rejection.
const KAISER_BETA: f64 = 10.0;

/// Modified Bessel function of the first kind, order zero.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..64 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Converts one channel from `from_rate` to `to_rate`.
///
/// The output has `ceil(len * to / from)` samples. Positions are computed in integers, so the
/// result does not depend on how the input is sliced or on anything but its samples.
pub fn resample_channel(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let from = from_rate as u64;
    let to = to_rate as u64;
    let out_len = (input.len() as u64 * to).div_ceil(from) as usize;

    // When down-sampling, the low-pass moves down to the new Nyquist and the kernel widens.
    let cutoff = (to_rate as f64 / from_rate as f64).min(1.0);
    let half = (HALF_WIDTH as f64 / cutoff).ceil() as i64;
    let i0_beta = bessel_i0(KAISER_BETA);
    let kernel = |distance: f64| -> f64 {
        let x = distance / half as f64;
        if x.abs() >= 1.0 {
            return 0.0;
        }
        let window = bessel_i0(KAISER_BETA * (1.0 - x * x).sqrt()) / i0_beta;
        let t = distance * cutoff * std::f64::consts::PI;
        let sinc = if t.abs() < 1e-12 { 1.0 } else { t.sin() / t };
        sinc * cutoff * window
    };

    let mut output = Vec::with_capacity(out_len);
    for n in 0..out_len as u64 {
        let numerator = n * from;
        let centre = (numerator / to) as i64;
        let fraction = (numerator % to) as f64 / to as f64;
        let mut acc = 0.0f64;
        for k in (centre - half + 1)..=(centre + half) {
            if k < 0 || k as usize >= input.len() {
                continue;
            }
            acc += input[k as usize] as f64 * kernel(k as f64 - centre as f64 - fraction);
        }
        output.push(acc as f32);
    }
    output
}

/// Converts every channel of `data` to `to_rate`. Returns the input unchanged if it is already
/// there.
pub fn resample(data: AudioData, to_rate: u32) -> AudioData {
    if data.sample_rate == to_rate {
        return data;
    }
    AudioData {
        channels: data
            .channels
            .iter()
            .map(|c| resample_channel(c, data.sample_rate, to_rate))
            .collect(),
        sample_rate: to_rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, len: usize) -> Vec<f32> {
        (0..len)
            .map(|n| (std::f64::consts::TAU * hz * n as f64 / rate as f64).sin() as f32)
            .collect()
    }

    /// Worst error against the ideal signal, ignoring the edges where the kernel runs off.
    fn worst_error(output: &[f32], rate: u32, hz: f64) -> f64 {
        let edge = 400;
        output[edge..output.len() - edge]
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                let ideal = (std::f64::consts::TAU * hz * (i + edge) as f64 / rate as f64).sin();
                (s as f64 - ideal).abs()
            })
            .fold(0.0, f64::max)
    }

    #[test]
    fn upsampling_and_downsampling_a_sine_is_accurate() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (48_000, 96_000),
            (96_000, 48_000),
            (44_100, 96_000),
        ] {
            let input = sine(from, 1000.0, from as usize / 4);
            let output = resample_channel(&input, from, to);
            assert_eq!(
                output.len(),
                (input.len() as u64 * to as u64).div_ceil(from as u64) as usize
            );
            let error = worst_error(&output, to, 1000.0);
            assert!(
                error < 1e-4,
                "{from} -> {to}: worst error {error:e} (must be below -80 dB)"
            );
        }
    }

    #[test]
    fn content_above_the_new_nyquist_is_removed() {
        // 30 kHz exists at 96 kHz but cannot exist at 48 kHz.
        let input = sine(96_000, 30_000.0, 9600);
        let output = resample_channel(&input, 96_000, 48_000);
        let peak = output[400..output.len() - 400]
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 1e-3, "alias leaked through at {peak}");
    }

    #[test]
    fn dc_and_identity() {
        let ones = vec![1.0f32; 4000];
        let output = resample_channel(&ones, 44_100, 48_000);
        assert!(output[400..output.len() - 400]
            .iter()
            .all(|s| (s - 1.0).abs() < 1e-4));
        let data = AudioData {
            sample_rate: 48_000,
            channels: vec![vec![0.25; 10]],
        };
        assert_eq!(resample(data.clone(), 48_000), data);
        assert!(resample_channel(&[], 44_100, 48_000).is_empty());
    }
}
