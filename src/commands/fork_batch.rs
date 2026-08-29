//! `smol machine branch-batch` — fan out many children from one live state.

use clap::Args;
use smolvm::data::network::PortMapping;
use std::collections::HashSet;

#[derive(Args, Debug)]
pub struct ForkBatchCmd {
    /// Running, forkable source machine.
    #[arg(long, visible_alias = "from", value_name = "NAME")]
    pub golden: String,

    /// Number of children to create (1..=64).
    #[arg(long, conflicts_with = "names")]
    pub count: Option<usize>,

    /// Explicit child name (repeatable instead of --count).
    #[arg(
        short = 'n',
        long = "name",
        value_name = "NAME",
        conflicts_with = "count"
    )]
    pub names: Vec<String>,

    /// Prefix for generated names (`PREFIX-1`, `PREFIX-2`, ...).
    #[arg(long, requires = "count")]
    pub name_prefix: Option<String>,

    /// Maximum concurrent local child boots.
    #[arg(long, default_value_t = 8)]
    pub parallel: usize,

    /// Pin inbound port forwards on every child (repeatable).
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:GUEST")]
    pub port: Vec<PortMapping>,

    /// Branch on the cloud control plane instead of locally.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local batch branch.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
}

impl ForkBatchCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        let (location, handle) = resolve::route(
            Some(&self.golden),
            Target::from_flags(self.local, self.cloud)?,
        )?;
        self.golden = handle;
        let names = self.resolved_names()?;
        if location == Location::Cloud {
            return self.run_cloud(names);
        }

        let pinned = PortMapping::to_tuples(&self.port);
        let db = smolvm::db::SmolvmDb::open()?;
        let runtime = smolvm::embedded::EmbeddedRuntime::with_db(db.clone());
        runtime.fork_machines_detached(&self.golden, &names, &pinned, self.parallel)?;
        for name in &names {
            let child = db
                .get_vm(name)?
                .ok_or_else(|| anyhow::anyhow!("child record '{name}' disappeared after branch"))?;
            println!("{}\tPID {}", name, child.pid.unwrap_or(0));
        }
        eprintln!("Branched {} machines from '{}'.", names.len(), self.golden);
        Ok(())
    }

    fn resolved_names(&self) -> anyhow::Result<Vec<String>> {
        let names = if self.names.is_empty() {
            let count = self
                .count
                .ok_or_else(|| anyhow::anyhow!("provide --count or at least one --name"))?;
            if !(1..=64).contains(&count) {
                anyhow::bail!("--count must be between 1 and 64");
            }
            (1..=count)
                .map(|index| {
                    format!(
                        "{}-{index}",
                        self.name_prefix.as_deref().unwrap_or("branch")
                    )
                })
                .collect()
        } else {
            self.names.clone()
        };
        if names.len() > 64 {
            anyhow::bail!("a branch batch may contain at most 64 machines");
        }
        if !(1..=64).contains(&self.parallel) {
            anyhow::bail!("--parallel must be between 1 and 64");
        }
        let mut unique = HashSet::with_capacity(names.len());
        if let Some(duplicate) = names.iter().find(|name| !unique.insert(name.as_str())) {
            anyhow::bail!("duplicate clone name '{duplicate}'");
        }
        Ok(names)
    }

    fn run_cloud(self, names: Vec<String>) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "names": names,
            "ports": self.port.iter().map(|port| {
                serde_json::json!({ "port": port.guest, "hostPort": port.host })
            }).collect::<Vec<_>>(),
        });
        super::cloud::run_cloud_command(
            Some(self.golden),
            move |http, endpoint, golden_id| async move {
                let mut response = http
                    .post(format!("{endpoint}/v1/machines/{golden_id}/branches/batch"))
                    .json(&body)
                    .send()
                    .await?;
                if response.status().as_u16() == 404 {
                    // Compatibility with control planes from before branch routes.
                    response = http
                        .post(format!("{endpoint}/v1/machines/{golden_id}/fork-batch"))
                        .json(&body)
                        .send()
                        .await?;
                }
                let batch: CloudForkBatch =
                    super::cloud::check_response(response, "branch machine batch")
                        .await?
                        .json()
                        .await?;
                for child in &batch.clones {
                    println!(
                        "{}\t{}\t{}",
                        child.name.as_deref().unwrap_or("<unnamed>"),
                        child.id,
                        child.state
                    );
                }
                eprintln!(
                    "Branched {} machines from '{}'.",
                    batch.clones.len(),
                    golden_id
                );
                Ok(())
            },
        )
    }
}

#[derive(serde::Deserialize)]
struct CloudForkBatch {
    clones: Vec<super::cloud::CloudMachine>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> ForkBatchCmd {
        ForkBatchCmd {
            golden: "base".into(),
            count: Some(3),
            names: Vec::new(),
            name_prefix: Some("episode".into()),
            parallel: 8,
            port: Vec::new(),
            cloud: false,
            local: true,
        }
    }

    #[test]
    fn resolves_count_to_stable_names() {
        assert_eq!(
            command().resolved_names().unwrap(),
            ["episode-1", "episode-2", "episode-3"]
        );
    }

    #[test]
    fn rejects_duplicate_explicit_names() {
        let mut cmd = command();
        cmd.count = None;
        cmd.names = vec!["same".into(), "same".into()];
        assert!(cmd
            .resolved_names()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }
}
