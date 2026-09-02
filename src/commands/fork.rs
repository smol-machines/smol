//! smol machine branch — branch a running machine (copy-on-write RAM + disks).

use clap::Args;
use smolvm::data::network::PortMapping;

#[derive(Args, Debug)]
pub struct ForkCmd {
    /// The running, branchable source machine
    #[arg(long = "from", visible_alias = "golden", value_name = "NAME")]
    pub golden: String,

    /// Name for the new child machine
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// Make the child a new branch source so it can be branched again
    #[arg(
        long = "branchable",
        visible_aliases = ["forkable", "checkpointable"]
    )]
    pub forkable: bool,

    /// Pin the child's inbound port forwards (repeatable). Without this, the
    /// source's forwards are remapped to freshly-allocated host ports.
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:GUEST")]
    pub port: Vec<PortMapping>,

    /// Branch on the cloud control plane instead of locally.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local branch. Equivalent to a `local/` prefix on the source.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,

    /// Share the source's loaded CUDA weights with this child instead of
    /// copying them. Use only when base weights remain frozen.
    #[arg(long)]
    pub share_weights: bool,
}

fn cloud_branch_body(
    clone_name: &str,
    ports: &[PortMapping],
    share_weights: bool,
    forkable: bool,
) -> serde_json::Value {
    let ports: Vec<serde_json::Value> = ports
        .iter()
        .map(|port| serde_json::json!({ "port": port.guest, "hostPort": port.host }))
        .collect();
    serde_json::json!({
        "name": clone_name,
        "ports": ports,
        "shareWeights": share_weights,
        "branchable": forkable,
    })
}

impl ForkCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        if self.forkable && self.share_weights {
            anyhow::bail!(
                "branchable CUDA descendants are not supported; drop either \
                 `--branchable` or `--share-weights`"
            );
        }
        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, golden_handle) = resolve::route(Some(&self.golden), target)?;
        self.golden = golden_handle;
        if location == Location::Cloud {
            return self.run_cloud();
        }

        let pinned = PortMapping::to_tuples(&self.port);
        let db = smolvm::db::SmolvmDb::open()?;
        let runtime = smolvm::embedded::EmbeddedRuntime::with_db(db.clone());
        if self.forkable {
            runtime.fork_checkpointable_machine_detached(&self.golden, &self.name, &pinned)?;
        } else {
            runtime.fork_machine_detached(&self.golden, &self.name, &pinned, self.share_weights)?;
        }

        let clone = db
            .get_vm(&self.name)?
            .ok_or_else(|| anyhow::anyhow!("child record disappeared after branch"))?;
        for (host, guest) in &clone.ports {
            eprintln!("  port {host}->{guest}");
        }
        eprintln!(
            "Branched '{}' -> '{}' (PID {}).",
            self.golden,
            self.name,
            clone.pid.unwrap_or(0)
        );
        Ok(())
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        let clone_name = self.name.clone();
        let body = cloud_branch_body(&clone_name, &self.port, self.share_weights, self.forkable);
        super::cloud::run_cloud_command(
            Some(self.golden),
            move |http, endpoint, golden_id| async move {
                eprintln!("Branching {golden_id} -> {clone_name}...");
                let mut resp = http
                    .post(format!("{}/v1/machines/{}/branches", endpoint, golden_id))
                    .json(&body)
                    .send()
                    .await?;
                if resp.status().as_u16() == 404 {
                    // Compatibility with control planes from before branch routes.
                    let mut legacy_body = body.clone();
                    if let Some(branchable) = legacy_body
                        .as_object_mut()
                        .and_then(|object| object.remove("branchable"))
                    {
                        legacy_body["forkable"] = branchable;
                    }
                    resp = http
                        .post(format!("{}/v1/machines/{}/fork", endpoint, golden_id))
                        .json(&legacy_body)
                        .send()
                        .await?;
                }
                match resp.status().as_u16() {
                    200 | 201 => {
                        let machine: super::cloud::CloudMachine = resp.json().await?;
                        println!(
                            "Machine '{}' ({}): {}",
                            machine.name.as_deref().unwrap_or(&clone_name),
                            machine.id,
                            machine.state
                        );
                    }
                    404 => anyhow::bail!("branch source '{}' not found", golden_id),
                    409 => {
                        let text = resp.text().await.unwrap_or_default();
                        anyhow::bail!("cannot branch source '{}': {}", golden_id, text);
                    }
                    _ => {
                        super::cloud::check_response(resp, "branch source").await?;
                    }
                }
                Ok(())
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_branch_preserves_weight_sharing_and_ports() {
        let body = cloud_branch_body("clone", &[PortMapping::new(49152, 9222)], true, true);
        assert_eq!(body["name"], "clone");
        assert_eq!(body["ports"][0]["hostPort"], 49152);
        assert_eq!(body["ports"][0]["port"], 9222);
        assert_eq!(body["shareWeights"], true);
        assert_eq!(body["branchable"], true);
    }
}
