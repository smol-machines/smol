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
                let bytes = std::fs::read(&local_path)
                    .map_err(|e| anyhow::anyhow!("{}: {}", local_path, e))?;
                let size = bytes.len();
                let resp = http.put(&url).body(bytes).send().await?;
                cloud::check_response(resp, "upload file to machine").await?;
                eprintln!("Uploaded {local_path} ({size} bytes) -> {label}:{guest_path}");
            } else {
                let resp = http.get(&url).send().await?;
                let resp = cloud::check_response(resp, "download file from machine").await?;
                let bytes = resp.bytes().await?;
                let size = bytes.len();
                std::fs::write(&local_path, &bytes)
                    .map_err(|e| anyhow::anyhow!("{}: {}", local_path, e))?;
                eprintln!("Downloaded {label}:{guest_path} ({size} bytes) -> {local_path}");
            }
            Ok(())
        },
    )
}
