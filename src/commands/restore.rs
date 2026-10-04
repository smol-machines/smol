//! `smol machine restore` — rehydrate a durable rollback point.

use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct RestoreCmd {
    /// Local `.smolcheckpoint` path or durable cloud checkpoint ID.
    #[arg(long, value_name = "PATH_OR_ID")]
    pub checkpoint: String,

    /// Name for the restored machine.
    #[arg(short, long)]
    pub name: String,

    /// Restore from Smol Cloud.
    #[arg(long)]
    pub cloud: bool,

    /// Restore a local artifact.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,

    /// Give a cloud machine outbound network access (blocked by default).
    #[arg(long)]
    pub net: bool,
}

impl RestoreCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let path = PathBuf::from(&self.checkpoint);
        let is_file = looks_like_local_checkpoint(&path);
        let local = self.local || (!self.cloud && is_file);
        // A file restored on the cloud is uploaded first.
        let checkpoint = if self.cloud && is_file {
            super::cloud::upload_checkpoint_file(&path)?.id
        } else {
            self.checkpoint.clone()
        };
        if local {
            let runtime = smolvm::embedded::EmbeddedRuntime::new()?;
            runtime.restore_checkpoint_machine_detached(&self.name, &path)?;
            println!(
                "Restored {} as '{}' (running and forkable)",
                path.display(),
                self.name
            );
            return Ok(());
        }

        let name = self.name;
        let net = self.net;
        let (http, cloud) = super::cloud::cloud_client()?;
        let endpoint = cloud.endpoint()?.to_string();
        tokio::runtime::Runtime::new()?.block_on(async move {
            let response = http
                .post(format!("{endpoint}/v1/checkpoints/{checkpoint}/restore"))
                .json(&super::cloud::restore_body(&name, net))
                .send()
                .await?;
            let machine: super::cloud::CloudMachine =
                super::cloud::check_response(response, "restore checkpoint")
                    .await?
                    .json()
                    .await?;
            let response = http
                .post(format!("{endpoint}/v1/machines/{}/start", machine.id))
                .query(&[("wait_ready", "true")])
                .timeout(std::time::Duration::from_secs(
                    super::common::HTTP_LONG_OPERATION_TIMEOUT_SECS,
                ))
                .send()
                .await?;
            super::cloud::check_response(response, "start restored machine").await?;
            println!(
                "Restored {} as '{}' ({}, running and forkable)",
                checkpoint,
                machine.name.as_deref().unwrap_or(&name),
                machine.id,
            );
            Ok(())
        })
    }
}

fn looks_like_local_checkpoint(path: &std::path::Path) -> bool {
    path.is_file()
        || path
            .extension()
            .is_some_and(|extension| extension == "smolcheckpoint" || extension == "checkpoint")
        || path.is_absolute()
        || path.components().count() > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_artifact_path_still_routes_locally() {
        assert!(looks_like_local_checkpoint(std::path::Path::new(
            "missing.smolcheckpoint"
        )));
        assert!(looks_like_local_checkpoint(std::path::Path::new(
            "./missing"
        )));
        assert!(!looks_like_local_checkpoint(std::path::Path::new(
            "chk_123"
        )));
    }
}
