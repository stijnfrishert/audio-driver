/// Buffer layout indicator for audio data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Samples are interleaved: [L0, R0, L1, R1, ...]
    Interleaved,
    /// Samples are planar: [L0, L1, ..., R0, R1, ...]
    NonInterleaved,
}

impl Layout {
    /// Convert non-interleaved (planar) data to interleaved format in-place
    ///
    /// # Arguments
    /// * `buf` - The buffer to convert (modified in-place)
    /// * `channels` - Number of audio channels
    /// * `scratch` - A scratch buffer at least as large as `buf`
    pub fn interleave<T: Copy>(buf: &mut [T], channels: usize, scratch: &mut [T]) {
        assert!(channels > 0, "channels must be > 0");

        if buf.is_empty() || channels == 1 {
            return; // nothing to do
        }

        assert!(
            buf.len().is_multiple_of(channels),
            "buffer length must be divisible by channels"
        );

        assert!(
            scratch.len() >= buf.len(),
            "scratch buffer too small: need {}, have {}",
            buf.len(),
            scratch.len()
        );

        let frames = buf.len() / channels;

        // Write interleaved into scratch
        for ch in 0..channels {
            let src_base = ch * frames;
            for f in 0..frames {
                let dst_idx = f * channels + ch;
                scratch[dst_idx] = buf[src_base + f];
            }
        }

        // Copy back to buf
        buf.copy_from_slice(&scratch[..buf.len()]);
    }

    /// Convert interleaved data to non-interleaved (planar) format in-place
    ///
    /// # Arguments
    /// * `buf` - The buffer to convert (modified in-place)
    /// * `channels` - Number of audio channels
    /// * `scratch` - A scratch buffer at least as large as `buf`
    pub fn deinterleave<T: Copy>(buf: &mut [T], channels: usize, scratch: &mut [T]) {
        assert!(channels > 0, "channels must be > 0");
        if buf.is_empty() || channels == 1 {
            return;
        }
        assert!(
            buf.len().is_multiple_of(channels),
            "buffer length must be divisible by channels"
        );
        assert!(
            scratch.len() >= buf.len(),
            "scratch buffer too small: need {}, have {}",
            buf.len(),
            scratch.len()
        );

        let frames = buf.len() / channels;

        // Write planar into scratch
        for ch in 0..channels {
            let dst_base = ch * frames;
            for f in 0..frames {
                let src_idx = f * channels + ch;
                scratch[dst_base + f] = buf[src_idx];
            }
        }

        // Copy back to buf
        buf.copy_from_slice(&scratch[..buf.len()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave() {
        let mut buf = [1, 2, 3, 4, 5, 6];
        let mut scratch = [0; 6];

        Layout::interleave(&mut buf, 2, &mut scratch);
        assert_eq!(buf, [1, 4, 2, 5, 3, 6]);
    }

    #[test]
    fn deinterleave() {
        let mut buf = [1, 4, 2, 5, 3, 6];
        let mut scratch = [0; 6];

        Layout::deinterleave(&mut buf, 2, &mut scratch);
        assert_eq!(buf, [1, 2, 3, 4, 5, 6]);
    }
}
