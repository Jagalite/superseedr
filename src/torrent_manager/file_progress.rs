// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only projection of the reducer's committed pieces onto manifest files.
use super::{
    piece_manager::PieceStatus,
    state::{TorrentState, TorrentStatus},
};

pub(super) fn verified_bytes(state: &TorrentState) -> Vec<Option<u64>> {
    let Some(layout) = state.multi_file_info.as_ref() else {
        return Vec::new();
    };
    let piece_length = state
        .torrent
        .as_ref()
        .map(|torrent| torrent.info.piece_length)
        .unwrap_or(0);
    layout
        .files
        .iter()
        .map(|file| {
            if piece_length <= 0
                || file.is_padding
                || file.is_skipped
                || matches!(
                    state.torrent_status,
                    TorrentStatus::Validating | TorrentStatus::AwaitingMetadata
                )
            {
                return None;
            }
            if file.length == 0 {
                return Some(0);
            }
            let piece_length = piece_length as u64;
            let end = file.global_start_offset.checked_add(file.length)?;
            let first = usize::try_from(file.global_start_offset / piece_length).ok()?;
            let last = usize::try_from((end - 1) / piece_length).ok()?;
            let pieces = state.piece_manager.bitfield.get(first..=last)?;
            Some(
                pieces
                    .iter()
                    .enumerate()
                    .filter_map(|(offset, status)| {
                        let index = first + offset;
                        if *status != PieceStatus::Done
                            || state.verifying_pieces.contains(&(index as u32))
                            || state.writing_pieces.contains(&(index as u32))
                        {
                            return None;
                        }
                        let start = index as u64 * piece_length;
                        Some(
                            end.min(start.saturating_add(piece_length))
                                - file.global_start_offset.max(start),
                        )
                    })
                    .sum(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        persistence::{FileInfo, MultiFileInfo},
        torrent_file::{Info, Torrent},
    };
    #[test]
    fn file_completion_uses_committed_shared_pieces_and_handles_recheck() {
        let mut state = TorrentState::default();
        state.torrent = Some(Torrent {
            info: Info {
                piece_length: 4,
                ..Default::default()
            },
            ..Default::default()
        });
        state.torrent_status = TorrentStatus::Standard;
        state.multi_file_info = Some(MultiFileInfo {
            total_size: 10,
            files: [
                (0, 3, false),
                (3, 4, false),
                (7, 2, true),
                (9, 1, false),
                (10, 0, false),
            ]
            .into_iter()
            .map(|(start, length, padding)| FileInfo {
                path: "sample.bin".into(),
                global_start_offset: start,
                length,
                is_padding: padding,
                is_skipped: false,
            })
            .collect(),
        });
        state.piece_manager.bitfield =
            vec![PieceStatus::Done, PieceStatus::Need, PieceStatus::Need];
        assert_eq!(
            verified_bytes(&state),
            vec![Some(3), Some(1), None, Some(0), Some(0)]
        );
        state.piece_manager.bitfield[1] = PieceStatus::Done;
        state.writing_pieces.insert(1);
        assert_eq!(verified_bytes(&state)[1], Some(1));
        state.writing_pieces.clear();
        assert_eq!(verified_bytes(&state)[1], Some(4));
        state.torrent_status = TorrentStatus::Validating;
        assert_eq!(verified_bytes(&state), vec![None; 5]);
        state.torrent_status = TorrentStatus::Standard;
        state.piece_manager.bitfield[0] = PieceStatus::Need;
        assert_eq!(verified_bytes(&state)[0], Some(0));
        state.multi_file_info.as_mut().unwrap().files[1].is_skipped = true;
        assert_eq!(verified_bytes(&state)[1], None);
        state.piece_manager.bitfield.clear();
        assert_eq!(verified_bytes(&state)[0], None);
    }
}
