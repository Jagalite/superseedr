// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later
//! Release excess collection capacity after churn, with headroom to avoid resize churn.
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};
pub(crate) fn compact_map<K: Eq + Hash, V>(map: &mut HashMap<K, V>) {
    if map.is_empty() && map.capacity() > 0 {
        *map = HashMap::new();
    } else if map.capacity() >= 128 && map.capacity() > map.len().saturating_mul(4) {
        map.shrink_to(map.len().saturating_mul(2));
    }
}
pub(crate) fn compact_set<K: Eq + Hash>(set: &mut HashSet<K>) {
    if set.is_empty() && set.capacity() > 0 {
        *set = HashSet::new();
    } else if set.capacity() >= 128 && set.capacity() > set.len().saturating_mul(4) {
        set.shrink_to(set.len().saturating_mul(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_preserves_entries_and_avoids_resizing_dense_tables() {
        for len in [0, 1, 31, 127, 1024] {
            let mut map: HashMap<usize, usize> = (0..len).map(|i| (i, i * 7)).collect();
            let mut set: HashSet<usize> = (0..len).collect();
            map.reserve(8192);
            set.reserve(8192);
            compact_map(&mut map);
            compact_set(&mut set);
            assert_eq!(map.len(), len);
            assert_eq!(set.len(), len);
            assert!((0..len).all(|i| map.get(&i) == Some(&(i * 7)) && set.contains(&i)));
            let capacities = (map.capacity(), set.capacity());
            compact_map(&mut map);
            compact_set(&mut set);
            assert_eq!(capacities, (map.capacity(), set.capacity()));
            if len == 0 {
                assert_eq!(capacities, (0, 0));
            } else {
                assert!(capacities.0 >= len && capacities.0 < 8192);
            }
        }
        let mut map: HashMap<_, _> = (0..64).map(|i| (i, i)).collect();
        let before = map.capacity();
        compact_map(&mut map);
        assert_eq!(map.capacity(), before);
    }
}
