// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, VecDeque};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::sync::{mpsc, oneshot};

// Process one batch of this many permits, then re-queue the work.
const PERMIT_GRANT_BATCH_SIZE: usize = 64;

#[derive(Debug)]
pub struct PermitGuard {
    pub resource_type: ResourceType,
    control_tx: mpsc::UnboundedSender<ControlCommand>,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        let _ = self.control_tx.send(ControlCommand::Release {
            resource: self.resource_type,
        });
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum ResourceType {
    Reserve,
    PeerConnection,
    DiskRead,
    DiskWrite,
}

#[derive(Error, Debug, Clone)]
pub enum ResourceManagerError {
    #[error("The resource manager has been shut down.")]
    ManagerShutdown,
    #[error("The request queue for the resource is full.")]
    QueueFull,
}

#[derive(Clone, Debug)]
pub struct ResourceManagerClient {
    acquire_txs: HashMap<ResourceType, mpsc::Sender<AcquireCommand>>,
    control_tx: mpsc::UnboundedSender<ControlCommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimitUpdate {
    pub limit: usize,
    pub max_queue_size: Option<usize>,
}

impl ResourceLimitUpdate {
    pub fn limit_and_queue(limit: usize, max_queue_size: usize) -> Self {
        Self {
            limit,
            max_queue_size: Some(max_queue_size),
        }
    }
}

#[cfg(feature = "synthetic-load")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceUsage {
    pub limit: usize,
    pub in_use: usize,
    pub queued: usize,
    pub max_queue_size: usize,
}

#[cfg(feature = "synthetic-load")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceManagerSnapshot {
    pub resources: HashMap<ResourceType, ResourceUsage>,
}

impl ResourceManagerClient {
    pub async fn acquire_peer_connection(&self) -> Result<PermitGuard, ResourceManagerError> {
        self.acquire(ResourceType::PeerConnection).await
    }
    pub async fn acquire_disk_read(&self) -> Result<PermitGuard, ResourceManagerError> {
        self.acquire(ResourceType::DiskRead).await
    }
    pub async fn acquire_disk_write(&self) -> Result<PermitGuard, ResourceManagerError> {
        self.acquire(ResourceType::DiskWrite).await
    }

    #[allow(dead_code)]
    pub async fn update_limits(
        &self,
        new_limits: HashMap<ResourceType, usize>,
    ) -> Result<(), ResourceManagerError> {
        let limits = new_limits
            .into_iter()
            .map(|(resource, limit)| {
                (
                    resource,
                    ResourceLimitUpdate {
                        limit,
                        max_queue_size: None,
                    },
                )
            })
            .collect();
        let command = ControlCommand::UpdateLimits { limits };
        self.control_tx
            .send(command)
            .map_err(|_| ResourceManagerError::ManagerShutdown)
    }

    pub async fn update_limits_and_queue_sizes(
        &self,
        new_limits: HashMap<ResourceType, (usize, usize)>,
    ) -> Result<(), ResourceManagerError> {
        let limits = new_limits
            .into_iter()
            .map(|(resource, (limit, max_queue_size))| {
                (
                    resource,
                    ResourceLimitUpdate::limit_and_queue(limit, max_queue_size),
                )
            })
            .collect();
        let command = ControlCommand::UpdateLimits { limits };
        self.control_tx
            .send(command)
            .map_err(|_| ResourceManagerError::ManagerShutdown)
    }

    #[cfg(feature = "synthetic-load")]
    pub async fn snapshot(&self) -> Result<ResourceManagerSnapshot, ResourceManagerError> {
        let (respond_to, rx) = oneshot::channel();
        let command = ControlCommand::Snapshot { respond_to };
        self.control_tx
            .send(command)
            .map_err(|_| ResourceManagerError::ManagerShutdown)?;
        rx.await.map_err(|_| ResourceManagerError::ManagerShutdown)
    }

    async fn acquire(&self, resource: ResourceType) -> Result<PermitGuard, ResourceManagerError> {
        let (respond_to, rx) = oneshot::channel();
        let command = AcquireCommand { respond_to };
        let tx = self.acquire_txs.get(&resource).unwrap();

        tx.send(command)
            .await
            .map_err(|_| ResourceManagerError::ManagerShutdown)?;

        match rx.await {
            Ok(result) => result,
            Err(_) => Err(ResourceManagerError::ManagerShutdown),
        }
    }
}

#[derive(Debug)]
struct AcquireCommand {
    respond_to: oneshot::Sender<Result<PermitGuard, ResourceManagerError>>,
}

#[derive(Debug)]
pub enum ControlCommand {
    Release {
        resource: ResourceType,
    },
    UpdateLimits {
        limits: HashMap<ResourceType, ResourceLimitUpdate>,
    },
    ProcessQueue {
        resource: ResourceType,
    },
    #[cfg(feature = "synthetic-load")]
    Snapshot {
        respond_to: oneshot::Sender<ResourceManagerSnapshot>,
    },
}

pub struct ResourceManager {
    acquire_rxs: HashMap<ResourceType, mpsc::Receiver<AcquireCommand>>,
    control_rx: mpsc::UnboundedReceiver<ControlCommand>,
    control_tx: mpsc::UnboundedSender<ControlCommand>,
    resources: HashMap<ResourceType, ResourceState>,
    shutdown_tx: broadcast::Sender<()>,
}

struct ResourceState {
    limit: usize,
    in_use: usize,
    max_queue_size: usize,
    wait_queue: VecDeque<oneshot::Sender<Result<PermitGuard, ResourceManagerError>>>,
}

impl ResourceManager {
    pub fn new(
        limits: HashMap<ResourceType, (usize, usize)>,
        shutdown_tx: broadcast::Sender<()>,
    ) -> (Self, ResourceManagerClient) {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let mut acquire_txs = HashMap::new();
        let mut acquire_rxs = HashMap::new();
        let mut resources = HashMap::new();
        // Iterate over all provided limits.
        for (res_type, (limit, max_queue_size)) in limits.iter() {
            // Create a ResourceState for *all* resource types provided.
            resources.insert(
                *res_type,
                ResourceState {
                    limit: *limit,
                    in_use: 0,
                    max_queue_size: *max_queue_size,
                    wait_queue: VecDeque::new(),
                },
            );

            // But *only* create acquire channels for acquirable types.
            // The Reserve pool is just a number to be traded, not acquired.
            if *res_type != ResourceType::Reserve {
                let (tx, rx) = mpsc::channel(256);
                acquire_txs.insert(*res_type, tx);
                acquire_rxs.insert(*res_type, rx);
            }
        }

        let client = ResourceManagerClient {
            acquire_txs,
            control_tx: control_tx.clone(),
        };
        let actor = Self {
            acquire_rxs,
            control_rx,
            control_tx,
            resources,
            shutdown_tx,
        };
        (actor, client)
    }

    pub async fn run(mut self) {
        let mut peer_rx = self
            .acquire_rxs
            .remove(&ResourceType::PeerConnection)
            .unwrap();
        let mut read_rx = self.acquire_rxs.remove(&ResourceType::DiskRead).unwrap();
        let mut write_rx = self.acquire_rxs.remove(&ResourceType::DiskWrite).unwrap();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                Some(cmd) = peer_rx.recv() => self.handle_acquire(ResourceType::PeerConnection, cmd.respond_to),
                Some(cmd) = read_rx.recv() => self.handle_acquire(ResourceType::DiskRead, cmd.respond_to),
                Some(cmd) = write_rx.recv() => self.handle_acquire(ResourceType::DiskWrite, cmd.respond_to),

                Some(cmd) = self.control_rx.recv() => {
                    match cmd {
                        ControlCommand::Release { resource } => self.handle_release(resource),
                        ControlCommand::UpdateLimits { limits } => self.handle_update_limits(limits),
                        ControlCommand::ProcessQueue { resource } => self.handle_process_queue(resource),
                        #[cfg(feature = "synthetic-load")]
                        ControlCommand::Snapshot { respond_to } => {
                            let _ = respond_to.send(self.snapshot());
                        }
                    }
                },
                else => { break; }
            }
        }
    }

    #[cfg(feature = "synthetic-load")]
    fn snapshot(&self) -> ResourceManagerSnapshot {
        let resources = self
            .resources
            .iter()
            .map(|(resource, state)| {
                (
                    *resource,
                    ResourceUsage {
                        limit: state.limit,
                        in_use: state.in_use,
                        queued: state.wait_queue.len(),
                        max_queue_size: state.max_queue_size,
                    },
                )
            })
            .collect();
        ResourceManagerSnapshot { resources }
    }

    fn handle_acquire(
        &mut self,
        resource: ResourceType,
        respond_to: oneshot::Sender<Result<PermitGuard, ResourceManagerError>>,
    ) {
        let state = self.resources.get_mut(&resource).unwrap();
        state.wait_queue.retain(|waiter| !waiter.is_closed());
        if respond_to.is_closed() {
            return;
        }

        if state.in_use < state.limit {
            Self::deliver_permit(&self.control_tx, state, resource, respond_to);
        } else if state.wait_queue.len() < state.max_queue_size {
            state.wait_queue.push_back(respond_to);
        } else {
            let _ = respond_to.send(Err(ResourceManagerError::QueueFull));
        }
    }

    fn deliver_permit(
        control_tx: &mpsc::UnboundedSender<ControlCommand>,
        state: &mut ResourceState,
        resource: ResourceType,
        respond_to: oneshot::Sender<Result<PermitGuard, ResourceManagerError>>,
    ) {
        state.in_use += 1;
        let guard = PermitGuard {
            resource_type: resource,
            control_tx: control_tx.clone(),
        };
        // A failed send returns and drops the undelivered guard. Its Drop queues
        // the single matching Release, so accounting must remain incremented
        // until the actor processes that command.
        let _ = respond_to.send(Ok(guard));
    }

    fn handle_release(&mut self, resource: ResourceType) {
        let state = self.resources.get_mut(&resource).unwrap();
        state.in_use = state.in_use.saturating_sub(1);
        let _ = self
            .control_tx
            .send(ControlCommand::ProcessQueue { resource });
    }

    fn handle_update_limits(&mut self, limits: HashMap<ResourceType, ResourceLimitUpdate>) {
        for (resource, update) in limits {
            if let Some(state) = self.resources.get_mut(&resource) {
                state.limit = update.limit;
                if let Some(max_queue_size) = update.max_queue_size {
                    state.max_queue_size = max_queue_size;
                    Self::trim_wait_queue(state);
                }
                let _ = self
                    .control_tx
                    .send(ControlCommand::ProcessQueue { resource });
            }
        }
    }

    fn trim_wait_queue(state: &mut ResourceState) {
        while state.wait_queue.len() > state.max_queue_size {
            if let Some(waiter) = state.wait_queue.pop_back() {
                let _ = waiter.send(Err(ResourceManagerError::QueueFull));
            }
        }
    }

    fn handle_process_queue(&mut self, resource: ResourceType) {
        let state = self.resources.get_mut(&resource).unwrap();
        for _ in 0..PERMIT_GRANT_BATCH_SIZE {
            if state.in_use >= state.limit {
                return;
            }
            if let Some(next_in_line) = state.wait_queue.pop_front() {
                if !next_in_line.is_closed() {
                    Self::deliver_permit(&self.control_tx, state, resource, next_in_line);
                }
            } else {
                return;
            }
        }
        if state.in_use < state.limit && !state.wait_queue.is_empty() {
            let _ = self
                .control_tx
                .send(ControlCommand::ProcessQueue { resource });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::{advance, sleep, timeout};

    /// Helper function to create a map of limits for the manager.
    fn create_limits(
        peer: (usize, usize),
        read: (usize, usize),
        write: (usize, usize),
    ) -> HashMap<ResourceType, (usize, usize)> {
        let mut limits = HashMap::new();
        limits.insert(ResourceType::PeerConnection, peer);
        limits.insert(ResourceType::DiskRead, read);
        limits.insert(ResourceType::DiskWrite, write);
        limits
    }

    /// Helper function to spawn the resource manager actor and return a client.
    /// The JoinHandle is returned so the actor task can be aborted if needed.
    fn setup_manager(
        limits: HashMap<ResourceType, (usize, usize)>,
    ) -> (ResourceManagerClient, tokio::task::JoinHandle<()>) {
        let (shutdown_tx, _) = broadcast::channel(1);
        let (actor, client) = ResourceManager::new(limits, shutdown_tx);
        let handle = tokio::spawn(actor.run());
        (client, handle)
    }

    fn create_trial_limits(
        resource: ResourceType,
        limit: usize,
        queue: usize,
    ) -> HashMap<ResourceType, (usize, usize)> {
        let mut limits = create_limits((1, 0), (1, 0), (1, 0));
        match resource {
            ResourceType::PeerConnection => {
                limits.insert(ResourceType::PeerConnection, (limit, queue));
            }
            ResourceType::DiskRead => {
                limits.insert(ResourceType::DiskRead, (limit, queue));
            }
            ResourceType::DiskWrite => {
                limits.insert(ResourceType::DiskWrite, (limit, queue));
            }
            ResourceType::Reserve => {}
        }
        limits
    }

    async fn measure_throughput_for_resource(resource: ResourceType, limit: usize) -> usize {
        let queue_size = 20_000;
        let worker_count = 64;
        let work_time = Duration::from_millis(10);
        let run_steps = 120;

        let limits = create_trial_limits(resource, limit, queue_size);
        let (client, manager_handle) = setup_manager(limits);
        let completed = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::new();
        for _ in 0..worker_count {
            let worker_client = client.clone();
            let worker_completed = completed.clone();
            let worker_stop = stop.clone();
            workers.push(tokio::spawn(async move {
                loop {
                    if worker_stop.load(Ordering::Relaxed) {
                        break;
                    }

                    let permit_result = match resource {
                        ResourceType::PeerConnection => {
                            worker_client.acquire_peer_connection().await
                        }
                        ResourceType::DiskRead => worker_client.acquire_disk_read().await,
                        ResourceType::DiskWrite => worker_client.acquire_disk_write().await,
                        ResourceType::Reserve => unreachable!("Reserve is not acquirable"),
                    };

                    let permit = match permit_result {
                        Ok(permit) => permit,
                        Err(ResourceManagerError::QueueFull) => {
                            tokio::task::yield_now().await;
                            continue;
                        }
                        Err(ResourceManagerError::ManagerShutdown) => break,
                    };

                    sleep(work_time).await;
                    drop(permit);
                    worker_completed.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        for _ in 0..run_steps {
            tokio::task::yield_now().await;
            advance(work_time).await;
        }
        stop.store(true, Ordering::Relaxed);
        tokio::task::yield_now().await;
        advance(work_time).await;
        tokio::task::yield_now().await;

        for worker in workers {
            worker.abort();
            let _ = worker.await;
        }
        manager_handle.abort();
        let _ = manager_handle.await;

        completed.load(Ordering::Relaxed)
    }

    #[tokio::test]
    async fn test_acquire_release_success() {
        // Limit 1, Queue 1 for PeerConnection
        let limits = create_limits((1, 1), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        // Acquire once, should succeed
        let guard1 = client.acquire_peer_connection().await;
        assert!(guard1.is_ok());

        // Drop the guard, releasing the permit
        drop(guard1);

        // Acquire again, should succeed
        let guard2 = client.acquire_peer_connection().await;
        assert!(guard2.is_ok());
    }

    #[tokio::test]
    async fn test_acquire_blocks_and_wakes() {
        // Limit 1, Queue 1
        let limits = create_limits((1, 1), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        // 1. Acquire the only permit
        let guard1 = client.acquire_peer_connection().await.unwrap();

        // 2. Spawn a task to acquire the next one.
        let client_clone = client.clone();
        let acquire_task =
            tokio::spawn(async move { client_clone.acquire_peer_connection().await });

        // 3. Assert that it is blocking (by checking it's not finished)
        sleep(Duration::from_millis(50)).await;
        assert!(
            !acquire_task.is_finished(),
            "Acquire did not block when it should have"
        );

        // 4. Drop the first guard, which should unblock the task
        drop(guard1);

        // 5. The task should now complete successfully
        let result = timeout(Duration::from_millis(100), acquire_task).await;
        assert!(result.is_ok(), "Task timed out, did not unblock");
        let inner_result = result.unwrap(); // This is Result<JoinResult<...>>
        assert!(inner_result.is_ok(), "Task join failed"); // JoinError
        assert!(inner_result.unwrap().is_ok(), "Acquire task failed"); // ResourceManagerError
    }

    #[tokio::test]
    async fn test_queue_full_rejection() {
        // Limit 1, Queue 1
        let limits = create_limits((1, 1), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        // 1. Acquire the permit
        let guard1 = client.acquire_peer_connection().await.unwrap();

        // 2. Spawn a task to take the only queue slot
        let client_clone = client.clone();
        let acquire_task2 =
            tokio::spawn(async move { client_clone.acquire_peer_connection().await });

        // Give it time to run and block
        sleep(Duration::from_millis(50)).await;
        assert!(!acquire_task2.is_finished());

        // 3. Attempt to acquire again, should fail immediately with QueueFull
        let result = client.acquire_peer_connection().await;
        match result {
            Err(ResourceManagerError::QueueFull) => { /* This is the expected success */ }
            _ => panic!("Expected QueueFull, got {:?}", result),
        }

        // Cleanup
        drop(guard1);
        let _ = acquire_task2.await;
    }

    #[tokio::test]
    async fn update_limits_and_queue_sizes_zero_queue_clears_waiters_and_blocks_queueing() {
        let limits = create_limits((1, 2), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        let guard = client.acquire_peer_connection().await.unwrap();

        let client_clone = client.clone();
        let queued_task = tokio::spawn(async move { client_clone.acquire_peer_connection().await });

        sleep(Duration::from_millis(50)).await;
        assert!(!queued_task.is_finished());

        let mut stress_limits = HashMap::new();
        stress_limits.insert(ResourceType::PeerConnection, (1, 0));
        client
            .update_limits_and_queue_sizes(stress_limits)
            .await
            .unwrap();

        let queued_result = timeout(Duration::from_millis(100), queued_task)
            .await
            .expect("queued waiter did not complete")
            .expect("queued task join failed");
        assert!(matches!(
            queued_result,
            Err(ResourceManagerError::QueueFull)
        ));

        let saturated_result = client.acquire_peer_connection().await;
        assert!(matches!(
            saturated_result,
            Err(ResourceManagerError::QueueFull)
        ));

        drop(guard);
        let mut reacquired_guard = None;
        for _ in 0..10 {
            match timeout(Duration::from_millis(100), client.acquire_peer_connection()).await {
                Ok(Ok(guard)) => {
                    reacquired_guard = Some(guard);
                    break;
                }
                Ok(Err(ResourceManagerError::QueueFull)) => {
                    sleep(Duration::from_millis(10)).await;
                }
                Ok(Err(error)) => panic!("acquire failed after release: {error:?}"),
                Err(_) => panic!("acquire timed out after release"),
            }
        }
        let guard = reacquired_guard.expect("acquire failed after release");

        let mut normal_limits = HashMap::new();
        normal_limits.insert(ResourceType::PeerConnection, (1, 2));
        client
            .update_limits_and_queue_sizes(normal_limits)
            .await
            .unwrap();

        let client_clone = client.clone();
        let queued_task = tokio::spawn(async move { client_clone.acquire_peer_connection().await });

        sleep(Duration::from_millis(50)).await;
        assert!(
            !queued_task.is_finished(),
            "restored queue should allow waiter to block"
        );

        drop(guard);
        let queued_result = timeout(Duration::from_millis(100), queued_task)
            .await
            .expect("queued waiter did not wake after normal queue restore")
            .expect("queued task join failed");
        assert!(queued_result.is_ok());
    }

    #[tokio::test]
    async fn test_update_limit_increase_wakes_waiters() {
        // Limit 1, Queue 1
        let limits = create_limits((1, 1), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        // 1. Acquire the permit
        let _guard1 = client.acquire_peer_connection().await.unwrap();

        // 2. Spawn task, it should block
        let client_clone = client.clone();
        let acquire_task =
            tokio::spawn(async move { client_clone.acquire_peer_connection().await });

        // Assert it's blocking
        sleep(Duration::from_millis(50)).await;
        assert!(!acquire_task.is_finished());

        // 3. Update limit to 2
        let mut new_limits = HashMap::new();
        new_limits.insert(ResourceType::PeerConnection, 2);
        client.update_limits(new_limits).await.unwrap();

        // 4. The task should now unblock because the limit was increased
        let result = timeout(Duration::from_millis(100), acquire_task).await;
        assert!(
            result.is_ok(),
            "Task timed out, did not unblock after limit update"
        );
        let inner_result = result.unwrap();
        assert!(inner_result.is_ok(), "Task join failed");
        assert!(inner_result.unwrap().is_ok(), "Acquire task failed");
    }

    #[tokio::test]
    async fn test_update_limit_decrease() {
        // Limit 2, Queue 1
        let limits = create_limits((2, 1), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        // 1. Acquire 2 permits
        let guard1 = client.acquire_peer_connection().await.unwrap();
        let guard2 = client.acquire_peer_connection().await.unwrap();

        // 2. Update limit to 1
        let mut new_limits = HashMap::new();
        new_limits.insert(ResourceType::PeerConnection, 1);
        client.update_limits(new_limits).await.unwrap();

        // 3. Spawn task, it should block (in_use is 2, limit is 1)
        let client_clone = client.clone();
        let acquire_task =
            tokio::spawn(async move { client_clone.acquire_peer_connection().await });

        sleep(Duration::from_millis(50)).await;
        assert!(!acquire_task.is_finished());

        // 4. Drop guard1. in_use becomes 1. Limit is 1. Task should still block.
        drop(guard1);
        sleep(Duration::from_millis(50)).await; // Give manager time to process
        assert!(!acquire_task.is_finished(), "Task unblocked too early");

        // 5. Drop guard2. in_use becomes 0. Limit is 1. Task should unblock.
        drop(guard2);
        let result = timeout(Duration::from_millis(100), acquire_task).await;
        assert!(
            result.is_ok(),
            "Task did not unblock after second guard dropped"
        );
        let inner_result = result.unwrap();
        assert!(inner_result.is_ok(), "Task join failed");
        assert!(inner_result.unwrap().is_ok(), "Acquire task failed");
    }

    #[tokio::test]
    async fn test_resources_are_independent() {
        // Limit 1 for Peer, Limit 1 for Read
        let limits = create_limits((1, 1), (1, 1), (0, 0));
        let (client, _handle) = setup_manager(limits);

        // 1. Acquire PeerConnection
        let _peer_guard = client.acquire_peer_connection().await.unwrap();

        // 2. Spawn task for another PeerConnection, it should block
        let client_clone = client.clone();
        let peer_task = tokio::spawn(async move { client_clone.acquire_peer_connection().await });

        sleep(Duration::from_millis(50)).await;
        assert!(
            !peer_task.is_finished(),
            "Peer connection acquire did not block"
        );

        // 3. Acquire DiskRead, it should succeed immediately
        let read_result = client.acquire_disk_read().await;
        assert!(
            read_result.is_ok(),
            "DiskRead acquire failed, was blocked by PeerConnection"
        );

        // 4. Acquire DiskWrite, should fail (limit is 0, queue is 0)
        let write_result = client.acquire_disk_write().await;
        match write_result {
            Err(ResourceManagerError::QueueFull) => { /* Success, queue size is 0 */ }
            _ => panic!("Expected QueueFull for 0-limit resource"),
        }

        // Cleanup
        drop(_peer_guard);
        let _ = peer_task.await;
    }

    #[tokio::test]
    async fn test_manager_shutdown() {
        let limits = create_limits((1, 1), (0, 0), (0, 0));
        let (client, handle) = setup_manager(limits);

        // 1. Abort the manager task
        handle.abort();

        // 2. Wait for the task to fully stop
        sleep(Duration::from_millis(20)).await;

        // 3. Try to acquire. Should fail with ManagerShutdown.
        let result = client.acquire_peer_connection().await;
        match result {
            Err(ResourceManagerError::ManagerShutdown) => { /* Success */ }
            _ => panic!("Expected ManagerShutdown, got {:?}", result),
        }

        // 4. Try to update limits. Should also fail.
        let result_update = client.update_limits(HashMap::new()).await;
        match result_update {
            Err(ResourceManagerError::ManagerShutdown) => { /* Success */ }
            _ => panic!("Expected ManagerShutdown, got {:?}", result_update),
        }
    }

    #[tokio::test]
    async fn test_multiple_waiters_are_woken() {
        // Test that the processing loop wakes multiple waiters
        let limit = 5;
        let queue = 5;
        let limits = create_limits((limit, queue), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        // 1. Acquire all permits
        let mut guards = Vec::new();
        for _ in 0..limit {
            guards.push(client.acquire_peer_connection().await.unwrap());
        }

        // 2. Spawn `queue` tasks to wait
        let mut tasks = Vec::new();
        for _ in 0..queue {
            let client_clone = client.clone();
            tasks.push(tokio::spawn(async move {
                client_clone.acquire_peer_connection().await
            }));
        }

        // 3. Give them time to queue up
        sleep(Duration::from_millis(50)).await;
        for (i, task) in tasks.iter().enumerate() {
            assert!(!task.is_finished(), "Task {} finished early", i);
        }

        // 4. Drop all guards, this should trigger `handle_process_queue`
        drop(guards);

        // 5. All tasks should unblock. We await them sequentially.
        // This replaces the need for `futures::future::join_all`.
        for (i, task) in tasks.into_iter().enumerate() {
            // Await each task with a timeout
            let res = timeout(Duration::from_millis(100), task).await;
            assert!(res.is_ok(), "Task {} timed out waiting to join", i);
            let join_res = res.unwrap();
            assert!(join_res.is_ok(), "Task {} join error", i);
            assert!(join_res.unwrap().is_ok(), "Task {} acquire failed", i);
        }
    }

    #[tokio::test]
    async fn test_dropped_waiter_does_not_leak_permit() {
        // Limit 1, Queue 2
        let limits = create_limits((1, 2), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        // 1. Acquire the only permit
        let guard1 = client.acquire_peer_connection().await.unwrap();

        // 2. Spawn a task that waits, then times out/drops
        let client_clone = client.clone();
        let waiting_task = tokio::spawn(async move {
            // This will block indefinitely
            client_clone.acquire_peer_connection().await
        });

        // Let it get into the queue
        sleep(Duration::from_millis(20)).await;

        // 3. ABORT the waiting task. This simulates a timeout or cancellation.
        waiting_task.abort();
        // Wait a moment for the abort to register
        sleep(Duration::from_millis(20)).await;

        // 4. Release the original guard.
        // The manager will try to give the permit to the aborted task.
        // It should detect the channel is closed, reclaim the permit, and be ready for the next request.
        drop(guard1);
        sleep(Duration::from_millis(20)).await;

        // 5. Acquire again.
        // If the permit leaked, this will block/fail (in_use would be 1 but should be 0).
        let result = timeout(Duration::from_millis(100), client.acquire_peer_connection()).await;

        assert!(
            result.is_ok(),
            "Permit leaked! The aborted waiter consumed a slot."
        );
        assert!(result.unwrap().is_ok());
    }

    #[test]
    fn failed_permit_delivery_releases_once_with_another_permit_held() {
        let limits = create_limits((2, 1), (0, 0), (0, 0));
        let (shutdown_tx, _) = broadcast::channel(1);
        let (mut manager, _client) = ResourceManager::new(limits, shutdown_tx);

        let (held_tx, mut held_rx) = oneshot::channel();
        manager.handle_acquire(ResourceType::PeerConnection, held_tx);
        let held_guard = held_rx
            .try_recv()
            .expect("held permit delivery")
            .expect("held permit acquisition");
        assert_eq!(
            manager
                .resources
                .get(&ResourceType::PeerConnection)
                .expect("peer resource state")
                .in_use,
            1
        );

        // Simulate the race where the receiver closes after is_closed() but
        // before send(). The failed send drops its undelivered PermitGuard.
        let (failed_tx, failed_rx) = oneshot::channel();
        drop(failed_rx);
        let control_tx = manager.control_tx.clone();
        ResourceManager::deliver_permit(
            &control_tx,
            manager
                .resources
                .get_mut(&ResourceType::PeerConnection)
                .expect("peer resource state"),
            ResourceType::PeerConnection,
            failed_tx,
        );
        assert_eq!(
            manager
                .resources
                .get(&ResourceType::PeerConnection)
                .expect("peer resource state")
                .in_use,
            2,
            "failed delivery must remain counted until its guard releases"
        );

        let released_resource = match manager
            .control_rx
            .try_recv()
            .expect("undelivered guard should queue one release")
        {
            ControlCommand::Release { resource } => resource,
            command => panic!("expected release command, got {command:?}"),
        };
        manager.handle_release(released_resource);
        assert_eq!(
            manager
                .resources
                .get(&ResourceType::PeerConnection)
                .expect("peer resource state")
                .in_use,
            1,
            "the unrelated held permit must remain accounted for"
        );

        drop(held_guard);
    }

    #[cfg(feature = "synthetic-load")]
    #[tokio::test]
    async fn test_release_storm_does_not_drop_permits() {
        let limit = 512;
        let limits = create_limits((limit, 0), (0, 0), (0, 0));
        let (client, _handle) = setup_manager(limits);

        let mut guards = Vec::with_capacity(limit);
        for _ in 0..limit {
            guards.push(client.acquire_peer_connection().await.unwrap());
        }

        drop(guards);

        let snapshot = timeout(Duration::from_secs(1), client.snapshot())
            .await
            .expect("snapshot timed out")
            .expect("snapshot failed");
        let peer_usage = snapshot
            .resources
            .get(&ResourceType::PeerConnection)
            .expect("missing peer resource snapshot");
        assert_eq!(peer_usage.in_use, 0, "release storm leaked permits");
    }

    #[tokio::test(start_paused = true)]
    async fn test_disk_permit_throughput_roughly_halves_when_limit_halves() {
        let baseline_limit = 16;
        let half_limit = baseline_limit / 2;

        let read_baseline =
            measure_throughput_for_resource(ResourceType::DiskRead, baseline_limit).await;
        let read_half = measure_throughput_for_resource(ResourceType::DiskRead, half_limit).await;
        assert!(
            read_baseline > 0,
            "Read baseline throughput should be non-zero"
        );
        let read_ratio = read_half as f64 / read_baseline as f64;
        assert!(
            (0.35..=0.75).contains(&read_ratio),
            "DiskRead throughput did not scale as expected: baseline={}, half={}, ratio={:.3}",
            read_baseline,
            read_half,
            read_ratio
        );

        let write_baseline =
            measure_throughput_for_resource(ResourceType::DiskWrite, baseline_limit).await;
        let write_half = measure_throughput_for_resource(ResourceType::DiskWrite, half_limit).await;
        assert!(
            write_baseline > 0,
            "Write baseline throughput should be non-zero"
        );
        let write_ratio = write_half as f64 / write_baseline as f64;
        assert!(
            (0.35..=0.75).contains(&write_ratio),
            "DiskWrite throughput did not scale as expected: baseline={}, half={}, ratio={:.3}",
            write_baseline,
            write_half,
            write_ratio
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_peer_limit_throughput_roughly_halves_when_limit_halves() {
        let baseline_limit = 16;
        let half_limit = baseline_limit / 2;

        let baseline =
            measure_throughput_for_resource(ResourceType::PeerConnection, baseline_limit).await;
        let half = measure_throughput_for_resource(ResourceType::PeerConnection, half_limit).await;
        assert!(baseline > 0, "Peer baseline throughput should be non-zero");

        let ratio = half as f64 / baseline as f64;
        assert!(
            (0.35..=0.75).contains(&ratio),
            "Peer throughput did not scale as expected: baseline={}, half={}, ratio={:.3}",
            baseline,
            half,
            ratio
        );
    }
}
