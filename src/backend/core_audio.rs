use super::{Backend, Configuration, ConfigureError, StartError};
use coreaudio::sys::{
    self, noErr, AudioBuffer, AudioBufferList, AudioDeviceCreateIOProcID,
    AudioDeviceDestroyIOProcID, AudioDeviceID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectSetPropertyData, AudioTimeStamp, AudioValueRange,
    OSStatus,
};
use std::{
    ffi::c_void,
    mem::{size_of, MaybeUninit},
    ops::RangeInclusive,
    slice,
};
use thiserror::Error;

pub struct CoreAudioBackend {
    device: AudioDeviceID,
    device_info: DeviceInfo,
    output_channel_count: usize,
    audio_io_proc: Option<AudioDeviceIOProcID>,

    #[allow(clippy::type_complexity)]
    callback: Option<Box<dyn FnMut(&mut [f32], usize)>>,
}

impl CoreAudioBackend {
    /// Construct the backend with the default output device selected
    pub fn new(configuration: Configuration) -> Result<Self, CoreAudioBackendNewError> {
        // The backend is just always going to use the default device
        let device =
            Self::default_device(false).map_err(|_| CoreAudioBackendNewError::NoDefaultDevice)?;

        // Retrieve the available settings for this device, and check if it supports out configuration
        let device_info = Self::get_device_info(device, false).unwrap();

        if !device_info.supports(&configuration) {
            return Err(CoreAudioBackendNewError::UnsupportedConfiguration);
        }

        // Set the sample rate and buffer size
        Self::set_property_data(
            device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyNominalSampleRate,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
            &(configuration.sample_rate as f64),
        )
        .unwrap();

        Self::set_property_data(
            device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyBufferFrameSize,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
            &(configuration.buffer_size as u32),
        )
        .unwrap();

        Ok(Self {
            device,
            device_info,
            output_channel_count: configuration.channel_count,
            audio_io_proc: None,
            callback: None,
        })
    }

    fn default_device(is_input: bool) -> Result<AudioDeviceID, coreaudio::Error> {
        let selector = if is_input {
            sys::kAudioHardwarePropertyDefaultInputDevice
        } else {
            sys::kAudioHardwarePropertyDefaultOutputDevice
        };

        let address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: sys::kAudioObjectPropertyScopeWildcard,
            mElement: sys::kAudioObjectPropertyElementMain,
        };

        Self::get_property_data(sys::kAudioObjectSystemObject, &address)
    }

    fn get_device_info(
        device: AudioDeviceID,
        is_input: bool,
    ) -> Result<DeviceInfo, coreaudio::Error> {
        let scope = if is_input {
            sys::kAudioObjectPropertyScopeInput
        } else {
            sys::kAudioObjectPropertyScopeOutput
        };

        // Sample rate ranges
        let sample_rate_ranges = {
            let bytes_len = Self::get_property_data_size(
                device,
                &AudioObjectPropertyAddress {
                    mSelector: sys::kAudioDevicePropertyAvailableNominalSampleRates,
                    mScope: scope,
                    mElement: sys::kAudioObjectPropertyElementMain,
                },
            )?;

            let mut bytes = vec![MaybeUninit::uninit(); bytes_len];
            let ranges = {
                Self::get_property_data_as_bytes(
                    device,
                    &AudioObjectPropertyAddress {
                        mSelector: sys::kAudioDevicePropertyAvailableNominalSampleRates,
                        mScope: scope,
                        mElement: sys::kAudioObjectPropertyElementMain,
                    },
                    &mut bytes,
                )?;
                let ptr = bytes.as_ptr() as *const AudioValueRange;
                unsafe { slice::from_raw_parts(ptr, bytes_len / size_of::<AudioValueRange>()) }
            };

            ranges
                .iter()
                .map(|range| range.mMinimum..=range.mMaximum)
                .collect()
        };

        let frame_size_range = {
            let range = Self::get_property_data::<AudioValueRange>(
                device,
                &AudioObjectPropertyAddress {
                    mSelector: sys::kAudioDevicePropertyBufferFrameSizeRange,
                    mScope: scope,
                    mElement: sys::kAudioObjectPropertyElementMain,
                },
            )?;

            range.mMinimum as usize..=range.mMaximum as usize
        };

        // Compute the max channel count
        let max_channel_count = {
            let bytes_len = Self::get_property_data_size(
                device,
                &AudioObjectPropertyAddress {
                    mSelector: sys::kAudioDevicePropertyStreamConfiguration,
                    mScope: scope,
                    mElement: sys::kAudioObjectPropertyElementMain,
                },
            )?;

            let mut bytes = vec![MaybeUninit::uninit(); bytes_len];
            let buffers = {
                Self::get_property_data_as_bytes(
                    device,
                    &AudioObjectPropertyAddress {
                        mSelector: sys::kAudioDevicePropertyStreamConfiguration,
                        mScope: scope,
                        mElement: sys::kAudioObjectPropertyElementMain,
                    },
                    &mut bytes,
                )?;
                let buffer_list = unsafe { &*(bytes.as_ptr() as *const AudioBufferList) };
                let ptr = &buffer_list.mBuffers[0] as *const AudioBuffer;
                unsafe { slice::from_raw_parts(ptr, buffer_list.mNumberBuffers as usize) }
            };

            buffers
                .iter()
                .map(|buffer| buffer.mNumberChannels)
                .sum::<u32>() as usize
        };

        Ok(DeviceInfo {
            max_channel_count,
            sample_rate_ranges,
            frame_size_range,
        })
    }

    fn set_property_data<T>(
        object: AudioObjectID,
        address: &AudioObjectPropertyAddress,
        value: &T,
    ) -> Result<(), coreaudio::Error> {
        coreaudio::Error::from_os_status(unsafe {
            AudioObjectSetPropertyData(
                object,
                address as *const _,
                0,
                std::ptr::null(),
                size_of::<T>() as u32,
                value as *const T as *const c_void,
            )
        })
    }

    fn get_property_data_size(
        object: AudioObjectID,
        address: &AudioObjectPropertyAddress,
    ) -> Result<usize, coreaudio::Error> {
        let mut size = 0u32;

        coreaudio::Error::from_os_status(unsafe {
            AudioObjectGetPropertyDataSize(
                object,
                address as *const _,
                0,
                std::ptr::null(),
                &mut size as *mut _,
            )
        })?;

        Ok(size as usize)
    }

    fn get_property_data<T>(
        object: AudioObjectID,
        address: &AudioObjectPropertyAddress,
    ) -> Result<T, coreaudio::Error> {
        let mut data = MaybeUninit::uninit();
        let mut size = size_of::<T>() as u32;

        coreaudio::Error::from_os_status(unsafe {
            AudioObjectGetPropertyData(
                object,
                address as *const _,
                0,
                std::ptr::null(),
                &mut size as *mut _,
                data.as_mut_ptr() as *mut c_void,
            )
        })?;

        Ok(unsafe { data.assume_init() })
    }

    fn get_property_data_as_bytes(
        object: AudioObjectID,
        address: &AudioObjectPropertyAddress,
        bytes: &mut [MaybeUninit<u8>],
    ) -> Result<(), coreaudio::Error> {
        let mut size = bytes.len() as u32;

        coreaudio::Error::from_os_status(unsafe {
            AudioObjectGetPropertyData(
                object,
                address as *const _,
                0,
                std::ptr::null(),
                &mut size as *mut _,
                bytes.as_mut_ptr() as *mut c_void,
            )
        })
    }
}

impl Drop for CoreAudioBackend {
    fn drop(&mut self) {
        self.stop()
    }
}

impl Backend for CoreAudioBackend {
    fn configure(&mut self, configuration: Configuration) -> Result<(), ConfigureError> {
        if self.has_started() {
            Err(ConfigureError::AlreadyStarted)
        } else if self.device_info.supports(&configuration) {
            Self::set_property_data(
                self.device,
                &AudioObjectPropertyAddress {
                    mSelector: sys::kAudioDevicePropertyNominalSampleRate,
                    mScope: sys::kAudioObjectPropertyScopeWildcard,
                    mElement: sys::kAudioObjectPropertyElementMain,
                },
                &(configuration.sample_rate as f64),
            )
            .unwrap();

            Self::set_property_data(
                self.device,
                &AudioObjectPropertyAddress {
                    mSelector: sys::kAudioDevicePropertyBufferFrameSize,
                    mScope: sys::kAudioObjectPropertyScopeWildcard,
                    mElement: sys::kAudioObjectPropertyElementMain,
                },
                &(configuration.buffer_size as u32),
            )
            .unwrap();

            Ok(())
        } else {
            Err(ConfigureError::UnsupportedConfiguration)
        }
    }

    fn configuration(&self) -> Configuration {
        let sample_rate = Self::get_property_data::<f64>(
            self.device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyNominalSampleRate,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
        )
        .unwrap() as u32;

        let buffer_size = Self::get_property_data::<u32>(
            self.device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyBufferFrameSize,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
        )
        .unwrap() as usize;

        Configuration {
            channel_count: self.output_channel_count,
            sample_rate,
            buffer_size,
        }
    }

    fn start<C>(&mut self, callback: C) -> Result<(), StartError>
    where
        C: FnMut(&mut [f32], usize) + Send + 'static,
    {
        self.stop();
        self.callback = Some(Box::new(callback));

        let mut proc_id = MaybeUninit::<AudioDeviceIOProcID>::uninit();
        coreaudio::Error::from_os_status(unsafe {
            AudioDeviceCreateIOProcID(
                self.device,
                Some(audio_io_proc),
                self as *mut Self as *mut c_void,
                proc_id.as_mut_ptr(),
            )
        })
        .unwrap();

        let proc_id = unsafe { proc_id.assume_init() };

        let result =
            coreaudio::Error::from_os_status(unsafe { AudioDeviceStart(self.device, proc_id) });

        match result {
            Ok(()) => {
                self.audio_io_proc = Some(proc_id);
                Ok(())
            }
            Err(_) => {
                let _ = coreaudio::Error::from_os_status(unsafe {
                    AudioDeviceDestroyIOProcID(self.device, proc_id)
                });
                Err(StartError)
            }
        }
    }

    fn stop(&mut self) {
        if let Some(proc_id) = self.audio_io_proc.take() {
            unsafe { AudioDeviceDestroyIOProcID(self.device, proc_id) };
            self.callback = None;
        }
    }

    fn has_started(&self) -> bool {
        self.audio_io_proc.is_some()
    }
}

unsafe extern "C" fn audio_io_proc(
    _: AudioDeviceID,
    _input_now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    user_data: *mut c_void,
) -> OSStatus {
    let _input_buffers = {
        let list = input.as_ref().unwrap();
        let ptr = &list.mBuffers[0] as *const AudioBuffer;
        slice::from_raw_parts(ptr, list.mNumberBuffers as usize)
    };

    let output_buffers = {
        let list = output.as_mut().unwrap();
        let ptr = &mut list.mBuffers[0] as *mut AudioBuffer;
        slice::from_raw_parts_mut(ptr, list.mNumberBuffers as usize)
    };

    let backend = user_data.cast::<CoreAudioBackend>().as_mut().unwrap();
    let callback = backend.callback.as_mut().unwrap();

    let channels = slice::from_raw_parts_mut(
        output_buffers[0].mData.cast::<f32>(),
        output_buffers[0].mDataByteSize as usize / size_of::<f32>(),
    );
    (*callback)(
        channels,
        channels.len() / output_buffers[0].mNumberChannels as usize,
    );

    noErr as i32
}

#[derive(Debug, Error)]
pub enum CoreAudioBackendNewError {
    #[error("Could not find the default output device")]
    NoDefaultDevice,

    #[error("The provided configuration was not supported")]
    UnsupportedConfiguration,
}

struct DeviceInfo {
    max_channel_count: usize,
    sample_rate_ranges: Vec<RangeInclusive<f64>>,
    frame_size_range: RangeInclusive<usize>,
}

impl DeviceInfo {
    pub fn supports(&self, configuration: &Configuration) -> bool {
        let sample_rate = configuration.sample_rate as f64;
        configuration.channel_count <= self.max_channel_count
            && self.frame_size_range.contains(&configuration.buffer_size)
            && self
                .sample_rate_ranges
                .iter()
                .any(|range| range.contains(&sample_rate))
    }
}
