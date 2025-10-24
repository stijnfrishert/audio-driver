use thiserror::Error;

#[cfg(target_os = "macos")]
pub mod core_audio;

pub trait Backend: Sized + Send + Sync {
    /// Construct a new audio backend
    fn new(configuration: Configuration) -> Result<Self, NewBackendError>;

    /// Configure the settings to use for the audio callback
    fn configure(&mut self, configuration: Configuration) -> Result<(), ConfigureError>;

    /// Retrieve the settings used for the audio callback
    fn configuration(&self) -> Configuration;

    /// Start the audio callback
    fn start(&mut self, callback: Box<AudioCallback>) -> Result<(), StartBackendError>;

    /// Stop the audio callback
    fn stop(&mut self);

    /// Is the audio callback running
    fn has_started(&self) -> bool;
}

/// The audio callback that will be called by the audio backend
pub type AudioCallback = dyn FnMut(&mut [f32], Layout, usize) + Send + 'static;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Interleaved,
    NonInterleaved,
}

impl Layout {
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

/// Settings used for running an audio callback
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    pub channel_count: usize,
    pub sample_rate: u32,
    pub buffer_size: usize,
}

impl Configuration {
    pub fn new(channel_count: usize, sample_rate: u32, buffer_size: usize) -> Self {
        Self {
            channel_count,
            sample_rate,
            buffer_size,
        }
    }
}

#[derive(Debug, Error)]
pub enum NewBackendError {
    #[error("Could not find the default output device")]
    NoDefaultDevice,

    #[error("The provided configuration was not supported")]
    UnsupportedConfiguration,
}

#[derive(Debug, Error)]
pub enum ConfigureError {
    #[error("The audio callback has already started. You need to stop it first.")]
    AlreadyStarted,

    #[error("The provided configuration is not supported")]
    UnsupportedConfiguration,
}

#[derive(Debug, Error)]
#[error("The device could not be started")]
pub struct StartBackendError;

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
