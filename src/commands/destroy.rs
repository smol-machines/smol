//! smol destroy — destroy a deployed app on smolfleet.

use clap::Args;

#[derive(Args, Debug)]
pub struct DestroyCmd {
    /// Machine name or ID to destroy
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,
}

impl DestroyCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let display_name = self.name.clone();
        super::cloud::run_cloud_command(Some(self.name), |http, endpoint, id| async move {
            eprintln!("Destroying {} ({})...", display_name, id);

            let resp = http
                .delete(format!("{}/v1/machines/{}", endpoint, id))
                .send()
                .await?;

            match resp.status().as_u16() {
                200 | 204 => eprintln!("Destroyed: {}", display_name),
                404 => anyhow::bail!("machine '{}' not found", display_name),
                _ => {
                    super::cloud::check_response(resp, "destroy").await?;
                }
            }

            Ok(())
        })
    }
}

