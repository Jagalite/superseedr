// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Errors produced by native tracker communication.

use thiserror::Error;

#[derive(Error, Debug)]
pub(crate) enum TrackerError {
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
