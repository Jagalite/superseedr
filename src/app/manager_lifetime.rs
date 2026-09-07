// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Registration lifetime for asynchronous manager observations.
//!
//! The host owns the lifetime; producers only hold a source token. Replacing or removing
//! a registration invalidates queued observations from that incarnation.

use super::torrent_manager_protocol::ManagerEvent;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub(crate) struct ManagerLifetime(Arc<AtomicBool>);

#[derive(Clone)]
pub(crate) struct ManagerSource(Arc<AtomicBool>);

impl ManagerLifetime {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }
    pub(crate) fn source(&self) -> ManagerSource {
        ManagerSource(self.0.clone())
    }
}
impl Drop for ManagerLifetime {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl ManagerSource {
    pub(crate) fn is_current(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub(crate) struct ManagerObservation {
    pub source: ManagerSource,
    pub event: ManagerEvent,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacing_an_incarnation_invalidates_its_queued_observations() {
        let lifetime = ManagerLifetime::new();
        let old_source = lifetime.source();
        assert!(old_source.is_current());
        drop(lifetime);
        let replacement = ManagerLifetime::new();
        assert!(!old_source.is_current());
        assert!(replacement.source().is_current());
    }
}
