use super::*;
use futures_util::TryStreamExt;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[cfg(test)]
mod tests;

fn cancelled() -> Error {
    Error::new(ErrorKind::Cancelled, "checkpoint upload stopped")
}

impl Client {
    /// Send the parts of an allocated upload without completing it. The file
    /// must remain unchanged from allocation through completion. On failure,
    /// the caller retains the checkpoint ID for inspection or cleanup.
    pub fn send_checkpoint_upload(
        &self,
        path: &std::path::Path,
        upload: &CheckpointUpload,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> Result<()> {
        self.send_checkpoint_upload_cancellable(path, upload, on_progress, &StreamCancel::new())
    }

    /// Upload with a per-operation stop token. Cancellation interrupts part
    /// requests and retry waits, and all upload tasks are joined before return.
    /// This never completes or deletes the allocation. A caller may inspect or
    /// delete it after return; already accepted object bytes can still exist.
    pub fn send_checkpoint_upload_cancellable(
        &self,
        path: &std::path::Path,
        upload: &CheckpointUpload,
        on_progress: &mut dyn FnMut(u64, u64),
        cancel: &StreamCancel,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let metadata = std::fs::metadata(path)
            .map_err(|e| Error::new(ErrorKind::Other, format!("read {}: {e}", path.display())))?;
        let size = metadata.len();
        if !metadata.is_file() || size == 0 || size != upload.checkpoint.size_bytes {
            return Err(Error::new(
                ErrorKind::Other,
                "checkpoint file size changed since upload allocation",
            ));
        }
        if upload.part_size_bytes == 0
            || u64::try_from(upload.upload_urls.len()).ok()
                != Some(size.div_ceil(upload.part_size_bytes))
        {
            return Err(Error::new(
                ErrorKind::Other,
                "checkpoint upload has an invalid part layout",
            ));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::new(ErrorKind::Other, format!("start upload runtime: {e}")))?;
        let sent = Arc::new(AtomicU64::new(0));
        let mut stopped = cancel.subscribe();
        let result = runtime.block_on(async {
            let mut tasks = tokio::task::JoinSet::new();
            let mut next = 0;
            let mut progress = tokio::time::interval(Duration::from_millis(250));
            let result = loop {
                if cancel.is_cancelled() {
                    break Err(cancelled());
                }
                while tasks.len() < UPLOAD_PARALLELISM && next < upload.upload_urls.len() {
                    let start = next as u64 * upload.part_size_bytes;
                    let len = upload.part_size_bytes.min(size - start);
                    let http = self.stream_http.clone();
                    let path = path.to_owned();
                    let url = upload.upload_urls[next].clone();
                    let sent = Arc::clone(&sent);
                    tasks.spawn(async move { put_part(http, path, url, start, len, sent).await });
                    next += 1;
                }
                if tasks.is_empty() {
                    break Ok(());
                }
                tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut stopped) => break Err(cancelled()),
                    result = tasks.join_next() => match result {
                        Some(Ok(Ok(()))) => {},
                        Some(Ok(Err(e))) => break Err(e),
                        Some(Err(_)) => break Err(Error::new(ErrorKind::Other, "checkpoint upload worker failed")),
                        None => {},
                    },
                    _ = progress.tick() => on_progress(sent.load(Ordering::SeqCst).min(size), size),
                }
            };
            // Dropping the request futures closes their bodies/connections;
            // wait for every task to stop before the caller can begin cleanup.
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            result
        });
        result?;
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        on_progress(size, size);
        Ok(())
    }
}

async fn put_part(
    http: reqwest::Client,
    path: std::path::PathBuf,
    url: String,
    start: u64,
    len: u64,
    sent: Arc<AtomicU64>,
) -> Result<()> {
    let mut last = None;
    for attempt in 0..UPLOAD_PART_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
        }
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|e| Error::new(ErrorKind::Other, format!("open {}: {e}", path.display())))?;
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| Error::new(ErrorKind::Other, format!("read the checkpoint: {e}")))?;
        let counted = Arc::new(AtomicU64::new(0));
        let read_count = Arc::clone(&counted);
        let read_sent = Arc::clone(&sent);
        let body = tokio_util::io::ReaderStream::new(file.take(len)).inspect_ok(move |bytes| {
            read_count.fetch_add(bytes.len() as u64, Ordering::SeqCst);
            read_sent.fetch_add(bytes.len() as u64, Ordering::SeqCst);
        });
        let result = http
            .put(&url)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(reqwest::Body::wrap_stream(body))
            .timeout(UPLOAD_PART_TIMEOUT)
            .send()
            .await;
        let counted = counted.load(Ordering::SeqCst);
        match result {
            Ok(response) if response.status().is_success() => {
                sent.fetch_add(len - counted, Ordering::SeqCst);
                return Ok(());
            }
            Ok(response) => {
                sent.fetch_sub(counted, Ordering::SeqCst);
                // Object URLs are credentials; don't put a signed URL or the
                // storage server's response body in a public error message.
                last = Some(Error::new(
                    ErrorKind::Connection,
                    format!(
                        "upload part at byte {start} failed: HTTP {}",
                        response.status()
                    ),
                ));
            }
            Err(_) => {
                sent.fetch_sub(counted, Ordering::SeqCst);
                last = Some(Error::new(
                    ErrorKind::Connection,
                    format!("upload part at byte {start} failed: connection error"),
                ));
            }
        }
    }
    Err(last.expect("at least one attempt"))
}
