//! The smol cloud control plane, in one place.
//!
//! The `smol` CLI and the Rust SDK both talk to smolfleet's `/v1` API. Each
//! used to carry its own copy of the machine, checkpoint and usage shapes, and
//! its own answer to "where is the API and what is my key" — which is how two
//! clients of one API drift apart.
//!
//! - [`types`] is the wire contract, usable with any HTTP client.
//! - [`credentials`] resolves the endpoint and key the same way everywhere.
//! - [`blocking`] is a synchronous client, behind the `blocking` feature.
//!
//! ```no_run
//! # #[cfg(feature = "blocking")]
//! # fn main() -> smol_cloud::Result<()> {
//! use smol_cloud::blocking::Client;
//!
//! // Credentials come from the argument, then SMOL_CLOUD_TOKEN, then the CLI
//! // session `smol auth login` left on disk.
//! let client = Client::resolve(None, None)?;
//! for machine in client.machines()? {
//!     println!("{} {}", machine.display_name(), machine.state);
//! }
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "blocking"))]
//! # fn main() {}
//! ```

#![warn(missing_docs)]

#[cfg(feature = "blocking")]
pub mod blocking;
pub mod credentials;
pub mod error;
pub mod types;

pub use credentials::{CliSession, Credentials, DEFAULT_BASE_URL};
pub use error::{Error, ErrorKind, Result};
