// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-neutral resource-limit vocabulary used by application state.

mod native;

pub(crate) use native::*;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum ResourceType {
    Reserve,
    PeerConnection,
    DiskRead,
    DiskWrite,
}
