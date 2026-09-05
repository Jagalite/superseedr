// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Application shutdown observations. This does not decide torrent or peer lifecycle.

use std::collections::{HashMap, HashSet};

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPhase {
    #[default]
    Running,
    Stopping,
    Stopped,
    Incomplete,
}

#[derive(Default, Debug)]
pub struct AppLifecycle {
    pub phase: AppPhase,
    pending: HashSet<Vec<u8>>,
    total: usize,
    pub failures: HashMap<Vec<u8>, String>,
    pub host_cleanup_failed: bool,
}

impl AppLifecycle {
    pub(crate) fn begin_shutdown(&mut self, managers: impl IntoIterator<Item = Vec<u8>>) {
        if self.phase != AppPhase::Running {
            return;
        }
        self.pending = managers.into_iter().collect();
        self.total = self.pending.len();
        self.phase = AppPhase::Stopping;
    }

    pub(crate) fn manager_stopped(&mut self, hash: &[u8], result: Result<(), String>) -> bool {
        if self.phase != AppPhase::Stopping || !self.pending.remove(hash) {
            return false;
        }
        if let Err(error) = result {
            self.failures.insert(hash.to_vec(), error);
        }
        true
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn progress(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            (self.total - self.pending.len()) as f64 / self.total as f64
        }
    }

    pub(crate) fn finish(&mut self, checkpoint_complete: bool) {
        self.phase = if self.pending.is_empty()
            && self.failures.is_empty()
            && !self.host_cleanup_failed
            && checkpoint_complete
        {
            AppPhase::Stopped
        } else {
            AppPhase::Incomplete
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_and_unknown_shutdown_results_do_not_finish_another_manager() {
        let mut lifecycle = AppLifecycle::default();
        lifecycle.begin_shutdown([vec![1], vec![2]]);
        assert!(lifecycle.manager_stopped(&[1], Ok(())));
        assert!(!lifecycle.manager_stopped(&[1], Ok(())));
        assert!(!lifecycle.manager_stopped(&[3], Ok(())));
        assert_eq!(lifecycle.pending_count(), 1);
        assert_eq!(lifecycle.progress(), 0.5);
        lifecycle.finish(true);
        assert_eq!(lifecycle.phase, AppPhase::Incomplete);
    }
    #[test]
    fn manager_failure_and_uncommitted_checkpoint_remain_incomplete() {
        let mut lifecycle = AppLifecycle::default();
        lifecycle.begin_shutdown([vec![1]]);
        lifecycle.manager_stopped(&[1], Err("cleanup failed".into()));
        lifecycle.finish(true);
        assert_eq!(lifecycle.phase, AppPhase::Incomplete);
        let mut lifecycle = AppLifecycle::default();
        lifecycle.begin_shutdown([]);
        lifecycle.finish(false);
        assert_eq!(lifecycle.phase, AppPhase::Incomplete);
    }
}
