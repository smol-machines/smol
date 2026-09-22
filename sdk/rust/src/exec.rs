//! Running commands in a machine.

use std::sync::mpsc::Receiver;
use std::time::Duration;

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
        let mut failed = false;
        for event in self {
            match event {
                ExecEvent::Stdout(chunk) => stdout.extend_from_slice(&chunk),
                ExecEvent::Stderr(chunk) => stderr.extend_from_slice(&chunk),
                ExecEvent::Exit(code) => exit_code = code,
                ExecEvent::Error(message) => {
                    failed = true;
                    stderr.extend_from_slice(message.as_bytes());
                }
            }
        }
        ExecResult {
            exit_code: if failed { -1 } else { exit_code },
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
