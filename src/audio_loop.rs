use crate::backend::Layout;
use crate::device::{AudioDevice, StreamConfig};
use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::RingBuffer;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{SendError, Sender, channel},
    },
    thread,
};
use thiserror::Error;

/// A running audio loop, plus the ability to send commands and receive updates
pub struct AudioLoop<R>
where
    R: Runner,
{
    /// The CPAL stream that drives the audio callback
    #[allow(dead_code)]
    stream: cpal::Stream,

    /// The stream configuration
    config: StreamConfig,

    /// The sender for incoming commands.
    /// This allows the audio processing to run on a separate thread
    command_sender: Sender<R::Command>,

    /// A handle to the thread receiving updates coming from the audio thread
    /// This is an option, because we will take it when dropping the audio loop
    update_join_handle: Option<thread::JoinHandle<()>>,

    /// Handle to unpark the update thread
    update_thread: thread::Thread,

    /// A flag to signal whether we're running or not
    running: Arc<AtomicBool>,

    /// The subscribers for updates coming from the audio loop
    subscribers: Arc<Mutex<SubscriptionMap<R::Update>>>,
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

    pub fn insert(&mut self, callback: SubscriberFn<T>) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.map.insert(key, callback);
        key
    }

    pub fn remove(&mut self, key: u64) -> bool {
        self.map.remove(&key).is_some()
    }
}

impl<R> AudioLoop<R>
where
    R: Runner + 'static,
{
    /// Create a new audio loop and start running it
    ///
    /// # Parameters
    /// - `device`: The audio device to use
    /// - `config`: The stream configuration
    /// - `update_count`: The number of updates that can be buffered
    /// - `runner`: The runner that will process the audio loop and handle commands
    pub fn new(
        device: AudioDevice,
        config: StreamConfig,
        update_count: usize,
        mut runner: R,
    ) -> Result<Self, AudioLoopNewError> {
        // Create a send/receive pair for the incoming commands
        let (command_sender, command_receiver) = channel();

        // Create send/receive pairs for outgoing updates
        let (mut update_sender, mut update_receiver) = RingBuffer::new(update_count);

        let running = Arc::new(AtomicBool::new(true));

        // Start a thread to receive updates (must be created before starting the backend
        // so we can pass its handle to the audio callback for unparking)
        let subscribers = Arc::new(Mutex::new(SubscriptionMap::<R::Update>::new()));
        let update_join_handle_running = Arc::clone(&running);
        let update_join_handle_subscribers = Arc::clone(&subscribers);
        let (update_join_handle, update_thread) = {
            // Channel to pass the thread handle to itself
            let (tx, rx) = std::sync::mpsc::sync_channel::<thread::Thread>(1);

            let handle = thread::spawn({
                let mut updates = Vec::with_capacity(update_count);
                move || {
                    // Get our own thread handle
                    let _self_handle = rx.recv().unwrap();

                    loop {
                        // Read all available updates
                        for update in update_receiver.read_chunk(update_receiver.slots()).unwrap() {
                            updates.push(update);
                        }

                        // Dispatch updates to subscribers
                        for update in updates.drain(..) {
                            for subscriber in update_join_handle_subscribers
                                .lock()
                                .unwrap()
                                .map
                                .values_mut()
                            {
                                subscriber(&update);
                            }
                        }

                        if update_join_handle_running.load(Ordering::SeqCst) {
                            // Park until woken by the audio callback or shutdown
                            thread::park();
                        } else {
                            break;
                        }
                    }
                }
            });

            let thread_handle = handle.thread().clone();
            tx.send(thread_handle.clone()).unwrap();
            (handle, thread_handle)
        };

        // Build the CPAL stream
        let callback_thread_handle = update_thread.clone();
        let channels = config.channels as usize;
        let cpal_config: cpal::StreamConfig = (&config).into();

        let stream = device
            .inner()
            .build_output_stream(
                &cpal_config,
                move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    // Handle the messages
                    while let Ok(command) = command_receiver.try_recv() {
                        runner.handle_command(command, |update| {
                            let _ = update_sender.push(update);
                        });
                    }

                    let frames = data.len() / channels;
                    runner.run(data, Layout::Interleaved, frames, |update| {
                        let _ = update_sender.push(update);
                    });

                    // Wake the update thread to process any new updates
                    callback_thread_handle.unpark();
                },
                |err| {
                    eprintln!("Audio stream error: {}", err);
                },
                None,
            )
            .map_err(|e| AudioLoopNewError::BuildStream(e.to_string()))?;

        // Start playing
        stream
            .play()
            .map_err(|e| AudioLoopNewError::PlayStream(e.to_string()))?;

        Ok(Self {
            stream,
            config,
            command_sender,
            update_join_handle: Some(update_join_handle),
            update_thread,
            running,
            subscribers,
        })
    }

    /// Retrieve the sample rate used by the audio loop
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    /// Retrieve the number of channels used by the audio loop
    pub fn channel_count(&self) -> usize {
        self.config.channels as usize
    }

    /// Send a command to the runner in the audio loop
    pub fn send_command(&self, command: R::Command) -> Result<(), SendError<R::Command>> {
        self.command_sender.send(command)
    }

    /// Register a listener for updates coming from the audio loop.
    ///
    /// # Warning
    /// The callback must not call `subscribe` or `unsubscribe` - doing so will deadlock.
    pub fn subscribe<F>(&self, callback: F) -> Subscription
    where
        F: FnMut(&R::Update) + Send + 'static,
    {
        let id = self.subscribers.lock().unwrap().insert(Box::new(callback));
        Subscription(id)
    }

    /// Unregister a listener for updates coming from the audio loop.
    ///
    /// # Warning
    /// Must not be called from within a subscriber callback - doing so will deadlock.
    pub fn unsubscribe(&self, subscription: Subscription) -> bool {
        self.subscribers.lock().unwrap().remove(subscription.0)
    }
}

impl<R> Drop for AudioLoop<R>
where
    R: Runner,
{
    fn drop(&mut self) {
        // Signal shutdown and wake the update thread so it can exit
        self.running.store(false, Ordering::SeqCst);
        self.update_thread.unpark();

        // Join the thread (ignore errors if thread panicked)
        let _ = self.update_join_handle.take().map(|t| t.join());

        // Stream is dropped automatically, which stops playback
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

/// An error that can occur when creating a new audio loop
#[derive(Debug, Error)]
pub enum AudioLoopNewError {
    #[error("Failed to build audio stream: {0}")]
    BuildStream(String),

    #[error("Failed to start audio stream: {0}")]
    PlayStream(String),
}

pub struct Subscription(u64);
