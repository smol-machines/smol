use clap::Args;

#[derive(Args, Debug)]
pub struct SavedExecutionCmd {
    #[arg(short = 'n', long)]
    pub name: String,
    #[arg(long, conflicts_with = "local")]
    pub cloud: bool,
    #[arg(long)]
    pub local: bool,
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
        println!(
            "Machine '{name}' {}",
            if resume { "resumed" } else { "paused" }
        );
        Ok(())
    }
}
