//! The machine handle, and the values its operations return.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{MachineBuilder, MachineConfig, Port};
use crate::connect::{ConnectOptions, Target};
use crate::error::{Error, ErrorKind, Result};
use crate::exec::{ExecOptions, ExecResult, ExecStream};
use crate::transport::{cloud, local::LocalTransport, ReadyOptions, Transport};

/// How long a restore waits for its machine, matching [`ReadyOptions`].
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// How often a restore looks.
const READY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Where a machine is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MachineState {
    /// Created or stopped; nothing is running.
    Stopped,
    /// Being created or booted.
    Starting,
    /// The VM process launched. This does *not* mean the guest is usable yet —
    /// see [`Machine::ready`].
    Started,
    /// Booted, with the guest agent connected.
    Running,
    /// Shutting down.
    Stopping,
    /// Failed. Waiting will not help.
    Error,
    /// Gone.
    Deleted,
    /// The target reported something this SDK does not know.
    Unknown,
}

impl MachineState {
    pub(crate) fn parse(state: &str) -> Self {
        match state {
            "stopped" => Self::Stopped,
            "creating" | "starting" => Self::Starting,
            "started" => Self::Started,
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            "error" | "failed" => Self::Error,
            "deleted" => Self::Deleted,
            _ => Self::Unknown,
        }
    }

    /// The target's own name for this state.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Started => "started",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Error => "error",
            Self::Deleted => "deleted",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for MachineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An OCI image cached in a machine's storage.
#[derive(Debug, Clone)]
pub struct ImageInfo {
    /// Image reference, as pulled.
    pub reference: String,
    /// Content digest.
    pub digest: String,
    /// Size on disk in bytes.
    pub size: u64,
    /// Image architecture.
    pub architecture: String,
    /// Image OS.
    pub os: String,
}

impl From<smolvm_protocol::ImageInfo> for ImageInfo {
    fn from(info: smolvm_protocol::ImageInfo) -> Self {
        Self {
            reference: info.reference,
            digest: info.digest,
            size: info.size,
            architecture: info.architecture,
            os: info.os,
        }
    }
}

/// How to reach a published guest port.
#[derive(Debug, Clone)]
pub struct PortEndpoint {
    /// HTTP URL for the port.
    pub http_url: String,
    /// WebSocket URL for the same port.
    pub ws_url: String,
    /// Headers the request must carry. Empty locally, a bearer token on the
    /// cloud, whose bridge is authenticated.
    pub headers: Vec<(String, String)>,
}

/// A shareable link to a machine.
#[derive(Debug, Clone)]
pub struct ShareLink {
    /// Bearer token for the link.
    pub token: String,
    /// Full URL, absent when there is no apps domain or the name is not
    /// DNS-safe. The token can still be attached as `?t=`.
    pub url: Option<String>,
}

/// Metered usage for one machine.
#[derive(Debug, Clone, Default)]
pub struct UsageTotals {
    /// Seconds the machine was running.
    pub total_uptime_seconds: f64,
    /// vCPU-hours consumed.
    pub cpu_hours: f64,
    /// GB-hours of guest RAM.
    pub memory_gb_hours: f64,
    /// GB-hours of disk.
    pub disk_gb_hours: f64,
    /// Gigabytes sent out.
    pub egress_gb: f64,
}

/// Cost for one machine, in micro-dollars (1e-6 USD).
#[derive(Debug, Clone, Default)]
pub struct CostBreakdown {
    /// CPU charge.
    pub cpu_micros: i64,
    /// Memory charge.
    pub memory_micros: i64,
    /// Disk charge.
    pub disk_micros: i64,
    /// Egress charge.
    pub egress_micros: i64,
    /// Flat charge for the machine existing.
    pub base_micros: i64,
    /// Everything above, added up.
    pub total_micros: i64,
    /// What is actually owed after credit.
    pub amount_due_micros: i64,
}

/// Usage and cost for one machine over a window.
#[derive(Debug, Clone)]
pub struct UsageReport {
    /// The machine this reports on.
    pub machine_id: String,
    /// Start of the reported window.
    pub from: String,
    /// End of the reported window.
    pub to: String,
    /// Metered totals.
    pub usage: UsageTotals,
    /// What those totals cost.
    pub cost: CostBreakdown,
}

/// Where a checkpoint writes, and what it may reuse.
#[derive(Debug, Clone, Default)]
pub struct CheckpointOptions {
    /// A content-addressed store to write chunks into. Reusing one store across
    /// captures of the same machine makes each later capture incremental: only
    /// changed chunks are written, and the rest are hard-linked. Local only —
    /// a cloud capture is stored by the control plane.
    pub store_dir: Option<PathBuf>,
    /// Scratch directory for capture. Defaults to the store's staging area.
    pub staging_dir: Option<PathBuf>,
    /// Cap on the prepared-chunk cache, in bytes.
    pub prepared_cache_budget_bytes: Option<u64>,
}

impl CheckpointOptions {
    /// Options with nothing set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Write chunks into this store, reusing anything already there.
    pub fn store_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.store_dir = Some(dir.into());
        self
    }

    /// Use this scratch directory during capture.
    pub fn staging_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.staging_dir = Some(dir.into());
        self
    }

    /// Cap the prepared-chunk cache.
    pub fn prepared_cache_budget_bytes(mut self, bytes: u64) -> Self {
        self.prepared_cache_budget_bytes = Some(bytes);
        self
    }
}

/// What a local capture cost and produced.
#[derive(Debug, Clone)]
pub struct CheckpointResult {
    /// Size of the artifact in bytes.
    pub size_bytes: u64,
    /// Bytes served from the store rather than written again. Zero without a
    /// store, and most of the artifact on a warm incremental capture.
    pub reused_bytes: u64,
    /// How long the source machine was paused. This is the only part the
    /// workload notices.
    pub source_pause: std::time::Duration,
    /// Wall-clock time for the whole capture.
    pub elapsed: std::time::Duration,
}

impl From<smolvm::portable_checkpoint::CaptureResult> for CheckpointResult {
    fn from(result: smolvm::portable_checkpoint::CaptureResult) -> Self {
        Self {
            size_bytes: result.size_bytes,
            reused_bytes: result.reused_bytes,
            source_pause: result.source_pause,
            elapsed: result.elapsed,
        }
    }
}

/// A capture the control plane is holding.
#[derive(Debug, Clone)]
pub struct CloudCheckpoint {
    /// Capture id.
    pub id: String,
    /// The machine this captures.
    pub machine_id: String,
    /// Capture status, `ready` once it can be restored from.
    pub status: String,
    /// Size of the stored artifact.
    pub size_bytes: u64,
    /// Architecture the capture was taken on. A capture only restores on its
    /// own architecture.
    pub arch: String,
    /// When the capture was taken.
    pub created_at: String,
    /// Pre-signed URL to download the artifact.
    pub download_url: Option<String>,
}

/// A capture, which is a different object on each target.
///
/// Locally it is a file you chose the path for, and the interesting part is
/// what it cost. On the cloud it is a durable object the control plane holds,
/// and the interesting part is how to get it back.
#[derive(Debug, Clone)]
pub enum Checkpoint {
    /// Written to the path you gave.
    Local(CheckpointResult),
    /// Stored by the control plane.
    Cloud(CloudCheckpoint),
}

impl Checkpoint {
    /// The local capture, if this was one.
    pub fn local(&self) -> Option<&CheckpointResult> {
        match self {
            Self::Local(result) => Some(result),
            Self::Cloud(_) => None,
        }
    }

    /// The cloud capture, if this was one.
    pub fn cloud(&self) -> Option<&CloudCheckpoint> {
        match self {
            Self::Cloud(info) => Some(info),
            Self::Local(_) => None,
        }
    }

    /// Size of the artifact, whichever kind it is.
    pub fn size_bytes(&self) -> u64 {
        match self {
            Self::Local(result) => result.size_bytes,
            Self::Cloud(info) => info.size_bytes,
        }
    }
}

/// How a branch is made.
#[derive(Debug, Clone, Default)]
pub struct BranchOptions {
    /// Inbound port forwards for the branch. A branch does not inherit its
    /// source's forwards, because two machines cannot listen on one host port.
    pub ports: Vec<Port>,
    /// Make the branch itself checkpointable, at some cost to branch time.
    pub checkpointable: bool,
    /// How many branches to boot at once in [`Machine::branch_batch`].
    pub parallel: usize,
}

impl BranchOptions {
    /// Options with nothing set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forward a host port into the branch.
    pub fn port(mut self, port: Port) -> Self {
        self.ports.push(port);
        self
    }

    /// Make the branch checkpointable in turn.
    pub fn checkpointable(mut self, checkpointable: bool) -> Self {
        self.checkpointable = checkpointable;
        self
    }

    /// Boot this many branches at once during a batch branch.
    pub fn parallel(mut self, parallel: usize) -> Self {
        self.parallel = parallel;
        self
    }

    pub(crate) fn pinned(&self) -> Vec<(u16, u16)> {
        self.ports.iter().map(|p| (p.host, p.guest)).collect()
    }
}

/// A handle to one machine, wherever it runs.
///
/// The handle owns nothing: dropping it neither stops nor deletes the machine.
/// Cloning is cheap, and every clone drives the same machine.
#[derive(Debug, Clone)]
pub struct Machine {
    transport: Arc<dyn Transport>,
}

impl Machine {
    pub(crate) fn from_transport(transport: Box<dyn Transport>) -> Self {
        Self {
            transport: Arc::from(transport),
        }
    }

    /// Start describing a machine of this name.
    pub fn builder(name: impl Into<String>) -> MachineBuilder {
        MachineBuilder::new(name)
    }

    /// Create a machine locally. The VM is not started yet.
    pub fn create(config: MachineConfig) -> Result<Self> {
        Self::create_with(config, &ConnectOptions::local())
    }

    /// Create a machine on the target these options select.
    ///
    /// Locally the machine is created stopped, and [`Machine::start`] boots it.
    /// On the cloud, create also starts and waits for readiness, because a
    /// cloud machine that exists but never became ready is an orphan that
    /// bills.
    pub fn create_with(config: MachineConfig, connect: &ConnectOptions) -> Result<Self> {
        match connect.target() {
            Target::Local => {
                let name = config.name.clone();
                let request = config.into_request()?;
                smolvm::embedded::runtime()?.create_machine_with_workload(
                    request.spec,
                    request.env,
                    request.workdir,
                    request.user,
                )?;
                Ok(Self::from_transport(Box::new(LocalTransport::new(name))))
            }
            Target::Cloud => {
                let branchable = config.branchable;
                let request = config.into_cloud_request()?;
                let client = connect.client()?;
                Ok(Self::from_transport(Box::new(cloud::create(
                    &client, &request, branchable,
                )?)))
            }
        }
    }

    /// Attach to an existing local machine, starting it if it is stopped.
    pub fn connect(name: impl Into<String>) -> Result<Self> {
        Self::connect_with(name, &ConnectOptions::local())
    }

    /// Attach to an existing machine on the target these options select.
    ///
    /// Locally this re-opens a persisted machine by name, starting it if
    /// stopped. On the cloud it looks the machine up by id, or by name.
    pub fn connect_with(name: impl Into<String>, connect: &ConnectOptions) -> Result<Self> {
        let name = name.into();
        match connect.target() {
            Target::Local => {
                smolvm::embedded::runtime()?.connect_or_start_machine(&name)?;
                Ok(Self::from_transport(Box::new(LocalTransport::new(name))))
            }
            Target::Cloud => {
                let client = connect.client()?;
                Ok(Self::from_transport(Box::new(cloud::connect(
                    &client, &name,
                )?)))
            }
        }
    }

    /// Attach to a local machine without starting it.
    ///
    /// Use this to inspect or delete a stopped machine, or to get a handle for
    /// one you know is already running.
    pub fn attach(name: impl Into<String>) -> Self {
        Self::from_transport(Box::new(LocalTransport::new(name)))
    }

    /// Create a stopped local machine from a portable checkpoint on disk.
    pub fn restore_checkpoint(name: impl Into<String>, artifact: impl AsRef<Path>) -> Result<Self> {
        let name = name.into();
        smolvm::embedded::runtime()?.restore_checkpoint_machine(&name, artifact.as_ref())?;
        Ok(Self::attach(name))
    }

    /// Create a machine from a capture the control plane is holding.
    ///
    /// `checkpoint` is the id from [`Machine::checkpoint`] or
    /// [`Machine::checkpoints`], not a path — the artifact never touches your
    /// disk. Unlike the local restore, this also starts the machine and waits
    /// for it, because a cloud machine that exists but never became ready is an
    /// orphan that bills.
    ///
    /// A capture only restores on the architecture it was taken on.
    pub fn restore_cloud_checkpoint(
        name: impl Into<String>,
        checkpoint: &str,
        connect: &ConnectOptions,
    ) -> Result<Self> {
        if connect.target() != Target::Cloud {
            return Err(Error::new(
                ErrorKind::NotSupported,
                "restoring by checkpoint id is a cloud operation; \
                 a local capture is a file, so pass its path to restore_checkpoint",
            ));
        }
        let name = name.into();
        let client = connect.client()?;
        let restored = client.restore_checkpoint(checkpoint, &name)?;
        let transport = crate::transport::cloud::CloudTransport::new(
            client.clone(),
            restored.display_name(),
            restored.id.clone(),
        );

        // Same bargain as create: if it cannot be made ready, do not hand back
        // a machine that quietly bills.
        if let Err(error) = client
            .start(&restored.id, false)
            .and_then(|()| client.wait_until_ready(&restored.id, READY_TIMEOUT, READY_INTERVAL))
        {
            let _ = client.delete(&restored.id);
            return Err(error.into());
        }
        Ok(Self::from_transport(Box::new(transport)))
    }

    /// The machine's name.
    pub fn name(&self) -> &str {
        self.transport.name()
    }

    /// The machine's identifier on its target: the name locally, the
    /// control-plane id on the cloud.
    pub fn id(&self) -> &str {
        self.transport.id()
    }

    /// Where this machine runs.
    pub fn target(&self) -> Target {
        self.transport.target()
    }

    /// The VM process id. Always `None` for a cloud machine, whose process
    /// lives on a node you do not own.
    pub fn pid(&self) -> Option<i32> {
        self.transport.pid()
    }

    /// Whether the VM is alive.
    pub fn is_running(&self) -> bool {
        self.transport.is_running()
    }

    /// Where the machine is in its lifecycle.
    pub fn state(&self) -> MachineState {
        self.transport.state()
    }

    /// Whether the machine is ready to do work.
    ///
    /// This is strictly later than running: a started machine is still booting,
    /// and acting then is the race that passes on a slow cold start and fails
    /// on a warm one.
    pub fn ready(&self) -> Result<bool> {
        self.transport.ready()
    }

    /// Block until the machine is ready to do work.
    pub fn wait_until_ready(&self) -> Result<()> {
        self.transport.wait_until_ready(ReadyOptions::default())
    }

    /// Block until the machine is ready, with your own deadline.
    pub fn wait_until_ready_with(&self, options: ReadyOptions) -> Result<()> {
        self.transport.wait_until_ready(options)
    }

    /// The host port forwarding to a published guest port.
    pub fn host_port(&self, guest_port: u16) -> Result<Option<u16>> {
        self.transport.host_port(guest_port)
    }

    /// Every published guest port.
    pub fn guest_ports(&self) -> Result<Vec<u16>> {
        self.transport.guest_ports()
    }

    /// How to reach a published guest port over HTTP or WebSocket.
    pub fn endpoint(&self, port: u16, path: &str) -> Result<PortEndpoint> {
        self.transport.endpoint(port, path)
    }

    /// The machine's public URL, if its target publishes one.
    pub fn url(&self) -> Result<Option<String>> {
        self.transport.url()
    }

    /// Boot the machine and wait for the guest agent to answer.
    pub fn start(&self) -> Result<()> {
        self.transport.start()
    }

    /// Boot the machine as a branch source, with cloneable guest RAM, so
    /// [`Machine::branch`] can clone it later.
    pub fn start_branchable(&self) -> Result<()> {
        self.transport.start_branchable()
    }

    /// Run a command in the guest and wait for it.
    pub fn exec<I, S>(&self, command: I) -> Result<ExecResult>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exec_with(command, ExecOptions::new())
    }

    /// Run a command with explicit environment, directory or timeout.
    pub fn exec_with<I, S>(&self, command: I, options: ExecOptions) -> Result<ExecResult>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.transport
            .exec(command.into_iter().map(Into::into).collect(), options)
    }

    /// Run a command and read its output as it arrives.
    pub fn exec_stream<I, S>(&self, command: I, options: ExecOptions) -> Result<ExecStream>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.transport
            .exec_stream(command.into_iter().map(Into::into).collect(), options)
    }

    /// Pull an OCI image if needed, run a command inside it on an overlay
    /// rootfs, then clean up. Local only: a cloud machine is created from its
    /// image instead.
    pub fn run<I, S>(&self, image: &str, command: I) -> Result<ExecResult>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.run_with(image, command, ExecOptions::new())
    }

    /// [`Machine::run`] with explicit environment, directory or timeout.
    pub fn run_with<I, S>(
        &self,
        image: &str,
        command: I,
        options: ExecOptions,
    ) -> Result<ExecResult>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.transport.run(
            image,
            command.into_iter().map(Into::into).collect(),
            options,
        )
    }

    /// Pull an OCI image into the machine's storage. Local only.
    pub fn pull_image(&self, image: &str) -> Result<ImageInfo> {
        self.transport.pull_image(image)
    }

    /// Every OCI image cached in the machine's storage. Local only.
    pub fn list_images(&self) -> Result<Vec<ImageInfo>> {
        self.transport.list_images()
    }

    /// Write a file into the running guest.
    pub fn write_file(&self, path: &str, data: impl Into<Vec<u8>>) -> Result<()> {
        self.transport.write_file(path, data.into(), None)
    }

    /// Write a file into the running guest with an explicit mode.
    pub fn write_file_with_mode(
        &self,
        path: &str,
        data: impl Into<Vec<u8>>,
        mode: u32,
    ) -> Result<()> {
        self.transport.write_file(path, data.into(), Some(mode))
    }

    /// Read a file out of the running guest.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        self.transport.read_file(path)
    }

    /// Capture this running machine.
    ///
    /// Locally this writes to `output`. On the cloud, pass `None`: the control
    /// plane stores the capture and hands back a download URL.
    pub fn checkpoint(&self, output: Option<&Path>) -> Result<Checkpoint> {
        self.checkpoint_with(output, CheckpointOptions::new())
    }

    /// Capture this running machine, reusing a content-addressed store.
    ///
    /// Pointing every local capture of a machine at the same store makes all
    /// but the first incremental.
    pub fn checkpoint_with(
        &self,
        output: Option<&Path>,
        options: CheckpointOptions,
    ) -> Result<Checkpoint> {
        self.transport.checkpoint(output, options)
    }

    /// Every stored capture of this machine. Cloud only: a local capture is a
    /// file you chose the path for, so the engine keeps no list.
    pub fn checkpoints(&self) -> Result<Vec<Checkpoint>> {
        self.transport.checkpoints()
    }

    /// Branch this running, branchable machine, sharing its live RAM and disks
    /// copy-on-write.
    pub fn branch(&self, name: impl AsRef<str>) -> Result<Machine> {
        self.branch_with(name, BranchOptions::new())
    }

    /// [`Machine::branch`] with port forwards, or a branch that is itself
    /// checkpointable.
    pub fn branch_with(&self, name: impl AsRef<str>, options: BranchOptions) -> Result<Machine> {
        Ok(Machine::from_transport(
            self.transport.branch(name.as_ref(), &options)?,
        ))
    }

    /// Branch this machine many times, booting the branches in bounded waves.
    ///
    /// The call is transactional: if any branch fails, every branch made by
    /// this call is removed before the error returns.
    pub fn branch_batch<I, S>(&self, names: I, options: BranchOptions) -> Result<Vec<Machine>>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let names: Vec<String> = names.into_iter().map(Into::into).collect();
        Ok(self
            .transport
            .branch_batch(&names, &options)?
            .into_iter()
            .map(Machine::from_transport)
            .collect())
    }

    /// Copy guest-local staged mounts back to their host sources. Local only.
    pub fn sync(&self) -> Result<()> {
        self.transport.sync()
    }

    /// Metered usage and cost so far. Cloud only.
    pub fn usage(&self) -> Result<UsageReport> {
        self.transport.usage()
    }

    /// Publish a shareable link to the machine. Cloud only.
    pub fn share(&self) -> Result<ShareLink> {
        self.transport.share()
    }

    /// Withdraw a previously published link. Cloud only.
    pub fn unshare(&self) -> Result<()> {
        self.transport.unshare()
    }

    /// Shut the machine down, keeping its disks.
    pub fn stop(&self) -> Result<()> {
        self.transport.stop()
    }

    /// Stop the machine and remove its storage. Not reversible.
    pub fn delete(&self) -> Result<()> {
        self.transport.delete()
    }

    /// Delete the machine and take a final, settled usage reading. Cloud only.
    ///
    /// The control plane samples usage synchronously before teardown, so this
    /// is the last chance to learn what a machine cost — after the delete there
    /// is nothing left to ask.
    pub fn delete_with_usage(&self) -> Result<UsageReport> {
        self.transport.delete_with_usage()
    }
}

/// Every machine in a cloud account.
///
/// There is no local equivalent: the embedded engine's machines are whatever
/// this process created, and it already knows their names.
pub fn list_cloud_machines(connect: &ConnectOptions) -> Result<Vec<Machine>> {
    if connect.target() != Target::Cloud {
        return Err(Error::new(
            ErrorKind::NotSupported,
            "listing machines is a cloud operation; pass ConnectOptions::cloud()",
        ));
    }
    let client = connect.client()?;
    Ok(client
        .machines()?
        .into_iter()
        .map(|machine| {
            let name = machine.display_name().to_string();
            Machine::from_transport(Box::new(crate::transport::cloud::CloudTransport::new(
                client.clone(),
                name,
                machine.id,
            )))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_this_sdk_does_not_know_reads_as_unknown_not_stopped() {
        assert_eq!(MachineState::parse("running"), MachineState::Running);
        assert_eq!(MachineState::parse("started"), MachineState::Started);
        assert_eq!(MachineState::parse("creating"), MachineState::Starting);
        assert_eq!(MachineState::parse("error"), MachineState::Error);
        // Guessing "stopped" for an unrecognised state would read as a verdict
        // the target never gave.
        assert_eq!(MachineState::parse("who knows"), MachineState::Unknown);
    }

    #[test]
    fn every_state_round_trips_through_its_wire_name() {
        for state in [
            MachineState::Stopped,
            MachineState::Started,
            MachineState::Running,
            MachineState::Stopping,
            MachineState::Error,
            MachineState::Deleted,
            MachineState::Unknown,
        ] {
            assert_eq!(MachineState::parse(state.as_str()), state);
        }
    }

    #[test]
    fn a_batch_branch_without_a_width_still_boots_in_waves() {
        assert_eq!(BranchOptions::new().parallel, 0);
        assert_eq!(BranchOptions::new().parallel(4).parallel, 4);
        assert_eq!(
            BranchOptions::new().port(Port::new(9000, 80)).pinned(),
            vec![(9000, 80)]
        );
    }

    #[test]
    fn a_handle_never_owns_the_machine_it_names() {
        let machine = Machine::attach("detached");
        assert_eq!(machine.name(), "detached");
        assert_eq!(machine.target(), Target::Local);
        assert_eq!(machine.clone().name(), "detached");
    }

    #[test]
    fn a_checkpoint_reports_its_size_whichever_kind_it_is() {
        let local = Checkpoint::Local(CheckpointResult {
            size_bytes: 10,
            reused_bytes: 4,
            source_pause: std::time::Duration::from_millis(80),
            elapsed: std::time::Duration::from_secs(2),
        });
        assert_eq!(local.size_bytes(), 10);
        assert!(local.local().is_some());
        assert!(local.cloud().is_none());
    }
}
