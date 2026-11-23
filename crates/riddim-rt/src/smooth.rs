//! Parameter smoothing.

/// Linear ramp towards a target over a fixed number of samples.
///
/// Used for gain, pan and other parameters where a step would click.
#[derive(Debug, Clone, Copy)]
pub struct LinearSmoother {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
    ramp_samples: u32,
}

impl LinearSmoother {
    /// `ramp_samples` is the length of every ramp; `0` means changes apply immediately.
    pub fn new(initial: f32, ramp_samples: u32) -> Self {
        Self {
            current: initial,
            target: initial,
            step: 0.0,
            remaining: 0,
            ramp_samples,
        }
    }

    /// Changes the ramp length used by later calls to [`set_target`](Self::set_target).
    pub fn set_ramp_samples(&mut self, ramp_samples: u32) {
        self.ramp_samples = ramp_samples;
    }

    /// Starts a ramp from the current value to `target`.
    #[inline]
    pub fn set_target(&mut self, target: f32) {
        if target == self.target {
            return;
        }
        self.target = target;
        if self.ramp_samples == 0 {
            self.current = target;
            self.remaining = 0;
        } else {
            self.remaining = self.ramp_samples;
            self.step = (target - self.current) / self.ramp_samples as f32;
        }
    }

    /// Jumps to `value` with no ramp.
    #[inline]
    pub fn reset(&mut self, value: f32) {
        self.current = value;
        self.target = value;
        self.remaining = 0;
    }

    #[inline]
    pub fn is_ramping(&self) -> bool {
        self.remaining > 0
    }

    #[inline]
    pub fn current(&self) -> f32 {
        self.current
    }

    #[inline]
    pub fn target(&self) -> f32 {
        self.target
    }

    /// Advances one sample and returns the value for it.
    #[inline]
    pub fn tick(&mut self) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            } else {
                self.current += self.step;
            }
        }
        self.current
    }

    /// Multiplies `buf` by the smoothed value, sample by sample.
    #[inline]
    pub fn apply_gain(&mut self, buf: &mut [f32]) {
        if !self.is_ramping() {
            let g = self.current;
            if g != 1.0 {
                for s in buf {
                    *s *= g;
                }
            }
            return;
        }
        for s in buf {
            *s *= self.tick();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_exactly_to_target() {
        let mut s = LinearSmoother::new(0.0, 4);
        s.set_target(1.0);
        let got: Vec<f32> = (0..6).map(|_| s.tick()).collect();
        assert_eq!(got, vec![0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
        assert!(!s.is_ramping());
    }

    #[test]
    fn zero_ramp_is_immediate() {
        let mut s = LinearSmoother::new(0.5, 0);
        s.set_target(2.0);
        assert_eq!(s.tick(), 2.0);
    }

    #[test]
    fn apply_gain_matches_per_sample() {
        let mut a = LinearSmoother::new(1.0, 8);
        let mut b = a;
        a.set_target(0.0);
        b.set_target(0.0);
        let mut buf = [1.0f32; 12];
        a.apply_gain(&mut buf);
        let expect: Vec<f32> = (0..12).map(|_| b.tick()).collect();
        assert_eq!(buf.to_vec(), expect);
    }
}
