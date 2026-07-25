//! The one path from this process to the terminal.
//!
//! A dedicated OS thread owns stdout and does blocking writes, so a terminal
//! that stops draining its pty can never stall the async runtime. The frame
//! channel holds a single slot: when it is full the renderer keeps its damage
//! and skips the tick, which coalesces frames instead of queueing stale ones.
//!
//! Everything on screen -- graphics, status line, overlays -- is composed into
//! one buffer per frame and submitted here, so ordering is never in question.

use std::io::{self, Write};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel,
};
use std::thread::JoinHandle;
use std::time::Duration;

/// Number of buffers kept in rotation. Two is enough: one being filled, one
/// being written.
const POOL: usize = 2;

/// How long teardown waits for the writer to finish before giving up on it.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

pub struct FrameWriter {
    frames: Option<SyncSender<Vec<u8>>>,
    returned: Receiver<Vec<u8>>,
    recycled: SyncSender<Vec<u8>>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl FrameWriter {
    pub fn spawn() -> Self {
        let (frame_tx, frame_rx) = sync_channel::<Vec<u8>>(1);
        let (back_tx, back_rx) = sync_channel::<Vec<u8>>(POOL);
        let back_tx_for_caller = back_tx.clone();
        let (done_tx, done_rx) = channel::<()>();

        let thread = std::thread::Builder::new()
            .name("desktui-writer".into())
            .spawn(move || {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                while let Ok(mut buf) = frame_rx.recv() {
                    // A broken pipe means the terminal is gone; the session
                    // loop will notice through its own channels.
                    if out.write_all(&buf).is_err() || out.flush().is_err() {
                        break;
                    }
                    buf.clear();
                    let _ = back_tx.try_send(buf);
                }
                let _ = done_tx.send(());
            })
            .expect("failed to spawn the terminal writer thread");

        Self {
            frames: Some(frame_tx),
            returned: back_rx,
            recycled: back_tx_for_caller,
            done: done_rx,
            thread: Some(thread),
        }
    }

    /// An empty buffer to compose the next frame into, recycled when possible.
    pub fn take_buffer(&self) -> Vec<u8> {
        match self.returned.try_recv() {
            Ok(buf) => buf,
            Err(_) => Vec::with_capacity(64 * 1024),
        }
    }

    /// Hand back a buffer whose frame was dropped, so the allocation is reused.
    pub fn recycle(&self, mut buf: Vec<u8>) {
        buf.clear();
        let _ = self.recycled.try_send(buf);
    }

    /// Hand a composed frame to the terminal.
    ///
    /// Returns the buffer back when the writer is still busy with the previous
    /// frame, so the caller can keep its damage and try again next tick.
    pub fn submit(&self, buf: Vec<u8>) -> Result<(), Busy> {
        let Some(tx) = &self.frames else {
            return Err(Busy::Closed);
        };
        match tx.try_send(buf) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(buf)) => Err(Busy::Full(buf)),
            Err(TrySendError::Disconnected(_)) => Err(Busy::Closed),
        }
    }

    /// Hand over a frame that must not be dropped, such as one carrying a mode
    /// change. Blocks until the writer takes it.
    pub fn submit_blocking(&self, buf: Vec<u8>) -> Result<(), Busy> {
        let Some(tx) = &self.frames else {
            return Err(Busy::Closed);
        };
        tx.send(buf).map_err(|_| Busy::Closed)
    }

    /// Flush everything queued and stop the thread.
    ///
    /// Deliberately does not join unconditionally: a terminal that has stopped
    /// draining its pty leaves the writer blocked inside `write_all` with no way
    /// to interrupt it, and quitting must not depend on the terminal's
    /// cooperation. After the grace period the thread is abandoned and teardown
    /// continues, which in the worst case interleaves the restore sequence with
    /// a half-written frame -- much better than never exiting.
    pub fn shutdown(&mut self) {
        drop(self.frames.take());
        let Some(thread) = self.thread.take() else {
            return;
        };
        match self.done.recv_timeout(DRAIN_GRACE) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
            }
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!("terminal stopped accepting output; abandoning the writer");
            }
        }
    }
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
pub enum Busy {
    /// The writer has not finished the previous frame; here is your buffer back.
    Full(Vec<u8>),
    /// The writer thread is gone.
    Closed,
}
