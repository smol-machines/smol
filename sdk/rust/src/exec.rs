//! Running commands in a machine.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

/// Environment, working directory, timeout and user for one command.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Extra environment for this command.
    pub env: Vec<(String, String)>,
    /// Working directory for this command.
    pub workdir: Option<String>,
    /// Give up after this long.
    pub timeout: Option<Duration>,
    /// Run as this user: a name from the image or a numeric `uid[:gid]`.
    /// Image machines only — a bare VM runs every command as root, so asking
    /// for a user there is an error rather than a silent root. On the cloud the
    /// SDK first checks that the control plane applies it, and refuses with
    /// [`crate::ErrorKind::NotSupported`] before running anything if it would not.
    pub user: Option<String>,
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

    /// Run as this user.
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Refuse options no target could honor, before anything runs.
    pub(crate) fn validate(&self) -> crate::Result<()> {
        use crate::{Error, ErrorKind};
        if self.timeout.is_some_and(|timeout| timeout.is_zero()) {
            return Err(Error::new(
                ErrorKind::Config,
                "an exec timeout must be greater than zero",
            ));
        }
        if self
            .user
            .as_deref()
            .is_some_and(|user| user.trim().is_empty())
        {
            return Err(Error::new(
                ErrorKind::Config,
                "an exec user must not be empty",
            ));
        }
        Ok(())
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
/// final [`ExecEvent::Exit`].
///
/// [`ExecStream::kill`] (or a [`KillHandle`] from another thread) stops the
/// command. Dropping the stream without killing it leaves the command running
/// to completion in the guest, like dropping a [`std::process::Child`].
pub struct ExecStream {
    rx: Receiver<ExecEvent>,
    kill: KillHandle,
}

/// Stops a streaming command from any thread. Cloned handles share one stream.
#[derive(Clone)]
pub struct KillHandle {
    killed: Arc<AtomicBool>,
    stop: Arc<dyn Fn() + Send + Sync>,
}

impl KillHandle {
    pub(crate) fn new(stop: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            killed: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(stop),
        }
    }

    /// Kill the command and end its stream: the iterator yields nothing more,
    /// not even an [`ExecEvent::Exit`]. Idempotent, and harmless once the
    /// command has exited.
    ///
    /// Locally the command and everything it started are killed in the
    /// machine. On the cloud the stream's connection is closed at once;
    /// whether the command keeps running is up to the control plane.
    pub fn kill(&self) {
        if !self.killed.swap(true, Ordering::SeqCst) {
            (self.stop)();
        }
    }

    /// Whether [`Self::kill`] has been called.
    pub fn is_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for KillHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KillHandle")
            .field("killed", &self.is_killed())
            .finish()
    }
}

impl ExecStream {
    /// Wrap a channel some other producer is feeding; `kill` stops that
    /// producer's command.
    pub(crate) fn new(rx: Receiver<ExecEvent>, kill: KillHandle) -> Self {
        Self { rx, kill }
    }

    /// Kill the command. See [`KillHandle::kill`].
    pub fn kill(&self) {
        self.kill.kill();
    }

    /// A handle that kills this command from another thread, for example
    /// while this one is blocked waiting for the next event.
    pub fn kill_handle(&self) -> KillHandle {
        self.kill.clone()
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
        if self.kill.is_killed() {
            return None;
        }
        let event = self.rx.recv().ok()?;
        // A kill that raced this event wins: what the teardown produced (an
        // exit by signal, a closed connection) is not the command's output.
        (!self.kill.is_killed()).then_some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_no_target_could_honor_are_refused() {
        let zero = ExecOptions::new().timeout(Duration::ZERO).validate();
        assert_eq!(zero.unwrap_err().kind(), crate::ErrorKind::Config);
        let blank = ExecOptions::new().user(" ").validate();
        assert_eq!(blank.unwrap_err().kind(), crate::ErrorKind::Config);
        ExecOptions::new()
            .timeout(Duration::from_millis(1))
            .user("1000:1000")
            .validate()
            .unwrap();
    }

    #[test]
    fn a_kill_stops_the_producer_once_and_ends_the_stream() {
        let (tx, rx) = std::sync::mpsc::channel();
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&stops);
        let mut stream = ExecStream::new(
            rx,
            KillHandle::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            }),
        );
        tx.send(ExecEvent::Stdout(b"a".to_vec())).unwrap();
        assert_eq!(stream.next(), Some(ExecEvent::Stdout(b"a".to_vec())));
        // Whatever the teardown produces after the kill is not the command's.
        tx.send(ExecEvent::Exit(-1)).unwrap();
        stream.kill();
        stream.kill_handle().kill();
        assert_eq!(stream.next(), None);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }
}
