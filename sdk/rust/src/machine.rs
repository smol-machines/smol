//! The machine handle: create, start, run things, fork, checkpoint, tear down.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use smolvm::embedded::{runtime, EmbeddedRuntime};

use crate::config::{MachineBuilder, MachineConfig, Port};
use crate::error::Result;
use crate::exec::{ExecOptions, ExecResult, ExecStream};

/// Where a machine is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MachineState {
    /// Created or stopped; no VM process.
    Stopped,
    /// Booting, or waiting for the guest agent.
    Starting,
    /// Booted, agent connected.
    Running,
    /// Shutting down.
    Stopping,
}

impl MachineState {
    fn parse(state: &str) -> Self {
        match state {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            _ => Self::Stopped,
        }
    }

    /// The engine's own name for this state.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
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

/// Where a checkpoint writes, and what it may reuse.
#[derive(Debug, Clone, Default)]
pub struct CheckpointOptions {
    /// A content-addressed store to write chunks into. Reusing one store across
    /// captures of the same machine makes each later capture incremental: only
    /// changed chunks are written, and the rest are hard-linked. This is the
    /// difference between a full save and a few seconds.
    pub store_dir: Option<PathBuf>,
    /// Scratch directory for capture. Defaults to the store's own staging area.
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

/// What a capture cost and produced.
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

    fn pinned(&self) -> Vec<(u16, u16)> {
        self.ports.iter().map(|p| (p.host, p.guest)).collect()
    }
}

/// How many branches boot at once when a batch branch does not say.
const DEFAULT_BRANCH_PARALLEL: usize = 8;

/// A handle to one machine.
///
/// The handle is just a name plus access to the process-wide runtime, so it is
/// cheap to clone and every handle for a given name talks to the same VM. It
/// owns nothing: dropping it does not stop or delete the machine.
#[derive(Debug, Clone)]
pub struct Machine {
    name: String,
}

fn rt() -> Result<Arc<EmbeddedRuntime>> {
    Ok(runtime()?)
}

impl Machine {
    /// Start describing a machine of this name.
    pub fn builder(name: impl Into<String>) -> MachineBuilder {
        MachineBuilder::new(name)
    }

    /// Create a machine from a config. The VM is not started yet.
    pub fn create(config: MachineConfig) -> Result<Self> {
        let name = config.name.clone();
        let request = config.into_request()?;
        rt()?.create_machine_with_workload(
            request.spec,
            request.env,
            request.workdir,
            request.user,
        )?;
        Ok(Self { name })
    }

    /// Attach to an existing machine, starting it if it is stopped.
    ///
    /// This is how a persisted machine is reopened from a new process.
    pub fn connect(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        rt()?.connect_or_start_machine(&name)?;
        Ok(Self { name })
    }

    /// Attach to an existing machine without starting it.
    ///
    /// Use this to inspect or delete a stopped machine, or to get a handle for
    /// one you know is already running.
    pub fn attach(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Create a stopped machine from a portable checkpoint on disk.
    pub fn restore_checkpoint(name: impl Into<String>, artifact: impl AsRef<Path>) -> Result<Self> {
        let name = name.into();
        rt()?.restore_checkpoint_machine(&name, artifact.as_ref())?;
        Ok(Self { name })
    }

    /// The machine's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The VM process id, if it is running.
    pub fn pid(&self) -> Option<i32> {
        runtime().ok().and_then(|runtime| runtime.pid(&self.name))
    }

    /// Whether the VM process is alive.
    pub fn is_running(&self) -> bool {
        runtime()
            .map(|runtime| runtime.is_running(&self.name))
            .unwrap_or(false)
    }

    /// Where the machine is in its lifecycle.
    pub fn state(&self) -> MachineState {
        runtime()
            .map(|runtime| MachineState::parse(&runtime.state(&self.name)))
            .unwrap_or(MachineState::Stopped)
    }

    /// The host port forwarding to a published guest port.
    pub fn host_port(&self, guest_port: u16) -> Result<Option<u16>> {
        Ok(rt()?.host_port(&self.name, guest_port)?)
    }

    /// Every published guest port.
    pub fn guest_ports(&self) -> Result<Vec<u16>> {
        Ok(rt()?.guest_ports(&self.name)?)
    }

    /// Boot the VM and wait for the guest agent to answer.
    pub fn start(&self) -> Result<()> {
        Ok(rt()?.start_machine(&self.name)?)
    }

    /// Boot the VM as a branch source, with memfd-backed guest RAM and a
    /// control socket, so [`Machine::branch`] can clone it later.
    pub fn start_branchable(&self) -> Result<()> {
        Ok(rt()?.start_forkable_machine(&self.name)?)
    }

    /// Run a command in the guest and wait for it.
    pub fn exec<I, S>(&self, command: I) -> Result<ExecResult>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exec_with(command, ExecOptions::new())
    }

    /// Run a command in the guest with explicit environment, directory or
    /// timeout.
    pub fn exec_with<I, S>(&self, command: I, options: ExecOptions) -> Result<ExecResult>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let command: Vec<String> = command.into_iter().map(Into::into).collect();
        let (env, workdir, timeout) = options.split();
        let (exit_code, stdout, stderr) = rt()?.exec(&self.name, command, env, workdir, timeout)?;
        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    /// Run a command in the guest and read its output as it arrives.
    pub fn exec_stream<I, S>(&self, command: I, options: ExecOptions) -> ExecStream
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let command: Vec<String> = command.into_iter().map(Into::into).collect();
        let (env, workdir, timeout) = options.split();
        ExecStream::spawn(self.name.clone(), command, env, workdir, timeout)
    }

    /// Pull an OCI image if needed, run a command inside it on an overlay
    /// rootfs, then clean up. The equivalent of `smolvm run`.
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
        let command: Vec<String> = command.into_iter().map(Into::into).collect();
        let (env, workdir, timeout) = options.split();
        let (exit_code, stdout, stderr) =
            rt()?.run(&self.name, image, command, env, workdir, timeout)?;
        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
        })
    }

    /// Pull an OCI image into the machine's storage.
    pub fn pull_image(&self, image: &str) -> Result<ImageInfo> {
        Ok(rt()?.pull_image(&self.name, image)?.into())
    }

    /// Every OCI image cached in the machine's storage.
    pub fn list_images(&self) -> Result<Vec<ImageInfo>> {
        Ok(rt()?
            .list_images(&self.name)?
            .into_iter()
            .map(ImageInfo::from)
            .collect())
    }

    /// Write a file into the running guest.
    pub fn write_file(&self, path: &str, data: impl Into<Vec<u8>>) -> Result<()> {
        Ok(rt()?.write_file(&self.name, path, data.into(), None)?)
    }

    /// Write a file into the running guest with an explicit mode.
    pub fn write_file_with_mode(
        &self,
        path: &str,
        data: impl Into<Vec<u8>>,
        mode: u32,
    ) -> Result<()> {
        Ok(rt()?.write_file(&self.name, path, data.into(), Some(mode))?)
    }

    /// Read a file out of the running guest.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        Ok(rt()?.read_file(&self.name, path)?)
    }

    /// Capture this running machine to a portable checkpoint on disk.
    pub fn checkpoint(&self, output: impl AsRef<Path>) -> Result<CheckpointResult> {
        self.checkpoint_with(output, CheckpointOptions::new())
    }

    /// Capture this running machine, reusing a content-addressed store.
    ///
    /// Pointing every capture of a machine at the same store makes all but the
    /// first incremental.
    pub fn checkpoint_with(
        &self,
        output: impl AsRef<Path>,
        options: CheckpointOptions,
    ) -> Result<CheckpointResult> {
        let capture = smolvm::portable_checkpoint::CaptureOptions {
            rootfs_dir: Some(smolvm::agent::AgentManager::default_rootfs_path()?),
            store_dir: options.store_dir,
            staging_dir: options.staging_dir,
            prepared_cache_budget_bytes: options.prepared_cache_budget_bytes,
            ..Default::default()
        };
        Ok(rt()?
            .checkpoint_machine(&self.name, output.as_ref(), &capture)?
            .into())
    }

    /// Branch this running, branchable machine, sharing its live RAM and disks
    /// copy-on-write. Returns a handle to the running branch.
    pub fn branch(&self, name: impl Into<String>) -> Result<Machine> {
        self.branch_with(name, BranchOptions::new())
    }

    /// [`Machine::branch`] with port forwards, or a branch that is itself
    /// checkpointable.
    pub fn branch_with(&self, name: impl Into<String>, options: BranchOptions) -> Result<Machine> {
        let name = name.into();
        let pinned = options.pinned();
        let runtime = rt()?;
        if options.checkpointable {
            runtime.fork_checkpointable_machine(&self.name, &name, &pinned)?;
        } else {
            runtime.fork_machine(&self.name, &name, &pinned)?;
        }
        Ok(Machine { name })
    }

    /// Branch this machine many times from one retained checkpoint, booting the
    /// branches in bounded parallel waves.
    ///
    /// The call is transactional: if any branch fails, every branch made by
    /// this call is removed before the error returns.
    pub fn branch_batch<I, S>(&self, names: I, options: BranchOptions) -> Result<Vec<Machine>>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let names: Vec<String> = names.into_iter().map(Into::into).collect();
        let pinned = options.pinned();
        let parallel = if options.parallel == 0 {
            DEFAULT_BRANCH_PARALLEL
        } else {
            options.parallel
        };
        rt()?.fork_machines(&self.name, &names, &pinned, parallel)?;
        Ok(names.into_iter().map(|name| Machine { name }).collect())
    }

    /// Copy guest-local staged mounts back to their host sources.
    pub fn sync(&self) -> Result<()> {
        Ok(rt()?.sync_machine(&self.name)?)
    }

    /// Shut the VM down gracefully, keeping its disks.
    pub fn stop(&self) -> Result<()> {
        Ok(rt()?.stop_machine(&self.name)?)
    }

    /// Stop the VM and remove its disks and config. Not reversible.
    pub fn delete(&self) -> Result<()> {
        Ok(rt()?.delete_machine(&self.name)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_state_string_reads_as_stopped() {
        assert_eq!(MachineState::parse("running"), MachineState::Running);
        assert_eq!(MachineState::parse("starting"), MachineState::Starting);
        assert_eq!(MachineState::parse("stopping"), MachineState::Stopping);
        assert_eq!(MachineState::parse("who knows"), MachineState::Stopped);
    }

    #[test]
    fn every_state_round_trips_through_its_engine_name() {
        for state in [
            MachineState::Stopped,
            MachineState::Starting,
            MachineState::Running,
            MachineState::Stopping,
        ] {
            assert_eq!(MachineState::parse(state.as_str()), state);
        }
    }

    #[test]
    fn a_batch_branch_without_a_width_still_boots_in_waves() {
        assert_eq!(BranchOptions::new().parallel, 0);
        let options = BranchOptions::new().parallel(4);
        assert_eq!(options.parallel, 4);
        assert_eq!(
            BranchOptions::new().port(Port::new(9000, 80)).pinned(),
            vec![(9000, 80)]
        );
    }

    #[test]
    fn a_handle_never_owns_the_machine_it_names() {
        let machine = Machine::attach("detached");
        assert_eq!(machine.name(), "detached");
        assert_eq!(machine.clone().name(), "detached");
    }
}
