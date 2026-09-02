// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Host timing selection for the platform-neutral paste state machine.

#[cfg(not(windows))]
pub type PasteBurst = super::kernel::paste_burst::PasteBurst<8>;
#[cfg(windows)]
pub type PasteBurst = super::kernel::paste_burst::PasteBurst<30>;
