//! Finding the control plane and the key to talk to it.

use std::path::PathBuf;

use crate::error::{Error, ErrorKind, Result};

/// Where the cloud lives when nothing says otherwise.
pub const DEFAULT_BASE_URL: &str = "https://api.smolmachines.com";

/// Environment variable holding an API key.
pub const TOKEN_ENV: &str = "SMOL_CLOUD_TOKEN";
/// Environment variable overriding the base URL.
pub const URL_ENV: &str = "SMOL_CLOUD_URL";

/// What to tell someone who reached the cloud without a credential.
pub const NO_KEY_HINT: &str = "pass an API key, set SMOL_CLOUD_TOKEN, or run `smol auth login` \
                               to create a CLI session";

/// A resolved base URL and API key.
#[derive(Clone)]
pub struct Credentials {
    base_url: String,
    api_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let the key reach a log line.
        f.debug_struct("Credentials")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Use exactly these.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// Resolve from, in order: the arguments, the environment, the CLI session
    /// on disk, and finally the public cloud for the URL.
    ///
    /// The precedence matters. An explicit key or `SMOL_CLOUD_TOKEN` is a
    /// deliberate act; a CLI session is a side effect of `smol auth login`, so
    /// it fills gaps but never overrides what the caller said.
    pub fn resolve(base_url: Option<String>, api_key: Option<String>) -> Result<Self> {
        let session = CliSession::read();
        let api_key = api_key
            .or_else(|| env_var(TOKEN_ENV))
            .or(session.api_key)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Unauthorized,
                    format!("the cloud requires an API key — {NO_KEY_HINT}"),
                )
            })?;
        let base_url = base_url
            .or_else(|| env_var(URL_ENV))
            .or(session.endpoint)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Ok(Self::new(base_url, api_key))
    }

    /// The control plane's base URL, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The API key.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// A login the `smol` CLI left on disk.
#[derive(Debug, Default)]
pub struct CliSession {
    /// The stored key, absent if there is none or it has expired.
    pub api_key: Option<String>,
    /// The stored endpoint, if the login named one.
    pub endpoint: Option<String>,
}

impl CliSession {
    /// Read `~/.config/smolvm/config.toml`.
    ///
    /// That is the path on every platform — the CLI does not use XDG or
    /// `~/Library`, so neither does this.
    pub fn read() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        Self::parse(&std::fs::read_to_string(path).unwrap_or_default())
    }

    /// Parse a config file's contents.
    pub fn parse(text: &str) -> Self {
        let table = cloud_table(text);
        let lookup = |key: &str| {
            table
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string())
        };
        let api_key = lookup("api_key");
        if api_key.is_none() || token_expired(lookup("token_expires_at").as_deref()) {
            return Self::default();
        }
        Self {
            api_key,
            endpoint: lookup("endpoint"),
        }
    }
}

/// Where the CLI keeps its config.
pub fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".config").join("smolvm").join("config.toml"))
}

/// Scan the flat, tool-written `[cloud]` table. A TOML parser is a heavy
/// dependency for six lines of key-equals-value.
fn cloud_table(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_cloud = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_cloud = line == "[cloud]";
            continue;
        }
        if !in_cloud || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        out.push((
            key.trim().to_string(),
            value.trim().trim_matches(['"', '\'']).to_string(),
        ));
    }
    out
}

/// Conservative: expired only when the stamp parses cleanly *and* is in the
/// past, so a usable key is never rejected over a formatting quirk.
fn token_expired(expires_at: Option<&str>) -> bool {
    let Some(expiry) = expires_at.and_then(parse_rfc3339_secs) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    expiry <= now
}

/// Seconds since the epoch for an RFC 3339 stamp in UTC, or `None` if it is not
/// one. Enough for an expiry check, and it keeps a date library out of the tree.
fn parse_rfc3339_secs(stamp: &str) -> Option<i64> {
    let bytes = stamp.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let field = |range: std::ops::Range<usize>| stamp.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil epoch, by Howard Hinnant's algorithm.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_cloud_table_is_read() {
        let session = CliSession::parse(
            "\
[core]
api_key = \"not-this-one\"

[cloud]
# a comment
api_key = \"smk_real\"
endpoint = 'https://cloud.test'
",
        );
        assert_eq!(session.api_key.as_deref(), Some("smk_real"));
        assert_eq!(session.endpoint.as_deref(), Some("https://cloud.test"));
    }

    #[test]
    fn an_expired_session_is_the_same_as_no_session() {
        let expired = CliSession::parse(
            "[cloud]\napi_key = \"smk_old\"\ntoken_expires_at = \"2000-01-01T00:00:00Z\"\n",
        );
        assert!(expired.api_key.is_none());
        let valid = CliSession::parse(
            "[cloud]\napi_key = \"smk_new\"\ntoken_expires_at = \"2999-01-01T00:00:00Z\"\n",
        );
        assert_eq!(valid.api_key.as_deref(), Some("smk_new"));
    }

    #[test]
    fn an_unparseable_expiry_never_blocks_a_usable_key() {
        assert!(!token_expired(None));
        assert!(!token_expired(Some("whenever")));
        assert!(!token_expired(Some("")));
    }

    #[test]
    fn the_epoch_conversion_matches_known_stamps() {
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_secs("2000-01-01T00:00:00Z"),
            Some(946_684_800)
        );
        assert_eq!(
            parse_rfc3339_secs("2026-09-18T10:30:00Z"),
            Some(1_789_727_400)
        );
    }

    #[test]
    fn a_trailing_slash_is_trimmed_so_paths_never_double_up() {
        let credentials = Credentials::new("https://cloud.test/", "smk_x");
        assert_eq!(credentials.base_url(), "https://cloud.test");
    }

    #[test]
    fn the_key_never_reaches_a_debug_line() {
        let rendered = format!("{:?}", Credentials::new("https://cloud.test", "smk_secret"));
        assert!(rendered.contains("cloud.test"));
        assert!(!rendered.contains("smk_secret"));
    }
}
