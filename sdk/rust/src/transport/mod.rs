//! The two backends a machine can be driven through.
//!
//! [`Transport`] is the seam between the SDK's public [`crate::Machine`] and
//! where the machine actually lives: in this process, or in smol cloud. The
//! two are not the same machine with a different address — a local machine can
//! bind host directories and pull images into its own store, a cloud machine
//! cannot; a cloud machine reports usage and cost, a local one has none. Rather
//! than pretend, operations that exist on only one side return
//! [`crate::ErrorKind::NotSupported`] on the other, saying which target they
//! need.

pub(crate) mod cloud;
pub(crate) mod local;

use std::path::Path;
use std::time::Duration;

use crate::error::Result;
use crate::exec::{ExecOptions, ExecResult, ExecStream};
use crate::machine::{
    BranchOptions, Checkpoint, CheckpointOptions, ImageInfo, MachineState, PortEndpoint, ShareLink,
    UsageReport,
};

/// How long to wait for a machine to become ready, and how often to look.
#[derive(Debug, Clone, Copy)]
pub struct ReadyOptions {
    /// Give up after this long.
    pub timeout: Duration,
    /// Time between polls.
    pub interval: Duration,
}

impl Default for ReadyOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            interval: Duration::from_secs(1),
        }
    }
}

impl ReadyOptions {
    /// Options with the default timeout and interval.
    pub fn new() -> Self {
        Self::default()
    }

    /// Give up after this long.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Look this often.
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

/// Everything a machine can be asked to do, wherever it runs.
///
/// Implementors are [`local::LocalTransport`] and [`cloud::CloudTransport`].
pub(crate) trait Transport: Send + Sync + std::fmt::Debug {
    /// The machine's name.
    fn name(&self) -> &str;

    /// The machine's identifier on its target: the name locally, the cloud id
    /// remotely.
    fn id(&self) -> &str;

    /// Which target this machine lives on.
    fn target(&self) -> crate::connect::Target;

    fn state(&self) -> MachineState;
    fn is_running(&self) -> bool;
    fn pid(&self) -> Option<i32>;

    /// Whether the machine is ready to do work, which is strictly later than
    /// running: the guest agent has to answer first.
    fn ready(&self) -> Result<bool>;

    /// Block until the machine is ready to do work.
    fn wait_until_ready(&self, options: ReadyOptions) -> Result<()>;

    fn start(&self) -> Result<()>;
    fn start_branchable(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn resume(&self) -> Result<()>;
    fn delete(&self) -> Result<()>;

    fn exec(&self, command: Vec<String>, options: ExecOptions) -> Result<ExecResult>;
    fn exec_stream(&self, command: Vec<String>, options: ExecOptions) -> Result<ExecStream>;
    fn run(&self, image: &str, command: Vec<String>, options: ExecOptions) -> Result<ExecResult>;

    fn read_file(&self, path: &str) -> Result<Vec<u8>>;
    fn write_file(&self, path: &str, data: Vec<u8>, mode: Option<u32>) -> Result<()>;

    fn pull_image(&self, image: &str) -> Result<ImageInfo>;
    fn list_images(&self) -> Result<Vec<ImageInfo>>;
    fn sync(&self) -> Result<()>;

    fn host_port(&self, guest_port: u16) -> Result<Option<u16>>;
    fn guest_ports(&self) -> Result<Vec<u16>>;

    /// How to reach a published guest port over HTTP or WebSocket.
    fn endpoint(&self, port: u16, path: &str) -> Result<PortEndpoint>;
    fn tunnel_target(&self, port: u16) -> Result<crate::tunnel::Target>;

    /// The machine's public URL, if its target publishes one.
    fn url(&self) -> Result<Option<String>>;

    fn checkpoint(&self, output: Option<&Path>, options: CheckpointOptions) -> Result<Checkpoint>;
    fn checkpoints(&self) -> Result<Vec<Checkpoint>>;

    fn branch(&self, name: &str, options: &BranchOptions) -> Result<Box<dyn Transport>>;
    fn branch_batch(
        &self,
        names: &[String],
        options: &BranchOptions,
    ) -> Result<Vec<Box<dyn Transport>>>;

    /// Metered usage and cost so far.
    fn usage(&self) -> Result<UsageReport>;

    /// Delete the machine and take a final, settled usage reading.
    fn delete_with_usage(&self) -> Result<UsageReport>;

    /// Publish a shareable link to the machine.
    fn share(&self) -> Result<ShareLink>;

    /// Withdraw a previously published link.
    fn unshare(&self) -> Result<()>;
}

/// Build the error for an operation this target does not have.
pub(crate) fn unsupported(operation: &str, reason: &str) -> crate::error::Error {
    crate::error::Error::new(
        crate::error::ErrorKind::NotSupported,
        format!("{operation} is not available on this target: {reason}"),
    )
}
