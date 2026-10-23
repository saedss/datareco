use std::io;

/// Errors from opening, reading or writing a disk, volume or image file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The OS refused access. On Windows, reading disks and volumes needs
    /// administrator rights.
    #[error("access to {name} was denied; reading disks and volumes needs administrator rights")]
    AccessDenied { name: String },

    #[error("{name} was not found")]
    NotFound { name: String },

    #[error(
        "cannot read {len} bytes at offset {offset} of {name}, which is only {size} bytes long"
    )]
    OutOfRange {
        name: String,
        offset: u64,
        len: u64,
        size: u64,
    },

    /// A copy or write that would be unsafe, such as writing over the disk
    /// being copied from. The text says why, for people.
    #[error("{reason}")]
    Refused { reason: String },

    #[error("{what} is not supported on this platform yet")]
    Unsupported { what: String },

    #[error("{name}: {source}")]
    Io {
        name: String,
        #[source]
        source: io::Error,
    },
}

impl Error {
    /// A short, stable name for the kind of error, for the desktop app.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::AccessDenied { .. } => "access_denied",
            Error::NotFound { .. } => "not_found",
            Error::OutOfRange { .. } => "out_of_range",
            Error::Refused { .. } => "refused",
            Error::Unsupported { .. } => "unsupported",
            Error::Io { .. } => "io",
        }
    }

    /// Wraps an OS error, turning "permission denied" and "not found" into
    /// their own variants.
    pub(crate) fn from_io(name: &str, source: io::Error) -> Self {
        let name = name.to_owned();
        match source.kind() {
            io::ErrorKind::PermissionDenied => Error::AccessDenied { name },
            io::ErrorKind::NotFound => Error::NotFound { name },
            _ => Error::Io { name, source },
        }
    }
}

impl Error {
    pub(crate) fn refused(reason: impl Into<String>) -> Self {
        Error::Refused {
            reason: reason.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
