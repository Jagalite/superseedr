// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use thiserror::Error;

#[cfg(not(target_arch = "wasm32"))]
#[derive(Error, Debug)]
pub enum TrackerError {
    #[error("Request failed networking with tracker.")]
    Request(#[from] reqwest::Error),

    #[error("Tracker I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse bencoded tracker response")]
    Bencode(#[from] serde_bencode::Error),

    #[error("Tracker returned a failure reason: {0}")]
    Tracker(String),

    #[error("Invalid tracker URL: {0}")]
    InvalidUrl(String),

    #[error("Tracker protocol error: {0}")]
    Protocol(String),
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    #[error("I/O error ({kind:?}): {message}")]
    Io {
        kind: std::io::ErrorKind,
        message: String,
    },

    #[error("Expected a regular file but found a different filesystem entry")]
    UnexpectedType,

    #[error("Size mismatch: expected {expected_size} bytes, found {observed_size} bytes")]
    SizeMismatch {
        expected_size: u64,
        observed_size: u64,
    },
}

impl From<std::io::Error> for StorageError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl StorageError {
    pub fn indicates_data_unavailability(&self) -> bool {
        match self {
            Self::Io { kind, .. } => matches!(
                kind,
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::IsADirectory
                    | std::io::ErrorKind::NotADirectory
            ),
            Self::UnexpectedType | Self::SizeMismatch { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StorageError;

    #[test]
    fn wrong_type_path_io_errors_mark_data_unavailable() {
        for kind in [
            std::io::ErrorKind::IsADirectory,
            std::io::ErrorKind::NotADirectory,
        ] {
            let error = StorageError::from(std::io::Error::new(kind, "wrong entry type"));
            assert!(error.indicates_data_unavailability());
        }
    }
}
