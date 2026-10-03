//! Finding the control plane and the key to talk to it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

/// The identity provider that issues `smol auth login` sessions.
pub const DEFAULT_ISSUER: &str = "https://smolmachines.us.auth0.com";
/// Environment variable overriding [`DEFAULT_ISSUER`].
pub const ISSUER_ENV: &str = "OIDC_ISSUER";
/// The CLI's OAuth client. It is public, with no secret, so renewing a session
/// needs nothing but the session's refresh token.
pub const CLIENT_ID: &str = "Df3M6TXvVVMmTTzfyo0mjaLl9rhaI7nZ";

/// Renew this long before the recorded expiry, as the CLI does, so a request
/// never leaves with a token about to lapse in flight.
const EXPIRY_SKEW_SECS: i64 = 60;

/// The CLI keeps a second copy of the session here for registry pulls, and
/// renews both copies together.
const SESSION_REGISTRY_TABLE: &str = "[machines.registries.\"registry.smolmachines.com\"]";

/// A resolved base URL and API key.
///
/// A key from a CLI session renews itself: the session's access token lasts a
/// day, and the refresh token the CLI stored beside it buys a new one. Keys
/// passed in or taken from `SMOL_CLOUD_TOKEN` are used exactly as given.
#[derive(Clone)]
pub struct Credentials {
    base_url: String,
    key: Arc<Mutex<Key>>,
    /// The CLI config a renewed session is written back to.
    config: Option<PathBuf>,
    /// Where a CLI session is renewed.
    issuer: String,
}

struct Key {
    access: String,
    /// Present only for a CLI session.
    refresh: Option<String>,
    expires_at: Option<i64>,
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
        Self::with_key(
            base_url,
            Key {
                access: api_key.into(),
                refresh: None,
                expires_at: None,
            },
            None,
        )
    }

    fn with_key(base_url: impl Into<String>, key: Key, config: Option<PathBuf>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            key: Arc::new(Mutex::new(key)),
            config,
            issuer: issuer_url(),
        }
    }

    /// Resolve from, in order: the arguments, the environment, the CLI session
    /// on disk, and finally the public cloud for the URL.
    ///
    /// The precedence matters. An explicit key or `SMOL_CLOUD_TOKEN` is a
    /// deliberate act; a CLI session is a side effect of `smol auth login`, so
    /// it fills gaps but never overrides what the caller said. An expired
    /// session is renewed here when the CLI left a refresh token.
    pub fn resolve(base_url: Option<String>, api_key: Option<String>) -> Result<Self> {
        let session = CliSession::read();
        let base_url = base_url
            .or_else(|| env_var(URL_ENV))
            .or_else(|| session.endpoint.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        if let Some(api_key) = api_key.or_else(|| env_var(TOKEN_ENV)) {
            return Ok(Self::new(base_url, api_key));
        }
        let expired = session.api_key.is_none() && session.expires_at.is_some();
        if session.api_key.is_none() && !(expired && session.refresh_token.is_some()) {
            return Err(Error::new(
                ErrorKind::Unauthorized,
                format!("the cloud requires an API key — {NO_KEY_HINT}"),
            ));
        }
        let credentials = Self::with_key(
            base_url,
            Key {
                access: session.api_key.unwrap_or_default(),
                refresh: session.refresh_token,
                expires_at: session.expires_at,
            },
            config_path(),
        );
        credentials.renew_if_expiring()?;
        Ok(credentials)
    }

    /// The control plane's base URL, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The API key, as of now. A CLI session's key changes when it is renewed,
    /// so read it per request rather than keeping a copy.
    pub fn api_key(&self) -> String {
        self.lock().access.clone()
    }

    /// Renew a CLI session that has expired or is about to. Does nothing for a
    /// key that cannot be renewed, and touches neither the network nor the disk
    /// while the session is still good.
    pub fn renew_if_expiring(&self) -> Result<()> {
        let mut key = self.lock();
        if key.refresh.is_none() || !expiring(key.expires_at) {
            return Ok(());
        }
        self.renew(&mut key)
    }

    /// Renew a CLI session after the control plane rejected `rejected`, its
    /// key at the time. Returns whether there is a newer key worth one retry:
    /// false for a key that cannot be renewed.
    pub fn renew_after_rejection(&self, rejected: &str) -> Result<bool> {
        let mut key = self.lock();
        if key.refresh.is_none() {
            return Ok(false);
        }
        // Another request already renewed it while this one was in flight.
        if key.access != rejected {
            return Ok(true);
        }
        self.renew(&mut key)?;
        Ok(true)
    }

    /// A CLI session renewed at `issuer` and saved to `config`, for tests that
    /// stand in for the control plane and the identity provider.
    #[cfg(all(test, feature = "blocking"))]
    pub(crate) fn test_session(
        base_url: &str,
        access: &str,
        refresh: &str,
        config: &Path,
        issuer: &str,
    ) -> Self {
        let mut credentials = Self::with_key(
            base_url,
            Key {
                access: access.to_string(),
                refresh: Some(refresh.to_string()),
                expires_at: Some(now_secs() + 86_400),
            },
            Some(config.to_path_buf()),
        );
        credentials.issuer = issuer.to_string();
        credentials
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Key> {
        self.key
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Trade the refresh token for a new access token and write both back to
    /// the CLI config. Runs under the caller's lock on `key`, so threads sharing
    /// these credentials renew once between them, and under a file lock, so
    /// other processes using the same login do too: a refresh token spent twice
    /// can revoke the whole session when the issuer rotates them.
    fn renew(&self, key: &mut Key) -> Result<()> {
        let _file_lock = self.config.as_deref().and_then(lock_config);
        // A process that renewed first has already written the new pair.
        if let Some(config) = &self.config {
            let session = CliSession::read_from(config);
            if let Some(refresh) = session.refresh_token {
                if let Some(access) = session.api_key.filter(|access| *access != key.access) {
                    key.access = access;
                    key.expires_at = session.expires_at;
                    key.refresh = Some(refresh);
                    return Ok(());
                }
                key.refresh = Some(refresh);
            }
        }
        let refresh = key.refresh.clone().unwrap_or_default();
        let renewed = refresh_session(&self.issuer, &refresh).map_err(|reason| {
            Error::new(
                ErrorKind::Unauthorized,
                format!(
                    "the CLI session expired and could not be renewed ({reason}); \
                     run `smol auth login`"
                ),
            )
        })?;
        key.access = renewed.access_token;
        if let Some(rotated) = renewed.refresh_token {
            key.refresh = Some(rotated);
        }
        key.expires_at = renewed
            .expires_in
            .map(|seconds| now_secs() + seconds as i64);
        if let Some(config) = &self.config {
            // The new key already works for this process; a config that cannot
            // be rewritten only means the next process renews again.
            let _ = write_session(config, key);
        }
        Ok(())
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn issuer_url() -> String {
    env_var(ISSUER_ENV)
        .unwrap_or_else(|| DEFAULT_ISSUER.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The token endpoint's answer to a refresh.
#[derive(serde::Deserialize)]
struct Renewed {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[cfg(feature = "blocking")]
fn refresh_session(issuer: &str, refresh_token: &str) -> std::result::Result<Renewed, String> {
    let response = reqwest::blocking::Client::new()
        .post(format!("{issuer}/oauth/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .map_err(|e| format!("could not reach {issuer}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        // The body names the OAuth error (`invalid_grant` for a revoked
        // session) and never echoes the token.
        let body = response.text().unwrap_or_default();
        return Err(format!("{status}: {body}"));
    }
    response
        .json::<Renewed>()
        .map_err(|e| format!("unreadable token response: {e}"))
}

/// Renewing needs a synchronous HTTP client; async callers (the CLI) renew
/// sessions with their own.
#[cfg(not(feature = "blocking"))]
fn refresh_session(_issuer: &str, _refresh_token: &str) -> std::result::Result<Renewed, String> {
    Err("this build renews sessions only with the `blocking` feature".to_string())
}

/// Hold an exclusive lock on a file beside the config until dropped. `None`
/// when the lock cannot be taken, which only costs the cross-process guard.
fn lock_config(config: &Path) -> Option<std::fs::File> {
    let path = config.with_extension("toml.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.lock().ok()?;
    Some(file)
}

/// Write a renewed session into both places the CLI keeps it, leaving every
/// other line of the config as it was, and replace the file in one rename so a
/// reader never sees half of it.
fn write_session(config: &Path, key: &Key) -> std::io::Result<()> {
    let text = std::fs::read_to_string(config)?;
    let mut cloud = vec![("api_key", Some(quoted(&key.access)))];
    let mut registry = vec![("identity_token", Some(quoted(&key.access)))];
    if let Some(refresh) = &key.refresh {
        cloud.push(("refresh_token", Some(quoted(refresh))));
        registry.push(("refresh_token", Some(quoted(refresh))));
    }
    let expires_at = key.expires_at.map(|at| at.to_string());
    cloud.push(("token_expires_at", expires_at.clone()));
    registry.push(("expires_at", expires_at));
    let text = set_table_values(&text, "[cloud]", &cloud);
    let text = set_table_values(&text, SESSION_REGISTRY_TABLE, &registry);

    let temporary = config.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&temporary, text)?;
    if let Ok(metadata) = std::fs::metadata(config) {
        std::fs::set_permissions(&temporary, metadata.permissions())?;
    }
    std::fs::rename(&temporary, config).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Set (or, for `None`, remove) keys in one table of a tool-written TOML file.
/// A key the table lacks is added after its last line; a missing table is left
/// missing, as the CLI only creates it at login.
fn set_table_values(text: &str, header: &str, values: &[(&str, Option<String>)]) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let Some(start) = lines.iter().position(|line| line.trim() == header) else {
        return text.to_string();
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            let line = line.trim();
            line.starts_with('[') && line.ends_with(']')
        })
        .map_or(lines.len(), |offset| start + 1 + offset);
    let mut missing = Vec::new();
    for (name, value) in values {
        let found = (start + 1..end).find(|&index| {
            lines[index]
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == *name)
        });
        match (found, value) {
            (Some(index), Some(value)) => lines[index] = format!("{name} = {value}"),
            (Some(index), None) => lines[index] = String::from("\u{0}"),
            (None, Some(value)) => missing.push(format!("{name} = {value}")),
            (None, None) => {}
        }
    }
    // Insert after the table's last non-blank line.
    let mut insert_at = end;
    while insert_at > start + 1 && lines[insert_at - 1].trim().is_empty() {
        insert_at -= 1;
    }
    for (offset, line) in missing.into_iter().enumerate() {
        lines.insert(insert_at + offset, line);
    }
    lines.retain(|line| line != "\u{0}");
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// A login the `smol` CLI left on disk.
#[derive(Debug, Default)]
pub struct CliSession {
    /// The stored key, absent if there is none or it has expired.
    pub api_key: Option<String>,
    /// The stored endpoint, if the login named one.
    pub endpoint: Option<String>,
    /// What renews the key, if the login stored it.
    pub refresh_token: Option<String>,
    /// When the key expires, in seconds since the epoch, if recorded.
    pub expires_at: Option<i64>,
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
        Self::read_from(&path)
    }

    fn read_from(path: &Path) -> Self {
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
                .filter(|value| !value.is_empty())
        };
        let expires_at = lookup("token_expires_at").as_deref().and_then(parse_expiry);
        let api_key = lookup("api_key").filter(|_| !expiring(expires_at));
        Self {
            api_key,
            endpoint: lookup("endpoint"),
            refresh_token: lookup("refresh_token"),
            expires_at,
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

/// The CLI records expiry as seconds since the epoch; older writers used an
/// RFC 3339 stamp. Anything else is no expiry at all, so a usable key is never
/// rejected over a formatting quirk.
fn parse_expiry(stamp: &str) -> Option<i64> {
    stamp
        .parse::<i64>()
        .ok()
        .or_else(|| parse_rfc3339_secs(stamp))
}

/// Expired, or close enough that it would lapse in flight.
fn expiring(expires_at: Option<i64>) -> bool {
    expires_at.is_some_and(|at| at - EXPIRY_SKEW_SECS <= now_secs())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
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
        assert_eq!(parse_expiry("whenever"), None);
        assert_eq!(parse_expiry(""), None);
        assert!(!expiring(None));
    }

    #[test]
    fn the_cli_records_expiry_in_epoch_seconds() {
        // What `smol auth login` actually writes: an unquoted integer.
        let expired = CliSession::parse(
            "[cloud]\napi_key = \"jwt-old\"\nrefresh_token = \"rt\"\ntoken_expires_at = 946684800\n",
        );
        assert!(
            expired.api_key.is_none(),
            "an expired session must not be sent"
        );
        assert_eq!(expired.refresh_token.as_deref(), Some("rt"));
        assert_eq!(expired.expires_at, Some(946_684_800));
        let valid = CliSession::parse(&format!(
            "[cloud]\napi_key = \"jwt-new\"\ntoken_expires_at = {}\n",
            now_secs() + 3_600
        ));
        assert_eq!(valid.api_key.as_deref(), Some("jwt-new"));
    }

    #[test]
    fn a_session_about_to_lapse_counts_as_expired() {
        assert!(expiring(Some(now_secs() + EXPIRY_SKEW_SECS / 2)));
        assert!(!expiring(Some(now_secs() + EXPIRY_SKEW_SECS * 10)));
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

    /// A config as `smol auth login` writes it, with a docker login beside it
    /// that a renewal must leave alone.
    fn cli_config(access: &str, refresh: &str, expires_at: i64) -> String {
        format!(
            "[cloud]\napi_key = \"{access}\"\nrefresh_token = \"{refresh}\"\ntoken_expires_at = {expires_at}\n\n\
             [machines.registries.\"docker.io\"]\nusername = \"me\"\npassword = \"hunter2\"\n\n\
             [machines.registries.\"registry.smolmachines.com\"]\nidentity_token = \"{access}\"\n\
             refresh_token = \"{refresh}\"\nexpires_at = {expires_at}\n\n[machines.defaults]\n"
        )
    }

    fn scratch_config(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("smol-cloud-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn session_credentials(config: &Path, issuer: &str) -> Credentials {
        let session = CliSession::read_from(config);
        let mut credentials = Credentials::with_key(
            "https://cloud.test",
            Key {
                access: "jwt-old".to_string(),
                refresh: session.refresh_token,
                expires_at: session.expires_at,
            },
            Some(config.to_path_buf()),
        );
        credentials.issuer = issuer.to_string();
        credentials
    }

    #[cfg(feature = "blocking")]
    /// Answer every `/oauth/token` request with a fresh pair, counting them.
    fn token_endpoint() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = served.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = vec![0_u8; 8192];
                let read = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.starts_with("POST /oauth/token"), "{request}");
                assert!(request.contains("grant_type=refresh_token"), "{request}");
                assert!(request.contains("refresh_token=rt-old"), "{request}");
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = r#"{"access_token":"jwt-new","refresh_token":"rt-new","expires_in":86400,"token_type":"Bearer"}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (address, served)
    }

    #[test]
    fn a_table_edit_touches_only_the_named_keys() {
        let text = "[cloud]\napi_key = \"a\"\nother = 1\n\n[next]\napi_key = \"keep\"\n";
        let edited = set_table_values(
            text,
            "[cloud]",
            &[
                ("api_key", Some("\"b\"".into())),
                ("other", None),
                ("added", Some("2".into())),
            ],
        );
        assert_eq!(
            edited,
            "[cloud]\napi_key = \"b\"\nadded = 2\n\n[next]\napi_key = \"keep\"\n"
        );
        assert_eq!(
            set_table_values(text, "[absent]", &[("x", Some("1".into()))]),
            text
        );
    }

    #[cfg(feature = "blocking")]
    #[test]
    fn an_expired_cli_session_is_renewed_and_written_back_in_both_places() {
        let (issuer, served) = token_endpoint();
        let config = scratch_config("renew", &cli_config("jwt-old", "rt-old", 946_684_800));
        let credentials = session_credentials(&config, &issuer);

        credentials.renew_if_expiring().unwrap();
        assert_eq!(credentials.api_key(), "jwt-new");
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1);

        let written = std::fs::read_to_string(&config).unwrap();
        let session = CliSession::parse(&written);
        assert_eq!(session.api_key.as_deref(), Some("jwt-new"));
        assert_eq!(session.refresh_token.as_deref(), Some("rt-new"));
        assert!(session.expires_at.unwrap() > now_secs() + 3_600);
        assert!(
            written.contains("identity_token = \"jwt-new\""),
            "{written}"
        );
        assert_eq!(
            written.matches("refresh_token = \"rt-new\"").count(),
            2,
            "{written}"
        );
        assert!(written.contains("password = \"hunter2\""), "{written}");
        assert!(
            !written.contains("jwt-old") && !written.contains("rt-old"),
            "{written}"
        );

        // Still good: no second trip.
        credentials.renew_if_expiring().unwrap();
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(config.parent().unwrap());
    }

    #[test]
    fn a_key_another_process_already_renewed_is_adopted_without_a_refresh() {
        // Nothing listens here: any token request would fail the test.
        let config = scratch_config("adopt", &cli_config("jwt-old", "rt-old", 946_684_800));
        let credentials = session_credentials(&config, "http://127.0.0.1:9");
        std::fs::write(
            &config,
            cli_config("jwt-other", "rt-other", now_secs() + 86_400),
        )
        .unwrap();

        assert!(credentials.renew_after_rejection("jwt-old").unwrap());
        assert_eq!(credentials.api_key(), "jwt-other");
        let _ = std::fs::remove_dir_all(config.parent().unwrap());
    }

    #[cfg(feature = "blocking")]
    #[test]
    fn concurrent_rejections_renew_once() {
        let (issuer, served) = token_endpoint();
        let config = scratch_config(
            "once",
            &cli_config("jwt-old", "rt-old", now_secs() + 86_400),
        );
        let credentials = session_credentials(&config, &issuer);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let credentials = credentials.clone();
                std::thread::spawn(move || credentials.renew_after_rejection("jwt-old").unwrap())
            })
            .collect();
        for thread in threads {
            assert!(thread.join().unwrap());
        }
        assert_eq!(credentials.api_key(), "jwt-new");
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(config.parent().unwrap());
    }

    #[test]
    fn an_explicit_key_is_never_renewed() {
        let credentials = Credentials::new("https://cloud.test", "smk_given");
        credentials.renew_if_expiring().unwrap();
        assert!(!credentials.renew_after_rejection("smk_given").unwrap());
        assert_eq!(credentials.api_key(), "smk_given");
    }
}
