use thiserror::Error;

#[cfg(target_os = "macos")]
pub mod core_audio;

pub trait Backend: Send + Sync {
    /// Configure the settings to use for the audio callback
    fn configure(&mut self, configuration: Configuration) -> Result<(), ConfigureError>;

    /// Retrieve the settings used for the audio callback
    fn configuration(&self) -> Configuration;

    /// Start the audio callback
    fn start(&mut self, callback: Box<AudioCallback>) -> Result<(), StartError>;

    /// Stop the audio callback
    fn stop(&mut self);

    /// Is the audio callback running
    fn has_started(&self) -> bool;
}

/// The audio callback that will be called by the audio backend
pub type AudioCallback = dyn FnMut(&mut [f32], usize) + Send + 'static;

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
pub enum ConfigureError {
    #[error("The audio callback has already started. You need to stop it first.")]
    AlreadyStarted,

    #[error("The provided configuration is not supported")]
    UnsupportedConfiguration,
}

#[derive(Debug, Error)]
#[error("The device could not be started")]
pub struct StartError;
