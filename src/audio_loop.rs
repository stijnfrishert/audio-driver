use crate::backend::{Backend, Configuration, NewBackendError, StartBackendError};
use std::{
    sync::mpsc::{SendError, SyncSender, sync_channel},
    thread,
    time::Duration,
};
use thiserror::Error;

/// A running audio loop, plus the ability to send commands and receive updates
pub struct AudioLoop<B, R>
where
    B: Backend,
    R: Runner,
{
    /// The backend that drives the loop
    backend: B,

    /// The sender for incoming commands.
    /// This allows the audio processing to run on a separate thread
    command_sender: SyncSender<R::Command>,

    /// The sender that will be used to send updates out of the audio thread
    /// This is an option, because we will take it when dropping the audio loop
    update_sender: Option<SyncSender<R::Update>>,

    /// A handle to the thread receiving updates coming from the audio thread
    /// This is an option, because we will take it when dropping the audio loop
    update_thread: Option<thread::JoinHandle<()>>,
}

impl<B, R> AudioLoop<B, R>
where
    B: Backend,
    R: Runner + 'static,
{
    /// Create a new audio loop and start running it
    ///
    /// # Parameters
    /// - `configuration`: The configuration to initialize the backend with
    /// - `command_count`: The number of commands that can be buffered
    /// - `update_count`: The number of updates that can be buffered
    /// - `runner`: The runner that will process the audio loop and handle commands
    /// - `on_update`: A closure that will be called with updates coming from the audio loop
    pub fn new(
        configuration: Configuration,
        command_count: usize,
        update_count: usize,
        mut runner: R,
        mut on_update: impl FnMut(R::Update) + Send + 'static,
    ) -> Result<Self, AudioLoopNewError> {
        // Create the backend that will run the audio loop
        let mut backend = B::new(configuration)?;

        // Create a send/receive pair for the incoming commands
        let (command_sender, command_receiver) = sync_channel(command_count);

        // Create send/receive pairs for outgoing updates
        let (update_sender, update_receiver) = sync_channel(update_count);

        // Start the backend and run the audio loop
        backend.start(Box::new({
            let update_sender = update_sender.clone();
            move |output, len| {
                // Handle the messages
                while let Ok(command) = command_receiver.try_recv() {
                    runner.handle_command(command, &update_sender);
                }

                runner.run(output, len, &update_sender);
            }
        }))?;

        // Start a thread to receive updates
        let update_thread = thread::spawn({
            move || {
                while let Ok(update) = update_receiver.recv() {
                    on_update(update);

                    thread::sleep(Duration::from_millis(10));
                }
            }
        });

        Ok(Self {
            backend,
            command_sender,
            update_sender: Some(update_sender),
            update_thread: Some(update_thread),
        })
    }

    /// Retrieve the sample rate used by the audio loop
    pub fn sample_rate(&self) -> u32 {
        self.backend.configuration().sample_rate
    }

    /// Retrieve the number of channels used by the audio loop
    pub fn channel_count(&self) -> usize {
        self.backend.configuration().channel_count
    }

    /// Retrieve the buffer size used by the audio loop
    pub fn buffer_size(&self) -> usize {
        self.backend.configuration().buffer_size
    }

    /// Send a command to the runner in the audio loop
    pub fn send_command(&self, command: R::Command) -> Result<(), SendError<R::Command>> {
        self.command_sender.send(command)
    }
}

impl<B, R> Drop for AudioLoop<B, R>
where
    B: Backend,
    R: Runner,
{
    fn drop(&mut self) {
        // Stop the backend itself
        // This stops it from sending new messages as well
        self.backend.stop();

        // Drop the sender, which will close the channel
        drop(self.update_sender.take());

        // Join the thread
        self.update_thread.take().unwrap().join().unwrap();
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
    fn handle_command(&mut self, command: Self::Command, on_update: &SyncSender<Self::Update>);

    /// Process audio and send out updates if need be
    fn run(&mut self, output: &mut [f32], len: usize, on_update: &SyncSender<Self::Update>);
}

/// An error that can occur when creating a new audio loop
#[derive(Debug, Error)]
pub enum AudioLoopNewError {
    #[error("The backend could not be created")]
    NewBackend(#[from] NewBackendError),

    #[error("The audio callback could not be started")]
    StartBackend(#[from] StartBackendError),
}
