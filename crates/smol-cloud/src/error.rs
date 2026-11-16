//! What a control-plane call can fail with.

use std::fmt;

/// Why a cloud call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The machine, checkpoint or route does not exist.
    NotFound,
    /// The credential was missing, rejected or expired.
    Unauthorized,
    /// The server refused because of the machine's current state.
    Conflict,
    /// The request outlived its deadline.
    Timeout,
    /// The caller stopped an operation before it completed.
    Cancelled,
    /// The request could not be delivered.
    Connection,
    /// The machine stopped or became unreachable while a command ran in it.
    /// Retrying can succeed, but the command may have partly run.
    MachineInterrupted,
    /// Anything else the server reported.
    Other,
}

impl ErrorKind {
    /// The stable string code, shared with the Node and Python SDKs.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Conflict => "CONFLICT",
            Self::Timeout => "TIMEOUT",
            Self::Cancelled => "CANCELLED",
            Self::Connection => "CONNECTION",
            Self::MachineInterrupted => "MACHINE_INTERRUPTED",
            Self::Other => "SMOLVM_ERROR",
        }
    }

    /// The kind an HTTP status maps to.
    pub fn from_status(status: u16) -> Self {
        match status {
            404 => Self::NotFound,
            401 | 403 => Self::Unauthorized,
            409 => Self::Conflict,
            _ => Self::Other,
        }
    }
}

/// A failed control-plane call.
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    request_id: Option<String>,
    status: Option<u16>,
    problem_code: Option<String>,
    retryable: bool,
    retry_after: Option<std::time::Duration>,
}

impl Error {
    /// Build an error.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            request_id: None,
            status: None,
            problem_code: None,
            retryable: false,
            retry_after: None,
        }
    }

    /// Attach what the server's response said: its status, problem code,
    /// whether a retry can succeed, and how long it asked callers to wait.
    pub fn with_response(
        mut self,
        status: u16,
        problem_code: Option<String>,
        retryable: bool,
        retry_after: Option<std::time::Duration>,
    ) -> Self {
        self.status = Some(status);
        self.problem_code = problem_code;
        self.retryable = retryable;
        self.retry_after = retry_after;
        self
    }

    /// HTTP status, for an error the cloud API returned.
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// The server's stable problem code, e.g. `machine_interrupted`.
    pub fn problem_code(&self) -> Option<&str> {
        self.problem_code.as_deref()
    }

    /// Whether sending the same request again can succeed. A command whose
    /// machine went away is retryable, but it may have partly run, so the
    /// client leaves that retry to the caller.
    pub fn retryable(&self) -> bool {
        self.retryable
    }

    /// How long the server asked callers to wait before retrying.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_after
    }

    /// Attach the server's correlation id.
    pub fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    /// What class of failure this is.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The description, without the code or correlation id.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The server's correlation id, when it sent one. Support needs this to
    /// find the call, and a client that only prints the body never sees it.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind.code(), self.message)?;
        if let Some(request_id) = &self.request_id {
            write!(f, " [request id: {request_id}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// The sentence a person should read from a failed response's body.
///
/// The control plane answers errors as RFC 9457 `application/problem+json`,
/// whose `detail` is the sentence; older builds sent `{"error": …}` or plain
/// text. Pasting the body whole would show JSON, so a JSON object yields its
/// first string among `detail`, `error`, `message` and `title`, and anything
/// else is returned trimmed, as before.
pub fn body_sentence(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(trimmed) {
        for key in ["detail", "error", "message", "title"] {
            if let Some(serde_json::Value::String(sentence)) = fields.get(key) {
                return sentence.trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

/// This crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::body_sentence;

    #[test]
    fn a_problem_document_yields_its_detail() {
        let body = r#"{"type":"about:blank","title":"Payment Required","status":402,"detail":"this organization has no credit yet","code":"payment_required"}"#;
        assert_eq!(body_sentence(body), "this organization has no credit yet");
    }

    #[test]
    fn the_older_error_shape_and_plain_text_still_read() {
        assert_eq!(
            body_sentence(r#"{"error":"API key has expired","code":"expired_key"}"#),
            "API key has expired"
        );
        assert_eq!(
            body_sentence("machine count quota exceeded\n"),
            "machine count quota exceeded"
        );
        // JSON with no sentence field, and text that only looks like JSON, pass through.
        assert_eq!(body_sentence(r#"{"code":"x"}"#), r#"{"code":"x"}"#);
        assert_eq!(body_sentence("{not json"), "{not json");
        assert_eq!(body_sentence("  "), "");
    }
}
