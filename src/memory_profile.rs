// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Opt-in requested Rust heap accounting. Excludes allocator overhead, native
//! libraries, stacks, mapped files, and kernel socket buffers. Atomics perturb
//! timing; compare equally instrumented runs. Peaks are process-lifetime peaks.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

struct CountingAllocator;
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static CALLS: AtomicU64 = AtomicU64::new(0);

fn add(size: usize) {
    let live = LIVE.fetch_add(size as u64, Relaxed) + size as u64;
    PEAK.fetch_max(live, Relaxed);
    ALLOCATED.fetch_add(size as u64, Relaxed);
}

// SAFETY: All allocation contracts and alignment requirements are delegated to
// System. Accounting never allocates and failed allocations leave LIVE unchanged.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            add(layout.size());
            CALLS.fetch_add(1, Relaxed);
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            add(layout.size());
            CALLS.fetch_add(1, Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size() as u64, Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, size) };
        if !new.is_null() {
            CALLS.fetch_add(1, Relaxed);
            if size >= layout.size() {
                add(size - layout.size());
            } else {
                LIVE.fetch_sub((layout.size() - size) as u64, Relaxed);
            }
        }
        new
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct Snapshot {
    pub live_bytes: u64,
    pub peak_bytes: u64,
    pub allocated_bytes: u64,
    pub allocation_calls: u64,
    pub network_tasks: [u64; 5],
    pub utp_drain_timeouts: [u64; 2],
    pub network_buffer_capacity_bytes: [u64; 5],
}

pub(crate) fn snapshot() -> Snapshot {
    Snapshot {
        live_bytes: LIVE.load(Relaxed),
        peak_bytes: PEAK.load(Relaxed),
        allocated_bytes: ALLOCATED.load(Relaxed),
        allocation_calls: CALLS.load(Relaxed),
        utp_drain_timeouts: std::array::from_fn(|i| DRAIN_TIMEOUTS[i].load(Relaxed)),
        network_tasks: std::array::from_fn(|i| TASKS[i].load(Relaxed)),
        network_buffer_capacity_bytes: std::array::from_fn(|i| BUFFERS[i].load(Relaxed)),
    }
}

// Indices: reader, writer, uTP driver, peer session, closing uTP driver. Buffer counters only cover
// the explicit protocol reader/writer Vec capacities, not all task allocations.
static TASKS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static BUFFERS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
pub(crate) struct TaskProbe {
    kind: usize,
    capacity: usize,
}
impl TaskProbe {
    pub(crate) fn new(kind: usize, capacity: usize) -> Self {
        TASKS[kind].fetch_add(1, Relaxed);
        BUFFERS[kind].fetch_add(capacity as u64, Relaxed);
        Self { kind, capacity }
    }
    pub(crate) fn set_capacity(&mut self, capacity: usize) {
        if capacity >= self.capacity {
            BUFFERS[self.kind].fetch_add((capacity - self.capacity) as u64, Relaxed);
        } else {
            BUFFERS[self.kind].fetch_sub((self.capacity - capacity) as u64, Relaxed);
        }
        self.capacity = capacity;
    }
}
impl Drop for TaskProbe {
    fn drop(&mut self) {
        BUFFERS[self.kind].fetch_sub(self.capacity as u64, Relaxed);
        TASKS[self.kind].fetch_sub(1, Relaxed);
    }
}

pub(crate) fn capture_metadata(info_hash: &[u8], torrent: &crate::torrent_file::Torrent) {
    use std::io::Write;
    let Some(root) = std::env::var_os("SUPERSEEDR_MEMORY_METADATA_DIR") else {
        return;
    };
    let path = std::path::Path::new(&root).join(format!("{}.torrent", hex::encode(info_hash)));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        // Exact encoded info preserves identity. These private offline snapshots
        // omit trackers; they are never used for an online swarm comparison.
        let _ = file
            .write_all(b"d4:info")
            .and_then(|_| file.write_all(&torrent.info_dict_bencode))
            .and_then(|_| file.write_all(b"e"));
    }
}

// Total abandoned-stream deadlines; subset known to have only FIN awaiting ACK.
static DRAIN_TIMEOUTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
pub(crate) fn record_drain_timeout(fin_only: bool) {
    DRAIN_TIMEOUTS[0].fetch_add(1, Relaxed);
    if fin_only {
        DRAIN_TIMEOUTS[1].fetch_add(1, Relaxed);
    }
}
