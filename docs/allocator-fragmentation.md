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
Restore the original executable after temporary diagnostic runs unless the operator
asks to retain the replacement. Verify the selected executable hash and catalog.

A short 5–7 minute restarted run can test whether relief releases pages in that
state. It does not automatically reproduce a mature process's allocation layout.
Record whether the catalog finished loading and whether allocation levels settled.
Verify that the allocator implements relief before interpreting its return value.
Apple's xzone implementation installs
[`malloc_zone_pressure_relief_fallback`](https://github.com/apple-oss-distributions/libmalloc/blob/c49dafa25f1efe8607701ae6014a663ad2ee437f/src/malloc_common.c#L161),
which simply returns zero. A host probe resolved this exact callback on the
investigated Mac's default allocator. A zero from that implementation measures
neither attempted reclamation nor irreducible fragmentation. Stack logging can
also install wrapper zones, so check the allocator actually used by each run.

Reconcile `vmmap` fragmentation with its **DIRTY + SWAPPED** columns, not
RESIDENT + SWAPPED. Resident storage can include reusable or non-dirty pages.
Gaps on occupied virtual pages provide an upper bound on the part attributable
to such gaps; they do not explain backing on pages with no enumerated blocks.

## Following sparse pages and their owners

Compare allocation addresses, sizes, and overlap within each page across two
snapshots of the same process. Distinguish unchanged sparse layouts, changed
sparse layouts, pages becoming empty, and pages becoming denser. Identical
addresses and sizes do not prove object survival: allocation reuse can occur
between snapshots.

For field attribution, record pointers while their owner is borrowed, then match
those pointers to the containing malloc block. Require matches in both snapshot
windows and retain an unattributed category. A pointer into a nonempty hash
table identifies its backing allocation; this does not cover empty retained
tables. Asynchronous census and heap snapshots can race with compaction or
free/reallocation. Do not assign all gaps on a mixed page to one named field.

Stack-logged address queries can identify allocation sites for representative
objects on persistent sparse pages. Use `ALLOC` records, exclude `VM_ALLOC`
region-reservation records, and report missed or mismatched addresses. Sampling
one object per page does not establish ownership of every object on that page.
Keep stack-logged attribution separate from normal-allocator footprint amounts.

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
