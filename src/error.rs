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
    /// A cross-device move was refused before copying anything because
    /// these source entries could not be removed afterwards.
    #[error("can't move: {} item(s) can't be removed from the source, nothing was copied", .0.len())]
    NotRemovable(Vec<crate::fsops::KeptEntry>),
    /// Copy instead was refused because these folders can't be read.
    #[error("can't copy: {} folder(s) can't be read, nothing was copied", .0.len())]
    Unreadable(Vec<crate::fsops::KeptEntry>),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: impl Into<std::io::Error>) -> Self {
        Self::Io {
            path: path.into(),
            source: source.into(),
        }
    }

    /// The path no longer exists or is no longer a folder.
    pub fn is_gone(&self) -> bool {
        match self {
            Self::Io { source, .. } => matches!(
                source.raw_os_error(),
                Some(e) if e == libc::ENOENT || e == libc::ENOTDIR
            ),
            _ => false,
        }
    }

    pub fn from_errno(path: impl Into<PathBuf>, err: rustix::io::Errno) -> Self {
        Self::io(path, std::io::Error::from(err))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
