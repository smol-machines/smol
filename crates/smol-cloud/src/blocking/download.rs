use super::*;
use std::{io::Write, path::Path};

fn stopped() -> Error {
    Error::new(ErrorKind::Cancelled, "checkpoint download stopped")
}
fn io_error(error: std::io::Error) -> Error {
    Error::new(
        ErrorKind::Other,
        format!("write checkpoint download: {error}"),
    )
}

impl Client {
    /// Download an available checkpoint through its authenticated API route.
    /// The destination is published atomically without overwriting an existing
    /// file, only after exactly the checkpoint's advertised size arrives.
    /// Cancellation closes the stream and removes the private partial file.
    /// Download URLs in metadata are not followed and redirects are refused.
    pub fn download_checkpoint_cancellable(
        &self,
        checkpoint: &Checkpoint,
        destination: &Path,
        on_progress: &mut dyn FnMut(u64, u64),
        cancel: &StreamCancel,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(stopped());
        }
        if checkpoint.status != "available" || checkpoint.size_bytes == 0 {
            return Err(Error::new(
                ErrorKind::Conflict,
                "checkpoint is not available with a known nonzero size",
            ));
        }
        if checkpoint.id.is_empty()
            || checkpoint.id.contains(['/', '\\'])
            || checkpoint.id == "."
            || checkpoint.id == ".."
        {
            return Err(Error::new(ErrorKind::Other, "invalid checkpoint ID"));
        }
        if destination.try_exists().map_err(io_error)? {
            return Err(Error::new(
                ErrorKind::Conflict,
                "checkpoint destination already exists",
            ));
        }
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut staged = tempfile::Builder::new()
            .prefix(".smolcheckpoint-")
            .tempfile_in(parent)
            .map_err(io_error)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| {
                Error::new(ErrorKind::Other, "cannot start checkpoint download runtime")
            })?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| {
                Error::new(
                    ErrorKind::Connection,
                    "cannot create checkpoint download client",
                )
            })?;
        let path = format!("/v1/checkpoints/{}/download", encode_path(&checkpoint.id));
        let mut cancelled = cancel.subscribe();
        self.credentials.renew_if_expiring()?;
        let mut response = None;
        for attempt in 0..2 {
            let key = self.credentials.api_key();
            let mut request = http
                .get(format!("{}{path}", self.credentials.base_url()))
                .bearer_auth(&key)
                .timeout(CHECKPOINT_TIMEOUT + transfer_timeout(checkpoint.size_bytes));
            if let Some(tenant) = &self.tenant {
                request = request.header("Smol-Tenant", tenant);
            }
            let received = runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut cancelled) => Err(stopped()),
                    result = request.send() => result.map_err(|error| Error::new(
                        if error.is_timeout() { ErrorKind::Timeout } else { ErrorKind::Connection },
                        "checkpoint download request failed")),
                }
            })?;
            let status = received.status().as_u16();
            if status == 401 && attempt == 0 && self.credentials.renew_after_rejection(&key)? {
                continue;
            }
            if status != 200 {
                return Err(Error::new(
                    ErrorKind::from_status(status),
                    "checkpoint download was refused",
                )
                .with_response(status, None, false, None));
            }
            response = Some(received);
            break;
        }
        let mut response = response.ok_or_else(|| {
            Error::new(
                ErrorKind::Unauthorized,
                "checkpoint download authentication failed",
            )
        })?;
        if response
            .content_length()
            .is_some_and(|length| length != checkpoint.size_bytes)
        {
            return Err(Error::new(
                ErrorKind::Other,
                "checkpoint download size does not match its metadata",
            ));
        }
        let mut received = 0u64;
        on_progress(0, checkpoint.size_bytes);
        runtime.block_on(async {
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut cancelled) => return Err(stopped()),
                    chunk = response.chunk() => chunk.map_err(|error| Error::new(
                        if error.is_timeout() { ErrorKind::Timeout } else { ErrorKind::Connection },
                        "checkpoint download was interrupted"))?,
                };
                let Some(chunk) = chunk else {
                    break;
                };
                if chunk.len() as u64 > checkpoint.size_bytes - received {
                    return Err(Error::new(
                        ErrorKind::Other,
                        "checkpoint download exceeds its expected size",
                    ));
                }
                staged.write_all(&chunk).map_err(io_error)?;
                received += chunk.len() as u64;
                on_progress(received, checkpoint.size_bytes);
            }
            Ok(())
        })?;
        if cancel.is_cancelled() {
            return Err(stopped());
        }
        if received != checkpoint.size_bytes {
            return Err(Error::new(
                ErrorKind::Other,
                "checkpoint download is incomplete",
            ));
        }
        staged.as_file().sync_all().map_err(io_error)?;
        if cancel.is_cancelled() {
            return Err(stopped());
        }
        staged
            .persist_noclobber(destination)
            .map_err(|error| io_error(error.error))?;
        Ok(())
    }
}
