// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(dead_code, unused_imports)]

mod errors;
pub mod fuzzing;
mod networking {
    pub(crate) mod protocol;
    pub(crate) mod runtime;
    pub(crate) mod shared_udp;
    pub(crate) mod transport;
    pub(crate) mod utp;
}

mod token_bucket;
mod torrent_file;
mod tracker;

#[cfg(feature = "dht")]
mod dht {
    pub(crate) mod krpc;
    pub(crate) mod service {
        pub(in crate::dht::service) fn observe_action_effect_reduction<I>(
            _domain: &'static str,
            _action: &'static str,
            _effects: I,
        ) where
            I: IntoIterator<Item = &'static str>,
        {
        }

        pub(crate) mod fuzzing;
        mod lifecycle;
    }
    pub(crate) mod types;
}
