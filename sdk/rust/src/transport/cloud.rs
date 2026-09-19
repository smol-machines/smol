//! smol cloud, over the shared [`smol_cloud`] client.

use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use smol_cloud::blocking::{Client, StreamEvent, REQUEST_TIMEOUT};
use smol_cloud::types as wire;

use super::{unsupported, ReadyOptions, Transport};
use crate::connect::Target;
use crate::error::{Error, ErrorKind, Result};
use crate::exec::{ExecEvent, ExecOptions, ExecResult, ExecStream};
use crate::machine::{
    BranchOptions, Checkpoint, CheckpointOptions, CloudCheckpoint, CostBreakdown, ImageInfo,
    MachineState, PortEndpoint, ShareLink, UsageReport, UsageTotals,
};

/// Slack over a command's own timeout, covering the round trip, so the client
/// never gives up before the server has had its chance.
const EXEC_TIMEOUT_HEADROOM: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct CloudTransport {
    client: Client,
    name: String,
    id: String,
}

impl CloudTransport {
    pub(crate) fn new(client: Client, name: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            client,
            name: name.into(),
            id: id.into(),
        }
    }

    fn from_machine(client: Client, machine: wire::Machine) -> Self {
        let name = machine.display_name().to_string();
        Self::new(client, name, machine.id)
    }

    fn command(command: Vec<String>, options: &ExecOptions) -> wire::Command {
        wire::Command {
            command,
            env: options.env.iter().cloned().collect(),
            cwd: options.workdir.clone(),
            timeout_seconds: options.timeout.map(|t| t.as_secs()),
        }
    }

    /// A command may legitimately outlive the default request window, so size
    /// the abort off the command's own timeout plus headroom — never below the
    /// default.
    fn exec_timeout(options: &ExecOptions) -> Duration {
        options
            .timeout
            .map(|t| (t + EXEC_TIMEOUT_HEADROOM).max(REQUEST_TIMEOUT))
            .unwrap_or(REQUEST_TIMEOUT)
    }

    fn ports(options: &BranchOptions) -> Vec<wire::Port> {
        options
            .ports
            .iter()
            .map(|port| wire::Port {
                port: port.guest,
                host_port: Some(port.host),
            })
            .collect()
    }
}

impl Transport for CloudTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn target(&self) -> Target {
        Target::Cloud
    }

    fn state(&self) -> MachineState {
        self.client
            .machine(&self.id)
            .map(|machine| MachineState::parse(&machine.state))
            .unwrap_or(MachineState::Unknown)
    }

    fn is_running(&self) -> bool {
        matches!(self.state(), MachineState::Running | MachineState::Started)
    }

    fn pid(&self) -> Option<i32> {
        // A cloud machine's process lives on a node you do not own.
        None
    }

    fn ready(&self) -> Result<bool> {
        Ok(self.client.machine(&self.id)?.ready == Some(true))
    }

    fn wait_until_ready(&self, options: ReadyOptions) -> Result<()> {
        Ok(self
            .client
            .wait_until_ready(&self.id, options.timeout, options.interval)?)
    }

    fn start(&self) -> Result<()> {
        self.client.start(&self.id, false)?;
        self.wait_until_ready(ReadyOptions::default())
    }

    fn start_branchable(&self) -> Result<()> {
        // Branchability is a create-time property the control plane persists;
        // this only asks the boot to use cloneable guest RAM.
        self.client.start(&self.id, true)?;
        self.wait_until_ready(ReadyOptions::default())
    }

    fn stop(&self) -> Result<()> {
        Ok(self.client.stop(&self.id)?)
    }

    fn delete(&self) -> Result<()> {
        Ok(self.client.delete(&self.id)?)
    }

    fn exec(&self, command: Vec<String>, options: ExecOptions) -> Result<ExecResult> {
        let timeout = Self::exec_timeout(&options);
        let output = self
            .client
            .exec(&self.id, &Self::command(command, &options), timeout)?;
        Ok(ExecResult {
            exit_code: output.exit_code.unwrap_or(0),
            stdout: output.stdout_bytes(),
            stderr: output.stderr_bytes(),
        })
    }

    fn exec_stream(&self, command: Vec<String>, options: ExecOptions) -> Result<ExecStream> {
        let events = self
            .client
            .exec_stream(&self.id, &Self::command(command, &options))?;
        let (tx, rx) = mpsc::channel();
        // Drain on a worker so the caller sees each chunk as it arrives rather
        // than when the command ends.
        thread::spawn(move || {
            for event in events {
                let event = match event {
                    StreamEvent::Stdout(data) => ExecEvent::Stdout(data.into_bytes()),
                    StreamEvent::Stderr(data) => ExecEvent::Stderr(data.into_bytes()),
                    StreamEvent::Exit(code) => ExecEvent::Exit(code),
                    StreamEvent::Error(message) => ExecEvent::Error(message),
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Ok(ExecStream::from_receiver(rx))
    }

    fn run(
        &self,
        _image: &str,
        _command: Vec<String>,
        _options: ExecOptions,
    ) -> Result<ExecResult> {
        Err(unsupported(
            "run(image, …)",
            "a cloud machine is created from its image; create one with that image and exec into it",
        ))
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self.client.read_file(&self.id, path)?)
    }

    fn write_file(&self, path: &str, data: Vec<u8>, mode: Option<u32>) -> Result<()> {
        self.client.write_file(&self.id, path, &data)?;
        // The files route carries no mode, so apply one with chmod when asked —
        // writing a script the caller then runs is the usual reason.
        if let Some(mode) = mode {
            self.exec(
                vec!["chmod".to_string(), format!("{mode:o}"), path.to_string()],
                ExecOptions::new(),
            )?;
        }
        Ok(())
    }

    fn pull_image(&self, _image: &str) -> Result<ImageInfo> {
        Err(unsupported(
            "pull_image()",
            "a cloud machine has no image store of its own to pull into",
        ))
    }

    fn list_images(&self) -> Result<Vec<ImageInfo>> {
        Err(unsupported(
            "list_images()",
            "a cloud machine has no image store of its own to list",
        ))
    }

    fn sync(&self) -> Result<()> {
        Err(unsupported(
            "sync()",
            "a cloud machine carries no host-directory mounts to sync back",
        ))
    }

    fn host_port(&self, guest_port: u16) -> Result<Option<u16>> {
        Ok(self
            .client
            .machine(&self.id)?
            .ports
            .into_iter()
            .find(|port| port.port == guest_port)
            .and_then(|port| port.host_port))
    }

    fn guest_ports(&self) -> Result<Vec<u16>> {
        Ok(self
            .client
            .machine(&self.id)?
            .ports
            .into_iter()
            .map(|port| port.port)
            .collect())
    }

    fn endpoint(&self, port: u16, path: &str) -> Result<PortEndpoint> {
        // Reach a published port through the control plane's authenticated
        // bridge: no tunnel, nothing exposed publicly.
        let http_url = self.client.connect_url(&self.id, port, path);
        let ws_url = http_url
            .strip_prefix("https")
            .map(|rest| format!("wss{rest}"))
            .or_else(|| {
                http_url
                    .strip_prefix("http")
                    .map(|rest| format!("ws{rest}"))
            })
            .unwrap_or_else(|| http_url.clone());
        Ok(PortEndpoint {
            http_url,
            ws_url,
            headers: vec![(
                "authorization".to_string(),
                format!("Bearer {}", self.client.credentials().api_key()),
            )],
        })
    }

    fn url(&self) -> Result<Option<String>> {
        Ok(self.client.machine(&self.id)?.url)
    }

    fn checkpoint(&self, output: Option<&Path>, _options: CheckpointOptions) -> Result<Checkpoint> {
        if output.is_some() {
            return Err(Error::new(
                ErrorKind::Config,
                "a cloud capture is stored by the control plane, not written to a path; \
                 use the returned download URL to save an artifact",
            ));
        }
        Ok(Checkpoint::Cloud(self.client.checkpoint(&self.id)?.into()))
    }

    fn checkpoints(&self) -> Result<Vec<Checkpoint>> {
        Ok(self
            .client
            .checkpoints(&self.id)?
            .into_iter()
            .map(|info| Checkpoint::Cloud(info.into()))
            .collect())
    }

    fn branch(&self, name: &str, options: &BranchOptions) -> Result<Box<dyn Transport>> {
        let clone = self.client.branch(
            &self.id,
            name,
            &Self::ports(options),
            options.checkpointable,
        )?;
        let transport = CloudTransport::from_machine(self.client.clone(), clone);
        transport.wait_until_ready(ReadyOptions::default())?;
        Ok(Box::new(transport))
    }

    fn branch_batch(
        &self,
        names: &[String],
        options: &BranchOptions,
    ) -> Result<Vec<Box<dyn Transport>>> {
        let clones = self
            .client
            .branch_batch(&self.id, names, &Self::ports(options))?;
        let mut transports: Vec<Box<dyn Transport>> = Vec::with_capacity(clones.len());
        for clone in clones {
            let transport = CloudTransport::from_machine(self.client.clone(), clone);
            transport.wait_until_ready(ReadyOptions::default())?;
            transports.push(Box::new(transport));
        }
        Ok(transports)
    }

    fn usage(&self) -> Result<UsageReport> {
        Ok(self.client.usage(&self.id)?.into())
    }

    fn delete_with_usage(&self) -> Result<UsageReport> {
        Ok(self.client.delete_with_usage(&self.id)?.into())
    }

    fn share(&self) -> Result<ShareLink> {
        let share = self.client.share(&self.id)?;
        Ok(ShareLink {
            token: share.token,
            url: share.url,
        })
    }

    fn unshare(&self) -> Result<()> {
        Ok(self.client.unshare(&self.id)?)
    }
}

impl From<wire::Checkpoint> for CloudCheckpoint {
    fn from(info: wire::Checkpoint) -> Self {
        Self {
            id: info.id,
            machine_id: info.machine_id,
            status: info.status,
            size_bytes: info.size_bytes,
            arch: info.arch,
            created_at: info.created_at,
            download_url: info.download_url,
        }
    }
}

impl From<wire::Usage> for UsageReport {
    fn from(usage: wire::Usage) -> Self {
        Self {
            machine_id: usage.machine_id,
            from: usage.from,
            to: usage.to,
            usage: UsageTotals {
                total_uptime_seconds: usage.usage.total_uptime_seconds,
                cpu_hours: usage.usage.cpu_hours,
                memory_gb_hours: usage.usage.memory_gb_hours,
                disk_gb_hours: usage.usage.disk_gb_hours,
                egress_gb: usage.usage.egress_gb,
            },
            cost: CostBreakdown {
                cpu_micros: usage.cost.cpu_micros,
                memory_micros: usage.cost.memory_micros,
                disk_micros: usage.cost.disk_micros,
                egress_micros: usage.cost.egress_micros,
                base_micros: usage.cost.base_micros,
                total_micros: usage.cost.total_micros,
                amount_due_micros: usage.cost.amount_due_micros,
            },
        }
    }
}

/// Create a machine on the cloud and wait for it to be ready to do work.
///
/// A create that fails after the record exists deletes it before returning:
/// otherwise it leaks, and bills, as an orphan nobody is holding.
pub(crate) fn create(
    client: &Client,
    request: &wire::CreateMachine,
    branchable: bool,
) -> Result<CloudTransport> {
    let created = client.create_machine(request)?;
    let transport = CloudTransport::from_machine(client.clone(), created);

    // The cloud may auto-start, and readiness is the real gate, so a failed
    // start is remembered rather than raised. It only surfaces if readiness
    // also fails — the machine record carries no error detail of its own.
    let start_error = client.start(&transport.id, branchable).err();

    match transport.wait_until_ready(ReadyOptions::default()) {
        Ok(()) => Ok(transport),
        Err(error) => {
            let _ = transport.delete();
            Err(match start_error {
                Some(start_error) => Error::new(
                    error.kind(),
                    format!("{} (start failed: {start_error})", error.message()),
                ),
                None => error,
            })
        }
    }
}

/// Attach to an existing cloud machine by id or name.
pub(crate) fn connect(client: &Client, name_or_id: &str) -> Result<CloudTransport> {
    let id = client.resolve_id(name_or_id)?;
    let machine = client.machine(&id)?;
    Ok(CloudTransport::from_machine(client.clone(), machine))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exec_timeout_never_shortens_the_default_request_window() {
        assert_eq!(
            CloudTransport::exec_timeout(&ExecOptions::new()),
            REQUEST_TIMEOUT
        );
        // A short command gets its own timeout plus headroom, which is already
        // past the default, so the default never truncates it.
        let brief = ExecOptions::new().timeout(Duration::from_secs(1));
        assert_eq!(
            CloudTransport::exec_timeout(&brief),
            Duration::from_secs(31)
        );
        // A long one gets its own timeout plus headroom.
        let long = ExecOptions::new().timeout(Duration::from_secs(600));
        assert_eq!(
            CloudTransport::exec_timeout(&long),
            Duration::from_secs(630)
        );
    }
}
