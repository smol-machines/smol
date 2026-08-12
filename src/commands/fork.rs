//! smol machine fork — clone a running, forkable machine (copy-on-write RAM + disks).

use clap::Args;
use smolvm::data::network::PortMapping;

#[derive(Args, Debug)]
pub struct ForkCmd {
    /// The running, forkable source machine to clone from
    #[arg(long, value_name = "NAME")]
    pub golden: String,

    /// Name for the new clone machine
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: String,

    /// (Rejected) make the clone itself forkable — nested fork is unsupported
    #[arg(long)]
    pub forkable: bool,

    /// Pin the clone's inbound port forwards (repeatable). Without this, the
    /// golden's forwards are remapped to freshly-allocated host ports.
    #[arg(short = 'p', long = "port", value_parser = PortMapping::parse, value_name = "HOST:GUEST")]
    pub port: Vec<PortMapping>,

    /// Fork on the cloud control plane instead of locally.
    #[arg(long)]
    pub cloud: bool,

    /// Force a local fork. Equivalent to a `local/` prefix on the golden.
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,

    /// Share the golden's loaded CUDA weights with this clone instead of
    /// copying them. Use only when base weights remain frozen.
    #[arg(long)]
    pub share_weights: bool,
}

fn cloud_fork_body(
    clone_name: &str,
    ports: &[PortMapping],
    share_weights: bool,
) -> serde_json::Value {
    let ports: Vec<serde_json::Value> = ports
        .iter()
        .map(|port| serde_json::json!({ "port": port.guest, "hostPort": port.host }))
        .collect();
    serde_json::json!({
        "name": clone_name,
        "ports": ports,
        "shareWeights": share_weights,
    })
}

impl ForkCmd {
    pub fn run(mut self) -> anyhow::Result<()> {
        use super::resolve::{self, Location, Target};

        let target = Target::from_flags(self.local, self.cloud)?;
        let (location, golden_handle) = resolve::route(Some(&self.golden), target)?;
        self.golden = golden_handle;
        if location == Location::Cloud {
            return self.run_cloud();
        }

        if self.forkable {
            anyhow::bail!(
                "nested fork is not supported: a clone cannot be re-forked, so \
                 `--forkable` on a fork has no effect (drop it)"
            );
        }

        let pinned = PortMapping::to_tuples(&self.port);
        let db = smolvm::db::SmolvmDb::open()?;
        let runtime = smolvm::embedded::EmbeddedRuntime::with_db(db.clone());
        runtime.fork_machine_detached(&self.golden, &self.name, &pinned, self.share_weights)?;

        let clone = db
            .get_vm(&self.name)?
            .ok_or_else(|| anyhow::anyhow!("clone record disappeared after fork"))?;
        for (host, guest) in &clone.ports {
            eprintln!("  port {host}->{guest}");
        }
        eprintln!(
            "Forked '{}' -> '{}' (PID {}). Golden stays frozen as the fork base \
             (do not start it again while clones exist).",
            self.golden,
            self.name,
            clone.pid.unwrap_or(0)
        );
        Ok(())
    }

    fn run_cloud(self) -> anyhow::Result<()> {
        if self.forkable {
            anyhow::bail!(
                "nested fork is not supported: a clone cannot be re-forked, so \
                 `--forkable` on a fork has no effect (drop it)"
            );
        }
        let clone_name = self.name.clone();
        let body = cloud_fork_body(&clone_name, &self.port, self.share_weights);
        super::cloud::run_cloud_command(
            Some(self.golden),
            move |http, endpoint, golden_id| async move {
                eprintln!("Forking {golden_id} -> {clone_name}...");
                let resp = http
                    .post(format!("{}/v1/machines/{}/fork", endpoint, golden_id))
                    .json(&body)
                    .send()
                    .await?;
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
                    404 => anyhow::bail!("golden '{}' not found", golden_id),
                    409 => {
                        let text = resp.text().await.unwrap_or_default();
                        anyhow::bail!("cannot fork golden '{}': {}", golden_id, text);
                    }
                    _ => {
                        super::cloud::check_response(resp, "fork golden").await?;
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
    fn cloud_fork_preserves_weight_sharing_and_ports() {
        let body = cloud_fork_body("clone", &[PortMapping::new(49152, 9222)], true);
        assert_eq!(body["name"], "clone");
        assert_eq!(body["ports"][0]["hostPort"], 49152);
        assert_eq!(body["ports"][0]["port"], 9222);
        assert_eq!(body["shareWeights"], true);
    }
}
