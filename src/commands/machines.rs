//! smol machines — list deployed apps on smolfleet.

use super::cloud;
use clap::Args;

#[derive(Args, Debug)]
pub struct MachinesCmd;

impl MachinesCmd {
    pub fn run(self) -> anyhow::Result<()> {
        // Phase 1 (sync): resolve credentials BEFORE entering any runtime.
        // cloud_client() does a silent token refresh with Runtime::new() +
        // block_on internally — calling it inside an active runtime panics.
        let (http, cloud_config) = cloud::cloud_client()?;
        let endpoint = cloud_config.endpoint()?.to_string();

        // Phase 2 (async): network I/O.
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(Self::run_async(http, endpoint))
    }

    async fn run_async(http: reqwest::Client, endpoint: String) -> anyhow::Result<()> {
        let machines = cloud::list_machines(&http, &endpoint).await?;

        if machines.is_empty() {
            eprintln!("No machines deployed.");
            return Ok(());
        }

        println!(
            "{:<36} {:<20} {:<25} {:<12} {:<20}",
            "ID", "NAME", "SOURCE", "STATE", "UPDATED"
        );

        for m in &machines {
            let source = m
                .source
                .as_ref()
                .and_then(|s| s.reference.as_deref())
                .unwrap_or("-");
            let updated = m.updated_at.as_deref().unwrap_or("-");
            let updated_short = updated.split('T').next().unwrap_or(updated);

            println!(
                "{:<36} {:<20} {:<25} {:<12} {:<20}",
                m.id,
                m.name.as_deref().unwrap_or("-"),
                source,
                m.state,
                updated_short
            );
        }

        Ok(())
    }
}
