// Ported from FFmpeg libavcodec/mlpdec.c error handling conventions
// (commit 2da55bf). Licensed under LGPL-2.1-or-later.

//! Crate-local error type (kept separate from `oxideav_core::Error` so the
//! decoder core can be exercised without the registry).

/// Errors the MLP decoder can produce on malformed input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The input violates the format's rules; not retryable.
    InvalidData(String),
    /// The input uses a feature this decoder does not implement.
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidData(m) => write!(f, "invalid data: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for oxideav_core::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidData(m) => oxideav_core::Error::InvalidData(m),
            Error::Unsupported(m) => oxideav_core::Error::Unsupported(m),
        }
    }
}
