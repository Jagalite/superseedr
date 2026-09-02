// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-neutral state and reducers shared by terminal runtimes.
//!
//! This module contains no terminal I/O, async-runtime ownership, filesystem,
//! network, storage, or browser-simulation integration.

pub mod effects;
pub mod paste_burst;
pub mod reducer;
pub mod render;
pub mod state;
pub mod terminal_event;
