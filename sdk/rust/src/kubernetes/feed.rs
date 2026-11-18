//! Managed local images use the selected engine's read-only archive feed.
use super::{check_cancel, failure, Kubernetes};
use crate::{ErrorKind, ExecEvent, ExecOptions, MachineState, Mount, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) const GUEST_PATH: &str = "/run/smolvm/image-feed";
const OUTPUT_LIMIT: u64 = 1024 * 1024;

pub(super) struct Feed {
    path: String,
    attached: bool,
}

impl Feed {
    pub(super) fn mount(&self) -> Mount {
        Mount::new(&self.path, GUEST_PATH).read_only()
    }

    /// Prove host events reach this boot, including when the feed is empty.
    /// A mount and a kernel hook alone do not prove the host watcher is alive.
    pub(super) fn verify(
        &self,
        cluster: &Kubernetes,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        check_cancel(cancelled)?;
        let timeout = timeout.min(Duration::from_secs(5));
        let deadline = Instant::now() + timeout;
        let nonce = tempfile::NamedTempFile::new()
            .map_err(|error| failure(ErrorKind::Storage, error.to_string()))?;
        let prefix = format!(
            ".smol-probe-{}-",
            nonce.path().file_name().unwrap().to_string_lossy()
        );
        let stream = cluster.machine.exec_stream(
            [
                "sh",
                "-c",
                "test -e /proc/smolvm-fsnotify && exec inotifyd - /run/smolvm/image-feed:n",
            ],
            ExecOptions::new().timeout(timeout),
        )?;
        let kill = stream.kill_handle();
        let seen = AtomicBool::new(false);
        let finished = AtomicBool::new(false);
        let mut probes = Vec::new();
        let mut write_error = None;
        let diagnostic = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut output = Vec::new();
                let mut diagnostic = Vec::new();
                for event in stream {
                    match event {
                        ExecEvent::Stdout(bytes) => {
                            if output.len() + bytes.len() > 64 * 1024 {
                                break;
                            }
                            output.extend(bytes);
                            if String::from_utf8_lossy(&output).lines().any(|line| {
                                line.split_whitespace()
                                    .last()
                                    .is_some_and(|name| name.starts_with(&prefix))
                            }) {
                                seen.store(true, Ordering::Release);
                                break;
                            }
                        }
                        ExecEvent::Stderr(bytes) => {
                            let count = bytes
                                .len()
                                .min((64 * 1024_usize).saturating_sub(diagnostic.len()));
                            diagnostic.extend_from_slice(&bytes[..count]);
                        }
                        ExecEvent::Error(error) => {
                            diagnostic.extend_from_slice(
                                error.as_bytes().get(..4096).unwrap_or(error.as_bytes()),
                            );
                            break;
                        }
                        ExecEvent::Exit(_) => break,
                    }
                }
                finished.store(true, Ordering::Release);
                String::from_utf8_lossy(&diagnostic).into_owned()
            });
            while !seen.load(Ordering::Acquire)
                && !finished.load(Ordering::Acquire)
                && Instant::now() < deadline
                && !cancelled.load(Ordering::Acquire)
            {
                // inotifyd may not yet have registered its watch. Distinct names
                // retry that small race without relying on a fixed startup sleep.
                match tempfile::Builder::new()
                    .prefix(&prefix)
                    .tempfile_in(&self.path)
                {
                    Ok(probe) => probes.push(probe),
                    Err(error) => {
                        write_error = Some(error);
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            kill.kill();
            reader
                .join()
                .unwrap_or_else(|_| "Notification probe stopped unexpectedly".into())
        });
        check_cancel(cancelled)?;
        if let Some(error) = write_error {
            return Err(failure(
                ErrorKind::Storage,
                format!("Check local image notifications: {error}"),
            ));
        }
        if !seen.load(Ordering::Acquire) {
            return Err(failure(ErrorKind::Other, format!(
                "Host file notifications did not reach this cluster; restart the cluster with an updated Smol CLI before using automatic local images. {}", diagnostic.trim())));
        }
        Ok(())
    }
}

pub(super) fn require_notifications() -> Result<()> {
    if std::env::var_os("SMOL_NO_HOT_RELOAD").is_some() {
        return Err(failure(ErrorKind::Config,
            "Automatic Kubernetes images require file notifications; unset SMOL_NO_HOT_RELOAD before creating or starting the cluster"));
    }
    Ok(())
}

pub(super) fn prepare(
    machine: Option<&str>,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<Feed> {
    let mut args = vec!["machine".into(), "image-feed".into()];
    if let Some(machine) = machine {
        args.extend(["--machine".into(), machine.into()]);
    }
    let output = engine(&args, timeout, cancelled)?;
    let value: serde_json::Value = serde_json::from_slice(&output).map_err(|error| {
        failure(
            ErrorKind::Other,
            format!("Read engine image feed: {error}; update the selected Smol CLI"),
        )
    })?;
    let feed = Feed {
        path: value["path"]
            .as_str()
            .ok_or_else(|| failure(ErrorKind::Other, "Engine image feed has no path"))?
            .into(),
        attached: value["attached"]
            .as_bool()
            .ok_or_else(|| failure(ErrorKind::Other, "Engine image feed has no mount status"))?,
    };
    // Mount serialization uses ':' as a delimiter on Unix. Refuse ambiguous
    // paths rather than accidentally mounting a different directory writable.
    if !Path::new(&feed.path).is_absolute()
        || feed.path.contains(['\n', '\r'])
        || (cfg!(unix) && feed.path.contains(':'))
    {
        return Err(failure(
            ErrorKind::Config,
            "The engine image-feed path cannot be mounted safely",
        ));
    }
    Ok(feed)
}

impl Kubernetes {
    pub(super) fn prepare_images(&self, timeout: Duration, cancelled: &AtomicBool) -> Result<Feed> {
        require_notifications()?;
        let feed = prepare(Some(self.machine.name()), timeout, cancelled)?;
        if feed.attached {
            return Ok(feed);
        }
        if self.machine.state() != MachineState::Stopped {
            return Err(failure(ErrorKind::InvalidState,
                "Stop this cluster, then start it to enable automatic local images; workloads and volumes remain on its disk"));
        }
        // Use the engine's existing lifecycle lock and mount validation. Never
        // rewrite its database from the SDK or replace unrelated mounts.
        engine(
            &[
                "machine".into(),
                "update".into(),
                "--name".into(),
                self.machine.name().into(),
                "--volume".into(),
                format!("{}:{GUEST_PATH}:ro", feed.path),
            ],
            timeout,
            cancelled,
        )?;
        let mounted = prepare(Some(self.machine.name()), timeout, cancelled)?;
        if !mounted.attached {
            return Err(failure(
                ErrorKind::Mount,
                "The managed image-feed mount was not persisted",
            ));
        }
        Ok(mounted)
    }
}

pub(super) fn engine(
    args: &[String],
    timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut command = crate::engine_command()?;
    command.args(args).env_remove("SMOLVM_BOOT_BINARY");
    capture(command, timeout, cancelled)
}

struct Reap(Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn capture(mut command: Command, timeout: Duration, cancelled: &AtomicBool) -> Result<Vec<u8>> {
    check_cancel(cancelled)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| failure(ErrorKind::Config, "Image-feed timeout is too large"))?;
    let io = |error| {
        failure(
            ErrorKind::Other,
            format!("Prepare automatic local images: {error}"),
        )
    };
    // File-backed output avoids pipe deadlocks and unbounded memory while the
    // engine prepares a large retagged archive. Poll size as well as cancellation.
    let mut stdout = tempfile::tempfile().map_err(io)?;
    let mut stderr = tempfile::tempfile().map_err(io)?;
    let mut child = Reap(
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().map_err(io)?)
            .stderr(stderr.try_clone().map_err(io)?)
            .spawn()
            .map_err(io)?,
    );
    let status = loop {
        check_cancel(cancelled)?;
        if stdout.metadata().map_err(io)?.len() > OUTPUT_LIMIT
            || stderr.metadata().map_err(io)?.len() > OUTPUT_LIMIT
        {
            return Err(failure(
                ErrorKind::Other,
                "Engine image-feed output exceeded 1 MiB",
            ));
        }
        if let Some(status) = child.0.try_wait().map_err(io)? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err(failure(
                ErrorKind::Other,
                "Preparing automatic local images timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    stdout.seek(SeekFrom::Start(0)).map_err(io)?;
    stderr.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut output = Vec::new();
    (&mut stdout)
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut output)
        .map_err(io)?;
    let mut diagnostic = String::new();
    (&mut stderr)
        .take(64 * 1024)
        .read_to_string(&mut diagnostic)
        .map_err(io)?;
    if !status.success() {
        return Err(failure(ErrorKind::Other, format!("Prepare automatic local images: {}; update the selected Smol CLI if image-feed is unsupported", diagnostic.trim())));
    }
    if output.len() as u64 > OUTPUT_LIMIT {
        return Err(failure(
            ErrorKind::Other,
            "Engine image-feed output exceeded 1 MiB",
        ));
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn cancellation_reaps_a_running_engine_command() {
        let cancelled = AtomicBool::new(false);
        let start = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancelled.store(true, Ordering::Release);
            });
            let mut command = Command::new("sh");
            command.args(["-c", "exec sleep 30"]);
            let error = capture(command, Duration::from_secs(60), &cancelled).unwrap_err();
            assert!(error.to_string().contains("cancelled"));
        });
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn bounds_engine_output_and_times_out_without_waiting_for_exit() {
        let cancelled = AtomicBool::new(false);
        let mut command = Command::new("sh");
        command.args(["-c", "printf '%02000000d' 0"]);
        assert!(capture(command, Duration::from_secs(5), &cancelled)
            .unwrap_err()
            .to_string()
            .contains("exceeded"));
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let start = Instant::now();
        assert!(capture(command, Duration::from_millis(50), &cancelled)
            .unwrap_err()
            .to_string()
            .contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
