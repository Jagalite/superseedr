// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later
//! Private opt-in experiments. One executable supports all four matched cohorts.
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::LazyLock,
};
static OPTIONS: LazyLock<String> =
    LazyLock::new(|| std::env::var("SUPERSEEDR_MEMORY_EXPERIMENTS").unwrap_or_default());
pub(crate) fn enabled(name: &str) -> bool {
    OPTIONS.split(',').any(|option| option == name)
}
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
