//! Embed smol machines microVMs directly in a Rust process.
//!
//! This is the Rust face of the same embedded engine the Node and Python SDKs
//! wrap. There is no daemon and no socket: the VM is a child of your process,
//! and every call here is a direct call into the engine.
//!
//! # A machine, start to finish
//!
//! ```no_run
//! use smolmachines::{Machine, Port};
//!
//! # fn main() -> smolmachines::Result<()> {
//! let machine = Machine::builder("hello")
//!     .image("alpine:latest")
//!     .cpus(2)
//!     .memory_mib(1024)
//!     .network(true)
//!     .port(Port::new(8080, 80))
//!     .create()?;
//!
//! machine.start()?;
//! let result = machine.exec(["uname", "-a"])?;
//! println!("{}", result.stdout_utf8());
//! machine.delete()?;
//! # Ok(())
//! # }
//! ```
//!
//! # Branching
//!
//! A machine started with [`Machine::start_branchable`] can be branched.
//! Branches share the source's guest RAM and disks copy-on-write, so a branch
//! costs a fraction of a boot and starts from the exact state the source was
//! in.
//!
//! ```no_run
//! use smolmachines::{BranchOptions, Machine};
//!
//! # fn main() -> smolmachines::Result<()> {
//! let source = Machine::builder("source")
//!     .image("alpine:latest")
//!     .branchable(true)
//!     .create()?;
//! source.start_branchable()?;
//!
//! let names: Vec<String> = (0..16).map(|i| format!("worker-{i}")).collect();
//! let workers = source.branch_batch(names, BranchOptions::new().parallel(8))?;
//! for worker in &workers {
//!     worker.exec(["true"])?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Blocking
//!
//! Every call blocks. The engine is synchronous, so an async caller should run
//! these on a blocking pool, the way [`tokio::task::spawn_blocking`] does. That
//! is exactly what the Node and Python SDKs do internally; in Rust the choice
//! is left to you rather than made for you.
//!
//! [`tokio::task::spawn_blocking`]: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
//!
//! # What you need on the host
//!
//! The engine needs a hypervisor: KVM on Linux, the Hypervisor framework on
//! macOS. It also needs a boot helper, and that one is worth knowing about: to
//! boot a VM the engine re-executes a binary that knows how to be a VM. In a
//! CLI that binary is the CLI itself. In an embedded process it is your
//! program, which does not, so the boot fails.
//!
//! Point the engine at a real helper once, before the first machine:
//!
//! ```no_run
//! use smolmachines::{configure_runtime_assets, RuntimeAssets};
//!
//! # fn main() -> smolmachines::Result<()> {
//! // Borrow an installed smolvm, if there is one on PATH.
//! if let Some(assets) = RuntimeAssets::from_path_lookup() {
//!     configure_runtime_assets(assets)?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! A program shipping its own engine names the paths itself instead. See
//! [`RuntimeAssets`].

#![warn(missing_docs)]

mod assets;
mod config;
mod error;
mod exec;
mod machine;

pub use assets::{configure_runtime_assets, RuntimeAssets};
pub use config::{MachineBuilder, MachineConfig, Mount, Port, Resources};
pub use error::{Error, ErrorKind, Result};
pub use exec::{ExecEvent, ExecOptions, ExecResult, ExecStream};
pub use machine::{
    BranchOptions, CheckpointOptions, CheckpointResult, ImageInfo, Machine, MachineState,
};

/// The SDK version, which tracks the engine it embeds.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
