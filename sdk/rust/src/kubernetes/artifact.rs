use super::{check_cancel, failure};
use crate::{ErrorKind, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub(super) const VERSION: &str = "v1.37.2+k3s1";

pub(super) struct Artifact {
    pub arch: &'static str,
    pub name: &'static str,
    pub sha256: &'static str,
    bytes: u64,
}

impl Artifact {
    pub fn for_arch(arch: &str) -> Result<Self> {
        match arch.trim() {
            "x86_64" | "amd64" => Ok(Self {
                arch: "amd64",
                name: "k3s",
                sha256: "265b3ee6a3198ef14033c63ef46cbb32370d83795199fb6d1b01f21575eddbda",
                bytes: 81_678_498,
            }),
            "aarch64" | "arm64" => Ok(Self {
                arch: "arm64",
                name: "k3s-arm64",
                sha256: "a779ccf1e5e2e33a62f5a29c2f1c0bbf99431e100579d4c10f0aa0742a6aa84b",
                bytes: 73_990_306,
            }),
            _ => Err(failure(
                ErrorKind::NotSupported,
                "Kubernetes requires an ARM64 or x86_64 guest",
            )),
        }
    }

    fn valid(&self, path: &Path) -> bool {
        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };
        if !file
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() == self.bytes)
        {
            return false;
        }
        let mut hash = Sha256::new();
        let mut buffer = [0; 65_536];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => hash.update(&buffer[..n]),
                Err(_) => return false,
            }
        }
        format!("{:x}", hash.finalize()) == self.sha256
    }

    pub fn ensure(&self, cancelled: &AtomicBool) -> Result<PathBuf> {
        check_cancel(cancelled)?;
        let dir = crate::bootstrap::cache_root()?
            .join("kubernetes")
            .join(VERSION)
            .join(self.arch);
        let path = dir.join(self.name);
        if self.valid(&path) {
            return Ok(path);
        }
        if std::env::var_os("SMOLMACHINES_NO_DOWNLOAD").is_some_and(|v| v == "1") {
            return Err(failure(
                ErrorKind::NotFound,
                "The pinned Kubernetes artifact is not cached and downloads are disabled",
            ));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| failure(ErrorKind::Storage, format!("create Kubernetes cache: {e}")))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| failure(ErrorKind::Other, format!("start Kubernetes download: {e}")))?;
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .https_only(true)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(180))
                .build()
                .map_err(|e| failure(ErrorKind::Connection, e.to_string()))?;
            let download = async {
                let url = format!(
                    "https://github.com/k3s-io/k3s/releases/download/v1.37.2%2Bk3s1/{}",
                    self.name
                );
                let mut response = client
                    .get(url)
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| {
                        failure(ErrorKind::Connection, format!("download Kubernetes: {e}"))
                    })?;
                let mut temp = tempfile::NamedTempFile::new_in(&dir)
                    .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
                let mut hash = Sha256::new();
                let mut bytes = 0u64;
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|e| failure(ErrorKind::Connection, e.to_string()))?
                {
                    bytes += chunk.len() as u64;
                    if bytes > self.bytes {
                        return Err(failure(
                            ErrorKind::Storage,
                            "Kubernetes download exceeds its pinned size",
                        ));
                    }
                    hash.update(&chunk);
                    temp.write_all(&chunk)
                        .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
                }
                if bytes != self.bytes || format!("{:x}", hash.finalize()) != self.sha256 {
                    return Err(failure(
                        ErrorKind::Storage,
                        "Kubernetes download checksum does not match the pinned release",
                    ));
                }
                check_cancel(cancelled)?;
                temp.as_file()
                    .sync_all()
                    .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
                temp.persist(&path)
                    .map_err(|e| failure(ErrorKind::Storage, e.to_string()))?;
                Ok(path.clone())
            };
            tokio::pin!(download);
            loop {
                tokio::select! {
                    result = &mut download => return result,
                    _ = tokio::time::sleep(Duration::from_millis(50)) => check_cancel(cancelled)?,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_same_size_cache_is_not_executable_input() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), b"wrong").unwrap();
        let a = Artifact {
            arch: "test",
            name: "test",
            bytes: 5,
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        };
        assert!(!a.valid(f.path()));
        let digest = format!("{:x}", Sha256::digest(b"wrong"));
        let a = Artifact {
            sha256: Box::leak(digest.into_boxed_str()),
            ..a
        };
        assert!(a.valid(f.path()));
        std::fs::write(f.path(), b"WRONG").unwrap();
        assert!(!a.valid(f.path()));
    }
}
