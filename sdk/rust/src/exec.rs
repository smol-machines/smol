//! Running commands in a machine.

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use smolvm::agent::ExecEvent as AgentExecEvent;

/// Environment, working directory and timeout for one command.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Extra environment for this command.
    pub env: Vec<(String, String)>,
    /// Working directory for this command.
    pub workdir: Option<String>,
    /// Give up after this long.
    pub timeout: Option<Duration>,
}

impl ExecOptions {
    /// Options with nothing set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Run in this directory.
    pub fn workdir(mut self, workdir: impl Into<String>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    /// Give up after this long.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub(crate) fn split(self) -> (Vec<(String, String)>, Option<String>, Option<Duration>) {
        (self.env, self.workdir, self.timeout)
    }
}

/// What a finished command produced.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Process exit code.
    pub exit_code: i32,
    /// Everything the command wrote to stdout.
    pub stdout: Vec<u8>,
    /// Everything the command wrote to stderr.
    pub stderr: Vec<u8>,
}

impl ExecResult {
    /// Stdout decoded as UTF-8, replacing invalid sequences.
    pub fn stdout_utf8(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// Stderr decoded as UTF-8, replacing invalid sequences.
    pub fn stderr_utf8(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }

    /// Whether the command exited zero.
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// One event from a live command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvent {
    /// A chunk of stdout.
    Stdout(Vec<u8>),
    /// A chunk of stderr.
    Stderr(Vec<u8>),
    /// The command exited with this code. Always the last event of a run.
    Exit(i32),
    /// The stream itself failed.
    Error(String),
}

impl From<AgentExecEvent> for ExecEvent {
    fn from(event: AgentExecEvent) -> Self {
        match event {
            AgentExecEvent::Stdout(data) => Self::Stdout(data),
            AgentExecEvent::Stderr(data) => Self::Stderr(data),
            AgentExecEvent::Exit(code) => Self::Exit(code),
            AgentExecEvent::Error(message) => Self::Error(message),
        }
    }
}

/// A command's output as it arrives.
///
/// The engine drives the command on a worker thread and feeds a channel, so
/// iterating yields each chunk as the guest produces it rather than waiting for
/// the command to finish. The iterator ends when the command exits, after the
/// final [`ExecEvent::Exit`]. Dropping the stream leaves the command running to
/// completion in the guest; it does not kill it.
pub struct ExecStream {
    rx: Receiver<ExecEvent>,
}

impl ExecStream {
    /// Drive the embedded engine's streaming exec on a worker thread.
    pub(crate) fn spawn_local(
        name: String,
        command: Vec<String>,
        env: Vec<(String, String)>,
        workdir: Option<String>,
        timeout: Option<Duration>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let error_tx = tx.clone();
        thread::spawn(move || {
            let result = smolvm::embedded::runtime().and_then(|runtime| {
                runtime.exec_streaming_with(&name, command, env, workdir, timeout, |event| {
                    let _ = tx.send(event.into());
                })
            });
            if let Err(error) = result {
                let _ = error_tx.send(ExecEvent::Error(error.to_string()));
            }
            // Both senders drop here, which ends the iterator.
        });
        Self { rx }
    }

    /// Wrap a channel some other producer is feeding.
    pub(crate) fn from_receiver(rx: Receiver<ExecEvent>) -> Self {
        Self { rx }
    }

    /// Drain the stream, collecting stdout and stderr into one result.
    ///
    /// A stream that fails before the command exits reports exit code -1 and
    /// carries the failure on stderr.
    pub fn collect_result(self) -> ExecResult {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = -1;
        for event in self {
            match event {
                ExecEvent::Stdout(chunk) => stdout.extend_from_slice(&chunk),
                ExecEvent::Stderr(chunk) => stderr.extend_from_slice(&chunk),
                ExecEvent::Exit(code) => exit_code = code,
                ExecEvent::Error(message) => stderr.extend_from_slice(message.as_bytes()),
            }
        }
        ExecResult {
            exit_code,
            stdout,
            stderr,
        }
    }
}

impl Iterator for ExecStream {
    type Item = ExecEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.rx.recv().ok()
    }
}
