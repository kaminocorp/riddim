//! Small real-time DSP pieces used by the nodes.

use riddim_rt::smooth::LinearSmoother;

/// A fixed integer delay for latency compensation.
pub struct DelayLine {
    channels: Vec<Vec<f32>>,
    position: usize,
}

impl DelayLine {
    /// A delay of `samples` on each of `channels` channels. Allocates; build on the control
    /// thread.
    pub fn new(channels: usize, samples: usize) -> Self {
        Self {
            channels: vec![vec![0.0; samples.max(1)]; channels],
            position: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    /// Adds the delayed version of `input` (channel `channel`) into `output`, and stores
    /// `input` for later. Call once per channel per block, then [`advance`](Self::advance).
    #[inline]
    pub fn process_add(&mut self, channel: usize, input: &[f32], output: &mut [f32]) {
        let Some(line) = self.channels.get_mut(channel) else {
            return;
        };
        let len = line.len();
        let mut at = self.position;
        for (i, o) in input.iter().zip(output.iter_mut()) {
            *o += line[at];
            line[at] = *i;
            at += 1;
            if at == len {
                at = 0;
            }
        }
    }

    /// Moves the line forward by one block of `frames`.
    #[inline]
    pub fn advance(&mut self, frames: usize) {
        self.position = (self.position + frames) % self.len().max(1);
    }
}

impl DelayLine {
    /// Forgets everything in the line.
    pub fn clear(&mut self) {
        for line in &mut self.channels {
            line.fill(0.0);
        }
    }
}

/// Per-channel gains for a stereo strip from its fader and pan.
///
/// A balance control: at centre both channels are at the fader's gain; turning towards one side
/// fades the other out linearly. Unity at centre keeps a default strip bit-transparent.
#[inline]
pub fn strip_gains(gain: f32, pan: f32, mute: bool) -> (f32, f32) {
    if mute {
        return (0.0, 0.0);
    }
    let pan = pan.clamp(-1.0, 1.0);
    (gain * (1.0 - pan).min(1.0), gain * (1.0 + pan).min(1.0))
}

/// Smoothed stereo gain.
pub struct StereoGain {
    left: LinearSmoother,
    right: LinearSmoother,
}

impl StereoGain {
    pub fn new(ramp_samples: u32) -> Self {
        // Start at unity so a default strip passes its first block unchanged.
        Self {
            left: LinearSmoother::new(1.0, ramp_samples),
            right: LinearSmoother::new(1.0, ramp_samples),
        }
    }

    /// Jumps straight to the gains (no ramp): used for the first block of a node's life.
    pub fn snap(&mut self, gains: (f32, f32)) {
        self.left.reset(gains.0);
        self.right.reset(gains.1);
    }

    #[inline]
    pub fn set(&mut self, gains: (f32, f32)) {
        self.left.set_target(gains.0);
        self.right.set_target(gains.1);
    }

    #[inline]
    pub fn apply(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.left.apply_gain(left);
        self.right.apply_gain(right);
    }
}

/// The metronome's voice: a short decaying sine.
pub struct Click {
    remaining: u32,
    phase: f32,
    increment: f32,
    level: f32,
    decay: f32,
}

impl Click {
    pub fn new() -> Self {
        Self {
            remaining: 0,
            phase: 0.0,
            increment: 0.0,
            level: 0.0,
            decay: 1.0,
        }
    }

    /// Starts a click. Accented clicks (bar lines) are an octave higher.
    pub fn trigger(&mut self, sample_rate: u32, accent: bool, gain: f32) {
        let hz = if accent { 1760.0 } else { 880.0 };
        let length = sample_rate / 25; // 40 ms
        self.remaining = length;
        self.phase = 0.0;
        self.increment = std::f32::consts::TAU * hz / sample_rate as f32;
        self.level = gain;
        // Fall by 60 dB over the click's length.
        self.decay = (0.001f32).powf(1.0 / length as f32);
    }

    pub fn silence(&mut self) {
        self.remaining = 0;
    }

    /// Adds the click into both channels.
    #[inline]
    pub fn render_add(&mut self, left: &mut [f32], right: &mut [f32]) {
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            if self.remaining == 0 {
                return;
            }
            let sample = self.phase.sin() * self.level;
            *l += sample;
            *r += sample;
            self.phase += self.increment;
            if self.phase > std::f32::consts::TAU {
                self.phase -= std::f32::consts::TAU;
            }
            self.level *= self.decay;
            self.remaining -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_line_delays_by_exactly_its_length_across_blocks() {
        let mut delay = DelayLine::new(1, 100);
        let mut output = Vec::new();
        let mut n = 0;
        for frames in [64usize, 1, 37, 128, 100, 7] {
            let input: Vec<f32> = (n..n + frames).map(|i| i as f32 + 1.0).collect();
            let mut block = vec![0.0; frames];
            delay.process_add(0, &input, &mut block);
            delay.advance(frames);
            output.extend(block);
            n += frames;
        }
        for (i, sample) in output.iter().enumerate() {
            let expected = if i < 100 { 0.0 } else { (i - 100) as f32 + 1.0 };
            assert_eq!(*sample, expected, "sample {i}");
        }
    }

    #[test]
    fn balance_is_unity_at_centre_and_fades_the_far_side() {
        assert_eq!(strip_gains(1.0, 0.0, false), (1.0, 1.0));
        assert_eq!(strip_gains(0.5, -1.0, false), (0.5, 0.0));
        assert_eq!(strip_gains(2.0, 0.5, false), (1.0, 2.0));
        assert_eq!(strip_gains(1.0, 0.0, true), (0.0, 0.0));
    }

    #[test]
    fn click_is_finite_and_ends() {
        let mut click = Click::new();
        click.trigger(48_000, true, 0.5);
        let mut left = vec![0.0f32; 4000];
        let mut right = vec![0.0f32; 4000];
        click.render_add(&mut left, &mut right);
        assert!(left[1] != 0.0);
        assert!(left.iter().all(|s| s.abs() <= 0.5));
        assert!(
            left[1920..].iter().all(|&s| s == 0.0),
            "a click lasts 40 ms"
        );
        assert_eq!(left, right);
    }
}
