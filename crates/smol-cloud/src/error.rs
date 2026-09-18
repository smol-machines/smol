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
    /// The request could not be delivered.
    Connection,
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
            Self::Connection => "CONNECTION",
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
}

impl Error {
    /// Build an error.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            request_id: None,
        }
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

/// This crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
