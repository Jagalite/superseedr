// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::networking::runtime::{wait_for_invalidation, NetworkLease, NetworkLeaseError};
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkScopeId {
    generation_id: u64,
    activation_id: u64,
}

impl NetworkScopeId {
    pub fn generation_id(self) -> u64 {
        self.generation_id
    }

    pub(crate) fn from_lease(lease: &NetworkLease) -> Option<Self> {
        Some(Self {
            generation_id: lease.generation_id(),
            activation_id: lease.activation_id()?,
        })
    }

    #[cfg(test)]
    pub(crate) const fn for_test(generation_id: u64) -> Self {
        Self {
            generation_id,
            activation_id: 1,
        }
    }
}

#[derive(Debug)]
struct ActivationLifetime {
    invalidated: AtomicBool,
    invalidation_tx: watch::Sender<bool>,
}

impl ActivationLifetime {
    fn new() -> Self {
        let (invalidation_tx, _) = watch::channel(false);
        Self {
            invalidated: AtomicBool::new(false),
            invalidation_tx,
        }
    }

    fn invalidate(&self) {
        if !self.invalidated.swap(true, Ordering::AcqRel) {
            self.invalidation_tx.send_replace(true);
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkScope {
    id: NetworkScopeId,
    lease: NetworkLease,
    lifetime: Arc<ActivationLifetime>,
}

impl NetworkScope {
    fn new(lease: NetworkLease, activation_id: u64) -> Self {
        let lifetime = Arc::new(ActivationLifetime::new());
        let scoped_lease =
            lease.with_activation(activation_id, lifetime.invalidation_tx.subscribe());
        let mut generation_rx = scoped_lease.generation().subscribe_invalidation();
        let mut activation_rx = lifetime.invalidation_tx.subscribe();
        let generation_lifetime = lifetime.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = wait_for_invalidation(&mut generation_rx) => {
                    generation_lifetime.invalidate();
                }
                _ = wait_for_invalidation(&mut activation_rx) => {}
            }
        });
        Self {
            id: NetworkScopeId {
                generation_id: scoped_lease.generation_id(),
                activation_id,
            },
            lease: scoped_lease,
            lifetime,
        }
    }

    pub fn id(&self) -> NetworkScopeId {
        self.id
    }

    pub fn lease(&self) -> &NetworkLease {
        &self.lease
    }

    pub fn ensure_valid(&self) -> Result<(), NetworkLeaseError> {
        self.lease.ensure_valid()?;
        if self.lifetime.invalidated.load(Ordering::Acquire) {
            Err(NetworkLeaseError::Invalidated {
                generation_id: self.id.generation_id,
            })
        } else {
            Ok(())
        }
    }

    pub fn subscribe_invalidation(&self) -> watch::Receiver<bool> {
        self.lifetime.invalidation_tx.subscribe()
    }

    pub async fn run<F, T>(&self, operation: F) -> Result<T, NetworkLeaseError>
    where
        F: Future<Output = T>,
    {
        let mut activation_rx = self.subscribe_invalidation();
        let mut generation_rx = self.lease.generation().subscribe_invalidation();
        self.ensure_valid()?;
        let output = tokio::select! {
            biased;
            _ = wait_for_invalidation(&mut activation_rx) => {
                return Err(NetworkLeaseError::Invalidated {
                    generation_id: self.id.generation_id,
                });
            }
            _ = wait_for_invalidation(&mut generation_rx) => {
                return Err(NetworkLeaseError::Invalidated {
                    generation_id: self.id.generation_id,
                });
            }
            output = operation => output,
        };
        self.ensure_valid()?;
        Ok(output)
    }

    pub fn scoped<T>(&self, value: T) -> Scoped<T> {
        Scoped {
            scope_id: self.id,
            value,
        }
    }

    fn invalidate(&self) {
        self.lifetime.invalidate();
    }
}

#[derive(Debug, Clone)]
pub struct ActiveNetwork {
    scope: NetworkScope,
    listen_port: u16,
}

impl ActiveNetwork {
    pub fn scope(&self) -> &NetworkScope {
        &self.scope
    }

    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }
}

#[derive(Debug, Clone)]
pub enum NetworkActivationState {
    Pending { generation_id: Option<u64> },
    Active(Arc<ActiveNetwork>),
    Blocked(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkActivationStatus {
    Pending {
        generation_id: Option<u64>,
    },
    Active {
        generation_id: u64,
        listen_port: u16,
    },
    Blocked {
        reason: Arc<str>,
    },
}

impl NetworkActivationState {
    fn status(&self) -> NetworkActivationStatus {
        match self {
            Self::Pending { generation_id } => NetworkActivationStatus::Pending {
                generation_id: *generation_id,
            },
            Self::Active(active) => NetworkActivationStatus::Active {
                generation_id: active.scope().id().generation_id(),
                listen_port: active.listen_port(),
            },
            Self::Blocked(reason) => NetworkActivationStatus::Blocked {
                reason: reason.clone(),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkActivationHandle {
    state_rx: watch::Receiver<NetworkActivationState>,
}

impl NetworkActivationHandle {
    pub fn subscribe(&self) -> watch::Receiver<NetworkActivationState> {
        let mut state_rx = self.state_rx.clone();
        state_rx.borrow_and_update();
        state_rx
    }

    pub fn status(&self) -> NetworkActivationStatus {
        self.state_rx.borrow().status()
    }

    pub fn try_active(&self) -> Result<Arc<ActiveNetwork>, NetworkActivationError> {
        let active = match &*self.state_rx.borrow() {
            NetworkActivationState::Active(active) => active.clone(),
            NetworkActivationState::Pending { generation_id } => {
                return Err(NetworkActivationError::Pending {
                    generation_id: *generation_id,
                });
            }
            NetworkActivationState::Blocked(reason) => {
                return Err(NetworkActivationError::Blocked(reason.clone()));
            }
        };
        active
            .scope
            .ensure_valid()
            .map_err(NetworkActivationError::Lease)?;
        Ok(active)
    }

    pub fn is_current(&self, scope_id: NetworkScopeId) -> bool {
        self.try_active()
            .is_ok_and(|active| active.scope.id == scope_id)
    }

    pub fn accept<T>(&self, scoped: Scoped<T>) -> Result<T, Scoped<T>> {
        if self.is_current(scoped.scope_id) {
            Ok(scoped.value)
        } else {
            Err(scoped)
        }
    }
}

#[derive(Debug)]
pub struct NetworkActivationPublisher {
    state_tx: watch::Sender<NetworkActivationState>,
    next_activation_id: AtomicU64,
    current_scope: Option<NetworkScope>,
}

impl NetworkActivationPublisher {
    pub fn channel() -> (Self, NetworkActivationHandle) {
        let (state_tx, state_rx) = watch::channel(NetworkActivationState::Pending {
            generation_id: None,
        });
        (
            Self {
                state_tx,
                next_activation_id: AtomicU64::new(1),
                current_scope: None,
            },
            NetworkActivationHandle { state_rx },
        )
    }

    pub fn pending(&mut self, generation_id: Option<u64>) {
        self.invalidate_current();
        self.state_tx
            .send_replace(NetworkActivationState::Pending { generation_id });
    }

    #[cfg(any(test, feature = "synthetic-load"))]
    pub fn activate(
        &mut self,
        lease: NetworkLease,
        listen_port: u16,
    ) -> Result<Arc<ActiveNetwork>, NetworkLeaseError> {
        let scope = self.prepare(lease)?;
        self.activate_prepared(scope, listen_port)
    }

    pub fn prepare(&mut self, lease: NetworkLease) -> Result<NetworkScope, NetworkLeaseError> {
        lease.ensure_valid()?;
        self.invalidate_current();
        let activation_id = self.next_activation_id.fetch_add(1, Ordering::Relaxed);
        let scope = NetworkScope::new(lease, activation_id);
        self.current_scope = Some(scope.clone());
        self.state_tx.send_replace(NetworkActivationState::Pending {
            generation_id: Some(scope.id().generation_id()),
        });
        Ok(scope)
    }

    pub fn activate_prepared(
        &mut self,
        scope: NetworkScope,
        listen_port: u16,
    ) -> Result<Arc<ActiveNetwork>, NetworkLeaseError> {
        scope.ensure_valid()?;
        if self.current_scope.as_ref().map(NetworkScope::id) != Some(scope.id()) {
            return Err(NetworkLeaseError::Invalidated {
                generation_id: scope.id().generation_id(),
            });
        }
        let active = Arc::new(ActiveNetwork {
            scope: scope.clone(),
            listen_port,
        });
        self.state_tx
            .send_replace(NetworkActivationState::Active(active.clone()));
        Ok(active)
    }

    pub fn block(&mut self, reason: impl Into<Arc<str>>) {
        self.invalidate_current();
        self.state_tx
            .send_replace(NetworkActivationState::Blocked(reason.into()));
    }

    pub(crate) fn active_scope_id(&self) -> Option<NetworkScopeId> {
        self.current_scope.as_ref().map(NetworkScope::id)
    }

    fn invalidate_current(&mut self) {
        if let Some(scope) = self.current_scope.take() {
            scope.invalidate();
        }
    }
}

impl Drop for NetworkActivationPublisher {
    fn drop(&mut self) {
        self.invalidate_current();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkActivationError {
    Pending { generation_id: Option<u64> },
    Blocked(Arc<str>),
    Lease(NetworkLeaseError),
}

impl fmt::Display for NetworkActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending { generation_id } => {
                write!(formatter, "network activation is pending")?;
                if let Some(generation_id) = generation_id {
                    write!(formatter, " for generation {generation_id}")?;
                }
                Ok(())
            }
            Self::Blocked(reason) => write!(formatter, "network activation is blocked: {reason}"),
            Self::Lease(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NetworkActivationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scoped<T> {
    scope_id: NetworkScopeId,
    value: T,
}

impl<T> Scoped<T> {
    pub fn scope_id(&self) -> NetworkScopeId {
        self.scope_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::networking::runtime::test_network_lease;
    use std::time::Duration;

    #[tokio::test]
    async fn pending_invalidates_the_previous_activation_immediately() {
        let (_network_handle, lease) = test_network_lease();
        let (mut publisher, handle) = NetworkActivationPublisher::channel();
        let active = publisher.activate(lease, 41000).unwrap();
        let scope = active.scope().clone();

        publisher.pending(Some(scope.id().generation_id()));

        assert!(scope.ensure_valid().is_err());
        assert!(matches!(
            handle.try_active(),
            Err(NetworkActivationError::Pending { .. })
        ));
    }

    #[tokio::test]
    async fn status_reports_pending_active_and_blocked_authority() {
        let (_network_handle, lease) = test_network_lease();
        let generation_id = lease.generation_id();
        let (mut publisher, handle) = NetworkActivationPublisher::channel();

        assert_eq!(
            handle.status(),
            NetworkActivationStatus::Pending {
                generation_id: None
            }
        );
        publisher.pending(Some(generation_id));
        assert_eq!(
            handle.status(),
            NetworkActivationStatus::Pending {
                generation_id: Some(generation_id)
            }
        );
        publisher.activate(lease, 41000).unwrap();
        assert_eq!(
            handle.status(),
            NetworkActivationStatus::Active {
                generation_id,
                listen_port: 41000,
            }
        );
        publisher.block("listener failed");
        assert_eq!(
            handle.status(),
            NetworkActivationStatus::Blocked {
                reason: Arc::from("listener failed")
            }
        );
    }

    #[tokio::test]
    async fn replacing_a_port_creates_one_new_scope_in_the_same_generation() {
        let (_network_handle, lease) = test_network_lease();
        let (mut publisher, handle) = NetworkActivationPublisher::channel();
        let first = publisher.activate(lease.clone(), 41000).unwrap();
        let stale = first.scope().scoped("stale");

        let second = publisher.activate(lease, 42000).unwrap();

        assert_eq!(
            first.scope().id().generation_id(),
            second.scope().id().generation_id()
        );
        assert_ne!(
            first.scope().id().activation_id,
            second.scope().id().activation_id
        );
        assert!(handle.accept(stale).is_err());
        assert_eq!(
            handle.accept(second.scope().scoped("current")).unwrap(),
            "current"
        );
    }

    #[tokio::test]
    async fn replacing_an_activation_releases_its_generation_watcher() {
        let (_network_handle, lease) = test_network_lease();
        let (mut publisher, _handle) = NetworkActivationPublisher::channel();
        let first = publisher.activate(lease.clone(), 41000).unwrap();
        let first_lifetime = Arc::downgrade(&first.scope().lifetime);

        publisher.activate(lease, 42000).unwrap();
        drop(first);

        tokio::time::timeout(Duration::from_secs(1), async {
            while first_lifetime.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replaced activation watcher should terminate promptly");
    }

    #[tokio::test]
    async fn invalidation_cancels_an_in_flight_scope_operation() {
        let (_network_handle, lease) = test_network_lease();
        let (mut publisher, _handle) = NetworkActivationPublisher::channel();
        let active = publisher.activate(lease, 41000).unwrap();
        let scope = active.scope().clone();
        let operation_scope = scope.clone();
        let operation =
            tokio::spawn(async move { operation_scope.run(std::future::pending::<()>()).await });

        tokio::task::yield_now().await;
        publisher.block("listener failed");

        let result = tokio::time::timeout(Duration::from_secs(1), operation)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        assert!(scope.ensure_valid().is_err());
    }

    #[tokio::test]
    async fn latest_state_is_retained_without_a_consumer_queue() {
        let (_network_handle, lease) = test_network_lease();
        let (mut publisher, handle) = NetworkActivationPublisher::channel();
        publisher.pending(Some(lease.generation_id()));
        publisher.block("first failure");
        let active = publisher.activate(lease, 43000).unwrap();

        let observed = handle.try_active().unwrap();
        assert_eq!(observed.scope().id(), active.scope().id());
        assert_eq!(observed.listen_port(), 43000);
    }
}
