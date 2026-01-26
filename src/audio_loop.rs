use crate::backend::Layout;
use crate::device::{AudioDevice, StreamConfig};
use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::RingBuffer;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Sender, channel},
    },
    thread,
};
use thiserror::Error;

/// A handle to an audio loop that can be started and stopped.
///
/// This handle is `Send + Sync + Clone` and can be stored in application state.
/// The actual audio stream is created when `start()` is called and destroyed
/// when `stop()` is called. Subscriptions persist across start/stop cycles.
pub struct AudioLoopHandle<R>
where
    R: Runner,
{
    inner: Arc<AudioLoopHandleInner<R>>,
}

struct AudioLoopHandleInner<R>
where
    R: Runner,
{
    /// The subscribers for updates - persists across start/stop cycles
    subscribers: Arc<Mutex<SubscriptionMap<R::Update>>>,

    /// Flag indicating whether the audio stream is currently running
    stream_active: Arc<AtomicBool>,

    /// Flag to signal the update thread to exit (on Drop)
    thread_alive: Arc<AtomicBool>,

    /// Sender to the stream owner thread (if running)
    /// Used to send commands and stop signal
    stream_command_sender: Mutex<Option<Sender<StreamThreadMessage<R::Command>>>>,

    /// Slot for passing update receiver to the update thread
    update_receiver_slot: Arc<Mutex<Option<rtrb::Consumer<R::Update>>>>,

    /// Handle for joining the stream owner thread
    stream_thread_handle: Mutex<Option<thread::JoinHandle<()>>>,

    /// Update thread handle for joining on drop
    update_thread_handle: Mutex<Option<thread::JoinHandle<()>>>,

    /// Thread handle for unparking the update thread
    update_thread: Mutex<Option<thread::Thread>>,

    /// Update buffer capacity
    update_buffer_capacity: usize,

    /// Current stream config (only valid when running)
    config: Mutex<Option<StreamConfig>>,
}

/// Messages sent to the stream owner thread
enum StreamThreadMessage<C> {
    Command(C),
    Stop,
}

type SubscriberFn<T> = Box<dyn FnMut(&T) + Send>;

struct SubscriptionMap<T> {
    map: HashMap<u64, SubscriberFn<T>>,
    next_key: u64,
}

impl<T> SubscriptionMap<T> {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next_key: 0,
        }
    }

    fn insert(&mut self, callback: SubscriberFn<T>) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.map.insert(key, callback);
        key
    }

    fn remove(&mut self, key: u64) -> bool {
        self.map.remove(&key).is_some()
    }
}

impl<R> AudioLoopHandle<R>
where
    R: Runner + 'static,
{
    /// Create a new inactive audio loop handle.
    ///
    /// The handle starts in a stopped state. Call `start()` to begin audio processing.
    ///
    /// # Parameters
    /// - `update_buffer_capacity`: Maximum number of updates that can be buffered
    pub fn new(update_buffer_capacity: usize) -> Self {
        let subscribers = Arc::new(Mutex::new(SubscriptionMap::<R::Update>::new()));
        let stream_active = Arc::new(AtomicBool::new(false));
        let thread_alive = Arc::new(AtomicBool::new(true));
        let update_receiver_slot: Arc<Mutex<Option<rtrb::Consumer<R::Update>>>> =
            Arc::new(Mutex::new(None));

        // Spawn the persistent update thread
        let (update_thread_handle, update_thread) = {
            let subscribers = Arc::clone(&subscribers);
            let thread_alive = Arc::clone(&thread_alive);
            let update_receiver_slot = Arc::clone(&update_receiver_slot);

            // Channel to pass the thread handle to itself
            let (tx, rx) = std::sync::mpsc::sync_channel::<thread::Thread>(1);

            let handle = thread::spawn({
                let mut updates = Vec::with_capacity(update_buffer_capacity);
                move || {
                    // Get our own thread handle
                    let _self_handle = rx.recv().unwrap();

                    loop {
                        // Check if we should exit
                        if !thread_alive.load(Ordering::SeqCst) {
                            break;
                        }

                        // Try to read updates if we have an active receiver
                        {
                            let mut slot = update_receiver_slot.lock().unwrap();
                            if let Some(ref mut receiver) = *slot {
                                // Read all available updates
                                if let Ok(chunk) = receiver.read_chunk(receiver.slots()) {
                                    for update in chunk {
                                        updates.push(update);
                                    }
                                }
                            }
                        }

                        // Dispatch updates to subscribers
                        if !updates.is_empty() {
                            let mut subs = subscribers.lock().unwrap();
                            for update in updates.drain(..) {
                                for subscriber in subs.map.values_mut() {
                                    subscriber(&update);
                                }
                            }
                        }

                        // Park until woken by audio callback, start/stop, or shutdown
                        if thread_alive.load(Ordering::SeqCst) {
                            thread::park();
                        }
                    }
                }
            });

            let thread_handle = handle.thread().clone();
            tx.send(thread_handle.clone()).unwrap();
            (handle, thread_handle)
        };

        Self {
            inner: Arc::new(AudioLoopHandleInner {
                subscribers,
                stream_active,
                thread_alive,
                stream_command_sender: Mutex::new(None),
                update_receiver_slot,
                stream_thread_handle: Mutex::new(None),
                update_thread_handle: Mutex::new(Some(update_thread_handle)),
                update_thread: Mutex::new(Some(update_thread)),
                update_buffer_capacity,
                config: Mutex::new(None),
            }),
        }
    }

    /// Start the audio loop with the given device, configuration, and runner.
    ///
    /// # Parameters
    /// - `device`: The audio device to use
    /// - `config`: The stream configuration
    /// - `runner`: The runner that will process audio and handle commands
    ///
    /// # Errors
    /// Returns an error if already running or if stream creation fails.
    pub fn start(
        &self,
        device: AudioDevice,
        config: StreamConfig,
        runner: R,
    ) -> Result<(), AudioLoopStartError> {
        // Check if already running
        if self.inner.stream_active.load(Ordering::SeqCst) {
            return Err(AudioLoopStartError::AlreadyRunning);
        }

        // Create the channel for stream thread communication
        let (stream_tx, stream_rx) = channel::<StreamThreadMessage<R::Command>>();

        // Create update ring buffer
        let (update_sender, update_receiver) = RingBuffer::new(self.inner.update_buffer_capacity);

        // Pass the update receiver to the update thread
        {
            let mut slot = self.inner.update_receiver_slot.lock().unwrap();
            *slot = Some(update_receiver);
        }

        // Prepare data for stream thread
        let update_thread = self.inner.update_thread.lock().unwrap().clone();
        let channels = config.channels as usize;
        let cpal_config: cpal::StreamConfig = (&config).into();
        let stream_active = Arc::clone(&self.inner.stream_active);

        // Channel to receive result from stream thread
        let (result_tx, result_rx) = channel::<Result<(), AudioLoopStartError>>();

        // Spawn the stream owner thread
        let stream_thread = thread::spawn(move || {
            // Build the stream on this thread (where it will live)
            let mut runner = runner;
            let mut update_sender = update_sender;

            // Create command channel for audio callback
            let (audio_cmd_tx, audio_cmd_rx) = channel();

            let stream_result = device.inner().build_output_stream(
                &cpal_config,
                move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    // Handle commands from audio command channel
                    while let Ok(command) = audio_cmd_rx.try_recv() {
                        runner.handle_command(command, |update| {
                            let _ = update_sender.push(update);
                        });
                    }

                    let frames = data.len() / channels;
                    runner.run(data, Layout::Interleaved, frames, |update| {
                        let _ = update_sender.push(update);
                    });

                    // Wake the update thread to process any new updates
                    if let Some(ref thread) = update_thread {
                        thread.unpark();
                    }
                },
                |err| {
                    eprintln!("Audio stream error: {}", err);
                },
                None,
            );

            let stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    let _ = result_tx.send(Err(AudioLoopStartError::BuildStream(e.to_string())));
                    return;
                }
            };

            // Start playing
            if let Err(e) = stream.play() {
                let _ = result_tx.send(Err(AudioLoopStartError::PlayStream(e.to_string())));
                return;
            }

            // Signal success
            stream_active.store(true, Ordering::SeqCst);
            let _ = result_tx.send(Ok(()));

            // Keep the stream alive and forward commands
            // The stream lives in this thread's scope
            while let Ok(StreamThreadMessage::Command(cmd)) = stream_rx.recv() {
                // Forward command to audio callback
                let _ = audio_cmd_tx.send(cmd);
            }

            // Stream is dropped here when thread exits
        });

        // Wait for the result
        let result = result_rx
            .recv()
            .map_err(|_| AudioLoopStartError::BuildStream("Stream thread died".to_string()))?;

        if let Err(e) = result {
            // Clean up on failure
            let _ = stream_thread.join();
            return Err(e);
        }

        // Store configuration and handles
        *self.inner.config.lock().unwrap() = Some(config);
        *self.inner.stream_command_sender.lock().unwrap() = Some(stream_tx);
        *self.inner.stream_thread_handle.lock().unwrap() = Some(stream_thread);

        Ok(())
    }

    /// Stop the audio loop.
    ///
    /// Subscribers are preserved and will continue receiving updates
    /// when the loop is started again.
    ///
    /// # Returns
    /// `true` if the loop was running and is now stopped, `false` if already stopped.
    pub fn stop(&self) -> bool {
        if !self.inner.stream_active.load(Ordering::SeqCst) {
            return false;
        }

        // Clear stream active flag
        self.inner.stream_active.store(false, Ordering::SeqCst);

        // Send stop message to stream thread
        if let Some(sender) = self.inner.stream_command_sender.lock().unwrap().take() {
            let _ = sender.send(StreamThreadMessage::Stop);
        }

        // Wait for stream thread to finish
        if let Some(handle) = self.inner.stream_thread_handle.lock().unwrap().take() {
            let _ = handle.join();
        }

        // Clear the update receiver slot
        {
            let mut slot = self.inner.update_receiver_slot.lock().unwrap();
            *slot = None;
        }

        // Clear config
        *self.inner.config.lock().unwrap() = None;

        // Wake the update thread so it can process any remaining updates
        if let Some(ref thread) = *self.inner.update_thread.lock().unwrap() {
            thread.unpark();
        }

        true
    }

    /// Check if the audio loop is currently running.
    pub fn is_running(&self) -> bool {
        self.inner.stream_active.load(Ordering::SeqCst)
    }

    /// Get the sample rate if the loop is running.
    pub fn sample_rate(&self) -> Option<u32> {
        self.inner
            .config
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.sample_rate)
    }

    /// Get the channel count if the loop is running.
    pub fn channel_count(&self) -> Option<usize> {
        self.inner
            .config
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.channels as usize)
    }

    /// Send a command to the runner in the audio loop.
    ///
    /// # Errors
    /// Returns an error if the loop is not running or if the send fails.
    pub fn send_command(&self, command: R::Command) -> Result<(), CommandError<R::Command>> {
        let sender = self.inner.stream_command_sender.lock().unwrap();
        match &*sender {
            Some(tx) => tx
                .send(StreamThreadMessage::Command(command))
                .map_err(|e| match e.0 {
                    StreamThreadMessage::Command(c) => CommandError::SendFailed(c),
                    StreamThreadMessage::Stop => unreachable!(),
                }),
            None => Err(CommandError::NotRunning(command)),
        }
    }

    /// Register a listener for updates coming from the audio loop.
    ///
    /// Subscriptions persist across start/stop cycles.
    ///
    /// # Warning
    /// The callback must not call `subscribe` or `unsubscribe` - doing so will deadlock.
    pub fn subscribe<F>(&self, callback: F) -> Subscription
    where
        F: FnMut(&R::Update) + Send + 'static,
    {
        let id = self
            .inner
            .subscribers
            .lock()
            .unwrap()
            .insert(Box::new(callback));
        Subscription(id)
    }

    /// Unregister a listener for updates coming from the audio loop.
    ///
    /// # Warning
    /// Must not be called from within a subscriber callback - doing so will deadlock.
    pub fn unsubscribe(&self, subscription: Subscription) -> bool {
        self.inner
            .subscribers
            .lock()
            .unwrap()
            .remove(subscription.0)
    }
}

impl<R> Clone for AudioLoopHandle<R>
where
    R: Runner,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<R> Drop for AudioLoopHandleInner<R>
where
    R: Runner,
{
    fn drop(&mut self) {
        // Stop the stream if running
        self.stream_active.store(false, Ordering::SeqCst);

        // Send stop to stream thread
        if let Some(sender) = self.stream_command_sender.lock().unwrap().take() {
            let _ = sender.send(StreamThreadMessage::Stop);
        }

        // Wait for stream thread
        if let Some(handle) = self.stream_thread_handle.lock().unwrap().take() {
            let _ = handle.join();
        }

        // Signal the update thread to exit
        self.thread_alive.store(false, Ordering::SeqCst);

        // Wake the update thread so it can exit
        if let Some(thread) = self.update_thread.lock().unwrap().take() {
            thread.unpark();
        }

        // Join the update thread
        if let Some(handle) = self.update_thread_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

/// A runner that processes audio and handles commands and updates
///
/// This trait is used to define the behavior of the audio loop.
/// It is responsible for processing audio, handling commands and sending
/// out updates in case the audio loop needs to communicate with the outside world.
pub trait Runner: Send {
    /// The type of commands that can be sent to the runner
    type Command: Send + 'static;

    /// The type of updates that can be sent out of the runner
    type Update: Send + 'static;

    /// Handle a command that was sent to the runner
    fn handle_command(&mut self, command: Self::Command, on_update: impl FnMut(Self::Update));

    /// Process audio and send out updates if need be
    fn run(
        &mut self,
        output: &mut [f32],
        layout: Layout,
        len: usize,
        on_update: impl FnMut(Self::Update),
    );
}

/// An error that can occur when starting the audio loop
#[derive(Debug, Error)]
pub enum AudioLoopStartError {
    #[error("Audio loop is already running")]
    AlreadyRunning,

    #[error("Failed to build audio stream: {0}")]
    BuildStream(String),

    #[error("Failed to start audio stream: {0}")]
    PlayStream(String),
}

/// An error that can occur when sending a command
#[derive(Debug, Error)]
pub enum CommandError<C> {
    #[error("Audio loop is not running")]
    NotRunning(C),

    #[error("Failed to send command")]
    SendFailed(C),
}

pub struct Subscription(u64);
