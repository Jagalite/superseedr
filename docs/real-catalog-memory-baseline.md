# Real catalog idle memory measurement

> Archived experiment report. The prototypes and raw artifacts live in the separate
> `superseedr-memory-lifecycle` worktree. See [selected implementation](memory-selected-implementation.md)
> for the narrower changes adopted on develop; measurements below are historical
> and do not describe a build containing only those changes.

Measured 2026-10-15 on this Mac using a disposable copy of the active shared
catalog. Both three-minute runs loaded all 125 entries. 77 entries had usable
piece metadata; 48 remained without it while networking was blocked. All entries
retained their copied Running state. Both runs had zero peers and zero transfer
rates. This is an offline loaded-catalog baseline, not a live swarm measurement.

| Variant | Settled median RSS (MiB) | Settled RSS range (MiB) | Whole-run peak RSS (MiB) | Native allocated heap (MiB) |
|---|---:|---:|---:|---:|
| Prior idle behavior | 42.81 | 41.95–52.62 | 52.62 | 23.73 |
| Combined idle candidates | 41.31 | 41.02–43.03 | 46.00 | 19.97 |

Settled samples are seconds 150–180 (31 samples), after the rolling startup loaded
all 125 entries. RSS was sampled once per second with `ps`. The earlier progress
update called the prior peak a startup peak; it was actually the whole-run peak,
including the settled window. This is one run per variant, prior then combined;
1.50 MiB lower median RSS is an observation, not a statistically qualified gain.
Native heap fell by 3.77 MiB. No startup or shutdown failures occurred; both
processes exited normally with code 0 and were confirmed stopped.

## What remains allocated

Native `heap` reports 25,645 allocations totaling 19.97 MiB for the combined run,
versus 25,553 totaling 23.73 MiB for prior behavior. The prior histogram contains
one 4,000 KiB allocation; the combined histogram removes it and adds a 128 KiB
allocation, consistent with the logging queue change. Allocation backtraces were
not enabled, so this is corroboration rather than direct ownership attribution.
There are no peer buffers to reclaim in this offline workload.

The combined `vmmap` snapshot reports 35.7M physical footprint, versus 38.1M prior
(as formatted by the tool). Combined malloc-small regions alone have 22.5M
resident plus 8,512K swapped, and stacks have 1,600K resident. Allocator regions
include free capacity/fragmentation; allocated heap, RSS, physical footprint,
and summed shared-library mappings are different metrics and must not be added
together. The app retains catalog/manager/UI/runtime state at idle, and native
heap plus allocator overhead explains a substantial part of this floor. Exact
per-subsystem attribution needs allocation backtraces; this run does not establish
that the remaining memory is unnecessary or that near-zero idle RAM is feasible.

## Isolation and build identity

- Shared settings, catalog and metadata were copied into private XDG directories;
  this host's 78 cached torrent files were copied too. Payload paths point to the
  original files for read-only access. The run uses standalone copied settings,
  rather than the live shared coordination mode.
- macOS sandbox denied networking and all writes outside the disposable run root
  (except terminal devices and `/dev/null`). An external write-denial probe passed.
  Original settings/catalog/metadata SHA-256 checks remained unchanged after both
  runs. No live catalog changes or payload writes were authorized or performed.
- Only the copies disable RSS feeds, use an empty watch folder, choose network
  binding `any` and port 0, and emit status once per second. Network-denial retry
  behavior and status instrumentation may affect this offline footprint.
- Both variants use the same saved Kyoto-built release executable, SHA-256
  `cbc7ea924c320e4ae00aafa929b34c6e29ad44490a2e6fea7633573efb832cff`.
  Build job `503c8fbab85b4806823e27c055029a6c` used
  `--no-default-features --features memory-profile,dht,pex`, LTO disabled and
  16 codegen units. WebRTC is excluded; this is not the installed normal build.
- `prior` disables only the three latest idle candidates; earlier lifecycle
  optimizations remain present in both variants. `combined` enables the smaller
  logging queue, lazy reader buffer, and idle buffer reclamation.

Native inspection happened after RSS sampling. Private raw samples, snapshots,
source hashes and aggregate `summary.json` remain under
`tmp/memory-lifecycle/real-catalog/`; the local runner is
`tmp/memory-lifecycle/run-real-catalog.py`. Raw terminal/config data can contain
real catalog names and remains outside tracked documentation. See
[idle-memory-experiments.md](idle-memory-experiments.md) for synthetic peer tests
and candidate tradeoffs.
