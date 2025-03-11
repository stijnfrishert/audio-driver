use crate::backend::{Backend, StartError};
use std::{
    sync::mpsc::{SendError, SyncSender, sync_channel},
    thread,
    time::Duration,
};

pub struct AudioLoop<Command, Update> {
    backend: Box<dyn Backend>,
    command_sender: SyncSender<Command>,
    update_sender: Option<SyncSender<Update>>,
    update_thread: Option<thread::JoinHandle<()>>,
}

impl<Command, Update> AudioLoop<Command, Update>
where
    Command: Send + 'static,
    Update: Send + 'static,
{
    pub fn new(
        mut backend: Box<dyn Backend>,
        command_count: usize,
        update_count: usize,
        mut on_run: impl FnMut(&[Command], &mut [f32], usize) + Send + 'static,
        mut on_update: impl FnMut(Update) + Send + 'static,
    ) -> Result<Self, StartError> {
        // Create senders and receivers for the audio loop
        let (command_sender, command_receiver) = sync_channel(command_count);
        let (update_sender, update_receiver) = sync_channel(update_count);

        let mut command_queue = Vec::with_capacity(command_count);

        // Start the backend and run the audio loop
        backend.start(Box::new(move |output, len| {
            // Handle the messages
            while let Ok(command) = command_receiver.try_recv() {
                command_queue.push(command);
            }

            on_run(&command_queue, output, len);

            command_queue.clear();
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

    pub fn send_command(&self, command: Command) -> Result<(), SendError<Command>> {
        self.command_sender.send(command)
    }
}

impl<Command, Update> Drop for AudioLoop<Command, Update> {
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
