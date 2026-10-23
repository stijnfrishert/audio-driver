use crate::backend::Layout;
use crate::device::{AudioDevice, StreamConfig};
use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::RingBuffer;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
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

    /// The running stream and its command queue (if running)
    stream: Mutex<Option<RunningStream<R::Command>>>,

    /// Slot for passing update receiver to the update thread
    update_receiver_slot: Arc<Mutex<Option<rtrb::Consumer<R::Update>>>>,

    /// Update thread handle for joining on drop
    update_thread_handle: Mutex<Option<thread::JoinHandle<()>>>,

    /// Thread handle for unparking the update thread
    update_thread: Mutex<Option<thread::Thread>>,

    /// Update buffer capacity
    update_buffer_capacity: usize,

    /// Command buffer capacity
    command_buffer_capacity: usize,

    /// Current stream config (only valid when running)
    config: Mutex<Option<StreamConfig>>,
}

/// A running stream, owned by the handle
struct RunningStream<C> {
    /// Dropping this stops the audio callback
    _stream: cpal::Stream,

    /// Producer of the command queue drained by the audio callback
    commands: rtrb::Producer<C>,
}

type SubscriberFn<T> = Box<dyn FnMut(&[T]) + Send>;

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
    /// - `command_buffer_capacity`: Maximum number of commands that can be buffered
    pub fn new(update_buffer_capacity: usize, command_buffer_capacity: usize) -> Self {
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

                        // Dispatch the whole drained batch to each subscriber in
                        // a single call, so consumers can amortize per-update work.
                        if !updates.is_empty() {
                            {
                                let mut subs = subscribers.lock().unwrap();
                                for subscriber in subs.map.values_mut() {
                                    subscriber(&updates);
                                }
                            }

                            updates.clear();
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
                stream: Mutex::new(None),
                update_receiver_slot,
                update_thread_handle: Mutex::new(Some(update_thread_handle)),
                update_thread: Mutex::new(Some(update_thread)),
                update_buffer_capacity,
                command_buffer_capacity,
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
        mut runner: R,
    ) -> Result<(), AudioLoopStartError> {
        // Check if already running
        if self.inner.stream_active.load(Ordering::SeqCst) {
            return Err(AudioLoopStartError::AlreadyRunning);
        }

        // Create command ring buffer
        let (command_sender, mut command_receiver) =
            RingBuffer::new(self.inner.command_buffer_capacity);

        // Create update ring buffer
        let (mut update_sender, update_receiver) =
            RingBuffer::new(self.inner.update_buffer_capacity);

        // Pass the update receiver to the update thread
        {
            let mut slot = self.inner.update_receiver_slot.lock().unwrap();
            *slot = Some(update_receiver);
        }

        // Prepare data for the audio callback
        let update_thread = self.inner.update_thread.lock().unwrap().clone();
        let channels = config.channels as usize;
        let cpal_config: cpal::StreamConfig = (&config).into();
        let mut clock = FrameClock::new();

        let stream = device
            .inner()
            .build_output_stream(
                cpal_config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    // Handle commands queued since the last callback
                    while let Ok(command) = command_receiver.pop() {
                        runner.handle_command(command, |update| {
                            let _ = update_sender.push(update);
                        });
                    }

                    debug_assert_eq!(data.len() % channels, 0, "partial frame in buffer");
                    let frames = data.len() / channels;
                    let timing = clock.advance(frames, info.timestamp());

                    runner.run(data, Layout::Interleaved, frames, timing, |update| {
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
            )
            .map_err(|e| AudioLoopStartError::BuildStream(e.to_string()))?;

        // Start playing
        stream
            .play()
            .map_err(|e| AudioLoopStartError::PlayStream(e.to_string()))?;

        // Store configuration and stream
        *self.inner.config.lock().unwrap() = Some(config);
        *self.inner.stream.lock().unwrap() = Some(RunningStream {
            _stream: stream,
            commands: command_sender,
        });
        self.inner.stream_active.store(true, Ordering::SeqCst);

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

        // Drop the stream outside the lock, which stops the audio callback
        let stream = self.inner.stream.lock().unwrap().take();
        drop(stream);

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
    /// Never blocks: the command is queued for the next audio callback, and handed back in the
    /// error if it can't be.
    ///
    /// # Errors
    /// Returns an error if the loop is not running, if the command queue is full, or if the
    /// stream is gone.
    pub fn send_command(&self, command: R::Command) -> Result<(), CommandError<R::Command>> {
        match &mut *self.inner.stream.lock().unwrap() {
            Some(stream) => push_command(&mut stream.commands, command),
            None => Err(CommandError::NotRunning(command)),
        }
    }

    /// Register a listener for updates coming from the audio loop.
    ///
    /// The callback receives each wake-up's drained updates as a single batch,
    /// in arrival order. Subscriptions persist across start/stop cycles.
    ///
    /// # Warning
    /// The callback must not call `subscribe` or `unsubscribe` - doing so will deadlock.
    pub fn subscribe<F>(&self, callback: F) -> Subscription
    where
        F: FnMut(&[R::Update]) + Send + 'static,
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

/// Queue a command for the audio callback, handing it back if it can't be delivered.
fn push_command<C>(commands: &mut rtrb::Producer<C>, command: C) -> Result<(), CommandError<C>> {
    if commands.is_abandoned() {
        return Err(CommandError::SendFailed(command));
    }

    commands
        .push(command)
        .map_err(|rtrb::PushError::Full(command)| CommandError::QueueFull(command))
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

        // Drop the stream outside the lock, which stops the audio callback
        let stream = self.stream.lock().unwrap().take();
        drop(stream);

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

/// A monotonic instant on the audio stream's own clock.
///
/// Not an absolute epoch — on some platforms it begins at zero when the stream is created — so it is
/// meaningful as a difference from another `StreamInstant`, not as a date. Each stream carries its
/// own origin: instants from two streams, or from either side of a restart, don't share a clock and
/// can't be compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamInstant {
    inner: cpal::StreamInstant,
}

impl StreamInstant {
    /// An instant `secs` seconds and `nanos` nanoseconds past the stream clock's origin.
    ///
    /// The origin is arbitrary, so this is mainly of use for building [`Timing`] values in tests.
    pub fn new(secs: u64, nanos: u32) -> Self {
        cpal::StreamInstant::new(secs, nanos).into()
    }

    /// The amount of time elapsed from an earlier instant to this one.
    ///
    /// Saturates to [`Duration::ZERO`] if `earlier` is actually later than `self`; use
    /// [`Self::checked_duration_since`] to tell that case apart from a genuine zero.
    pub fn duration_since(self, earlier: StreamInstant) -> Duration {
        self.checked_duration_since(earlier).unwrap_or_default()
    }

    /// The amount of time elapsed from an earlier instant to this one.
    ///
    /// Returns `None` if `earlier` is actually later than `self`.
    pub fn checked_duration_since(self, earlier: StreamInstant) -> Option<Duration> {
        self.inner.checked_duration_since(earlier.inner)
    }

    /// The instant `duration` after this one.
    ///
    /// Returns `None` if the result falls outside the range the clock can represent.
    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        self.inner.checked_add(duration).map(Self::from)
    }

    /// The instant `duration` before this one.
    ///
    /// Returns `None` if the result falls outside the range the clock can represent.
    pub fn checked_sub(self, duration: Duration) -> Option<Self> {
        self.inner.checked_sub(duration).map(Self::from)
    }
}

impl From<cpal::StreamInstant> for StreamInstant {
    fn from(inner: cpal::StreamInstant) -> Self {
        Self { inner }
    }
}

/// Where a callback's buffer sits in time.
///
/// The frame index is the stream's own timeline: it starts at zero when the stream starts and counts
/// every frame handed to the runner, so a caller that needs to place events at absolute positions can
/// do so without keeping its own count. **It resets on restart** — `stop()` followed by `start()` is a
/// new timeline, not a continuation of the old one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timing {
    /// Index of the first frame of this buffer, counted from the start of the stream.
    ///
    /// This counts frames the runner was handed, not frames the device consumed. The two agree until
    /// the stream drops a callback, after which the frame index trails the audio clock by the dropped
    /// frames for the rest of the stream. Use it to place events against one another, not to convert
    /// between frames and instants.
    pub frame: u64,

    /// The instant this callback was invoked.
    pub callback: StreamInstant,

    /// The predicted instant this buffer reaches the device.
    ///
    /// This, not [`Self::callback`], is the one to timestamp a sound against. But backends derive it
    /// from the latency the device reports rather than measure it, so it is a stable reference for
    /// *when* a buffer sounds relative to another, not an absolute measure of how late it sounds —
    /// that needs a round-trip calibration.
    ///
    /// It never decreases, but isn't strictly increasing: when latency drops mid-stream (e.g. the
    /// default device reroutes), it holds at its previous value until the clock catches up, so
    /// consecutive buffers can share the same instant.
    pub playback: StreamInstant,
}

/// The stream's frame timeline, advanced by every buffer the runner sees.
struct FrameClock {
    frame: u64,
}

impl FrameClock {
    fn new() -> Self {
        Self { frame: 0 }
    }

    /// The timing of a buffer of `frames`, advancing the clock past it.
    fn advance(&mut self, frames: usize, timestamp: cpal::OutputStreamTimestamp) -> Timing {
        let timing = Timing {
            frame: self.frame,
            callback: timestamp.callback.into(),
            playback: timestamp.playback.into(),
        };

        self.frame += frames as u64;

        timing
    }
}

/// A runner that processes audio and handles commands and updates
///
/// This trait is used to define the behavior of the audio loop.
/// It is responsible for processing audio, handling commands and sending
/// out updates in case the audio loop needs to communicate with the outside world.
///
/// [`Self::handle_command`] and [`Self::run`] are called from the audio callback, which runs at
/// real-time priority where the platform allows it, so neither may block, take locks, allocate or
/// free; any of these can make a buffer late and cause an audible dropout.
pub trait Runner: Send {
    /// The type of commands that can be sent to the runner
    type Command: Send + 'static;

    /// The type of updates that can be sent out of the runner
    type Update: Send + 'static;

    /// Handle a command that was sent to the runner.
    ///
    /// Commands are drained just before the buffer that follows them is rendered, and carry no
    /// timing of their own; a command that has to sit somewhere on the stream's timeline should be
    /// stored here and applied in [`Self::run`], where [`Timing`] is available.
    fn handle_command(&mut self, command: Self::Command, on_update: impl FnMut(Self::Update));

    /// Process audio and send out updates if need be.
    ///
    /// `timing` gives this buffer's position on the stream's timeline; see [`Timing`].
    fn run(
        &mut self,
        output: &mut [f32],
        layout: Layout,
        len: usize,
        timing: Timing,
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

    #[error("Command queue is full")]
    QueueFull(C),
}

pub struct Subscription(u64);

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a cpal timestamp for driving [`FrameClock`] directly.
    fn timestamp(callback: StreamInstant, playback: StreamInstant) -> cpal::OutputStreamTimestamp {
        cpal::OutputStreamTimestamp {
            callback: callback.inner,
            playback: playback.inner,
        }
    }

    #[test]
    fn handle_is_send_and_sync() {
        struct Silence;

        impl Runner for Silence {
            type Command = ();
            type Update = ();

            fn handle_command(&mut self, _: (), _: impl FnMut(())) {}

            fn run(&mut self, _: &mut [f32], _: Layout, _: usize, _: Timing, _: impl FnMut(())) {}
        }

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AudioLoopHandle<Silence>>();
    }

    #[test]
    fn push_command_queues_until_full() {
        let (mut commands, mut receiver) = RingBuffer::new(1);

        assert!(push_command(&mut commands, 1).is_ok());
        assert!(matches!(
            push_command(&mut commands, 2),
            Err(CommandError::QueueFull(2))
        ));

        assert_eq!(receiver.pop(), Ok(1));
        assert!(push_command(&mut commands, 3).is_ok());
    }

    #[test]
    fn push_command_fails_once_the_stream_is_gone() {
        let (mut commands, receiver) = RingBuffer::new(1);
        drop(receiver);

        assert!(matches!(
            push_command(&mut commands, 1),
            Err(CommandError::SendFailed(1))
        ));
    }

    #[test]
    fn duration_between_instants() {
        let start = StreamInstant::new(1, 0);
        let later = StreamInstant::new(1, 500_000_000);

        assert_eq!(later.duration_since(start), Duration::from_millis(500));
        assert_eq!(start.duration_since(later), Duration::ZERO);
    }

    #[test]
    fn checked_duration_between_instants() {
        let start = StreamInstant::new(1, 0);
        let later = StreamInstant::new(1, 500_000_000);

        assert_eq!(
            later.checked_duration_since(start),
            Some(Duration::from_millis(500))
        );
        assert_eq!(start.checked_duration_since(later), None);
    }

    #[test]
    fn offset_an_instant() {
        let instant = StreamInstant::new(1, 0);
        let buffer = Duration::from_millis(10);

        assert_eq!(
            instant.checked_add(buffer),
            Some(StreamInstant::new(1, 10_000_000))
        );
        assert_eq!(
            instant.checked_sub(buffer),
            Some(StreamInstant::new(0, 990_000_000))
        );
    }

    #[test]
    fn instants_order_by_time() {
        assert!(StreamInstant::new(1, 0) < StreamInstant::new(1, 1));
        assert!(StreamInstant::new(2, 0) > StreamInstant::new(1, 999_999_999));
    }

    #[test]
    fn frame_clock_starts_at_zero_and_advances_by_buffer() {
        let ts = timestamp(StreamInstant::new(1, 0), StreamInstant::new(1, 10_000_000));
        let mut clock = FrameClock::new();

        assert_eq!(clock.advance(512, ts).frame, 0);
        assert_eq!(clock.advance(512, ts).frame, 512);
        assert_eq!(clock.advance(512, ts).frame, 1024);
    }

    #[test]
    fn frame_clock_handles_varying_buffer_sizes() {
        let ts = timestamp(StreamInstant::new(1, 0), StreamInstant::new(1, 10_000_000));
        let mut clock = FrameClock::new();

        clock.advance(512, ts);
        assert_eq!(clock.advance(256, ts).frame, 512);
        assert_eq!(clock.advance(1024, ts).frame, 768);
        assert_eq!(clock.advance(0, ts).frame, 1792);
        assert_eq!(clock.advance(64, ts).frame, 1792);
    }

    #[test]
    fn frame_clock_passes_timestamps_through() {
        let callback = StreamInstant::new(1, 0);
        let playback = StreamInstant::new(1, 10_000_000);
        let mut clock = FrameClock::new();

        let timing = clock.advance(512, timestamp(callback, playback));

        assert_eq!(timing.callback, callback);
        assert_eq!(timing.playback, playback);
        assert_eq!(
            timing.playback.duration_since(timing.callback),
            Duration::from_millis(10)
        );
    }
}
