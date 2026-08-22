//! Shared error types for the Fero foundation.

use std::fmt::{self, Display, Formatter};

/// Result alias used across the Fero foundation.
pub type Result<T> = std::result::Result<T, VaultError>;

/// Error type for path validation, planning, and serialization failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// The configured vault root is not usable.
    InvalidVaultPath(String),
    /// A relative vault path escaped the vault boundary.
    InvalidRelativePath(String),
    /// An unsupported or malformed media type was encountered.
    InvalidMediaType(String),
    /// A property value could not be accepted.
    InvalidProperty(String),
    /// A dry-run or import plan could not be created.
    Planning(String),
    /// The desktop application could not start.
    AppStartup(String),
    /// Metadata serialization failed.
    Serialization(String),
    /// An external metadata API failed.
    ExternalApi(String),
    /// Duplicate detection could not be performed.
    DuplicateDetectionUnavailable,
    /// The code could not determine a file name.
    MissingFileName,
    /// Wrapper for I/O failures.
    Io(String),
}

impl Display for VaultError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVaultPath(message) => write!(f, "invalid vault path: {message}"),
            Self::InvalidRelativePath(message) => {
                write!(f, "invalid relative vault path: {message}")
            }
            Self::InvalidMediaType(message) => write!(f, "invalid media type: {message}"),
            Self::InvalidProperty(message) => write!(f, "invalid property: {message}"),
            Self::Planning(message) => write!(f, "import planning failed: {message}"),
            Self::AppStartup(message) => write!(f, "app startup failed: {message}"),
            Self::Serialization(message) => write!(f, "serialization failed: {message}"),
            Self::ExternalApi(message) => write!(f, "external api failure: {message}"),
            Self::DuplicateDetectionUnavailable => write!(f, "duplicate detection unavailable"),
            Self::MissingFileName => write!(f, "missing file name"),
            Self::Io(message) => write!(f, "i/o failure: {message}"),
        }
    }
}

impl std::error::Error for VaultError {}

impl From<std::io::Error> for VaultError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
