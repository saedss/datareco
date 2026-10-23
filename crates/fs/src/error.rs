/// Errors from reading a file system.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error(transparent)]
    Disk(#[from] datareco_io::Error),

    /// The on-disk structures don't make sense.
    #[error("{0}")]
    Damaged(String),

    #[error("{0}")]
    Unsupported(String),

    #[error("{0} was not found")]
    NotFound(String),
}

impl FsError {
    /// A short, stable name for the kind of error, for the desktop app.
    pub fn kind(&self) -> &'static str {
        match self {
            FsError::Disk(error) => error.kind(),
            FsError::Damaged(_) => "damaged",
            FsError::Unsupported(_) => "unsupported",
            FsError::NotFound(_) => "not_found",
        }
    }

    pub(crate) fn damaged(message: impl Into<String>) -> Self {
        FsError::Damaged(message.into())
    }
}
