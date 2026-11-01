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

    /// Keep the source's machine-id in the child; hostname and SSH host keys
    /// are still re-minted. For tools that key a per-machine cache by
    /// /etc/machine-id (Nx), so a warm source's cache stays visible.
    #[arg(long)]
    pub keep_machine_id: bool,
}

/// A branch pins TCP host ports only; the engine gives each UDP forward
/// a fresh host port, so a `/udp` pin is refused rather than ignored.
pub(super) fn tcp_pins(ports: &[PortMapping]) -> anyhow::Result<Vec<(u16, u16)>> {
    if let Some(udp) = ports.iter().find(|port| !port.protocol.is_tcp()) {
        anyhow::bail!(
            "{} cannot be pinned: a branch pins TCP ports, and gets fresh host ports for UDP forwards automatically",
            udp.label()
        );
    }
    Ok(PortMapping::to_tuples(ports))
}

fn cloud_branch_body(
    clone_name: &str,
    ports: &[PortMapping],
    share_weights: bool,
    forkable: bool,
    keep_machine_id: bool,
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
        "keepMachineId": keep_machine_id,
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

        let pinned = tcp_pins(&self.port)?;
        let db = smolvm::db::SmolvmDb::open()?;
        let runtime = smolvm::embedded::EmbeddedRuntime::with_db(db.clone());
        runtime.fork_machine_opts(
            &self.golden,
            &self.name,
            &pinned,
            smolvm::embedded::ForkOptions {
                checkpointable: self.forkable,
                share_weights: self.share_weights,
                detached: true,
                keep_machine_id: self.keep_machine_id,
                ..Default::default()
            },
        )?;

        let clone = db
            .get_vm(&self.name)?
            .ok_or_else(|| anyhow::anyhow!("child record disappeared after branch"))?;
        for (host, guest) in &clone.ports {
            eprintln!("  port {host}->{guest}");
        }
        for (host, guest) in &clone.udp_ports {
            eprintln!("  port {host}->{guest}/udp");
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
        tcp_pins(&self.port)?;
        let clone_name = self.name.clone();
        let body = cloud_branch_body(
            &clone_name,
            &self.port,
            self.share_weights,
            self.forkable,
            self.keep_machine_id,
        );
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
        let body = cloud_branch_body("clone", &[PortMapping::new(49152, 9222)], true, true, false);
        assert_eq!(body["name"], "clone");
        assert_eq!(body["ports"][0]["hostPort"], 49152);
        assert_eq!(body["ports"][0]["port"], 9222);
        assert_eq!(body["shareWeights"], true);
        assert_eq!(body["branchable"], true);
        assert_eq!(body["keepMachineId"], false);
    }

    #[test]
    fn cloud_branch_forwards_keep_machine_id() {
        let body = cloud_branch_body("clone", &[], false, false, true);
        assert_eq!(body["keepMachineId"], true);
    }
}
