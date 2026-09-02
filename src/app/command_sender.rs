// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Target-selected transport for ordered application-command batches.

#[cfg(target_arch = "wasm32")]
#[path = "command_sender/browser.rs"]
mod browser;
#[cfg(not(target_arch = "wasm32"))]
#[path = "command_sender/native.rs"]
mod native;

#[cfg(target_arch = "wasm32")]
pub(crate) use browser::spawn_app_command_batch_sender;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::spawn_app_command_batch_sender;
