//! Error types and converters.

use data_encoding::DecodeError;
use std::io;
use std::path::Path;
use thiserror::Error;

use crate::metadata::Role;

/// Error type for all TUF related errors.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum Error {
    /// The metadata had a bad signature.
    #[error("bad signature")]
    BadSignature,

    /// There was a problem encoding or decoding.
    #[error("encoding: {0}")]
    Encoding(String),

    /// Metadata was expired.
    #[error("expired {0} metadata")]
    ExpiredMetadata(Role),

    /// An illegal argument was passed into a function.
    #[error("illegal argument: {0}")]
    IllegalArgument(String),

    /// Generic error for HTTP connections.
    #[error("http: {0}")]
    Http(http::Error),

    /// Errors that can occur parsing HTTP streams.
    #[error("hyper: {0}")]
    Hyper(hyper::Error),

    /// The metadata was missing, so an operation could not be completed.
    #[error("missing {0} metadata")]
    MissingMetadata(Role),

    /// There were no available hash algorithms.
    #[error("no supported hash algorithm")]
    NoSupportedHashAlgorithm,

    /// The metadata or target was not found.
    #[error("not found")]
    NotFound,

    /// Opaque error type, to be interpreted similar to HTTP 500. Something went wrong, and you may
    /// or may not be able to do anything about it.
    #[error("opaque: {0}")]
    Opaque(String),

    /// There was a library internal error. These errors are *ALWAYS* bugs and should be reported.
    #[error("programming: {0}")]
    Programming(String),

    /// The target is unavailable. This may mean it is either not in the metadata or the metadata
    /// chain to the target cannot be fully verified.
    #[error("target unavailable")]
    TargetUnavailable,

    /// There is no known or available key type.
    #[error("unknown key type: {0}")]
    UnknownKeyType(String),

    /// The metadata or target failed to verify.
    #[error("verification failure: {0}")]
    VerificationFailure(String),
}

impl From<serde_json::error::Error> for Error {
    fn from(err: serde_json::error::Error) -> Error {
        Error::Encoding(format!("JSON: {:?}", err))
    }
}

impl Error {
    /// Helper to include the path that causd the error for FS I/O errors.
    pub fn from_io(err: &io::Error, path: &Path) -> Error {
        Error::Opaque(format!("Path {:?} : {:?}", path, err))
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        match err.kind() {
            std::io::ErrorKind::NotFound => Error::NotFound,
            _ => Error::Opaque(format!("IO: {:?}", err)),
        }
    }
}

impl From<http::Error> for Error {
    fn from(err: http::Error) -> Error {
        Error::Http(err)
    }
}

impl From<hyper::Error> for Error {
    fn from(err: hyper::Error) -> Error {
        Error::Hyper(err)
    }
}

impl From<DecodeError> for Error {
    fn from(err: DecodeError) -> Error {
        Error::Encoding(format!("{:?}", err))
    }
}

impl From<derp::Error> for Error {
    fn from(err: derp::Error) -> Error {
        Error::Encoding(format!("DER: {:?}", err))
    }
}

impl From<tempfile::PersistError> for Error {
    fn from(err: tempfile::PersistError) -> Error {
        Error::Opaque(format!("Error persisting temp file: {:?}", err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_io_error_display_string() {
        let err = Error::from(io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(err.to_string(), "not found");
        assert_eq!(Error::NotFound.to_string(), "not found");

        let err = Error::from(io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert_eq!(err.to_string(), "opaque: IO: Kind(PermissionDenied)");
    }
}
