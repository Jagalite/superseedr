// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, VecDeque};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::sync::{mpsc, oneshot};

// Process one batch of this many permits, then re-queue the work.
const PERMIT_GRANT_BATCH_SIZE: usize = 64;

enum RequestPriority {
    High,
    Medium,
    Low,
}
impl RequestPriority {
    fn get_priority(&self) -> u32 {
        match self {
            RequestPriority::High => 1,
            RequestPriority::Medium => 2,
            RequestPriority::Low => 3,
        }
    }
}

#[derive(Debug)]
pub struct PermitGuard {
    pub resource_type: ResourceType,
    control_tx: mpsc::Sender<ControlCommand>,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        let _ = self.control_tx.try_send(ControlCommand::Release {
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
    control_tx: mpsc::Sender<ControlCommand>,
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

    pub async fn update_limits(
        &self,
        new_limits: HashMap<ResourceType, usize>,
    ) -> Result<(), ResourceManagerError> {
        let command = ControlCommand::UpdateLimits { limits: new_limits };
        self.control_tx
            .send(command)
            .await
            .map_err(|_| ResourceManagerError::ManagerShutdown)
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
        limits: HashMap<ResourceType, usize>,
    },
    ProcessQueue {
        resource: ResourceType,
    },
}

pub struct ResourceManager {
    acquire_rxs: HashMap<ResourceType, mpsc::Receiver<AcquireCommand>>,
    control_rx: mpsc::Receiver<ControlCommand>,
    control_tx: mpsc::Sender<ControlCommand>,
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
        let (control_tx, control_rx) = mpsc::channel(256);
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
                    }
                },
                else => { break; }
            }
        }
    }

    fn handle_acquire(
        &mut self,
        resource: ResourceType,
        respond_to: oneshot::Sender<Result<PermitGuard, ResourceManagerError>>,
    ) {
        let state = self.resources.get_mut(&resource).unwrap();

        if state.in_use < state.limit {
            state.in_use += 1;
            let guard = PermitGuard {
                resource_type: resource,
                control_tx: self.control_tx.clone(),
            };
            let _ = respond_to.send(Ok(guard));
        } else if state.wait_queue.len() < state.max_queue_size {
            state.wait_queue.push_back(respond_to);
        } else {
            let _ = respond_to.send(Err(ResourceManagerError::QueueFull));
        }
    }

    fn handle_release(&mut self, resource: ResourceType) {
        let state = self.resources.get_mut(&resource).unwrap();
        state.in_use = state.in_use.saturating_sub(1);
        let _ = self
            .control_tx
            .try_send(ControlCommand::ProcessQueue { resource });
    }

    fn handle_update_limits(&mut self, limits: HashMap<ResourceType, usize>) {
        for (resource, new_limit) in limits {
            if let Some(state) = self.resources.get_mut(&resource) {
                state.limit = new_limit;
                let _ = self
                    .control_tx
                    .try_send(ControlCommand::ProcessQueue { resource });
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
                    state.in_use += 1;
                    let guard = PermitGuard {
                        resource_type: resource,
                        control_tx: self.control_tx.clone(),
                    };
                    if next_in_line.send(Ok(guard)).is_err() {
                        state.in_use -= 1;
                    }
                }
            } else {
                return;
            }
        }
        if state.in_use < state.limit && !state.wait_queue.is_empty() {
            let _ = self
                .control_tx
                .try_send(ControlCommand::ProcessQueue { resource });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

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
}
