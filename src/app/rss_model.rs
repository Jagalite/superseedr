// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared application rss model definitions and transitions.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RssScreen {
    #[default]
    Unified,
    History,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RssSectionFocus {
    Links,
    Filters,
    #[default]
    Explorer,
}
