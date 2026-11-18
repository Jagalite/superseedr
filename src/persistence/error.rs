// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Errors produced by persistence capabilities.

use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StorageError {
    #[error("I/O error ({kind:?}): {message}")]
    Io {
        #[serde(with = "io_error_kind")]
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

/// `io::ErrorKind` has no serde support. Kinds round-trip by name; kinds not
/// listed here (rare for file probes) decode as `Other`.
mod io_error_kind {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::io::ErrorKind;

    const KINDS: &[ErrorKind] = &[
        ErrorKind::NotFound,
        ErrorKind::PermissionDenied,
        ErrorKind::UnexpectedEof,
        ErrorKind::IsADirectory,
        ErrorKind::NotADirectory,
        ErrorKind::AlreadyExists,
        ErrorKind::InvalidData,
        ErrorKind::InvalidInput,
        ErrorKind::Interrupted,
        ErrorKind::TimedOut,
        ErrorKind::WouldBlock,
        ErrorKind::StorageFull,
        ErrorKind::ReadOnlyFilesystem,
        ErrorKind::Unsupported,
        ErrorKind::OutOfMemory,
        ErrorKind::Other,
    ];

    pub(super) fn serialize<S: Serializer>(
        kind: &ErrorKind,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{kind:?}"))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ErrorKind, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(KINDS
            .iter()
            .copied()
            .find(|kind| format!("{kind:?}") == name)
            .unwrap_or(ErrorKind::Other))
    }
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
