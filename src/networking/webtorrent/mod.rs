// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! WebTorrent signaling and peer transport.
//!
//! This module is intentionally feature-gated. WebSocket trackers perform signaling, while an
//! ordered binary WebRTC DataChannel is adapted into the byte stream consumed by the existing
//! BitTorrent peer-wire session.

pub mod rtc;
pub mod signaling;
pub mod stream;
pub mod tracker_worker;

#[cfg(test)]
pub(crate) mod browser_interop_tests;

pub const DATA_CHANNEL_LABEL: &str = "webtorrent";
pub const MAX_SDP_SIZE: usize = 64 * 1024;
pub const STREAM_BUFFER_SIZE: usize = 256 * 1024;
pub const SEND_BUFFER_LIMIT: usize = 1024 * 1024;
pub const MAX_OFFERS_PER_ANNOUNCE: usize = 10;
pub const MAX_SIGNALING_MESSAGE_SIZE: usize = 128 * 1024;
pub const NEGOTIATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
