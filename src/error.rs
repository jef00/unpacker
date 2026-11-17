//! One error type for the whole library.
//!
//! Messages are written for end users (people dragging files onto an .exe),
//! not for developers. Keep them short and say what to do about it.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Part \"{0}\" is missing. Put all parts in the same folder and try again.")]
    MissingVolume(String),

    #[error("\"{0}\" and \"{1}\" both claim to be the same part. Remove one of them.")]
    ConflictingVolumes(String, String),

    #[error("\"{}\" is not a RAR, 7z, ZIP or tar archive.", file_name(.0))]
    UnknownFormat(PathBuf),

    #[error("Skipped \"{0}\": the archive tries to write outside the target folder.")]
    UnsafePath(String),

    #[error("This archive is password protected.")]
    PasswordRequired,

    #[error("Wrong password.")]
    WrongPassword,

    #[error("The archive is damaged: {0}")]
    Corrupt(String),

    #[error("Not supported yet: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Just the file name, like every other message; the full path is noise.
fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}
