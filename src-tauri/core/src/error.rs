//! Error types shared by the providers and the service layer.

use serde::Serialize;
use thiserror::Error;

/// An error coming from a social network (or from talking to it).
///
/// The variants decide how the service reacts: `Unauthorized` triggers a token
/// refresh and, if that fails too, marks the account as needing a reconnect.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum ProviderError {
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{message}")]
    RateLimited {
        message: String,
        retry_after: Option<u64>,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Server(String),
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    pub fn kind(&self) -> &'static str {
        match self {
            ProviderError::Unauthorized(_) => "unauthorized",
            ProviderError::Forbidden(_) => "forbidden",
            ProviderError::RateLimited { .. } => "rateLimited",
            ProviderError::Invalid(_) => "invalid",
            ProviderError::NotFound(_) => "notFound",
            ProviderError::Unsupported(_) => "unsupported",
            ProviderError::Network(_) => "network",
            ProviderError::Server(_) => "server",
            ProviderError::Other(_) => "other",
        }
    }

    pub fn other(msg: impl Into<String>) -> Self {
        ProviderError::Other(msg.into())
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        ProviderError::Invalid(msg.into())
    }

    pub fn unsupported(msg: impl Into<String>) -> Self {
        ProviderError::Unsupported(msg.into())
    }

    /// Error for a response body that did not have the shape we expected.
    pub fn parse(what: &str, err: impl std::fmt::Display) -> Self {
        ProviderError::Other(format!("Unexpected response from {what}: {err}"))
    }
}

impl From<reqwest::Error> for ProviderError {
    fn from(err: reqwest::Error) -> Self {
        // URLs can carry secrets (bot tokens, webhook tokens); never show them.
        let err = err.without_url();
        if err.is_timeout() {
            ProviderError::Network("The server took too long to answer.".into())
        } else if err.is_connect() {
            ProviderError::Network(format!("Could not connect: {}", root_cause(&err)))
        } else if err.is_decode() {
            ProviderError::Other(format!("Could not read the server's answer: {}", root_cause(&err)))
        } else {
            ProviderError::Network(root_cause(&err))
        }
    }
}

fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        let s_msg = s.to_string();
        if !msg.contains(&s_msg) {
            msg = format!("{msg}: {s_msg}");
        }
        source = s.source();
    }
    msg
}

/// Errors returned by the service layer (and to the user interface).
#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("Database error: {0}")]
    Db(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("Credential storage error: {0}")]
    Secret(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Cancelled(String),
    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub fn kind(&self) -> &'static str {
        match self {
            AppError::Provider(p) => p.kind(),
            AppError::Db(_) => "db",
            AppError::NotFound(_) => "notFound",
            AppError::Invalid(_) => "invalid",
            AppError::Secret(_) => "secret",
            AppError::Io(_) => "io",
            AppError::Cancelled(_) => "cancelled",
            AppError::Other(_) => "other",
        }
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        AppError::Invalid(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        AppError::NotFound(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        AppError::Other(msg.into())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        AppError::Db(err.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::Io(err.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        AppError::Other(format!("Data error: {err}"))
    }
}

/// How errors travel to the frontend: `{ "kind": "...", "message": "..." }`.
impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("AppError", 2)?;
        s.serialize_field("kind", self.kind())?;
        s.serialize_field("message", &self.to_string())?;
        s.end()
    }
}

pub type AppResult<T> = Result<T, AppError>;
pub type PResult<T> = Result<T, ProviderError>;
