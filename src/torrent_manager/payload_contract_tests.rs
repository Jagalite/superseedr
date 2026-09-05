// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use crate::persistence::{Backend, FileStat, IoFuture, Operation, Reply};
use std::sync::Mutex;
struct Recorded {
    calls: Arc<Mutex<Vec<&'static str>>>,
    length: u64,
}
impl Backend for Recorded {
    fn submit(&self, operation: Operation, _lease: IoLease) -> IoFuture {
        let (name, reply) = match operation {
            Operation::Allocate { .. } => ("allocate", Reply::Fresh(true)),
            Operation::Inspect { .. } => (
                "inspect",
                Reply::Metadata(FileStat {
                    is_file: true,
                    length: self.length,
                }),
            ),
            Operation::Read { length, .. } => ("read", Reply::Bytes(vec![0; length])),
            Operation::Write { .. } => ("write", Reply::Done),
            Operation::Remove { .. } => ("remove", Reply::Done),
            Operation::Close => ("close", Reply::Done),
        };
        self.calls.lock().unwrap().push(name);
        Box::pin(async move { Ok(reply) })
    }
}
#[tokio::test]
async fn startup_revalidation_probes_and_removal_use_the_injected_capability() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let directory = tempfile::tempdir().unwrap();
        let torrent = resource_tests::create_dummy_torrent(2);
        let mut params = resource_tests::build_test_params();
        params.torrent_data_path = Some(directory.path().into());
        let (events, mut received) = mpsc::channel(16);
        params.manager_event_tx = events;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let payload = Payload::new(Recorded {
            calls: calls.clone(),
            length: torrent.info.length as u64,
        });
        let mut manager =
            TorrentManager::from_torrent(params.with_payload(payload), torrent).unwrap();
        // Construction already schedules validation through the state effect path.
        loop {
            if matches!(
                manager.torrent_manager_rx.recv().await,
                Some(TorrentCommand::ValidationComplete(_))
            ) {
                break;
            }
        }
        manager.validate_local_file().await.unwrap();
        loop {
            if matches!(
                manager.torrent_manager_rx.recv().await,
                Some(TorrentCommand::ValidationComplete(_))
            ) {
                break;
            }
        }
        manager.handle_effect(Effect::StartValidation);
        loop {
            if matches!(
                manager.torrent_manager_rx.recv().await,
                Some(TorrentCommand::ValidationComplete(_))
            ) {
                break;
            }
        }
        manager.spawn_file_probe_batch(7, 0, 10);
        loop {
            if matches!(
                received.recv().await,
                Some(ManagerEvent::FileProbeBatchResult { .. })
            ) {
                break;
            }
        }
        let files = manager
            .state
            .multi_file_info
            .as_ref()
            .unwrap()
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect();
        manager.handle_effect(Effect::DeleteFiles {
            files,
            directories: Vec::new(),
        });
        loop {
            if matches!(
                received.recv().await,
                Some(ManagerEvent::DeletionComplete(_, Ok(())))
            ) {
                break;
            }
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["allocate", "allocate", "allocate", "inspect", "remove"]
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            0,
            "injected storage must not fall back to native files"
        );
    })
    .await
    .expect("injected storage workflow deadline");
}
