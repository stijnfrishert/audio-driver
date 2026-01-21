use super::{
    AudioCallback, Backend, Configuration, ConfigureError, Layout, NewBackendError,
    StartBackendError,
};
use coreaudio::sys::{
    self, AudioBuffer, AudioBufferList, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID,
    AudioDeviceID, AudioDeviceIOProcID, AudioDeviceStart, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectSetPropertyData, AudioTimeStamp, AudioValueRange, OSStatus, noErr,
};
use std::{
    ffi::c_void,
    mem::{MaybeUninit, size_of},
    ops::RangeInclusive,
    slice,
};

#[allow(clippy::complexity)]
struct UserData {
    callback: Box<dyn FnMut(&mut [f32], Layout, usize) + Send>,
}

pub struct CoreAudioBackend {
    device: AudioDeviceID,
    device_info: DeviceInfo,
    configuration: Configuration,
    audio_io_proc: Option<AudioDeviceIOProcID>,
    callback: Option<*mut UserData>,
}

impl CoreAudioBackend {
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

    fn query_configuration(device: AudioDeviceID, channel_count: usize) -> Result<Configuration, coreaudio::Error> {
        let sample_rate = Self::get_property_data::<f64>(
            device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyNominalSampleRate,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
        )? as u32;

        let buffer_size = Self::get_property_data::<u32>(
            device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyBufferFrameSize,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
        )? as usize;

        Ok(Configuration {
            channel_count,
            sample_rate,
            buffer_size,
        })
    }
}

unsafe impl Send for CoreAudioBackend {}
unsafe impl Sync for CoreAudioBackend {}

impl Drop for CoreAudioBackend {
    fn drop(&mut self) {
        self.stop()
    }
}

impl Backend for CoreAudioBackend {
    /// Construct the backend with the default output device selected
    fn new(configuration: Configuration) -> Result<Self, NewBackendError> {
        // The backend is just always going to use the default device
        let device = Self::default_device(false).map_err(|_| NewBackendError::NoDefaultDevice)?;

        // Retrieve the available settings for this device, and check if it supports out configuration
        let device_info =
            Self::get_device_info(device, false).map_err(|_| NewBackendError::DeviceQueryFailed)?;

        if !device_info.supports(&configuration) {
            return Err(NewBackendError::UnsupportedConfiguration);
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
        .map_err(|_| NewBackendError::DeviceQueryFailed)?;

        Self::set_property_data(
            device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyBufferFrameSize,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
            &(configuration.buffer_size as u32),
        )
        .map_err(|_| NewBackendError::DeviceQueryFailed)?;

        // Query the actual configuration - the device may not have honored our request exactly
        let configuration = Self::query_configuration(device, configuration.channel_count)
            .map_err(|_| NewBackendError::DeviceQueryFailed)?;

        Ok(Self {
            device,
            device_info,
            configuration,
            audio_io_proc: None,
            callback: None,
        })
    }

    fn new_with_default_configuration() -> Result<Self, NewBackendError> {
        let device = Self::default_device(false).map_err(|_| NewBackendError::NoDefaultDevice)?;

        let device_info =
            Self::get_device_info(device, false).map_err(|_| NewBackendError::DeviceQueryFailed)?;

        let configuration =
            Self::query_configuration(device, device_info.max_channel_count)
                .map_err(|_| NewBackendError::DeviceQueryFailed)?;

        Ok(Self {
            device,
            device_info,
            configuration,
            audio_io_proc: None,
            callback: None,
        })
    }

    fn configure(&mut self, configuration: Configuration) -> Result<(), ConfigureError> {
        if self.has_started() {
            return Err(ConfigureError::AlreadyStarted);
        }

        if !self.device_info.supports(&configuration) {
            return Err(ConfigureError::UnsupportedConfiguration);
        }

        Self::set_property_data(
            self.device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyNominalSampleRate,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
            &(configuration.sample_rate as f64),
        )
        .map_err(|_| ConfigureError::DeviceConfigureFailed)?;

        Self::set_property_data(
            self.device,
            &AudioObjectPropertyAddress {
                mSelector: sys::kAudioDevicePropertyBufferFrameSize,
                mScope: sys::kAudioObjectPropertyScopeWildcard,
                mElement: sys::kAudioObjectPropertyElementMain,
            },
            &(configuration.buffer_size as u32),
        )
        .map_err(|_| ConfigureError::DeviceConfigureFailed)?;

        // Query the actual configuration - the device may not have honored our request exactly
        self.configuration = Self::query_configuration(self.device, configuration.channel_count)
            .map_err(|_| ConfigureError::DeviceConfigureFailed)?;
        Ok(())
    }

    fn configuration(&self) -> Configuration {
        self.configuration
    }

    fn start(&mut self, callback: Box<AudioCallback>) -> Result<(), StartBackendError> {
        self.stop();

        let callback = Box::new(UserData { callback });
        let callback = Box::into_raw(callback);

        let mut proc_id = MaybeUninit::<AudioDeviceIOProcID>::uninit();
        let result = coreaudio::Error::from_os_status(unsafe {
            AudioDeviceCreateIOProcID(
                self.device,
                Some(audio_io_proc),
                callback as *mut c_void,
                proc_id.as_mut_ptr(),
            )
        });

        if result.is_err() {
            // Clean up the callback before returning
            let _ = unsafe { Box::from_raw(callback) };
            return Err(StartBackendError);
        }

        self.callback = Some(callback);

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
                Err(StartBackendError)
            }
        }
    }

    fn stop(&mut self) {
        if let Some(proc_id) = self.audio_io_proc.take() {
            unsafe { AudioDeviceDestroyIOProcID(self.device, proc_id) };

            if let Some(callback) = self.callback.take() {
                let _ = unsafe { Box::from_raw(callback) };
            }
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
        let list = unsafe { input.as_ref() }.unwrap();
        let ptr = &list.mBuffers[0] as *const AudioBuffer;
        unsafe { slice::from_raw_parts(ptr, list.mNumberBuffers as usize) }
    };

    let output_buffers = {
        let list = unsafe { output.as_mut() }.unwrap();
        let ptr = &mut list.mBuffers[0] as *mut AudioBuffer;
        unsafe { slice::from_raw_parts_mut(ptr, list.mNumberBuffers as usize) }
    };

    let user_data = user_data.cast::<UserData>();
    let user_data = unsafe { user_data.as_mut() }.unwrap();

    let channels = {
        let data = output_buffers[0].mData.cast::<f32>();
        let len = output_buffers[0].mDataByteSize as usize / size_of::<f32>();
        unsafe { slice::from_raw_parts_mut(data, len) }
    };

    (user_data.callback)(
        channels,
        Layout::Interleaved,
        channels.len() / output_buffers[0].mNumberChannels as usize,
    );

    noErr as i32
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
