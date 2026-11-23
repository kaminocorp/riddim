//! Presenting planar audio as slices of slices without allocating per block.

/// Reusable storage for the slice headers of one block's channels.
///
/// Audio APIs in this workspace take `&[&[f32]]` and `&mut [&mut [f32]]`. Building those from
/// buffers that live elsewhere needs somewhere to put the inner slice headers; allocating that
/// on every block is not an option on the audio thread. A `PlanarScratch` is that somewhere,
/// allocated once.
pub struct PlanarScratch {
    inputs: Vec<&'static [f32]>,
    outputs: Vec<&'static mut [f32]>,
}

// SAFETY: the stored headers are only ever read through the borrows `fill` hands out, which
// cannot outlive the buffers they point into (see `fill`). A scratch at rest holds nothing
// anyone can reach.
unsafe impl Send for PlanarScratch {}

impl PlanarScratch {
    /// Room for up to this many channels in each direction.
    pub fn new(max_inputs: usize, max_outputs: usize) -> Self {
        Self {
            inputs: Vec::with_capacity(max_inputs),
            outputs: Vec::with_capacity(max_outputs),
        }
    }

    pub fn input_capacity(&self) -> usize {
        self.inputs.capacity()
    }

    pub fn output_capacity(&self) -> usize {
        self.outputs.capacity()
    }

    /// Lays out `frames`-long views of the given channels and returns them as slices of
    /// slices. Never allocates: channels beyond the capacity given to [`new`](Self::new) are
    /// left out.
    ///
    /// Panics if a channel is shorter than `frames`.
    #[allow(clippy::type_complexity)]
    pub fn fill<'s>(
        &'s mut self,
        inputs: impl Iterator<Item = &'s [f32]>,
        outputs: impl Iterator<Item = &'s mut [f32]>,
        frames: usize,
    ) -> (&'s [&'s [f32]], &'s mut [&'s mut [f32]]) {
        self.inputs.clear();
        self.outputs.clear();
        // SAFETY: the vectors are typed `'static` only because a struct cannot name the
        // lifetime of each block. Every header stored here borrows a channel for `'s`, and the
        // only way to read one back is through the slices returned below, which also live for
        // `'s` and keep `self` mutably borrowed for that long. So a header is never reachable
        // after its channel's borrow ends: the next access to the vectors is the `clear` at the
        // top of the next `fill` (or the drop of the scratch), neither of which reads them.
        unsafe {
            for channel in inputs.take(self.inputs.capacity()) {
                let channel = &channel[..frames];
                self.inputs
                    .push(std::slice::from_raw_parts(channel.as_ptr(), frames));
            }
            for channel in outputs.take(self.outputs.capacity()) {
                let channel = &mut channel[..frames];
                self.outputs
                    .push(std::slice::from_raw_parts_mut(channel.as_mut_ptr(), frames));
            }
            let outputs = std::mem::transmute::<
                &'s mut [&'static mut [f32]],
                &'s mut [&'s mut [f32]],
            >(&mut self.outputs[..]);
            (&self.inputs[..], outputs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presents_channels_and_truncates_to_frames() {
        let inputs = [vec![1.0f32; 8], vec![2.0f32; 8]];
        let mut outputs = vec![vec![0.0f32; 8]; 3];
        let mut scratch = PlanarScratch::new(2, 3);
        {
            let (ins, outs) = scratch.fill(
                inputs.iter().map(|c| c.as_slice()),
                outputs.iter_mut().map(|c| c.as_mut_slice()),
                4,
            );
            assert_eq!((ins.len(), outs.len()), (2, 3));
            for (i, out) in outs.iter_mut().enumerate() {
                assert_eq!(out.len(), 4);
                out.fill(i as f32);
            }
            assert_eq!(ins.iter().map(|c| c.iter().sum::<f32>()).sum::<f32>(), 12.0);
        }
        assert_eq!(outputs[2], vec![2.0, 2.0, 2.0, 2.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn channels_beyond_capacity_are_left_out_and_reuse_is_clean() {
        let inputs = vec![vec![0.0f32; 4]; 5];
        let mut outputs = vec![vec![0.0f32; 4]; 5];
        let mut scratch = PlanarScratch::new(1, 2);
        for _ in 0..3 {
            let (ins, outs) = scratch.fill(
                inputs.iter().map(|c| c.as_slice()),
                outputs.iter_mut().map(|c| c.as_mut_slice()),
                4,
            );
            assert_eq!((ins.len(), outs.len()), (1, 2));
        }
    }
}
