// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![cfg_attr(target_arch = "wasm32", allow(dead_code, unused_imports))]

pub mod action_style;
pub mod component_visualizations;
pub mod effects;
#[cfg(test)]
pub mod events;
pub mod formatters;
pub(crate) mod interaction_effects;
pub mod layout;
pub mod particles;
pub mod paste_burst;
mod paste_burst_state;
pub mod peer_stream;
pub(crate) mod reducer;
pub(crate) mod render;
pub mod screen_context;
pub mod screens;
pub(crate) mod state;
pub(crate) mod terminal_event;
pub mod tree;
