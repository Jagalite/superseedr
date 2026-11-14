# Measuring allocator fragmentation on macOS

Keep three quantities separate:

- Live malloc block bytes include allocation-size rounding and unused collection capacity.
- `vmmap -summary` allocator fragmentation is allocator accounting, not a promise of reclaimable physical memory.
- Physical footprint also reflects non-malloc memory and the OS's memory accounting. RSS alone is especially misleading under compression or swapping.

Capture the mature process before restarting it. Record uptime, loaded torrent count, connected peers, and DHT activity alongside each snapshot. Do not use startup peaks to infer steady-state costs.

```sh
sysctl -n hw.pagesize
vmmap -summary "$daemon_pid" > before.vmmap
heap --noContent --addresses=all "$daemon_pid" > before.heap
python3 scripts/heap_page_occupancy.py before.heap --page-size 16384 > before.pages.json
```

Use the target's actual page size. The example value is for the measured Apple Silicon host. Heap inspection may pause the process; adjacent status, heap, and VM snapshots are not atomic.

The analyzer verifies non-overlapping blocks and reconciles block bytes with page occupancy. Sparse-page contributors count only the portions of blocks lying on pages at most 25% occupied. A large block's tail can therefore contribute to a sparse page. Size classes do not identify Rust types or owners.

Page holes are virtual address space outside enumerated live blocks. They may contain allocator metadata or already-decommitted storage. Entirely empty pages and waste inside malloc blocks are outside this measurement. Do not subtract this total from a different snapshot's fragmentation figure.

## Controlled pressure-relief experiment

Use a diagnostic build with an explicit one-shot call to
`malloc_zone_pressure_relief(NULL, 0)`. Keep application collection/buffer trims
disabled, and record the API's returned byte count. Run without `MallocStackLogging`
or other allocator overrides; stack logging can change allocator behavior.
Capture immediately before, immediately after, and later during ordinary activity.
Always gracefully restore the original executable, checking its hash and catalog.

A short 5–7 minute restarted run can test whether relief releases pages in that
state. It does not automatically reproduce a mature process's allocation layout.
Record whether the catalog finished loading and whether allocation levels settled.
A zero return supports avoiding periodic relief in the tested state, not a claim
that every future state is unreclaimable.

## Choosing follow-up changes

First measure which long-lived allocations share sparse pages with short-lived
ones. Then compare bounded capacities, fewer temporary allocations, or storage
grouped by lifetime under the same workload. An alternate allocator needs its own
throughput, latency, footprint, and churn comparison. Application compaction can
reduce live bytes while leaving allocator pages retained, so measure both.

Run the parser's boundary and overlap checks with:

```sh
python3 -m unittest discover -s scripts -p test_heap_page_occupancy.py
```
