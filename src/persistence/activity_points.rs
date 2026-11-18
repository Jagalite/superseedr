// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Exact logical samples, with zero runs and a dense fallback for busy series.
use super::activity_history::ActivityHistoryPoint as Point;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::VecDeque;

#[derive(Clone, Debug)]
enum Entry {
    Value(Point),
    Zero { start: u64, step: u64, count: usize },
}
#[derive(Clone, Debug)]
enum Storage {
    Dense(Vec<Point>),
    Runs(VecDeque<Entry>),
}
#[derive(Clone, Debug)]
pub struct ActivityPoints {
    storage: Storage,
    len: usize,
    since_check: usize,
}
impl Default for ActivityPoints {
    fn default() -> Self {
        Self {
            storage: Storage::Runs(VecDeque::new()),
            len: 0,
            since_check: 0,
        }
    }
}
impl PartialEq for ActivityPoints {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}
impl Eq for ActivityPoints {}
impl PartialEq<Vec<Point>> for ActivityPoints {
    fn eq(&self, other: &Vec<Point>) -> bool {
        self.iter().eq(other.iter().cloned())
    }
}
impl ActivityPoints {
    pub fn empty() -> &'static Self {
        static EMPTY: std::sync::LazyLock<ActivityPoints> =
            std::sync::LazyLock::new(ActivityPoints::default);
        &EMPTY
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    #[cfg(test)]
    pub(crate) fn backing_bytes(&self) -> usize {
        match &self.storage {
            Storage::Dense(d) => d.capacity() * std::mem::size_of::<Point>(),
            Storage::Runs(r) => r.capacity() * std::mem::size_of::<Entry>(),
        }
    }
    pub fn first(&self) -> Option<Point> {
        self.iter().next()
    }
    pub fn last(&self) -> Option<Point> {
        self.iter().next_back()
    }
    #[cfg(test)]
    pub(crate) fn get(&self, index: usize) -> Option<Point> {
        self.iter().nth(index)
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Point> + '_ {
        // A one-element enum iterator avoids allocating a boxed iterator on every graph draw.
        enum Source<'a> {
            Dense(std::slice::Iter<'a, Point>),
            Runs(std::collections::vec_deque::Iter<'a, Entry>),
        }
        impl Iterator for Source<'_> {
            type Item = Entry;
            fn next(&mut self) -> Option<Entry> {
                match self {
                    Self::Dense(d) => d.next().cloned().map(Entry::Value),
                    Self::Runs(r) => r.next().cloned(),
                }
            }
        }
        impl DoubleEndedIterator for Source<'_> {
            fn next_back(&mut self) -> Option<Entry> {
                match self {
                    Self::Dense(d) => d.next_back().cloned().map(Entry::Value),
                    Self::Runs(r) => r.next_back().cloned(),
                }
            }
        }
        let source = match &self.storage {
            Storage::Dense(d) => Source::Dense(d.iter()),
            Storage::Runs(r) => Source::Runs(r.iter()),
        };
        source.flat_map(|entry| {
            let count = match &entry {
                Entry::Value(_) => 1,
                Entry::Zero { count, .. } => *count,
            };
            (0..count).map(move |i| match &entry {
                Entry::Value(p) => p.clone(),
                Entry::Zero { start, step, .. } => Point {
                    ts_unix: start + (i as u64) * step,
                    primary: 0,
                    secondary: 0,
                },
            })
        })
    }
    fn append_run(r: &mut VecDeque<Entry>, p: Point) {
        if p.primary == 0 && p.secondary == 0 {
            if let Some(Entry::Zero { start, step, count }) = r.back_mut() {
                if *count == 1 && p.ts_unix > *start {
                    *step = p.ts_unix - *start;
                    *count = 2;
                    return;
                }
                if (*count as u64)
                    .checked_mul(*step)
                    .and_then(|n| start.checked_add(n))
                    == Some(p.ts_unix)
                {
                    *count += 1;
                    return;
                }
            }
            r.push_back(Entry::Zero {
                start: p.ts_unix,
                step: 0,
                count: 1,
            });
        } else {
            r.push_back(Entry::Value(p));
        }
    }
    pub fn push(&mut self, p: Point) {
        match &mut self.storage {
            Storage::Dense(d) => {
                if d.len() == d.capacity() {
                    d.reserve_exact(d.len().clamp(4, 64));
                }
                d.push(p);
            }
            Storage::Runs(r) => Self::append_run(r, p),
        }
        self.len += 1;
        self.since_check += 1;
        self.rebalance();
    }
    pub fn restore(&self, step: u64, cap: usize, now: u64) -> Self {
        let mut out = Self::default();
        if self.is_empty() || step == 0 || cap == 0 {
            return out;
        }
        let last = self.last().unwrap().ts_unix;
        let end = if last >= now {
            last
        } else {
            last.saturating_add(((now - last) / step).saturating_mul(step))
        };
        let start = self
            .first()
            .unwrap()
            .ts_unix
            .max(end.saturating_sub(step.saturating_mul(cap.saturating_sub(1) as u64)));
        let mut next = start;
        for p in self.iter().skip_while(|p| p.ts_unix < start) {
            while next < p.ts_unix && next <= end {
                out.push(Point {
                    ts_unix: next,
                    primary: 0,
                    secondary: 0,
                });
                let advanced = next.saturating_add(step);
                if advanced == next {
                    return out;
                }
                next = advanced;
            }
            if next > end {
                break;
            }
            let ts = p.ts_unix;
            out.push(p);
            if ts >= end {
                return out;
            }
            let advanced = ts.saturating_add(step);
            if advanced == ts {
                return out;
            }
            next = advanced;
        }
        while next <= end {
            out.push(Point {
                ts_unix: next,
                primary: 0,
                secondary: 0,
            });
            let advanced = next.saturating_add(step);
            if advanced == next {
                break;
            }
            next = advanced;
        }
        out
    }
    pub fn push_retained(&mut self, p: Point, cap: usize) {
        if cap == 0 {
            self.clear();
            return;
        }
        self.retain_last(cap - 1);
        self.push(p);
    }
    pub fn retain_last(&mut self, cap: usize) {
        if self.len <= cap {
            return;
        }
        let mut remove = self.len - cap;
        match &mut self.storage {
            Storage::Dense(d) => {
                d.drain(..remove);
            }
            Storage::Runs(r) => {
                while remove > 0 {
                    match r.front_mut().unwrap() {
                        Entry::Zero { start, step, count } if *count > remove => {
                            *start += remove as u64 * *step;
                            *count -= remove;
                            remove = 0;
                        }
                        entry => {
                            let n = match entry {
                                Entry::Value(_) => 1,
                                Entry::Zero { count, .. } => *count,
                            };
                            remove -= n;
                            r.pop_front();
                        }
                    }
                }
            }
        }
        self.len = cap;
    }
    fn rebalance(&mut self) {
        let mut replacement = None;
        match &mut self.storage {
            Storage::Runs(r) => {
                if r.capacity() > r.len().saturating_mul(2).max(8) {
                    r.shrink_to_fit();
                }
                if self.len >= 8
                    && r.capacity() * std::mem::size_of::<Entry>()
                        > self.len * std::mem::size_of::<Point>()
                {
                    let mut dense: Vec<_> = self.iter().collect();
                    dense.shrink_to_fit();
                    replacement = Some(Storage::Dense(dense));
                    self.since_check = 0;
                }
            }
            Storage::Dense(d) if self.since_check >= d.len().clamp(64, 1024) => {
                self.since_check = 0;
                // Every nonzero sample needs its own entry. Reject busy series
                // before allocating a temporary run buffer that cannot win.
                let nonzero = d
                    .iter()
                    .filter(|p| p.primary != 0 || p.secondary != 0)
                    .count();
                if nonzero * std::mem::size_of::<Entry>() * 2
                    >= d.capacity() * std::mem::size_of::<Point>()
                {
                    return;
                }
                let mut runs = VecDeque::new();
                for p in d.iter() {
                    Self::append_run(&mut runs, p.clone());
                }
                if runs.capacity() * std::mem::size_of::<Entry>() * 2
                    < d.capacity() * std::mem::size_of::<Point>()
                {
                    replacement = Some(Storage::Runs(runs));
                }
            }
            _ => {}
        }
        if let Some(storage) = replacement {
            self.storage = storage;
        }
    }
}
impl From<Vec<Point>> for ActivityPoints {
    fn from(points: Vec<Point>) -> Self {
        points.into_iter().collect()
    }
}
impl FromIterator<Point> for ActivityPoints {
    fn from_iter<T: IntoIterator<Item = Point>>(iter: T) -> Self {
        let mut out = Self::default();
        for point in iter {
            out.push(point);
        }
        out
    }
}
impl Serialize for ActivityPoints {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.len))?;
        for point in self.iter() {
            sequence.serialize_element(&point)?;
        }
        sequence.end()
    }
}
impl<'de> Deserialize<'de> for ActivityPoints {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ActivityPoints;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("activity samples")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut out = ActivityPoints::default();
                while let Some(point) = seq.next_element()? {
                    out.push(point);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_seq(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn point(t: u64, p: u64) -> Point {
        Point {
            ts_unix: t,
            primary: p,
            secondary: 0,
        }
    }
    #[test]
    fn exact_retention_dense_idle_and_irregular_transitions() {
        for cap in [0, 1, 7, 64, 257] {
            let mut sparse = ActivityPoints::default();
            let mut dense = Vec::new();
            let mut timestamp = 0u64;
            for i in 0..8000 {
                timestamp += if i % 97 == 0 {
                    0
                } else if i % 89 == 0 {
                    7
                } else {
                    1
                };
                let p = point(
                    timestamp,
                    if (i / 1500) % 2 == 0 {
                        0
                    } else {
                        i as u64 % 23
                    },
                );
                sparse.push_retained(p.clone(), cap);
                dense.push(p);
                if dense.len() > cap {
                    dense.drain(..dense.len() - cap);
                }
                assert_eq!(sparse.iter().collect::<Vec<_>>(), dense);
                assert_eq!(
                    sparse.iter().rev().collect::<Vec<_>>(),
                    dense.iter().rev().cloned().collect::<Vec<_>>()
                );
            }
        }
    }
    #[test]
    fn restores_exact_legacy_points_without_retaining_zeros() {
        for step in [0, 1, 60, 900, 3600, u64::MAX] {
            for cap in [0, 1, 17, 3600] {
                for end in [0, 37, 5000, u64::MAX] {
                    for timestamps in [
                        vec![],
                        vec![0],
                        vec![3, 3, 37, 91],
                        vec![9, 3, 7],
                        vec![u64::MAX - 7, u64::MAX - 3, u64::MAX],
                    ] {
                        let input: Vec<_> = timestamps
                            .into_iter()
                            .enumerate()
                            .map(|(i, t)| point(t, if i % 2 == 0 { i as u64 + 1 } else { 0 }))
                            .collect();
                        let sparse: ActivityPoints = input.clone().into();
                        let expected =
                            crate::telemetry::restore_densify::densify_points_for_restore(
                                &input,
                                step,
                                cap,
                                end,
                                |p| p.ts_unix,
                                |t| point(t, 0),
                            );
                        assert_eq!(
                            sparse.restore(step, cap, end).iter().collect::<Vec<_>>(),
                            expected
                        );
                    }
                }
            }
        }
        let p: ActivityPoints = vec![point(1, 0), point(3600, 0)].into();
        let p = p.restore(1, 3600, 3600);
        assert_eq!(p.len(), 3600);
        assert!(p.backing_bytes() < 1024);
        let encoded = serde_json::to_vec(&p).unwrap();
        let decoded: ActivityPoints = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(p, decoded);
        assert!(decoded.backing_bytes() < 1024);
    }
    #[test]
    fn serde_sequence_matches_legacy_vector() {
        for points in [
            vec![],
            vec![point(1, 7)],
            vec![point(1, 0), point(2, 0), point(3, 9)],
        ] {
            let packed: ActivityPoints = points.clone().into();
            assert_eq!(
                serde_json::to_vec(&packed).unwrap(),
                serde_json::to_vec(&points).unwrap()
            );
            assert_eq!(
                serde_bencode::to_bytes(&packed).unwrap(),
                serde_bencode::to_bytes(&points).unwrap()
            );
        }
    }
    #[test]
    fn busy_tiers_stay_dense_and_idle_tiers_release_peak() {
        let mut p = ActivityPoints::default();
        for t in 0..3600 {
            p.push_retained(point(t, 7), 3600);
        }
        assert!(
            p.backing_bytes()
                <= 3600 * std::mem::size_of::<Point>() + 64 * std::mem::size_of::<Point>()
        );
        for t in 3600..9000 {
            p.push_retained(point(t, 0), 3600);
        }
        assert_eq!(p.len(), 3600);
        assert!(p.backing_bytes() < 1024);
        for t in 9000..13000 {
            p.push_retained(point(t, 9), 3600);
        }
        assert!(p.iter().all(|v| v.primary == 9));
    }
}
