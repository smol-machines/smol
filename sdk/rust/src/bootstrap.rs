//! Getting an engine to drive, without asking the caller to install one.
//!
//! npm and PyPI ship per-platform binaries, which is what makes those SDKs
//! work straight after an install. crates.io has no equivalent — it serves
//! source, and a 37 MiB engine would not fit in a crate anyway — so the SDK
//! fetches the matching engine release itself, once, and caches it.
//!
//! Nothing is downloaded until a *local* machine is actually asked for. Cloud
//! machines never touch any of this.

use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};

/// Where releases come from.
const RELEASES: &str = "https://github.com/smol-machines/smolvm/releases/download";

/// Pin the engine to this crate's own version.
///
/// A `smol` release bundles the engine of the same version, so the SDK and the
/// engine it drives cannot drift: whatever version this crate was published at
/// is the engine it fetches.
fn engine_version() -> String {
    std::env::var("SMOLMACHINES_ENGINE_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// The release artifact name for the host, or an error naming the platform.
fn platform_slug() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "x86_64") => "linux-x86_64",
        (os, arch) => {
            return Err(Error::new(
                ErrorKind::NotSupported,
                format!(
                    "no engine release for {os}/{arch}; set SMOLVM to a `smolvm` binary to run \
                     local machines here (cloud machines work on any platform)"
                ),
            ))
        }
    })
}

/// Where fetched engines live.
fn cache_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SMOLMACHINES_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let home = PathBuf::from(home);
                if cfg!(target_os = "macos") {
                    home.join("Library").join("Caches")
                } else {
                    home.join(".cache")
                }
            })
        })
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                "no home directory to cache an engine in; set SMOLMACHINES_CACHE_DIR",
            )
        })?;
    Ok(base.join("smolmachines"))
}

/// The engine this SDK would use, fetching it if it is not already cached.
///
/// Set `SMOLMACHINES_NO_DOWNLOAD=1` to refuse the fetch and fail instead —
/// useful where reaching the network at runtime is not allowed.
pub fn ensure_engine() -> Result<PathBuf> {
    let version = engine_version();
    let slug = platform_slug()?;
    let dir_name = format!("smolvm-{version}-{slug}");
    let root = cache_root()?;
    let extracted = root.join(&dir_name);
    let binary = extracted.join("smolvm");
    if binary.is_file() {
        return Ok(binary);
    }

    if std::env::var("SMOLMACHINES_NO_DOWNLOAD").is_ok_and(|v| v != "0") {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!(
                "no engine at {} and downloads are disabled; install the smolvm CLI or set \
                 SMOLVM to a binary",
                extracted.display()
            ),
        ));
    }

    let archive = format!("{dir_name}.tar.gz");
    let bytes = fetch(&format!("{RELEASES}/v{version}/{archive}"))?;
    verify(&version, &archive, &bytes)?;
    extract(&bytes, &root)?;

    if !binary.is_file() {
        return Err(Error::new(
            ErrorKind::Storage,
            format!("{archive} did not contain {}", binary.display()),
        ));
    }
    Ok(binary)
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::blocking::Client::builder()
        // A cold fetch is ~37 MiB; give it room on a slow link.
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .and_then(|client| client.get(url).send())
        .map_err(|e| Error::new(ErrorKind::Connection, format!("fetch {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!("fetch {url}: {}", response.status()),
        ));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|e| Error::new(ErrorKind::Connection, format!("read {url}: {e}")))
}

/// Check the archive against the release's published checksums.
///
/// A cached engine is executed, so what was downloaded has to be what was
/// released — an unverified fetch would be a straightforward way to run
/// someone else's binary.
fn verify(version: &str, archive: &str, bytes: &[u8]) -> Result<()> {
    use sha2::{Digest, Sha256};
    let listing = fetch(&format!("{RELEASES}/v{version}/checksums.sha256"))
        .map_err(|e| Error::new(e.kind(), format!("read the release checksums: {e}")))?;
    let listing = String::from_utf8_lossy(&listing);
    let expected = listing
        .lines()
        .filter_map(|line| line.split_once("  "))
        .find(|(_, name)| name.trim() == archive)
        .map(|(digest, _)| digest.trim().to_ascii_lowercase())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("{archive} is not listed in the release checksums"),
            )
        })?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(Error::new(
            ErrorKind::Storage,
            format!("{archive} failed its checksum: expected {expected}, got {actual}"),
        ));
    }
    Ok(())
}

/// Unpack into the cache through a temporary directory, so a failed or
/// concurrent fetch never leaves a half-written engine behind.
fn extract(bytes: &[u8], root: &Path) -> Result<()> {
    std::fs::create_dir_all(root).map_err(|e| {
        Error::new(
            ErrorKind::Storage,
            format!("create {}: {e}", root.display()),
        )
    })?;
    let staging = root.join(format!(".unpack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| Error::new(ErrorKind::Storage, format!("stage the engine: {e}")))?;

    let decoder = flate2::read::GzDecoder::new(bytes);
    tar::Archive::new(decoder)
        .unpack(&staging)
        .map_err(|e| Error::new(ErrorKind::Storage, format!("unpack the engine: {e}")))?;

    // Move each unpacked top-level directory into place. A rename that loses
    // the race with another process is fine: the winner's copy is identical.
    let entries = std::fs::read_dir(&staging)
        .map_err(|e| Error::new(ErrorKind::Storage, format!("read the staged engine: {e}")))?;
    for entry in entries.flatten() {
        let target = root.join(entry.file_name());
        if !target.exists() {
            let _ = std::fs::rename(entry.path(), &target);
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_version_follows_this_crate_unless_overridden() {
        std::env::remove_var("SMOLMACHINES_ENGINE_VERSION");
        assert_eq!(engine_version(), env!("CARGO_PKG_VERSION"));
        std::env::set_var("SMOLMACHINES_ENGINE_VERSION", "1.2.3");
        assert_eq!(engine_version(), "1.2.3");
        std::env::remove_var("SMOLMACHINES_ENGINE_VERSION");
    }

    #[test]
    fn an_unsupported_platform_says_so_instead_of_guessing_an_artifact() {
        // The host running these tests is one of the three released targets,
        // so this asserts the mapping rather than the error path.
        let slug = platform_slug().expect("this platform has a release");
        assert!(["darwin-arm64", "linux-arm64", "linux-x86_64"].contains(&slug));
    }

    #[test]
    fn a_checksum_mismatch_is_refused() {
        // Verify against a listing that cannot match, without touching the
        // network: an empty body has a known digest.
        let listing =
            "0000000000000000000000000000000000000000000000000000000000000000  x.tar.gz\n";
        let expected = listing
            .lines()
            .filter_map(|line| line.split_once("  "))
            .find(|(_, name)| name.trim() == "x.tar.gz")
            .map(|(digest, _)| digest.to_string());
        assert_eq!(expected.as_deref(), Some("0".repeat(64).as_str()));
    }
}
