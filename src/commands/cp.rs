//! smol machine cp — copy files between host and machine.
//!
//! Targets a local VM by default (over the agent vsock), or a deployed cloud
//! machine with `--cloud` (over the smolfleet files API). The environment is
//! explicit — matching `smol machine exec/start/stop/status/logs --cloud` — so a
//! copy never lands in the wrong place by surprise:
//!   smol machine cp ./index.html devvm:/srv/index.html              # local VM
//!   smol machine cp --cloud ./index.html webdemo2:/usr/share/nginx/html/index.html  # cloud

use super::{cloud, common};
use clap::Args;

#[derive(Args, Debug)]
pub struct CpCmd {
    /// Source (local path or machine:path)
    #[arg(value_name = "SRC")]
    pub src: String,

    /// Destination (local path or machine:path)
    #[arg(value_name = "DST")]
    pub dst: String,

    /// Copy to/from a deployed cloud machine (by name or ID) instead of a local
    /// VM. Usually unnecessary — the machine's location is resolved
    /// automatically; equivalent to a `cloud/` prefix on the machine ref.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local machine. Equivalent to a `local/` prefix on the machine ref.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
}

impl CpCmd {
    pub fn run(self) -> anyhow::Result<()> {
        // Parse src/dst to determine direction
        let (machine_name, guest_path, local_path, is_upload) =
            if let Some((name, path)) = self.src.split_once(':') {
                // Download: machine:path -> local
                (name.to_string(), path.to_string(), self.dst.clone(), false)
            } else if let Some((name, path)) = self.dst.split_once(':') {
                // Upload: local -> machine:path
                (name.to_string(), path.to_string(), self.src.clone(), true)
            } else {
                anyhow::bail!(
                    "one of SRC or DST must use machine:path syntax (e.g., myvm:/workspace/file)"
                );
            };

        // The machine's location is resolved from its ref (+ optional
        // --local/--cloud): a name that is exclusively a cloud machine routes to
        // the smolfleet files API, everything else to the local VM. A `local/` or
        // `cloud/` prefix on the machine ref forces the choice.
        use super::resolve::{self, Location, Target};
        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, handle) = resolve::route(Some(&machine_name), target)?;
        if location == Location::Cloud {
            return cp_cloud(&handle, &guest_path, &local_path, is_upload);
        }

        let (manager, mut client) = common::ensure_connected(&handle)?;
        manager.detach();
        if is_upload {
            // Stream the file in chunks so large files don't get read entirely
            // into memory or exceed the protocol frame cap.
            let file = std::fs::File::open(&local_path)
                .map_err(|e| anyhow::anyhow!("{}: {}", local_path, e))?;
            let size = file
                .metadata()
                .map_err(|e| anyhow::anyhow!("{}: {}", local_path, e))?
                .len();
            client.write_file_from_reader(&guest_path, file, size, None)?;
            eprintln!("Uploaded {} ({} bytes) -> {}", local_path, size, guest_path);
        } else {
            // Stream chunks straight to disk so large files don't get buffered
            // entirely in memory.
            let size =
                client.read_file_to_path(&guest_path, std::path::Path::new(&local_path), |_| {})?;
            eprintln!(
                "Downloaded {} ({} bytes) -> {}",
                guest_path, size, local_path
            );
        }
        Ok(())
    }
}

/// Copy to/from a deployed cloud machine via the smolfleet files API
/// (`PUT|GET /v1/machines/{id}/files/{path}`).
fn cp_cloud(
    machine_name: &str,
    guest_path: &str,
    local_path: &str,
    is_upload: bool,
) -> anyhow::Result<()> {
    let guest_path = guest_path.to_string();
    let local_path = local_path.to_string();
    let label = machine_name.to_string();
    cloud::run_cloud_command(
        Some(machine_name.to_string()),
        move |http, endpoint, id| async move {
            // The files route captures everything after `/files/`; it stores the
            // path under the container root (a leading slash is redundant).
            let rel = guest_path.trim_start_matches('/');
            let url = format!(
                "{}/v1/machines/{}/files/{}",
                endpoint.trim_end_matches('/'),
                id,
                rel
            );
            if is_upload {
                // Stream the file straight from disk into the request body so a
                // large upload is never read wholly into RAM (mirrors the local
                // path's chunked `write_file_from_reader`).
                let meta = tokio::fs::metadata(&local_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}: {}", local_path, e))?;
                if meta.is_dir() {
                    anyhow::bail!("{}: is a directory (cp copies a single file)", local_path);
                }
                let size = meta.len();
                let file = tokio::fs::File::open(&local_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}: {}", local_path, e))?;
                let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
                // Set Content-Length explicitly (a streamed body has unknown length
                // and would otherwise be chunked) to preserve the prior semantics.
                let resp = http
                    .put(&url)
                    .header(reqwest::header::CONTENT_LENGTH, size)
                    .body(body)
                    .send()
                    .await?;
                cloud::check_response(resp, "upload file to machine").await?;
                eprintln!("Uploaded {local_path} ({size} bytes) -> {label}:{guest_path}");
            } else {
                // Refuse to clobber a directory target, mirroring local `cp`.
                if std::path::Path::new(&local_path).is_dir() {
                    anyhow::bail!(
                        "{}: is a directory (specify a destination file path)",
                        local_path
                    );
                }
                let resp = http.get(&url).send().await?;
                let resp = cloud::check_response(resp, "download file from machine").await?;
                // Stream the response body chunk-by-chunk to disk so a large
                // download is never buffered entirely in memory.
                let size = write_chunks_to_file(resp.bytes_stream(), &local_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}: {}", local_path, e))?;
                eprintln!("Downloaded {label}:{guest_path} ({size} bytes) -> {local_path}");
            }
            Ok(())
        },
    )
}

/// Drain a byte stream to `path`, returning the number of bytes written.
///
/// Writes each chunk straight to the file as it arrives so a large download is
/// never held wholly in memory. Generic over the chunk/error type so it works
/// with `reqwest::Response::bytes_stream()` (chunks are `bytes::Bytes`, errors
/// `reqwest::Error`) and is unit-testable with an in-memory stream.
async fn write_chunks_to_file<S, B, E>(mut stream: S, path: &str) -> anyhow::Result<u64>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: Into<anyhow::Error>,
{
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::File::create(path).await?;
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Into::into)?;
        let bytes = chunk.as_ref();
        file.write_all(bytes).await?;
        written += bytes.len() as u64;
    }
    file.flush().await?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The streamed-download path round-trips a multi-chunk body to disk without
    /// buffering it whole (the R2-C4 fix). Exercises the exact helper the cloud
    /// download uses, fed an in-memory chunk stream standing in for `bytes_stream`.
    #[tokio::test]
    async fn write_chunks_round_trips_multi_chunk_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin").to_string_lossy().into_owned();

        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![
            Ok(b"hello ".to_vec()),
            Ok(b"streamed ".to_vec()),
            Ok(b"world".to_vec()),
        ];
        let stream = futures_util::stream::iter(chunks);

        let written = write_chunks_to_file(stream, &path).await.unwrap();
        assert_eq!(written, 20);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello streamed world");
    }

    /// A mid-stream error propagates instead of leaving a silently-truncated file
    /// look like success.
    #[tokio::test]
    async fn write_chunks_propagates_stream_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("err.bin").to_string_lossy().into_owned();

        let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
            vec![Ok(b"partial".to_vec()), Err(std::io::Error::other("boom"))];
        let stream = futures_util::stream::iter(chunks);

        let err = write_chunks_to_file(stream, &path).await.unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    /// The upload side streams from a tokio file reader; confirm the reader-stream
    /// reassembles to the original bytes (i.e. nothing is dropped/duplicated).
    #[tokio::test]
    async fn reader_stream_reassembles_file() {
        use futures_util::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.bin");
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
        std::fs::write(&path, &payload).unwrap();

        let file = tokio::fs::File::open(&path).await.unwrap();
        let mut stream = tokio_util::io::ReaderStream::new(file);
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, payload);
    }
}
