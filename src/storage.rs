// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::errors::StorageError;
use std::path::{Path, PathBuf};
use tokio::fs::{self, try_exists, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

use crate::torrent_file::InfoFile;

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,            // The full path to the file on the disk.
    pub length: u64,              // The length of the file in bytes.
    pub global_start_offset: u64, // The starting offset of this file within the torrent's complete data stream.
}

/// Manages the file layout for a torrent, abstracting away the difference
/// between single and multi-file torrents.
#[derive(Debug, Clone)]
pub struct MultiFileInfo {
    pub files: Vec<FileInfo>,
    pub total_size: u64,
}

impl MultiFileInfo {
    /// Creates a new MultiFileInfo map. This is the central point of unification.
    /// It intelligently handles both single and multi-file torrent metadata.
    pub fn new(
        root_dir: &Path,
        torrent_name: &str,
        files: Option<&Vec<InfoFile>>,
        length: Option<u64>,
    ) -> std::io::Result<Self> {
        if let Some(torrent_files) = files {
            let mut files_vec = Vec::new();
            let mut current_offset = 0;

            for f in torrent_files {
                let mut full_path = root_dir.to_path_buf();
                // The path in the torrent metadata can contain subdirectories.
                for component in &f.path {
                    full_path.push(component);
                }

                files_vec.push(FileInfo {
                    path: full_path,
                    length: f.length as u64,
                    global_start_offset: current_offset,
                });

                current_offset += f.length as u64;
            }
            Ok(Self {
                files: files_vec,
                total_size: current_offset,
            })
        } else {
            let total_size = length.unwrap_or(0);
            let file_path = root_dir.join(torrent_name);
            let single_file = FileInfo {
                path: file_path,
                length: total_size,
                global_start_offset: 0,
            };
            Ok(Self {
                files: vec![single_file],
                total_size,
            })
        }
    }
}

/// Creates all necessary directories and pre-allocates all files for a torrent.
/// This function works for both single and multi-file torrents.
pub async fn create_and_allocate_files(
    multi_file_info: &MultiFileInfo,
) -> Result<(), StorageError> {
    for file_info in &multi_file_info.files {
        // Ensure the parent directory for the file exists.
        if let Some(parent_dir) = file_info.path.parent() {
            if !try_exists(parent_dir).await? {
                fs::create_dir_all(parent_dir).await?;
            }
        }

        // Create and set the length of the file if it doesn't exist.
        if !try_exists(&file_info.path).await? {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file_info.path)
                .await?;
            file.set_len(file_info.length).await?;
        }
    }
    Ok(())
}

pub async fn read_data_from_disk(
    multi_file_info: &MultiFileInfo,
    global_offset: u64,
    bytes_to_read: usize,
) -> Result<Vec<u8>, StorageError> {
    let mut buffer = Vec::with_capacity(bytes_to_read);
    let mut bytes_read = 0;

    for file_info in &multi_file_info.files {
        let file_start = file_info.global_start_offset;
        let file_end = file_start + file_info.length;
        let read_start = global_offset + bytes_read as u64;

        if read_start < file_end && global_offset < file_end {
            let local_offset = read_start.saturating_sub(file_start);
            let bytes_to_read_in_this_file = std::cmp::min(
                (bytes_to_read - bytes_read) as u64,
                file_info.length - local_offset,
            ) as usize;

            if bytes_to_read_in_this_file > 0 {
                let mut file = File::open(&file_info.path).await?;
                file.seek(SeekFrom::Start(local_offset)).await?;

                let mut temp_buf = vec![0; bytes_to_read_in_this_file];
                file.read_exact(&mut temp_buf).await?;
                buffer.extend_from_slice(&temp_buf);

                bytes_read += bytes_to_read_in_this_file;
            }

            if bytes_read == bytes_to_read {
                return Ok(buffer);
            }
        }
    }

    Err(StorageError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Failed to read all data, offset likely out of bounds",
    )))
}

pub async fn write_data_to_disk(
    multi_file_info: &MultiFileInfo,
    global_offset: u64,
    data_to_write: &[u8],
) -> Result<(), StorageError> {
    let mut bytes_written = 0;
    let data_len = data_to_write.len();

    for file_info in &multi_file_info.files {
        let file_start = file_info.global_start_offset;
        let file_end = file_start + file_info.length;
        let write_start = global_offset + bytes_written as u64;

        if write_start < file_end && global_offset < file_end {
            let local_offset = write_start.saturating_sub(file_start);
            let bytes_to_write_in_this_file = std::cmp::min(
                (data_len - bytes_written) as u64,
                file_info.length - local_offset,
            ) as usize;

            if bytes_to_write_in_this_file > 0 {
                let mut file = OpenOptions::new().write(true).open(&file_info.path).await?;
                file.seek(SeekFrom::Start(local_offset)).await?;

                let data_slice =
                    &data_to_write[bytes_written..bytes_written + bytes_to_write_in_this_file];
                file.write_all(data_slice).await?;

                bytes_written += bytes_to_write_in_this_file;
            }

            if bytes_written == data_len {
                return Ok(());
            }
        }
    }

    Err(StorageError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Failed to write all data, offset likely out of bounds",
    )))
}

#[cfg(test)]
mod tests {
    use super::*; // Our module's functions
    use crate::errors::StorageError; // As used in your file
    use crate::torrent_file::InfoFile; // As used in your file

    use std::path::PathBuf;
    use tempfile::tempdir;
    use tokio::fs::{self, File};
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

    /// Helper to create a single-file setup
    fn setup_single_file() -> (tempfile::TempDir, MultiFileInfo) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "single_file.txt";
        let length = 100;
        let mfi = MultiFileInfo::new(root, torrent_name, None, Some(length)).unwrap();
        (dir, mfi)
    }

    /// Helper to create a multi-file setup
    fn setup_multi_file() -> (tempfile::TempDir, MultiFileInfo) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let torrent_name = "multi_file_torrent";
        let files = vec![
            InfoFile {
                path: vec!["file_a.txt".to_string()],
                length: 50, // Ends at 49
                md5sum: None,
            },
            InfoFile {
                path: vec!["subdir".to_string(), "file_b.txt".to_string()],
                length: 70, // Starts at 50
                md5sum: None,
            },
        ];
        // Total size 120
        let mfi = MultiFileInfo::new(root, torrent_name, Some(&files), None).unwrap();
        (dir, mfi)
    }

    #[tokio::test]
    async fn test_multi_file_info_new_single() {
        let (dir, mfi) = setup_single_file();
        assert_eq!(mfi.files.len(), 1);
        assert_eq!(mfi.total_size, 100);
        assert_eq!(mfi.files[0].length, 100);
        assert_eq!(mfi.files[0].global_start_offset, 0);
        assert_eq!(mfi.files[0].path, dir.path().join("single_file.txt"));
    }

    #[tokio::test]
    async fn test_multi_file_info_new_multi() {
        let (dir, mfi) = setup_multi_file();
        assert_eq!(mfi.files.len(), 2);
        assert_eq!(mfi.total_size, 120); // 50 + 70

        // File 1
        assert_eq!(mfi.files[0].length, 50);
        assert_eq!(mfi.files[0].global_start_offset, 0);
        assert_eq!(mfi.files[0].path, dir.path().join("file_a.txt"));

        // File 2
        assert_eq!(mfi.files[1].length, 70);
        assert_eq!(mfi.files[1].global_start_offset, 50); // Offset of file 1
        assert_eq!(
            mfi.files[1].path,
            dir.path().join("subdir").join("file_b.txt")
        );
    }

    #[tokio::test]
    async fn test_create_and_allocate_files_single() {
        let (_dir, mfi) = setup_single_file();
        create_and_allocate_files(&mfi).await.unwrap();

        let file_path = &mfi.files[0].path;
        assert!(tokio::fs::try_exists(file_path).await.unwrap());
        let metadata = tokio::fs::metadata(file_path).await.unwrap();
        assert_eq!(metadata.len(), 100);
    }

    #[tokio::test]
    async fn test_create_and_allocate_files_multi() {
        let (dir, mfi) = setup_multi_file();
        create_and_allocate_files(&mfi).await.unwrap();

        let file_a_path = &mfi.files[0].path;
        let file_b_path = &mfi.files[1].path;
        let subdir_path = dir.path().join("subdir");

        // Check that subdir was created
        assert!(tokio::fs::try_exists(subdir_path).await.unwrap());

        // Check file A
        assert!(tokio::fs::try_exists(file_a_path).await.unwrap());
        let metadata_a = tokio::fs::metadata(file_a_path).await.unwrap();
        assert_eq!(metadata_a.len(), 50);

        // Check file B
        assert!(tokio::fs::try_exists(file_b_path).await.unwrap());
        let metadata_b = tokio::fs::metadata(file_b_path).await.unwrap();
        assert_eq!(metadata_b.len(), 70);
    }

    #[tokio::test]
    async fn test_write_read_single_file() {
        let (_dir, mfi) = setup_single_file();
        create_and_allocate_files(&mfi).await.unwrap();

        let data1: Vec<u8> = (0..20).collect(); // 20 bytes
        let data2: Vec<u8> = (20..50).collect(); // 30 bytes

        // Write data1 at offset 10
        write_data_to_disk(&mfi, 10, &data1).await.unwrap();
        // Write data2 at offset 50
        write_data_to_disk(&mfi, 50, &data2).await.unwrap();

        // Read data1 back
        let read_data1 = read_data_from_disk(&mfi, 10, 20).await.unwrap();
        assert_eq!(data1, read_data1);

        // Read data2 back
        let read_data2 = read_data_from_disk(&mfi, 50, 30).await.unwrap();
        assert_eq!(data2, read_data2);

        // Read pre-allocated (empty) space
        let empty_data = read_data_from_disk(&mfi, 0, 10).await.unwrap();
        assert_eq!(empty_data, vec![0; 10]);
    }

    #[tokio::test]
    async fn test_write_read_across_files() {
        let (_dir, mfi) = setup_multi_file(); // FileA: [0-49], FileB: [50-119]
        create_and_allocate_files(&mfi).await.unwrap();

        // Data that will span the boundary (offset 50)
        // We'll write 30 bytes starting at offset 40.
        // 10 bytes should go to file A [40-49]
        // 20 bytes should go to file B [0-19] (global [50-69])
        let write_data: Vec<u8> = (0..30).collect();
        write_data_to_disk(&mfi, 40, &write_data).await.unwrap();

        // Read the 30 bytes back
        let read_data = read_data_from_disk(&mfi, 40, 30).await.unwrap();
        assert_eq!(write_data, read_data);

        // --- Verify manually ---
        // 1. Read last 10 bytes from file A
        let mut file_a = File::open(&mfi.files[0].path).await.unwrap();
        file_a.seek(SeekFrom::Start(40)).await.unwrap();
        let mut buf_a = vec![0; 10];
        file_a.read_exact(&mut buf_a).await.unwrap();
        assert_eq!(buf_a, &write_data[0..10]);

        // 2. Read first 20 bytes from file B
        let mut file_b = File::open(&mfi.files[1].path).await.unwrap();
        let mut buf_b = vec![0; 20];
        file_b.read_exact(&mut buf_b).await.unwrap();
        assert_eq!(buf_b, &write_data[10..30]);
    }

    #[tokio::test]
    async fn test_read_out_of_bounds() {
        let (_dir, mfi) = setup_single_file(); // total_size = 100
        create_and_allocate_files(&mfi).await.unwrap();

        // Try to read 10 bytes starting at offset 95 (would read 95-104)
        let res = read_data_from_disk(&mfi, 95, 10).await;
        assert!(res.is_err());
        if let Err(StorageError::Io(err)) = res {
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        } else {
            panic!("Expected Io Error");
        }

        // A read right up to the boundary should be fine
        let res_ok = read_data_from_disk(&mfi, 90, 10).await;
        assert!(res_ok.is_ok());
        assert_eq!(res_ok.unwrap().len(), 10);
    }

    #[tokio::test]
    async fn test_write_out_of_bounds() {
        let (_dir, mfi) = setup_single_file(); // total_size = 100
        create_and_allocate_files(&mfi).await.unwrap();

        let data = vec![1; 10];
        // Try to write 10 bytes starting at offset 95 (would write 95-104)
        let res = write_data_to_disk(&mfi, 95, &data).await;
        assert!(res.is_err());
        if let Err(StorageError::Io(err)) = res {
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        } else {
            panic!("Expected Io Error");
        }

        // A write right up to the boundary should be fine
        let res_ok = write_data_to_disk(&mfi, 90, &data).await;
        assert!(res_ok.is_ok());

        // And we should be able to read it back
        let read_back = read_data_from_disk(&mfi, 90, 10).await.unwrap();
        assert_eq!(read_back, data);
    }
}
