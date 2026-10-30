use std::path::PathBuf;

/// Recoverable failure. Paths are included when the operation had one.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("refusing to touch a pseudo filesystem ({kind})")]
    Pseudo { kind: &'static str },
    #[error("cancelled")]
    Cancelled,
    #[error("invalid file name")]
    InvalidName,
    #[error("{0}")]
    Message(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: impl Into<std::io::Error>) -> Self {
        Self::Io {
            path: path.into(),
            source: source.into(),
        }
    }

    pub fn from_errno(path: impl Into<PathBuf>, err: rustix::io::Errno) -> Self {
        Self::io(path, std::io::Error::from(err))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
