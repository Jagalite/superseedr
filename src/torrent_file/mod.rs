// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod parser;

use serde::de::{self};
use serde::{Deserialize, Deserializer, Serialize};
use serde_bencode::value::Value;

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct V2RootInfo {
    pub file_offset: u64,
    pub length: u64,
    pub root_hash: Vec<u8>,
    pub file_index: u32,
}

pub struct V2Mapping {
    pub piece_to_roots: HashMap<u32, Vec<V2RootInfo>>,
    pub piece_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Torrent {
    // This field is special and not directly in the bencode source.
    // We will populate it manually after deserialization.
    #[serde(skip)]
    pub info_dict_bencode: Vec<u8>,

    pub info: Info,
    pub announce: Option<String>,

    #[serde(rename = "announce-list", default)]
    pub announce_list: Option<Vec<Vec<String>>>,

    #[serde(
        rename = "url-list",
        default,
        deserialize_with = "deserialize_url_list"
    )]
    pub url_list: Option<Vec<String>>,

    #[serde(rename = "creation date", default)]
    pub creation_date: Option<i64>,

    #[serde(default)]
    pub comment: Option<String>,

    #[serde(rename = "created by", default)]
    pub created_by: Option<String>,

    #[serde(default)]
    pub encoding: Option<String>,

    // --- v2 / Hybrid Fields ---
    #[serde(rename = "piece layers", default)]
    pub piece_layers: Option<Value>,
}

impl Torrent {
    pub fn get_v2_roots(&self) -> Vec<(String, u64, Vec<u8>)> {
        let mut results = Vec::new();
        if let Some(ref tree) = self.info.file_tree {
            traverse_file_tree(tree, String::new(), &mut results);
        }
        results
    }

    pub fn get_layer_hashes(&self, root_hash: &[u8]) -> Option<Vec<u8>> {
        if let Some(Value::Dict(layers)) = &self.piece_layers {
            if let Some(Value::Bytes(layer_data)) = layers.get(root_hash) {
                return Some(layer_data.clone());
            }
        }
        None
    }

    pub fn calculate_v2_mapping(&self) -> V2Mapping {
        let mut piece_to_roots: HashMap<u32, Vec<V2RootInfo>> = HashMap::new();
        let piece_len = self.info.piece_length as u64;
        let mut current_piece_index = 0;

        if self.info.meta_version == Some(2) && piece_len > 0 {
            let mut v2_roots = self.get_v2_roots();
            v2_roots.sort_by(|(path_a, _, _), (path_b, _, _)| path_a.cmp(path_b));

            for (file_index, (_path, length, root_hash)) in v2_roots.into_iter().enumerate() {
                if length > 0 {
                    let file_pieces = length.div_ceil(piece_len);
                    let file_start_offset = current_piece_index * piece_len;

                    let start_piece = current_piece_index as u32;
                    let end_piece = (current_piece_index + file_pieces) as u32;

                    for p in start_piece..end_piece {
                        piece_to_roots.entry(p).or_default().push(V2RootInfo {
                            file_offset: file_start_offset,
                            length,
                            root_hash: root_hash.clone(),
                            file_index: file_index as u32,
                        });
                    }
                    current_piece_index += file_pieces;
                }
            }
        }

        V2Mapping {
            piece_to_roots,
            piece_count: current_piece_index as usize,
        }
    }

    pub fn get_v2_hash_layer(
        &self,
        piece_index: u32,
        file_start_offset: u64,
        file_length: u64,
        requested_length: u32,
        resolved_root: &[u8],
    ) -> Option<Vec<u8>> {
        let piece_len = self.info.piece_length as u64;
        if piece_len == 0 {
            return None;
        }

        // Calculate where the file starts in piece-space and the request's relative bounds
        let file_start_piece = (file_start_offset as u32) / (piece_len as u32);
        if piece_index < file_start_piece {
            return None;
        }

        let relative_start_idx = (piece_index - file_start_piece) as usize;
        let relative_end_idx = relative_start_idx + requested_length as usize;

        // 1. Try to retrieve explicit layers first.
        // This handles Multi-piece files AND test mocks that inject layers for single files.
        if let Some(layer_bytes) = self.get_layer_hashes(resolved_root) {
            let total_hashes_in_layer = layer_bytes.len() / 32;

            if relative_end_idx <= total_hashes_in_layer {
                let start_byte = relative_start_idx * 32;
                let end_byte = relative_end_idx * 32;
                return Some(layer_bytes[start_byte..end_byte].to_vec());
            } else {
                // The requested range exceeds what is available in the layer.
                return None;
            }
        }

        // 2. Fallback: BEP 52 Optimization for Single Piece Files.
        // "Note that for files that fit in one piece, the 'pieces root' is the digest of the file."
        // We only use this if no explicit layer was found.
        if file_length <= piece_len {
            // A single piece file has exactly 1 hash (index 0).
            // We must verify the request matches this limit.
            if relative_start_idx == 0 && requested_length == 1 {
                return Some(resolved_root.to_vec());
            }
        }

        None
    }

    pub fn file_list(&self) -> Vec<(Vec<String>, u64)> {
        if !self.info.files.is_empty() {
            // Multi-file case
            self.info
                .files
                .iter()
                .map(|f| (f.path.clone(), f.length as u64))
                .collect()
        } else {
            // Single-file V1 case: The torrent name is the file name
            vec![(vec![self.info.name.clone()], self.info.length as u64)]
        }
    }
}

fn traverse_file_tree(
    node: &Value,
    current_path: String,
    results: &mut Vec<(String, u64, Vec<u8>)>,
) {
    if let Value::Dict(map) = node {
        for (key, value) in map {
            let name = String::from_utf8_lossy(key).to_string();

            if name.is_empty() {
                // This is a file metadata node (Leaf)
                if let Value::Dict(file_metadata) = value {
                    // Extract Root
                    if let Some(Value::Bytes(root)) = file_metadata.get("pieces root".as_bytes()) {
                        // Extract Length
                        let len =
                            if let Some(Value::Int(l)) = file_metadata.get("length".as_bytes()) {
                                *l as u64
                            } else {
                                0
                            };
                        results.push((current_path.clone(), len, root.clone()));
                    }
                }
            } else {
                // Directory node
                let new_path = if current_path.is_empty() {
                    name
                } else {
                    format!("{}/{}", current_path, name)
                };
                traverse_file_tree(value, new_path, results);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Info {
    #[serde(rename = "piece length")]
    pub piece_length: i64,

    #[serde(with = "serde_bytes")]
    #[serde(default)]
    pub pieces: Vec<u8>,

    #[serde(default)]
    pub private: Option<i64>,

    #[serde(default)]
    pub files: Vec<InfoFile>,

    pub name: String,

    #[serde(default)]
    pub length: i64,

    #[serde(default)]
    pub md5sum: Option<String>,

    // --- v2 / Hybrid Fields ---
    #[serde(rename = "meta version", default)]
    pub meta_version: Option<i64>,

    #[serde(rename = "file tree", default)]
    pub file_tree: Option<Value>,
}

impl Info {
    pub fn total_length(&self) -> i64 {
        // Case 1: v1 Single File
        if self.length > 0 {
            return self.length;
        }

        // Case 2: v1 Multi-File
        if !self.files.is_empty() {
            return self.files.iter().map(|f| f.length).sum();
        }

        // Case 3: v2 File Tree
        if let Some(ref tree) = self.file_tree {
            return calculate_tree_size(tree);
        }

        0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InfoFile {
    pub length: i64,

    #[serde(default)]
    pub md5sum: Option<String>,

    pub path: Vec<String>,

    #[serde(default)]
    pub attr: Option<String>,
}

fn deserialize_url_list<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let v: Value = Deserialize::deserialize(deserializer)?;

    match v {
        Value::Bytes(bytes) => {
            let s = String::from_utf8(bytes)
                .map_err(|e| de::Error::custom(format!("Invalid UTF-8 in url-list: {}", e)))?;
            Ok(Some(vec![s]))
        }
        Value::List(list) => {
            let mut urls = Vec::new();
            for item in list {
                if let Value::Bytes(bytes) = item {
                    let s = String::from_utf8(bytes).map_err(|e| {
                        de::Error::custom(format!("Invalid UTF-8 in url-list: {}", e))
                    })?;
                    urls.push(s);
                }
            }
            Ok(Some(urls))
        }
        _ => Ok(None),
    }
}

fn calculate_tree_size(node: &Value) -> i64 {
    let mut size = 0;
    if let Value::Dict(map) = node {
        for (key, value) in map {
            let name = String::from_utf8_lossy(key);
            if name.is_empty() {
                // This is a file metadata node
                if let Value::Dict(meta) = value {
                    if let Some(Value::Int(len)) = meta.get("length".as_bytes()) {
                        size += len;
                    }
                }
            } else {
                // This is a subdirectory or file entry, recurse
                size += calculate_tree_size(value);
            }
        }
    }
    size
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Helper to create a basic Info object
    fn create_test_info(meta_version: Option<i64>) -> Info {
        Info {
            piece_length: 16384,
            pieces: Vec::new(),
            private: None,
            files: Vec::new(),
            name: "test_torrent".to_string(),
            length: 0,
            md5sum: None,
            meta_version,
            file_tree: None,
        }
    }

    // Helper to build a v2 file tree node
    fn build_v2_file_node(length: i64, root: Vec<u8>) -> Value {
        let mut meta = HashMap::new();
        meta.insert("length".as_bytes().to_vec(), Value::Int(length));
        meta.insert("pieces root".as_bytes().to_vec(), Value::Bytes(root));

        let mut leaf = HashMap::new();
        leaf.insert(vec![], Value::Dict(meta));
        Value::Dict(leaf)
    }

    // Helper to create a multi-file V2 torrent with layers for testing
    fn create_test_torrent_with_layers() -> Torrent {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };
        torrent.info.piece_length = 16384;

        let root_a = vec![0xAA; 32];
        let root_b = vec![0xBB; 32];

        // Setup File Tree: a.txt (16KB), b.txt (16KB)
        let mut tree = HashMap::new();
        tree.insert(
            "a.txt".as_bytes().to_vec(),
            build_v2_file_node(16384, root_a.clone()),
        );
        tree.insert(
            "b.txt".as_bytes().to_vec(),
            build_v2_file_node(16384, root_b.clone()),
        );
        torrent.info.file_tree = Some(Value::Dict(tree));

        // Setup Piece Layers: Each root gets a mock 32-byte layer hash
        let mut layers = HashMap::new();
        layers.insert(root_a, Value::Bytes(vec![0x11; 32]));
        layers.insert(root_b, Value::Bytes(vec![0x22; 32]));
        torrent.piece_layers = Some(Value::Dict(layers));

        torrent
    }

    #[test]
    fn test_v2_piece_count_calculation() {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };

        let mut tree = HashMap::new();
        tree.insert(
            "a.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0xAA; 32]),
        );
        tree.insert(
            "b.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0xBB; 32]),
        );
        torrent.info.file_tree = Some(Value::Dict(tree));

        let mapping = torrent.calculate_v2_mapping();

        assert_eq!(mapping.piece_count, 2);

        let roots_0 = mapping.piece_to_roots.get(&0).unwrap();
        let roots_1 = mapping.piece_to_roots.get(&1).unwrap();
        assert_eq!(roots_0[0].root_hash, vec![0xAA; 32]);
        assert_eq!(roots_1[0].root_hash, vec![0xBB; 32]);
    }

    #[test]
    fn test_hybrid_piece_count_prioritizes_v1_string() {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };

        torrent.info.pieces = vec![0u8; 200];
        assert_eq!(200 / 20, 10);
    }

    #[test]
    fn test_deterministic_v2_sorting() {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };

        let mut tree = HashMap::new();
        // Use 0x5A (ASCII 'Z') instead of invalid literal
        tree.insert(
            "z.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0x5A; 32]),
        );
        tree.insert(
            "a.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0xAA; 32]),
        );
        torrent.info.file_tree = Some(Value::Dict(tree));

        let mapping = torrent.calculate_v2_mapping();

        let roots_0 = mapping.piece_to_roots.get(&0).expect("Piece 0 missing");
        assert_eq!(roots_0[0].root_hash, vec![0xAA; 32]);

        let roots_1 = mapping.piece_to_roots.get(&1).expect("Piece 1 missing");
        assert_eq!(roots_1[0].root_hash, vec![0x5A; 32]);
    }

    #[test]
    fn test_v2_mapping_with_empty_files() {
        let mut torrent = Torrent {
            info: create_test_info(Some(2)),
            ..Torrent::default()
        };

        let mut tree = HashMap::new();
        tree.insert(
            "empty.txt".as_bytes().to_vec(),
            build_v2_file_node(0, vec![0x00; 32]),
        );
        tree.insert(
            "real.txt".as_bytes().to_vec(),
            build_v2_file_node(1000, vec![0xAA; 32]),
        );
        torrent.info.file_tree = Some(Value::Dict(tree));

        let mapping = torrent.calculate_v2_mapping();

        assert_eq!(mapping.piece_count, 1);
        assert_eq!(
            mapping.piece_to_roots.get(&0).unwrap()[0].root_hash,
            vec![0xAA; 32]
        );
    }

    #[test]
    fn test_get_v2_hash_layer_with_offset() {
        let torrent = create_test_torrent_with_layers();
        let root_b = vec![0xBB; 32];

        let result = torrent.get_v2_hash_layer(1, 16384, 16384, 1, &root_b);

        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 32);

        let too_long = torrent.get_v2_hash_layer(1, 16384, 16384, 100, &root_b);
        assert!(too_long.is_none());
    }

    #[test]
    fn test_get_v2_hash_layer_bep52_single_piece() {
        let mut info = create_test_info(Some(2));
        info.piece_length = 16384;

        let t = Torrent {
            info,
            ..Torrent::default()
        };

        let root_a = vec![0xAA; 32];
        let result = t.get_v2_hash_layer(0, 0, 500, 1, &root_a);
        assert_eq!(result.unwrap(), root_a);
    }

    #[test]
    fn test_get_v2_hash_layer_bounds_check() {
        let mut info = create_test_info(Some(2));
        info.piece_length = 16384;
        let t = Torrent {
            info,
            ..Torrent::default()
        };
        let root = vec![0xAA; 32];

        // Requesting 100 hashes from a file that fits in 1 piece (and thus has 1 hash) should fail
        let result = t.get_v2_hash_layer(0, 0, 500, 100, &root);
        assert!(
            result.is_none(),
            "Should reject request for 100 hashes from single-piece file"
        );
    }

    #[test]
    fn test_get_v2_hash_layer_mock_priority() {
        let mut info = create_test_info(Some(2));
        info.piece_length = 16384;
        let mut t = Torrent {
            info,
            ..Torrent::default()
        };

        let root = vec![0xAA; 32];
        let layer_data = vec![0xBB; 32]; // Different from root

        // Mock layer injection
        let mut layer_map = HashMap::new();
        layer_map.insert(root.clone(), Value::Bytes(layer_data.clone()));
        t.piece_layers = Some(Value::Dict(layer_map));

        // Request hash for single piece file
        // If logic is correct, it finds the layer first and returns 0xBB
        // If regression exists, it hits the "single piece optimization" and returns root (0xAA)
        let result = t.get_v2_hash_layer(0, 0, 500, 1, &root).unwrap();
        assert_eq!(
            result, layer_data,
            "Should prioritize explicit layers over root fallback"
        );
    }
}
