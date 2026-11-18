//! Standard SSH clients and editors, using the engine's on-demand transport.

use anyhow::Context;
use clap::Args;

#[derive(Args, Debug)]
pub struct Configure {
    /// Machine name or local/name reference (default: "default")
    #[arg(short = 'n', long)]
    pub name: Option<String>,
    #[arg(long, conflicts_with = "cloud")]
    pub local: bool,
    #[arg(long)]
    pub cloud: bool,
}

impl Configure {
    fn connection(self) -> anyhow::Result<smolvm::ssh::Connection> {
        use super::resolve::{self, Location, Target};
        let (location, name) = resolve::route(
            self.name.as_deref(),
            Target::from_flags(self.local, self.cloud)?,
        )?;
        anyhow::ensure!(
            location == Location::Local,
            "SSH is currently available for local environments; use `smol machine shell --cloud --name {name}` for this cloud machine"
        );
        smolvm::ssh::configure(&name)
    }

    pub fn run(self) -> anyhow::Result<()> {
        println!("{}", serde_json::to_string(&self.connection()?)?);
        Ok(())
    }
}

#[derive(Args, Debug)]
pub struct Connect {
    #[command(flatten)]
    pub machine: Configure,
    /// Remote command; omit it for an interactive shell
    #[arg(last = true)]
    pub command: Vec<String>,
}

impl Connect {
    pub fn run(self) -> anyhow::Result<()> {
        let connection = self.machine.connection()?;
        let mut command = std::process::Command::new("ssh");
        command
            .arg("-F")
            .arg(connection.config)
            .arg(connection.host);
        command.args(self.command);
        // Replacing the CLI preserves SSH's exit status, signal handling, and
        // terminal job control without leaving another process behind.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Err(command.exec()).context("launch the system SSH client")
        }
        #[cfg(not(unix))]
        {
            let status = command.status().context("launch the system SSH client")?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }
}

#[derive(Args, Debug)]
pub struct Proxy {
    #[arg(long)]
    context_hex: String,
}

impl Proxy {
    pub fn run(self) -> anyhow::Result<()> {
        let name = smolvm::ssh::proxy_machine(&self.context_hex)?;
        smolvm::ssh::proxy(&name)
    }
}

#[derive(Args, Debug)]
pub struct Start {
    #[arg(long)]
    name: String,
}

impl Start {
    pub fn run(self) -> anyhow::Result<()> {
        // The generated connection is pinned to a local identity. A public
        // machine start can follow a move to the cloud; the native engine must
        // start exactly the machine whose SSH identity was verified instead.
        smolvm::ssh::start_owned(&self.name)
    }
}
