// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Host timing selection for the shared paste state machine.

#[cfg(not(windows))]
pub type PasteBurst = super::paste_burst_state::PasteBurst<8>;
#[cfg(windows)]
pub type PasteBurst = super::paste_burst_state::PasteBurst<30>;
