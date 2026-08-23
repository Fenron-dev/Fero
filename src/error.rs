//! Shared error type for Fero.

use std::fmt::{self, Display, Formatter};

/// Result alias used throughout Fero.
pub type Result<T> = std::result::Result<T, FeroError>;

/// Everything that can go wrong while acquiring and delivering a work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeroError {
    /// No usable delivery target — nothing configured, or it is offline.
    InvalidTarget(String),
    /// A path was rejected before it could touch the filesystem.
    InvalidPath(String),
    /// A property value could not be accepted.
    InvalidProperty(String),
    /// The desktop application could not start.
    AppStartup(String),
    /// Metadata serialization failed.
    Serialization(String),
    /// An external metadata API failed.
    ExternalApi(String),
    /// Wrapper for I/O failures.
    Io(String),
}

impl Display for FeroError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget(message) => write!(f, "kein nutzbares Ziel: {message}"),
            Self::InvalidPath(message) => write!(f, "ungültiger Pfad: {message}"),
            Self::InvalidProperty(message) => write!(f, "invalid property: {message}"),
            Self::AppStartup(message) => write!(f, "app startup failed: {message}"),
            Self::Serialization(message) => write!(f, "serialization failed: {message}"),
            Self::ExternalApi(message) => write!(f, "external api failure: {message}"),
            Self::Io(message) => write!(f, "i/o failure: {message}"),
        }
    }
}

impl std::error::Error for FeroError {}

impl From<std::io::Error> for FeroError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
