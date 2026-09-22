// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native process composition and optional native-only tooling.

pub(crate) mod entrypoint;
pub(crate) mod logging;

#[cfg(feature = "synthetic-load")]
pub(crate) mod synthetic_load;
