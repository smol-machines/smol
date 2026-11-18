use super::{check_cancel, failure, Kubernetes, BIN, ROOT};
use crate::{ErrorKind, ExecEvent, ExecOptions, Result};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CLEANUP_GRACE: Duration = Duration::from_secs(3);

/// An image archive imported into this cluster's container store.
#[derive(Debug)]
pub struct ImageImport {
    /// Archive bytes streamed from the host, without a second guest archive copy.
    pub bytes: u64,
    /// A bounded tail of containerd's import diagnostics.
    pub output: String,
}

fn tail(buffer: &mut Vec<u8>, bytes: &[u8]) {
    const LIMIT: usize = 64 * 1024;
    if bytes.len() >= LIMIT {
        buffer.clear();
        buffer.extend_from_slice(&bytes[bytes.len() - LIMIT..]);
    } else {
        let excess = (buffer.len() + bytes.len()).saturating_sub(LIMIT);
        buffer.drain(..excess);
        buffer.extend_from_slice(bytes);
    }
}

impl Kubernetes {
    /// Stream a Docker or OCI image archive into the running cluster.
    ///
    /// Named images retain their tags. Use a non-latest tag with
    /// `imagePullPolicy: IfNotPresent`, or `Never`, to use them without a registry.
    /// The archive stays on the host; only bounded stream buffers are used.
    /// Cancellation or an import error can leave already imported content in
    /// containerd, but never deletes existing images or workloads.
    pub fn import_images(
        &self,
        archive: impl AsRef<Path>,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<ImageImport> {
        check_cancel(cancelled)?;
        if timeout.is_zero() {
            return Err(failure(
                ErrorKind::Config,
                "Image import timeout must be positive",
            ));
        }
        let path = archive.as_ref();
        let metadata = std::fs::metadata(path)
            .map_err(|e| failure(ErrorKind::Storage, format!("Inspect image archive: {e}")))?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(failure(
                ErrorKind::Config,
                "Choose a nonempty regular image archive",
            ));
        }
        let mut file = File::open(path)
            .map_err(|e| failure(ErrorKind::Storage, format!("Open image archive: {e}")))?;
        let before = file
            .metadata()
            .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
        if !before.is_file() || before.len() == 0 {
            return Err(failure(
                ErrorKind::Config,
                "The image archive changed before import",
            ));
        }
        self.owned()?;
        if self.status()?["phase"] != "ready" {
            return Err(failure(
                ErrorKind::InvalidState,
                "Start the cluster before importing images",
            ));
        }
        check_cancel(cancelled)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| failure(ErrorKind::Config, "Image import timeout is too large"))?;
        // The SDK closes stdin at the requested deadline. Give ctr time to
        // release its ingest lease before the guest's hard timeout fires.
        let command_timeout = timeout
            .checked_add(CLEANUP_GRACE + Duration::from_secs(1))
            .ok_or_else(|| failure(ErrorKind::Config, "Image import timeout is too large"))?;
        let mut stream = self.machine.exec_stream(
            [
                BIN,
                "ctr",
                "--namespace",
                "k8s.io",
                "images",
                "import",
                "--local",
                "-",
            ],
            ExecOptions::new()
                .stdin(true)
                .timeout(command_timeout)
                .env("K3S_DATA_DIR", format!("{ROOT}/data")),
        )?;
        let kill = stream.kill_handle();
        let Some(mut input) = stream.stdin() else {
            kill.kill();
            return Err(failure(ErrorKind::Other, "Image import has no stdin"));
        };
        let finished = AtomicBool::new(false);
        let expired = AtomicBool::new(false);
        let input_failed = AtomicBool::new(false);
        let mut output = Vec::new();
        let mut exit = None;
        let mut stream_error = None;
        let copied = std::thread::scope(|scope| {
            let monitor = scope.spawn(|| {
                let mut stopping = None;
                while !finished.load(Ordering::Acquire) {
                    if cancelled.load(Ordering::Acquire)
                        || Instant::now() >= deadline
                        || input_failed.load(Ordering::Acquire)
                    {
                        expired.store(Instant::now() >= deadline, Ordering::Release);
                        // Let stdin EOF unwind ctr's import and release its lease;
                        // killing it immediately leaves resumable ingest references.
                        let since = stopping.get_or_insert_with(Instant::now);
                        if since.elapsed() >= CLEANUP_GRACE {
                            kill.kill();
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            });
            let writer = scope.spawn(|| {
                let mut bytes = 0_u64;
                let mut buffer = [0_u8; 64 * 1024];
                let copied = (|| -> std::io::Result<u64> {
                    loop {
                        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Interrupted,
                                "Image import stopped",
                            ));
                        }
                        let count = file.read(&mut buffer)?;
                        if count == 0 {
                            break;
                        }
                        input.write_all(&buffer[..count])?;
                        bytes += count as u64;
                    }
                    Ok(bytes)
                })();
                drop(input);
                input_failed.store(copied.is_err(), Ordering::Release);
                copied
            });
            for event in stream {
                match event {
                    ExecEvent::Stdout(bytes) | ExecEvent::Stderr(bytes) => {
                        tail(&mut output, &bytes)
                    }
                    ExecEvent::Exit(code) => exit = Some(code),
                    ExecEvent::Error(error) => stream_error = Some(error),
                }
            }
            finished.store(true, Ordering::Release);
            // End a writer promptly if the guest exits before consuming stdin.
            kill.kill();
            let copied = writer
                .join()
                .map_err(|_| failure(ErrorKind::Other, "Image archive reader stopped"));
            let _ = monitor.join();
            copied
        })?;
        check_cancel(cancelled)?;
        if expired.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(failure(
                ErrorKind::Timeout,
                "Image import timed out; some content may have been imported",
            ));
        }
        let output = String::from_utf8_lossy(&output).into_owned();
        if exit != Some(0) || stream_error.is_some() {
            return Err(failure(
                ErrorKind::CommandFailed,
                format!(
                    "Image import failed; some content may have been imported: {} {}",
                    stream_error.unwrap_or_default(),
                    output.trim()
                ),
            ));
        }
        let bytes = copied
            .map_err(|e| failure(ErrorKind::Storage, format!("Stream image archive: {e}")))?;
        let after = file
            .metadata()
            .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
        if before.len() != bytes
            || after.len() != before.len()
            || after.modified().ok() != before.modified().ok()
        {
            return Err(failure(
                ErrorKind::Conflict,
                "The archive changed during import; inspect cluster images before retrying",
            ));
        }
        Ok(ImageImport { bytes, output })
    }
}
