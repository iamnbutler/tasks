//! Transport layer for host ↔ supervisor communication.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::mpsc;
use std::thread;

use thiserror::Error;

use crate::protocol::{encode, decode_line, Command, Event};

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport closed")]
    Closed,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Transport for communicating with a container supervisor over stdio.
///
/// Owns the child process and provides send/receive methods for the protocol.
pub struct StdioTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    event_rx: mpsc::Receiver<Event>,
    reader_handle: Option<thread::JoinHandle<()>>,
}

impl StdioTransport {
    /// Create a new transport wrapping a child process.
    ///
    /// Spawns a reader thread to receive events from stdout.
    pub fn new(mut child: Child) -> Self {
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();

        let (event_tx, event_rx) = mpsc::channel();

        let reader_handle = stdout.map(|stdout| {
            thread::spawn(move || {
                Self::reader_loop(stdout, event_tx);
            })
        });

        Self {
            child,
            stdin,
            event_rx,
            reader_handle,
        }
    }

    fn reader_loop(stdout: ChildStdout, event_tx: mpsc::Sender<Event>) {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if let Some(event) = decode_line(&line) {
                        if event_tx.send(event).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// Send a command to the supervisor.
    pub fn send(&mut self, command: &Command) -> Result<(), TransportError> {
        let stdin = self.stdin.as_mut().ok_or(TransportError::Closed)?;
        let line = encode(command);
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
        Ok(())
    }

    /// Try to receive an event without blocking.
    pub fn try_recv(&self) -> Option<Event> {
        self.event_rx.try_recv().ok()
    }

    /// Receive an event, blocking until one is available.
    pub fn recv(&self) -> Result<Event, TransportError> {
        self.event_rx.recv().map_err(|_| TransportError::Closed)
    }

    /// Receive an event with a timeout.
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Result<Event, TransportError> {
        self.event_rx
            .recv_timeout(timeout)
            .map_err(|_| TransportError::Closed)
    }

    /// Check if the transport is still connected.
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            _ => false,
        }
    }

    /// Kill the child process.
    pub fn kill(&mut self) -> Result<(), TransportError> {
        self.child.kill()?;
        Ok(())
    }

    /// Wait for the child process to exit.
    pub fn wait(&mut self) -> Result<std::process::ExitStatus, TransportError> {
        Ok(self.child.wait()?)
    }

    /// Close the transport, killing the process if still running.
    pub fn close(mut self) -> Result<(), TransportError> {
        drop(self.stdin.take());

        if self.is_alive() {
            self.kill()?;
        }

        self.wait()?;

        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }

        Ok(())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        // Best effort cleanup
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
