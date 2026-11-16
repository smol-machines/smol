use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use smolmachines::kubernetes::{Kubernetes, KubernetesConfig};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Args)]
pub struct KubernetesCmd {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    /// Create and start a local cluster, keeping its data if startup fails
    Create {
        name: String,
        #[arg(long, default_value_t = 2)]
        cpus: u8,
        #[arg(long, default_value_t = 3072)]
        memory_mib: u32,
        #[arg(long, default_value_t = 8)]
        storage_gib: u64,
        /// Host API port; zero chooses an available port
        #[arg(long, default_value_t = 0)]
        api_port: u16,
        /// Seconds to wait for readiness after provisioning
        #[arg(long, default_value_t = 180)]
        timeout: u64,
    },
    /// Start a stopped cluster
    Start {
        name: String,
        #[arg(long, default_value_t = 180)]
        timeout: u64,
    },
    /// List managed clusters and their readiness as JSON
    List,
    /// Inspect one cluster without starting it
    Status { name: String },
    /// Stop compute while retaining cluster data
    Stop { name: String },
    /// Permanently delete a cluster and its persistent volumes
    Delete {
        name: String,
        /// Confirm deletion of this named cluster
        #[arg(long, required = true)]
        yes: bool,
    },
    /// Write a private kubeconfig to a new file without changing host contexts
    Kubeconfig {
        name: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Read recent server logs
    Logs {
        name: String,
        #[arg(long, default_value_t = 100)]
        lines: u32,
    },
    /// Run the bundled kubectl inside the guest (file paths refer to the guest)
    Kubectl {
        name: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
}

fn output(value: serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn start(cluster: Kubernetes, timeout: u64) -> Result<()> {
    if timeout == 0 {
        bail!("Startup timeout must be positive");
    }
    let name = cluster.name().to_owned();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let result = runtime.block_on(async move {
        let mut worker = tokio::task::spawn_blocking(move || {
            let mut status = cluster.start(Duration::from_secs(timeout), &worker_cancelled)?;
            status["endpoints"] = cluster.endpoints()?;
            Ok::<_, smolmachines::Error>(status)
        });
        tokio::select! {
            result = &mut worker => result?,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                cancelled.store(true, Ordering::Release);
                worker.await?
            }
        }
        .map_err(anyhow::Error::from)
    });
    output(result.map_err(|error| anyhow::anyhow!(
        "Cluster {name} failed to start: {error:#}; it was retained, so use 'smol kubernetes logs {name}' to inspect it or 'smol kubernetes stop {name}' to stop compute"
    ))?)
}

impl KubernetesCmd {
    pub fn run(self) -> Result<()> {
        // Keep SDK operations in this exact installation, including a renamed
        // binary selected by Studio or a CLI launched outside PATH.
        smolmachines::set_local_cli(std::env::current_exe()?)?;
        match self.command {
            Operation::Create {
                name,
                cpus,
                memory_mib,
                storage_gib,
                api_port,
                timeout,
            } => {
                if timeout == 0 {
                    bail!("Startup timeout must be positive");
                }
                start(
                    Kubernetes::create(KubernetesConfig {
                        name,
                        cpus,
                        memory_mib,
                        storage_gib,
                        api_port,
                    })?,
                    timeout,
                )
            }
            Operation::Start { name, timeout } => start(Kubernetes::attach(&name)?, timeout),
            Operation::List => {
                let rows = Kubernetes::list()?
                    .iter()
                    .map(|name| Kubernetes::attach(name)?.status())
                    .collect::<smolmachines::Result<Vec<_>>>()?;
                output(serde_json::json!(rows))
            }
            Operation::Status { name } => {
                let cluster = Kubernetes::attach(&name)?;
                let mut status = cluster.status()?;
                status["endpoints"] = cluster.endpoints()?;
                output(status)
            }
            Operation::Stop { name } => Ok(Kubernetes::attach(&name)?.stop()?),
            Operation::Delete { name, yes } => {
                if !yes {
                    bail!("Deletion requires --yes");
                }
                Ok(Kubernetes::attach(&name)?.delete()?)
            }
            Operation::Kubeconfig { name, output } => {
                Ok(Kubernetes::attach(&name)?.write_kubeconfig(output)?)
            }
            Operation::Logs { name, lines } => {
                let result = Kubernetes::attach(&name)?.logs(lines)?;
                std::io::stdout().write_all(&result.stdout)?;
                std::io::stderr().write_all(&result.stderr)?;
                if !result.success() {
                    bail!("Cluster logs are not available");
                }
                Ok(())
            }
            Operation::Kubectl {
                name,
                timeout,
                args,
            } => {
                if timeout == 0 {
                    bail!("Command timeout must be positive");
                }
                let result =
                    Kubernetes::attach(&name)?.kubectl(args, Duration::from_secs(timeout))?;
                std::io::stdout().write_all(&result.stdout)?;
                std::io::stderr().write_all(&result.stderr)?;
                if !result.success() {
                    std::process::exit(result.exit_code.clamp(1, 255));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn delete_cannot_parse_without_explicit_confirmation() {
        assert!(crate::Cli::try_parse_from(["smol", "kubernetes", "delete", "dev"]).is_err());
        assert!(
            crate::Cli::try_parse_from(["smol", "kubernetes", "delete", "dev", "--yes"]).is_ok()
        );
    }

    #[test]
    fn kubectl_flags_remain_literal_after_separator() {
        let cli = crate::Cli::try_parse_from([
            "smol",
            "kubernetes",
            "kubectl",
            "dev",
            "--",
            "get",
            "pods",
            "-A",
            "-o",
            "jsonpath={.items[*].metadata.name}",
        ])
        .unwrap();
        let crate::Commands::Kubernetes(KubernetesCmd {
            command: Operation::Kubectl { args, .. },
        }) = cli.command
        else {
            panic!("wrong command")
        };
        assert_eq!(
            args,
            [
                "get",
                "pods",
                "-A",
                "-o",
                "jsonpath={.items[*].metadata.name}"
            ]
        );
    }
}
