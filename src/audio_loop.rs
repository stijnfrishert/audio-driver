use crate::backend::Layout;
use crate::device::{AudioDevice, StreamConfig};
use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::RingBuffer;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use thiserror::Error;

pub use cpal::{Error as StreamError, ErrorKind as StreamErrorKind};

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
    /// State shared with the update thread
    shared: Arc<Shared<R>>,

    /// Flag to signal the update thread to exit (on Drop)
    thread_alive: Arc<AtomicBool>,

    /// Update thread handle for joining on drop
    update_thread_handle: Mutex<Option<thread::JoinHandle<()>>>,

    /// Thread handle for unparking the update thread
    update_thread: Mutex<Option<thread::Thread>>,

    /// Update buffer capacity
    update_buffer_capacity: usize,

    /// Command buffer capacity
    command_buffer_capacity: usize,

    /// Error buffer capacity
    error_buffer_capacity: usize,
}

/// State shared between the handle and the update thread
struct Shared<R>
where
    R: Runner,
{
    /// The generation for the next stream; held for the whole of `start` and `stop` (and the
    /// update thread's fatal-error stop), so each runs as a single transition
    lifecycle: Mutex<u64>,

    /// The running stream, its config and its command queue (if running)
    stream: Mutex<Option<RunningStream<R::Command>>>,

    /// Update receivers of current and stopped streams, oldest first; each is dropped once its
    /// stream is gone and it has been drained
    update_receivers: Mutex<VecDeque<rtrb::Consumer<R::Update>>>,

    /// Error receivers of current and stopped streams, kept the same way
    error_receivers: Mutex<VecDeque<ErrorReceiver>>,

    /// The subscribers for updates - persists across start/stop cycles
    subscribers: Mutex<SubscriptionMap<UpdateSubscriberFn<R::Update>>>,

    /// The subscribers for stream errors - persists across start/stop cycles
    error_subscribers: Mutex<SubscriptionMap<ErrorSubscriberFn>>,
}

/// A running stream, owned by the handle
struct RunningStream<C> {
    /// Dropping this stops the audio callback
    _stream: cpal::Stream,

    /// Tells this stream apart from earlier and later ones
    generation: u64,

    /// The config the stream was built with
    config: StreamConfig,

    /// Producer of the command queue drained by the audio callback
    commands: rtrb::Producer<C>,
}

/// The update thread's side of a stream's error queue
struct ErrorReceiver {
    /// Consumer of the errors reported by the stream
    errors: rtrb::Consumer<StreamError>,

    /// Errors dropped because the queue was full, since the last batch
    dropped: Arc<AtomicUsize>,

    /// The generation of the stream reporting into this queue
    generation: u64,
}

/// Errors reported by the stream since the last batch.
#[derive(Debug)]
pub struct ErrorBatch<'a> {
    /// The errors, in the order they were reported
    pub errors: &'a [StreamError],

    /// Errors lost because the error queue was full, reported before the ones in `errors`
    pub dropped: usize,

    /// Whether the loop was stopped because of an error in this batch.
    ///
    /// The loop stops itself on [`StreamErrorKind::DeviceNotAvailable`] and
    /// [`StreamErrorKind::StreamInvalidated`], after which the stream can't continue. By the
    /// time this batch is delivered, [`AudioLoopHandle::is_running`] already returns `false`.
    pub stopped: bool,
}

type UpdateSubscriberFn<T> = dyn FnMut(&[T]) + Send;
type ErrorSubscriberFn = dyn FnMut(&ErrorBatch<'_>) + Send;

struct SubscriptionMap<F: ?Sized> {
    map: HashMap<u64, Box<F>>,
    next_key: u64,
}

impl<F: ?Sized> SubscriptionMap<F> {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next_key: 0,
        }
    }

    fn insert(&mut self, callback: Box<F>) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.map.insert(key, callback);
        key
    }

    fn remove(&mut self, key: u64) -> bool {
        self.map.remove(&key).is_some()
    }
}

impl<R> Shared<R>
where
    R: Runner,
{
    /// Take the running stream out, if any and if it has the given generation.
    ///
    /// The caller must hold `lifecycle`, and drops the stream after this returns, outside the
    /// `stream` lock.
    fn take_stream(&self, generation: Option<u64>) -> Option<RunningStream<R::Command>> {
        let mut stream = self.stream.lock().unwrap();
        match &*stream {
            Some(running) if generation.is_none_or(|g| g == running.generation) => stream.take(),
            _ => None,
        }
    }

    /// Drain every update and error queue, stop the stream if it reported a fatal error, and
    /// dispatch what was drained to the subscribers.
    ///
    /// Returns whether the stream was stopped, in which case its queues should be drained once
    /// more.
    fn pump(&self, updates: &mut Vec<R::Update>, errors: &mut Vec<StreamError>) -> bool {
        drain_receivers(&mut self.update_receivers.lock().unwrap(), |receiver| {
            if let Ok(chunk) = receiver.read_chunk(receiver.slots()) {
                updates.extend(chunk);
            }
            receiver.is_abandoned()
        });

        let mut dropped = 0;
        let mut fatal_generation = None;
        drain_receivers(&mut self.error_receivers.lock().unwrap(), |receiver| {
            let start = errors.len();
            if let Ok(chunk) = receiver.errors.read_chunk(receiver.errors.slots()) {
                errors.extend(chunk);
            }
            dropped += receiver.dropped.swap(0, Ordering::Relaxed);
            if errors[start..].iter().any(|e| stops_stream(e.kind())) {
                fatal_generation = Some(receiver.generation);
            }
            receiver.errors.is_abandoned()
        });

        // Stop off the audio thread; dropping a stream from inside its own callbacks can deadlock
        let stopped = fatal_generation.is_some_and(|generation| {
            let _lifecycle = self.lifecycle.lock().unwrap();
            let stream = self.take_stream(Some(generation));
            stream.is_some()
        });

        // Dispatch the whole drained batch to each subscriber in
        // a single call, so consumers can amortize per-update work.
        if !updates.is_empty() {
            for subscriber in self.subscribers.lock().unwrap().map.values_mut() {
                subscriber(updates);
            }
            updates.clear();
        }

        if !errors.is_empty() || dropped > 0 {
            let batch = ErrorBatch {
                errors,
                dropped,
                stopped,
            };
            for subscriber in self.error_subscribers.lock().unwrap().map.values_mut() {
                subscriber(&batch);
            }
            errors.clear();
        }

        stopped
    }
}

/// Drain each receiver in order, dropping the ones whose stream is gone.
///
/// `drain` empties a receiver and then returns whether its producer is gone.
fn drain_receivers<T>(receivers: &mut VecDeque<T>, mut drain: impl FnMut(&mut T) -> bool) {
    receivers.retain_mut(|receiver| {
        let abandoned = drain(receiver);
        if abandoned {
            // Catch anything pushed between the drain and the producer going away
            drain(receiver);
        }
        !abandoned
    });
}

/// Whether an error means the stream can't continue, so the loop stops itself.
fn stops_stream(kind: StreamErrorKind) -> bool {
    matches!(
        kind,
        StreamErrorKind::DeviceNotAvailable | StreamErrorKind::StreamInvalidated
    )
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
    /// - `error_buffer_capacity`: Maximum number of stream errors that can be buffered
    pub fn new(
        update_buffer_capacity: usize,
        command_buffer_capacity: usize,
        error_buffer_capacity: usize,
    ) -> Self {
        let shared = Arc::new(Shared {
            lifecycle: Mutex::new(0),
            stream: Mutex::new(None),
            update_receivers: Mutex::new(VecDeque::new()),
            error_receivers: Mutex::new(VecDeque::new()),
            subscribers: Mutex::new(SubscriptionMap::new()),
            error_subscribers: Mutex::new(SubscriptionMap::new()),
        });
        let thread_alive = Arc::new(AtomicBool::new(true));

        // Spawn the persistent update thread
        let (update_thread_handle, update_thread) = {
            let shared = Arc::clone(&shared);
            let thread_alive = Arc::clone(&thread_alive);

            // Channel to pass the thread handle to itself
            let (tx, rx) = std::sync::mpsc::sync_channel::<thread::Thread>(1);

            let handle = thread::spawn({
                let mut updates = Vec::with_capacity(update_buffer_capacity);
                let mut errors = Vec::with_capacity(error_buffer_capacity);
                move || {
                    // Get our own thread handle
                    let _self_handle = rx.recv().unwrap();

                    loop {
                        // Check if we should exit
                        if !thread_alive.load(Ordering::SeqCst) {
                            break;
                        }

                        // A fatal-error stop leaves the stopped stream's queues to drain
                        if shared.pump(&mut updates, &mut errors) {
                            continue;
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
                shared,
                thread_alive,
                update_thread_handle: Mutex::new(Some(update_thread_handle)),
                update_thread: Mutex::new(Some(update_thread)),
                update_buffer_capacity,
                command_buffer_capacity,
                error_buffer_capacity,
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
        let shared = &self.inner.shared;
        let mut next_generation = shared.lifecycle.lock().unwrap();

        // Check if already running
        if shared.stream.lock().unwrap().is_some() {
            return Err(AudioLoopStartError::AlreadyRunning);
        }

        let generation = *next_generation;
        *next_generation += 1;

        // Create command ring buffer
        let (command_sender, mut command_receiver) =
            RingBuffer::new(self.inner.command_buffer_capacity);

        // Create update ring buffer
        let (mut update_sender, update_receiver) =
            RingBuffer::new(self.inner.update_buffer_capacity);

        // Create error ring buffer
        let (mut error_sender, error_receiver) = RingBuffer::new(self.inner.error_buffer_capacity);
        let dropped_errors = Arc::new(AtomicUsize::new(0));

        // Prepare data for the audio and error callbacks
        let update_thread = self.inner.update_thread.lock().unwrap().clone();
        let error_update_thread = update_thread.clone();
        let error_dropped = Arc::clone(&dropped_errors);
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
                move |err| {
                    // May run on the audio thread, so this stays as lock-free as the data callback
                    report_error(&mut error_sender, &error_dropped, err);

                    if let Some(ref thread) = error_update_thread {
                        thread.unpark();
                    }
                },
                None,
            )
            .map_err(|e| AudioLoopStartError::BuildStream(e.to_string()))?;

        // Start playing
        stream
            .play()
            .map_err(|e| AudioLoopStartError::PlayStream(e.to_string()))?;

        // Pass the update and error receivers to the update thread
        shared
            .update_receivers
            .lock()
            .unwrap()
            .push_back(update_receiver);
        shared
            .error_receivers
            .lock()
            .unwrap()
            .push_back(ErrorReceiver {
                errors: error_receiver,
                dropped: dropped_errors,
                generation,
            });

        // Store the stream, which marks the loop as running
        *shared.stream.lock().unwrap() = Some(RunningStream {
            _stream: stream,
            generation,
            config,
            commands: command_sender,
        });

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
        let _lifecycle = self.inner.shared.lifecycle.lock().unwrap();

        // Drop the stream outside the lock, which stops the audio callback
        let Some(stream) = self.inner.shared.take_stream(None) else {
            return false;
        };
        drop(stream);

        // Wake the update thread so it can deliver what the stream left in its queues
        if let Some(ref thread) = *self.inner.update_thread.lock().unwrap() {
            thread.unpark();
        }

        true
    }

    /// Check if the audio loop is currently running.
    pub fn is_running(&self) -> bool {
        self.inner.shared.stream.lock().unwrap().is_some()
    }

    /// Get the sample rate if the loop is running.
    pub fn sample_rate(&self) -> Option<u32> {
        self.inner
            .shared
            .stream
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.config.sample_rate)
    }

    /// Get the channel count if the loop is running.
    pub fn channel_count(&self) -> Option<usize> {
        self.inner
            .shared
            .stream
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.config.channels as usize)
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
        match &mut *self.inner.shared.stream.lock().unwrap() {
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
            .shared
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
            .shared
            .subscribers
            .lock()
            .unwrap()
            .remove(subscription.0)
    }

    /// Register a listener for errors reported by the audio stream.
    ///
    /// The callback runs on the update thread and receives each wake-up's errors as a single
    /// batch, in the order they were reported. Subscriptions persist across start/stop cycles.
    ///
    /// # Warning
    /// The callback must not call `subscribe_errors` or `unsubscribe_errors` - doing so will
    /// deadlock.
    pub fn subscribe_errors<F>(&self, callback: F) -> ErrorSubscription
    where
        F: FnMut(&ErrorBatch<'_>) + Send + 'static,
    {
        let id = self
            .inner
            .shared
            .error_subscribers
            .lock()
            .unwrap()
            .insert(Box::new(callback));
        ErrorSubscription(id)
    }

    /// Unregister a listener for errors reported by the audio stream.
    ///
    /// # Warning
    /// Must not be called from within an error subscriber callback - doing so will deadlock.
    pub fn unsubscribe_errors(&self, subscription: ErrorSubscription) -> bool {
        self.inner
            .shared
            .error_subscribers
            .lock()
            .unwrap()
            .remove(subscription.0)
    }
}

/// Queue a stream error for the update thread, counting it as dropped if the queue is full.
fn report_error(
    errors: &mut rtrb::Producer<StreamError>,
    dropped: &AtomicUsize,
    error: StreamError,
) {
    if errors.push(error).is_err() {
        dropped.fetch_add(1, Ordering::Relaxed);
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
        // Drop the stream outside the lock, which stops the audio callback
        let _lifecycle = self.shared.lifecycle.lock().unwrap();
        let stream = self.shared.take_stream(None);
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

pub struct ErrorSubscription(u64);

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

    struct Silence;

    impl Runner for Silence {
        type Command = ();
        type Update = ();

        fn handle_command(&mut self, _: (), _: impl FnMut(())) {}

        fn run(&mut self, _: &mut [f32], _: Layout, _: usize, _: Timing, _: impl FnMut(())) {}
    }

    #[test]
    fn handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AudioLoopHandle<Silence>>();
    }

    #[test]
    fn report_error_counts_errors_that_dont_fit() {
        let (mut errors, mut receiver) = RingBuffer::new(1);
        let dropped = AtomicUsize::new(0);

        report_error(
            &mut errors,
            &dropped,
            StreamError::new(StreamErrorKind::Xrun),
        );
        report_error(
            &mut errors,
            &dropped,
            StreamError::new(StreamErrorKind::Xrun),
        );
        report_error(
            &mut errors,
            &dropped,
            StreamError::new(StreamErrorKind::Xrun),
        );

        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        assert_eq!(receiver.pop().map(|e| e.kind()), Ok(StreamErrorKind::Xrun));
        assert!(receiver.pop().is_err());
    }

    #[test]
    fn update_thread_delivers_errors_as_a_batch() {
        let handle = AudioLoopHandle::<Silence>::new(1, 1, 4);
        let (batches_tx, batches_rx) = std::sync::mpsc::channel();
        let _subscription = handle.subscribe_errors(move |batch| {
            let kinds: Vec<_> = batch.errors.iter().map(|e| e.kind()).collect();
            batches_tx.send((kinds, batch.dropped)).unwrap();
        });

        // Stand in for a running stream's error callback
        let (mut errors, receiver) = RingBuffer::new(4);
        let dropped = Arc::new(AtomicUsize::new(3));
        report_error(
            &mut errors,
            &dropped,
            StreamError::new(StreamErrorKind::Xrun),
        );
        report_error(
            &mut errors,
            &dropped,
            StreamError::new(StreamErrorKind::RealtimeDenied),
        );
        handle
            .inner
            .shared
            .error_receivers
            .lock()
            .unwrap()
            .push_back(ErrorReceiver {
                errors: receiver,
                dropped: Arc::clone(&dropped),
                generation: 0,
            });
        handle
            .inner
            .update_thread
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .unpark();

        let (kinds, batch_dropped) = batches_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            kinds,
            [StreamErrorKind::Xrun, StreamErrorKind::RealtimeDenied]
        );
        assert_eq!(batch_dropped, 3);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    /// A `Shared` with nothing running, for driving [`Shared::pump`] directly.
    fn idle_shared() -> Shared<Silence> {
        Shared {
            lifecycle: Mutex::new(0),
            stream: Mutex::new(None),
            update_receivers: Mutex::new(VecDeque::new()),
            error_receivers: Mutex::new(VecDeque::new()),
            subscribers: Mutex::new(SubscriptionMap::new()),
            error_subscribers: Mutex::new(SubscriptionMap::new()),
        }
    }

    #[test]
    fn drain_receivers_keeps_order_and_retires_finished_queues() {
        let (mut old_tx, old_rx) = RingBuffer::new(4);
        let (mut new_tx, new_rx) = RingBuffer::new(4);
        old_tx.push(1).unwrap();
        old_tx.push(2).unwrap();
        new_tx.push(3).unwrap();
        drop(old_tx);

        let mut receivers = VecDeque::from([old_rx, new_rx]);
        let mut drained = Vec::new();
        let drain = |receiver: &mut rtrb::Consumer<i32>| {
            while let Ok(value) = receiver.pop() {
                drained.push(value);
            }
            receiver.is_abandoned()
        };
        drain_receivers(&mut receivers, drain);

        assert_eq!(drained, [1, 2, 3]);
        assert_eq!(
            receivers.len(),
            1,
            "only the stopped stream's queue is retired"
        );
    }

    #[test]
    fn errors_left_by_a_stopped_stream_are_still_delivered() {
        let shared = idle_shared();
        let batches = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&batches);
        shared
            .error_subscribers
            .lock()
            .unwrap()
            .insert(Box::new(move |batch: &ErrorBatch<'_>| {
                let kinds: Vec<_> = batch.errors.iter().map(|e| e.kind()).collect();
                sink.lock().unwrap().push((kinds, batch.stopped));
            }));

        // A stream that reported a fatal error and is already gone
        let (mut errors, receiver) = RingBuffer::new(4);
        let dropped = Arc::new(AtomicUsize::new(0));
        report_error(
            &mut errors,
            &dropped,
            StreamError::new(StreamErrorKind::DeviceNotAvailable),
        );
        drop(errors);
        shared
            .error_receivers
            .lock()
            .unwrap()
            .push_back(ErrorReceiver {
                errors: receiver,
                dropped,
                generation: 7,
            });

        let stopped = shared.pump(&mut Vec::new(), &mut Vec::new());

        assert!(!stopped, "no running stream of that generation to stop");
        assert_eq!(
            *batches.lock().unwrap(),
            [(vec![StreamErrorKind::DeviceNotAvailable], false)]
        );
        assert!(shared.error_receivers.lock().unwrap().is_empty());
    }

    #[test]
    fn only_errors_after_which_the_stream_cant_continue_stop_it() {
        assert!(stops_stream(StreamErrorKind::DeviceNotAvailable));
        assert!(stops_stream(StreamErrorKind::StreamInvalidated));
        assert!(!stops_stream(StreamErrorKind::Xrun));
        assert!(!stops_stream(StreamErrorKind::RealtimeDenied));
        assert!(!stops_stream(StreamErrorKind::DeviceChanged));
        assert!(!stops_stream(StreamErrorKind::BackendError));
        assert!(!stops_stream(StreamErrorKind::Other));
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
