use clap::Args;

#[derive(Args, Debug)]
pub struct SavedExecutionCmd {
    #[arg(short = 'n', long)]
    pub name: String,
    #[arg(long, conflicts_with = "local")]
    pub cloud: bool,
    #[arg(long)]
    pub local: bool,
    /// Return the local machine identity and saved checkpoint path as JSON.
    #[arg(long, requires = "local")]
    pub json: bool,
}

impl SavedExecutionCmd {
    pub fn run(self, resume: bool) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};
        // Pausing a moved machine pauses it where it runs now; resuming one
        // brings the paused copy here back.
        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, name) = if resume {
            resolve::route_here(Some(&self.name), target)?
        } else {
            resolve::route(Some(&self.name), target)?
        };
        let operation = if resume { "resume" } else { "pause" };
        if self.json && location == Location::Cloud {
            anyhow::bail!("structured saved execution is available for local machines only");
        }
        if location == Location::Cloud {
            return super::cloud::run_cloud_command(
                Some(name),
                move |http, endpoint, id| async move {
                    let response = http
                        .post(format!("{endpoint}/v1/machines/{id}/{operation}"))
                        .timeout(std::time::Duration::from_secs(1800))
                        .send()
                        .await?;
                    super::cloud::check_response(response, operation).await?;
                    Ok(())
                },
            );
        }
        let runtime = smolvm::embedded::EmbeddedRuntime::new()?;
        if resume {
            runtime.resume_machine_detached(&name)?;
            if let Some(moved) = super::moved::clear(&name)? {
                eprintln!(
                    "note: '{name}' runs here again, and its cloud copy cloud/{} ({}) is still \
                     running; stop it with `smol machine stop --name cloud/{}` if you no longer need it.",
                    moved.name, moved.id, moved.name
                );
            }
        } else {
            runtime.pause_machine(&name)?;
        }
        if self.json {
            let record = runtime
                .list_machines()?
                .into_iter()
                .find(|record| record.name == name)
                .ok_or_else(|| anyhow::anyhow!("machine disappeared after {operation}"))?;
            if !resume && record.paused_checkpoint.is_none() {
                anyhow::bail!("machine paused without a saved checkpoint path");
            }
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "name": record.name,
                    "created_at": record.created_at,
                    "state": record.state.to_string(),
                    "checkpoint": record.paused_checkpoint,
                }))?
            );
            return Ok(());
        }
        println!(
            "Machine '{name}' {}",
            if resume { "resumed" } else { "paused" }
        );
        Ok(())
    }
}
