// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application version model definitions and transitions.

#[derive(serde::Deserialize)]
pub(super) struct CratesResponse {
    #[serde(rename = "crate")]
    pub(super) krate: CrateInfo,
}

#[derive(serde::Deserialize)]
pub(super) struct CrateInfo {
    pub(super) max_version: String,
}

pub(super) type VersionCheckError = Box<dyn std::error::Error + Send + Sync>;
