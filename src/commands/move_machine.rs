//! `smol machine move` — move a running local machine to Smol Cloud.

use clap::Args;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct MoveCmd {
    /// Running local machine to move.
    #[arg(short = 'n', long = "name")]
    pub machine: String,

    /// Name for the cloud machine (default: the local name).
    #[arg(long = "as", value_name = "NAME")]
    pub cloud_name: Option<String>,

    /// Give the cloud machine outbound network access (default: the same as
    /// the local machine had).
    #[arg(long, conflicts_with = "no_net")]
    pub net: bool,

    /// Restore the cloud machine with networking blocked.
    #[arg(long)]
    pub no_net: bool,

    /// Keep the local machine running; the cloud gets a copy as of now.
    #[arg(long)]
    pub keep_local: bool,
}

impl MoveCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let runtime = smolvm::embedded::EmbeddedRuntime::new()?;
        let name = self.machine.as_str();
        let cloud_name = self.cloud_name.clone().unwrap_or_else(|| name.to_string());

        // The checkpoint the cloud resumes. Moving pauses the local machine
        // at that exact point, so nothing it does afterwards is lost and a
        // failed move can resume it right where it was. A copy captures it
        // while it keeps running.
        let staged = tempfile::Builder::new().prefix("smol-move-").tempdir()?;
        let artifact = if self.keep_local {
            let path = staged.path().join(format!("{name}.checkpoint"));
            eprintln!("Checkpointing '{name}'...");
            let options = smolvm::portable_checkpoint::CaptureOptions {
                rootfs_dir: Some(smolvm::agent::AgentManager::default_rootfs_path()?),
                ..Default::default()
            };
            runtime.checkpoint_machine(name, &path, &options)?;
            path
        } else {
            eprintln!("Pausing '{name}' and saving its execution...");
            runtime.pause_machine(name)?;
            runtime
                .list_machines()?
                .into_iter()
                .find(|record| record.name == name)
                .and_then(|record| record.paused_checkpoint)
                .ok_or_else(|| anyhow::anyhow!("'{name}' paused without a saved checkpoint"))?
        };

        let net = if self.net {
            true
        } else if self.no_net {
            false
        } else {
            had_network(&artifact)
        };
        let moved = upload_and_resume(&artifact, &cloud_name, net);
        let (checkpoint_id, machine_id) = match moved {
            Ok(ids) => ids,
            Err(error) if !self.keep_local => {
                anyhow::bail!(
                    "{error:#}\n'{name}' is paused locally with its execution saved; \
                     `smol machine resume --name {name}` continues it on this computer"
                );
            }
            Err(error) => return Err(error),
        };
        drop(staged);
        println!(
            "Moved '{name}' to Smol Cloud as '{cloud_name}' ({machine_id}), from checkpoint {checkpoint_id}"
        );
        if self.keep_local {
            println!("'{name}' is still running here; the cloud copy is cloud/{cloud_name}.");
        } else {
            super::moved::record(name, &cloud_name, &machine_id)?;
            println!(
                "Commands on '{name}' now reach the cloud machine. The paused copy here is \
                 'local/{name}': `smol machine resume --name {name}` runs it here again, \
                 `smol machine rm --name {name}` removes it."
            );
        }
        Ok(())
    }
}

/// Upload a checkpoint file and resume it as a running cloud machine.
/// Returns the checkpoint and machine ids.
fn upload_and_resume(
    artifact: &std::path::Path,
    cloud_name: &str,
    net: bool,
) -> anyhow::Result<(String, String)> {
    let checkpoint = super::cloud::upload_checkpoint_file(artifact)?;
    let client = super::cloud::cloud_api()?;
    let network = smol_cloud::types::Network {
        mode: Some(if net { "open" } else { "blocked" }.to_string()),
        cidrs: Vec::new(),
        hosts: Vec::new(),
    };
    eprintln!("Resuming it in the cloud as '{cloud_name}'...");
    let machine = client.restore_checkpoint_with_network(&checkpoint.id, cloud_name, &network)?;
    client.start(&machine.id, false)?;
    client.wait_until_ready(
        &machine.id,
        Duration::from_secs(10 * 60),
        Duration::from_secs(1),
    )?;
    Ok((checkpoint.id, machine.id))
}

/// Whether the checkpointed machine had network access, from its manifest.
fn had_network(artifact: &std::path::Path) -> bool {
    smolvm_pack::packer::read_manifest_from_sidecar(artifact)
        .ok()
        .and_then(|manifest| manifest.checkpoint)
        .and_then(|checkpoint| checkpoint.network)
        .is_some_and(|network| network.enabled)
}
