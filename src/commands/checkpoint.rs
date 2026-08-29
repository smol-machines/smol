//! `smol machine checkpoint` — persist one live rollback point.

use clap::Args;
use futures_util::StreamExt;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

#[derive(Args, Debug)]
pub struct CheckpointCmd {
    /// Running checkpointable machine (name, `local/name`, or `cloud/name`).
    #[arg(short = 'n', long = "name")]
    pub machine: String,

    /// Destination `.smolcheckpoint` file.
    #[arg(short, long, value_name = "PATH")]
    pub output: PathBuf,

    /// Capture on Smol Cloud.
    #[arg(long)]
    pub cloud: bool,

    /// Capture locally.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
}

impl CheckpointCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        if self.output.exists() {
            anyhow::bail!("refusing to overwrite {}", self.output.display());
        }
        let (location, handle) = resolve::route(
            Some(&self.machine),
            Target::from_flags(self.local, self.cloud)?,
        )?;
        self.machine = handle;
        match location {
            Location::Local => self.run_local(),
            Location::Cloud => self.run_cloud(),
        }
    }

    fn run_local(self) -> anyhow::Result<()> {
        let runtime = smolvm::embedded::EmbeddedRuntime::new()?;
        let options = smolvm::portable_checkpoint::CaptureOptions {
            rootfs_dir: Some(smolvm::agent::AgentManager::default_rootfs_path()?),
            ..Default::default()
        };
        let result = runtime.checkpoint_machine(&self.machine, &self.output, &options)?;
        println!(
            "Checkpointed '{}' to {} ({:.1} MiB, {:.1} ms pause, {:.1} ms total)",
            self.machine,
            self.output.display(),
            result.size_bytes as f64 / (1024.0 * 1024.0),
            result.source_pause.as_secs_f64() * 1000.0,
            result.elapsed.as_secs_f64() * 1000.0,
        );
        Ok(())
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        let output = self.output;
        super::cloud::run_cloud_command(
            Some(self.machine),
            move |http, endpoint, machine_id| async move {
                let response = http
                    .post(format!("{endpoint}/v1/machines/{machine_id}/checkpoints"))
                    .send()
                    .await?;
                let checkpoint: CloudCheckpoint =
                    super::cloud::check_response(response, "capture checkpoint")
                        .await?
                        .json()
                        .await?;
                let response = http
                    .get(format!(
                        "{endpoint}/v1/checkpoints/{}/download",
                        checkpoint.id
                    ))
                    .send()
                    .await?;
                let response =
                    super::cloud::check_response(response, "download checkpoint").await?;
                let parent = output
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                let staged = tempfile::Builder::new()
                    .prefix(".smolcheckpoint-")
                    .tempfile_in(parent)?;
                let mut file = tokio::fs::File::from_std(staged.reopen()?);
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    file.write_all(&chunk?).await?;
                }
                file.flush().await?;
                file.sync_all().await?;
                drop(file);
                staged
                    .persist_noclobber(&output)
                    .map_err(|error| error.error)?;
                println!(
                    "Checkpointed '{}' to {} ({:.1} MiB)",
                    machine_id,
                    output.display(),
                    checkpoint.size_bytes as f64 / (1024.0 * 1024.0),
                );
                Ok(())
            },
        )
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloudCheckpoint {
    id: String,
    size_bytes: u64,
}
