use super::*;
use crate::types::CheckpointRequirements;

const PATH: &str = "/v1/checkpoints/check-format";
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE: usize = 16 * 1024;

fn cancelled_error() -> Error {
    Error::new(
        ErrorKind::Cancelled,
        "checkpoint compatibility check stopped",
    )
}

impl Client {
    /// Verify a ready cloud worker explicitly supports these reader requirements.
    /// This read-only POST reserves nothing and must run before pausing local work.
    /// Unknown endpoints, malformed answers and unsupported formats fail closed.
    /// Cancellation closes the request, including a stalled response body; a
    /// transient refusal is returned immediately so the source can stay running.
    pub fn check_checkpoint_format_cancellable(
        &self,
        requirements: &CheckpointRequirements,
        cancel: &StreamCancel,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::new(ErrorKind::Other, "cannot start compatibility check"))?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(TIMEOUT)
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| Error::new(ErrorKind::Connection, "cannot create compatibility client"))?;
        self.credentials.renew_if_expiring()?;
        let mut cancelled = cancel.subscribe();
        for attempt in 0..2 {
            if cancel.is_cancelled() {
                return Err(cancelled_error());
            }
            let key = self.credentials.api_key();
            let mut request = http
                .post(format!("{}{PATH}", self.credentials.base_url()))
                .bearer_auth(&key)
                .json(requirements)
                .timeout(TIMEOUT);
            if let Some(tenant) = &self.tenant {
                request = request.header("Smol-Tenant", tenant);
            }
            let (status, retry, id, bytes) = runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut cancelled) => Err(cancelled_error()),
                    result = async {
                        let mut response = request.send().await.map_err(network_error)?;
                        let status = response.status();
                        let retry = retry_after(response.headers());
                        let id = request_id(response.headers());
                        let mut bytes = Vec::new();
                        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
                            if bytes.len() + chunk.len() > MAX_RESPONSE {
                                return Err(Error::new(ErrorKind::Other, "compatibility response is too large"));
                            }
                            bytes.extend_from_slice(&chunk);
                        }
                        Ok((status, retry, id, bytes))
                    } => result,
                }
            })?;
            if status == reqwest::StatusCode::UNAUTHORIZED
                && attempt == 0
                && self.credentials.renew_after_rejection(&key)?
            {
                continue;
            }
            if status != reqwest::StatusCode::OK {
                return Err(status_error(
                    status,
                    &reqwest::Method::POST,
                    PATH,
                    &String::from_utf8_lossy(&bytes),
                    retry,
                )
                .with_request_id(id));
            }
            #[derive(serde::Deserialize)]
            struct Support {
                supported: bool,
            }
            let response: Support = serde_json::from_slice(&bytes).map_err(|_| {
                Error::new(
                    ErrorKind::Other,
                    "cloud did not confirm checkpoint compatibility",
                )
            })?;
            if !response.supported {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "cloud cannot restore this checkpoint format",
                ));
            }
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::Unauthorized,
            "compatibility authentication failed",
        ))
    }
}

fn network_error(error: reqwest::Error) -> Error {
    Error::new(
        if error.is_timeout() {
            ErrorKind::Timeout
        } else {
            ErrorKind::Connection
        },
        "checkpoint compatibility request failed",
    )
}
