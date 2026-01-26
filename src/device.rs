use cpal::traits::{DeviceTrait, HostTrait};
use thiserror::Error;

/// An audio device that can be used for playback or recording
pub struct AudioDevice {
    inner: cpal::Device,
}

impl AudioDevice {
    /// Get the name of this device
    pub fn name(&self) -> Result<String, DeviceNameError> {
        self.inner.name().map_err(|_| DeviceNameError)
    }

    /// Get the supported output configurations for this device
    pub fn supported_output_configs(
        &self,
    ) -> Result<impl Iterator<Item = SupportedStreamConfigRange>, SupportedConfigsError> {
        self.inner
            .supported_output_configs()
            .map(|iter| iter.map(SupportedStreamConfigRange::from))
            .map_err(|_| SupportedConfigsError)
    }

    /// Get the default output configuration for this device
    pub fn default_output_config(&self) -> Result<StreamConfig, DefaultConfigError> {
        self.inner
            .default_output_config()
            .map(StreamConfig::from)
            .map_err(|_| DefaultConfigError)
    }

    /// Find an output configuration matching the given requirements.
    ///
    /// Returns the first config that matches all specified criteria.
    /// Use `None` for channels/sample_rate you don't care about.
    ///
    /// # Example
    /// ```ignore
    /// // Find mono config at 48kHz with 512 frame buffer
    /// let config = device.find_output_config(Some(1), Some(48000), BufferSize::Fixed(512));
    ///
    /// // Find stereo config at any sample rate, default buffer
    /// let config = device.find_output_config(Some(2), None, BufferSize::Default);
    /// ```
    pub fn find_output_config(
        &self,
        channels: Option<u16>,
        sample_rate: Option<u32>,
        buffer_size: BufferSize,
    ) -> Option<StreamConfig> {
        for range in self.supported_output_configs().ok()? {
            // Check channel count if specified
            if let Some(ch) = channels
                && range.channels() != ch
            {
                continue;
            }

            // Check if sample rate is in range (if specified)
            if let Some(sr) = sample_rate
                && (sr < range.min_sample_rate() || sr > range.max_sample_rate())
            {
                continue;
            }

            // Found a match - use requested sample rate or pick middle of range
            let rate = sample_rate
                .unwrap_or_else(|| (range.min_sample_rate() + range.max_sample_rate()) / 2);

            let mut config = range.with_sample_rate(rate);
            config.buffer_size = buffer_size;

            return Some(config);
        }

        None
    }

    pub(crate) fn inner(&self) -> &cpal::Device {
        &self.inner
    }
}

/// A supported stream configuration range
pub struct SupportedStreamConfigRange {
    inner: cpal::SupportedStreamConfigRange,
}

impl From<cpal::SupportedStreamConfigRange> for SupportedStreamConfigRange {
    fn from(inner: cpal::SupportedStreamConfigRange) -> Self {
        Self { inner }
    }
}

impl SupportedStreamConfigRange {
    /// Get the number of channels
    pub fn channels(&self) -> u16 {
        self.inner.channels()
    }

    /// Get the minimum sample rate
    pub fn min_sample_rate(&self) -> u32 {
        self.inner.min_sample_rate().0
    }

    /// Get the maximum sample rate
    pub fn max_sample_rate(&self) -> u32 {
        self.inner.max_sample_rate().0
    }

    /// Get a config with a specific sample rate (clamped to supported range)
    pub fn with_sample_rate(&self, sample_rate: u32) -> StreamConfig {
        let clamped = sample_rate
            .max(self.min_sample_rate())
            .min(self.max_sample_rate());
        StreamConfig::from(self.inner.with_sample_rate(cpal::SampleRate(clamped)))
    }
}

/// A stream configuration
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Number of channels
    pub channels: u16,
    /// Sample rate in Hz
    pub sample_rate: u32,
    /// Buffer size preference
    pub buffer_size: BufferSize,
}

impl From<cpal::StreamConfig> for StreamConfig {
    fn from(config: cpal::StreamConfig) -> Self {
        Self {
            channels: config.channels,
            sample_rate: config.sample_rate.0,
            buffer_size: match config.buffer_size {
                cpal::BufferSize::Default => BufferSize::Default,
                cpal::BufferSize::Fixed(size) => BufferSize::Fixed(size),
            },
        }
    }
}

impl From<cpal::SupportedStreamConfig> for StreamConfig {
    fn from(config: cpal::SupportedStreamConfig) -> Self {
        Self {
            channels: config.channels(),
            sample_rate: config.sample_rate().0,
            buffer_size: BufferSize::Default,
        }
    }
}

impl From<&StreamConfig> for cpal::StreamConfig {
    fn from(config: &StreamConfig) -> Self {
        cpal::StreamConfig {
            channels: config.channels,
            sample_rate: cpal::SampleRate(config.sample_rate),
            buffer_size: match config.buffer_size {
                BufferSize::Default => cpal::BufferSize::Default,
                BufferSize::Fixed(size) => cpal::BufferSize::Fixed(size),
            },
        }
    }
}

/// Buffer size preference
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferSize {
    /// Let the backend choose the buffer size
    Default,
    /// Request a specific buffer size (in frames)
    Fixed(u32),
}

/// Enumerate all available audio output devices
pub fn enumerate_output_devices() -> Result<impl Iterator<Item = AudioDevice>, EnumerateError> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|iter| iter.map(|d| AudioDevice { inner: d }))
        .map_err(|_| EnumerateError)
}

/// Get the default output device
pub fn default_output_device() -> Option<AudioDevice> {
    cpal::default_host()
        .default_output_device()
        .map(|d| AudioDevice { inner: d })
}

#[derive(Debug, Error)]
#[error("Failed to enumerate audio devices")]
pub struct EnumerateError;

#[derive(Debug, Error)]
#[error("Failed to get device name")]
pub struct DeviceNameError;

#[derive(Debug, Error)]
#[error("Failed to get supported configurations")]
pub struct SupportedConfigsError;

#[derive(Debug, Error)]
#[error("Failed to get default configuration")]
pub struct DefaultConfigError;
