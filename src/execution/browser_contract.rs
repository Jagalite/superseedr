// SPDX-License-Identifier: GPL-3.0-or-later
//! Opt-in real-browser contracts for the execution boundary, not native runtime tests.
use super::{spawn, time, JoinSet};
use std::{cell::Cell, rc::Rc, time::Duration};
use wasm_bindgen::prelude::*;
#[wasm_bindgen]
pub async fn browser_runtime_contract() -> Result<String, JsValue> {
    tokio::task::LocalSet::new().run_until(async {
        let local = Rc::new(Cell::new(0));
        let captured = local.clone();
        let start = time::Instant::now();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move { time::sleep(Duration::from_millis(15)).await; captured.set(17); });
        tasks.join_next().await.unwrap().unwrap();
        assert_eq!(local.get(), 17);
        assert!(start.elapsed() >= Duration::from_millis(15));
        assert!(time::timeout(Duration::from_millis(5), std::future::pending::<()>()).await.is_err());
        let mut ticks = time::interval(Duration::from_millis(20));
        ticks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        ticks.tick().await;
        time::sleep(Duration::from_millis(75)).await;
        ticks.tick().await;
        let next = ticks.tick().await;
        assert!(next > start + Duration::from_millis(75));
        ticks.reset();
        assert!(time::timeout(Duration::from_millis(2), ticks.tick()).await.is_err());
        ticks.tick().await; // Dropping a tick future must not lose the next deadline.
        struct Dropped(Rc<Cell<bool>>);
        impl Drop for Dropped { fn drop(&mut self) { self.0.set(true); } }
        let flag = Rc::new(Cell::new(false)); let observed = flag.clone();
        let (send, begun) = tokio::sync::oneshot::channel();
        let job = spawn(async move {
            let _guard = Dropped(observed); let _ = send.send(());
            time::sleep(Duration::from_secs(60)).await;
        });
        begun.await.unwrap(); job.abort(); assert!(job.await.unwrap_err().is_cancelled()); assert!(flag.get());
        let task = spawn(async { tokio::task::id() }); let id = task.id(); assert_eq!(id, task.await.unwrap());
        let (mut publisher, network) = crate::networking::NetworkActivationPublisher::channel();
        let first = publisher.activate_browser();
        let active = network.try_active().unwrap();
        let scoped = active.scope().scoped(17);
        let scope = active.scope().clone();
        let pending = spawn(async move { scope.run(std::future::pending::<()>()).await });
        let second = publisher.activate_browser();
        assert_ne!(first, second); assert!(network.accept(scoped).is_err());
        assert!(pending.await.unwrap().is_err());
        drop(publisher); assert!(network.try_active().is_err());
        use crate::resource::{ResourceManager, ResourceType};
        let (stop, _) = tokio::sync::broadcast::channel(1);
        let limits = [(ResourceType::PeerConnection,(1,1)),(ResourceType::DiskRead,(1,1)),(ResourceType::DiskWrite,(1,1)),(ResourceType::Reserve,(0,0))].into_iter().collect();
        let (actor, client) = ResourceManager::new(limits, stop.clone()); let actor = spawn(actor.run());
        let held = client.acquire_disk_read().await.unwrap();
        let waiting = spawn(async move { client.acquire_disk_read().await.unwrap() });
        time::sleep(Duration::from_millis(5)).await; assert!(!waiting.is_finished());
        drop(held); drop(waiting.await.unwrap()); let _ = stop.send(()); actor.await.unwrap();
        use crate::persistence::{Backend, DeferredOpfs, IoLease, Operation};
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        struct LeaseDrop(Arc<AtomicUsize>);
        impl Drop for LeaseDrop { fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); } }
        let dropped = Arc::new(AtomicUsize::new(0));
        let store = DeferredOpfs::new("deferred-admission-contract".into());
        // Fill the queue without polling any reply. Caller cancellation cannot
        // surrender the leases of already admitted physical operations.
        for _ in 0..32 {
            drop(store.submit(Operation::Inspect { path: "unopened.bin".into() }, IoLease::retain(LeaseDrop(dropped.clone()))));
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        let overflow = store.submit(Operation::Inspect { path: "unopened.bin".into() }, IoLease::none());
        let close = store.submit(Operation::Close, IoLease::none());
        let repeated_close = store.submit(Operation::Close, IoLease::none());
        assert!(overflow.await.is_err());
        close.await.unwrap(); repeated_close.await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 32);
        assert!(store.submit(Operation::Inspect { path: "unopened.bin".into() }, IoLease::none()).await.is_err());
        removal_shutdown_contract();
        Ok("browser clocks, intervals, cancellation, task identity, activation generations, resource permits, deferred storage cancellation/close and removal/shutdown reconciliation passed".into())
    }).await
}

fn removal_shutdown_contract() {
    use crate::app::torrent_manager_protocol::{ManagerCommand, ManagerEvent};
    use crate::app::{TorrentControlState, TorrentMetrics};
    use crate::web_integration::BrowserSession;
    for delete_files in [false, true] {
        for cleanup_ok in [false, true] {
            let hash = vec![0x5b; 20];
            let mut app = BrowserSession::from_settings(120, 40, Default::default());
            let mut manager = app.register_torrent_manager(hash.clone()).unwrap();
            manager.publish_metrics(TorrentMetrics {
                info_hash: hash.clone(),
                torrent_name: "Orbital recovery sample".into(),
                torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hex::encode(&hash)),
                number_of_pieces_total: 1,
                number_of_pieces_completed: 1,
                is_complete: true,
                ..Default::default()
            });
            app.drain_manager_messages();
            let saved = app.prepare_checkpoint(10);
            assert_eq!(saved.settings.torrents.len(), 1);
            app.complete_checkpoint(saved.revision, Ok(()));
            manager.drain_commands();
            assert!(app.send_manager_command(
                &hash,
                if delete_files {
                    ManagerCommand::DeleteFile
                } else {
                    ManagerCommand::Shutdown
                }
            ));
            let saved = app.request_shutdown(20);
            app.complete_checkpoint(saved.revision, Ok(()));
            // Shutdown waits for the already-admitted removal, without another
            // terminal command or treating it as an ordinary retained manager.
            assert_eq!(manager.drain_commands().len(), 1);
            assert!(!app.shutdown_complete());
            manager
                .publish_event(ManagerEvent::DeletionComplete(
                    hash.clone(),
                    if cleanup_ok {
                        Ok(())
                    } else {
                        Err("fixture cleanup failed".into())
                    },
                ))
                .unwrap();
            app.drain_manager_messages();
            let final_saved = app.prepare_checkpoint(30);
            if cleanup_ok {
                assert!(final_saved.settings.torrents.is_empty());
            } else {
                assert_eq!(final_saved.settings.torrents.len(), 1);
                let recovery = &final_saved.settings.torrents[0];
                assert_eq!(recovery.torrent_control_state, TorrentControlState::Paused);
                assert!(!recovery.validation_status);
            }
            app.complete_checkpoint(final_saved.revision, Ok(()));
            assert_eq!(app.shutdown_complete(), cleanup_ok);
            assert_eq!(app.shutdown_failed(), !cleanup_ok);
        }
    }
    // Rejected removal is not an accepted catalog intent. Global shutdown must
    // still send its own command when capacity becomes available and retain it.
    let hash = vec![0x5c; 20];
    let mut app = BrowserSession::from_settings(120, 40, Default::default());
    let mut manager = app.register_torrent_manager(hash.clone()).unwrap();
    manager.publish_metrics(TorrentMetrics {
        info_hash: hash.clone(),
        torrent_or_magnet: format!("magnet:?xt=urn:btih:{}", hex::encode(&hash)),
        ..Default::default()
    });
    app.drain_manager_messages();
    manager.drain_commands();
    for _ in 0..100 {
        assert!(app.send_manager_command(&hash, ManagerCommand::Pause));
    }
    assert!(!app.send_manager_command(&hash, ManagerCommand::DeleteFile));
    let saved = app.request_shutdown(10);
    assert_eq!(manager.drain_commands().len(), 100);
    app.drain_manager_messages();
    assert_eq!(manager.drain_commands(), vec![ManagerCommand::Shutdown]);
    manager
        .publish_event(ManagerEvent::DeletionComplete(hash, Ok(())))
        .unwrap();
    app.drain_manager_messages();
    app.complete_checkpoint(saved.revision, Ok(()));
    assert!(app.shutdown_complete());
    assert_eq!(app.prepare_checkpoint(20).settings.torrents.len(), 1);
    // An accepted removal does not hide an endpoint lost before acknowledgment.
    let hash = vec![0x5d; 20];
    let mut app = BrowserSession::from_settings(120, 40, Default::default());
    let manager = app.register_torrent_manager(hash.clone()).unwrap();
    assert!(app.send_manager_command(&hash, ManagerCommand::DeleteFile));
    drop(manager);
    let saved = app.request_shutdown(10);
    app.complete_checkpoint(saved.revision, Ok(()));
    assert!(app.shutdown_failed());
    assert!(!app.shutdown_complete());
}
