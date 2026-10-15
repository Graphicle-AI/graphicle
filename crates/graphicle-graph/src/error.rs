// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__init__.py
//! Errors retain their cause in Rust; the compatibility wire format remains text.
use serde::{Serialize, Serializer};
use std::{fmt, io};

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    InvalidInput(String),
    InvalidGraph(String),
    UnsupportedOperation(String),
    LimitExceeded {
        resource: &'static str,
        limit: usize,
    },
    Io(io::Error),
    Json(serde_json::Error),
    Parse(Box<dyn std::error::Error + Send + Sync>),
    Provider(Box<dyn std::error::Error + Send + Sync>),
    #[cfg(unix)]
    System(nix::errno::Errno),
    Model(crate::llm::Error),
    External(String),
    Internal(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput,
    InvalidGraph,
    UnsupportedOperation,
    LimitExceeded,
    Io,
    Json,
    Parse,
    Model,
    External,
    Internal,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidInput(_) => ErrorKind::InvalidInput,
            Self::InvalidGraph(_) => ErrorKind::InvalidGraph,
            Self::UnsupportedOperation(_) => ErrorKind::UnsupportedOperation,
            Self::LimitExceeded { .. } => ErrorKind::LimitExceeded,
            Self::Io(_) => ErrorKind::Io,
            Self::Json(_) => ErrorKind::Json,
            Self::Parse(_) => ErrorKind::Parse,
            Self::Provider(_) => ErrorKind::External,
            #[cfg(unix)]
            Self::System(_) => ErrorKind::Io,
            Self::Model(_) => ErrorKind::Model,
            Self::External(_) => ErrorKind::External,
            Self::Internal(_) => ErrorKind::Internal,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    pub fn graph(message: impl Into<String>) -> Self {
        Self::InvalidGraph(message.into())
    }

    pub fn parse(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Parse(Box::new(error))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(s)
            | Self::InvalidGraph(s)
            | Self::UnsupportedOperation(s)
            | Self::External(s)
            | Self::Internal(s) => f.write_str(s),
            Self::LimitExceeded { resource, limit } => {
                write!(f, "{resource} exceeds {limit} bytes")
            }
            Self::Io(e) => e.fmt(f),
            Self::Json(e) => e.fmt(f),
            Self::Parse(e) | Self::Provider(e) => e.fmt(f),
            #[cfg(unix)]
            Self::System(e) => e.fmt(f),
            Self::Model(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::Parse(e) | Self::Provider(e) => Some(e.as_ref()),
            #[cfg(unix)]
            Self::System(e) => Some(e),
            Self::Model(e) => Some(e),
            _ => None,
        }
    }
}

// Preserve the established JSONL error field. Native callers match Error/Kind.
impl Serialize for Error {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<std::fs::TryLockError> for Error {
    fn from(error: std::fs::TryLockError) -> Self {
        Self::Io(match error {
            std::fs::TryLockError::Error(error) => error,
            std::fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        })
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
impl From<crate::llm::Error> for Error {
    fn from(error: crate::llm::Error) -> Self {
        Self::Model(error)
    }
}
impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::InvalidInput(message)
    }
}
impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self::invalid(message)
    }
}

macro_rules! from_source {
    ($variant:ident: $($source:ty),+ $(,)?) => {$(
        impl From<$source> for Error {
            fn from(error: $source) -> Self { Self::$variant(Box::new(error)) }
        }
    )+};
}
from_source!(Parse: regex::Error, fancy_regex::Error, glob::PatternError,
    std::string::FromUtf8Error, std::num::ParseIntError, std::num::ParseFloatError,
    roxmltree::Error, chrono::ParseError, tree_sitter::LanguageError,
    yaml_rust2::scanner::ScanError, zip::result::ZipError, calamine::XlsxError);
from_source!(Provider: reqwest::Error, neo4rs::Error, redis::RedisError);
#[cfg(unix)]
impl From<nix::errno::Errno> for Error {
    fn from(error: nix::errno::Errno) -> Self {
        Self::System(error)
    }
}
impl<T> From<std::sync::PoisonError<T>> for Error {
    fn from(error: std::sync::PoisonError<T>) -> Self {
        Self::Internal(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn io_kind_and_source_survive_the_native_boundary() {
        let error = Error::from(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "read denied",
        ));
        assert_eq!(error.kind(), ErrorKind::Io);
        assert_eq!(
            error
                .source()
                .and_then(|s| s.downcast_ref::<io::Error>())
                .map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
        assert_eq!(serde_json::to_value(&error).unwrap(), "read denied");
    }

    #[test]
    fn model_timeouts_remain_distinguishable_from_bad_input() {
        let error = Error::from(crate::llm::Error::timeout("deadline"));
        assert!(matches!(
            error,
            Error::Model(crate::llm::Error {
                kind: crate::llm::ErrorKind::Timeout,
                ..
            })
        ));
        assert_eq!(Error::invalid("missing id").kind(), ErrorKind::InvalidInput);
    }
}
