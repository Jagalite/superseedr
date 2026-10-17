# Why idle memory stays allocated

> Archived experiment report. The prototypes and raw artifacts live in the separate
> `superseedr-memory-lifecycle` worktree. See [selected implementation](memory-selected-implementation.md)
> for the narrower changes adopted on develop; measurements below are historical
> and do not describe a build containing only those changes.

Measured 2026-10-15. The earlier 41 MiB RSS measurement is **not an established
minimum**. These experiments identify current allocations and distinguish
releasable allocator pages from live application state. They do not prove that
a much smaller idle design is impossible.

## Findings

The full offline catalog retained roughly 18 MiB of requested Rust heap. Native
malloc inspection found about 20 MiB, including allocator size rounding and
non-Rust allocations. Symbolized allocation backtraces account for 20.04 MiB,
within 0.02 MiB of the adjacent native heap snapshot:

| Allocation path | MiB |
|---|---:|
| Torrent metadata cloned into manager state | 6.64 |
| Message channels, across the application | 2.99 |
| Torrent manager task allocations | 1.46 |
| Packed block flags | 1.36 |
| Download diagnostics | 0.90 |
| Activity history and checkpoint copies | 0.87 |
| UI histories and aggregation | 0.74 |
| Default torrent-state allocations | 0.73 |
| Terminal rendering | 0.62 |
| Other piece state | 0.38 |
| Other traced allocations | 3.36 |

These categories are mutually exclusive allocation-path groups, not independently
measured savings. Backtraces identify allocation sites, not necessarily the final
retaining owner. LLVM folds some identical generic functions: a generic `Vec`
type name can be misleading, so classification uses higher-level application
frames. Checkpoint copies can be transient. Thread-stack reservations and VM
mappings were excluded from this heap total. The remaining group includes app
maps, metrics/watch snapshots, tracker state, runtime structures, logging, native
library objects, and small configuration/string allocations.

There were 125 displayed catalog entries, 77 with usable metadata and 48 without.
The trace showed 124 manager-task allocations; displayed entries must not be
assumed to equal live manager count. All measurements blocked external networking,
had zero peers and zero transfer rates, and excluded WebRTC.

The 77 source torrent files describe 164,840 pieces, matching the loaded status
aggregate. Their encoded info dictionaries total 3,310,215 bytes, and their piece
hash fields total 3,296,800 bytes. `Torrent` retains both encoded info and parsed
hashes. That is 6.30 MiB of represented bytes before other metadata and allocation
rounding, consistent with the 6.64 MiB trace category. This is a representation
cost, not an information-theoretic lower bound: shared storage or deferred loading
could reduce it, subject to exact-info-hash and metadata-serving correctness.

## Experiments

### Asking the allocator to release unused pages

The diagnostic build sampled requested Rust heap every second. At 160 seconds it
called macOS `malloc_zone_pressure_relief(NULL, 0)` once. Apple's installed
`malloc.h` defines this as best-effort maximal relief across all zones.

| Window | Median RSS (MiB) | Median requested Rust heap (MiB) |
|---|---:|---:|
| Before request, seconds 135–155 | 51.75 | 17.94 |
| After request, seconds 180–200 | 52.61 | 18.56 |

The API reported **0 bytes released**. This did not solve the idle footprint.
It does not prove that every allocator byte is necessary: fragmentation and mixed
live/free pages can prevent release, and this is one attempt at one instant.
Histories continued growing during the run, so the later increase is not evidence
that the pressure-relief call caused growth. At the end, native allocated heap was
20.28 MiB and `vmmap` reported physical footprint `36.6M`.

The original run had 11.5M swapped across mapped regions; this run's malloc regions
showed no swapped pages. That helps explain why the earlier RSS was around 41 MiB
while this diagnostic reached around 53 MiB. Different executable builds and
instrumentation also prevent a direct optimization comparison between those runs.
RSS, native malloc bytes, requested Rust heap, physical footprint, and virtual
reservations are different metrics; do not add them together.

### Empty catalog, worker count, and viewport

All four controls below use the same new diagnostic executable, the same sampler,
30-second durations, and medians from seconds 15–29. No build ran concurrently.
Only disposable copies were changed. Empty-catalog runs still include the copied
settings and metadata cache; they are a zero-manager control, not a minimal Rust
program or a fully headless service.

| Control | Requested Rust heap (MiB) | RSS (MiB) |
|---|---:|---:|
| Empty, default workers, 40 × 140 terminal | 1.451 | 23.91 |
| Empty, one worker, 40 × 140 terminal | 1.424 | 23.41 |
| Empty, default workers, 10 × 40 terminal | 1.086 | 23.47 |
| Repeat empty/default/40 × 140 | 1.450 | 23.58 |

The smaller viewport saved about 0.36 MiB of live Rust heap. Reducing workers saved
about 0.026 MiB. Initial exploratory native-heap snapshots using the older binary
suggested larger differences (1.09 and 0.58 MiB); those are not used as established
savings. The matched live-heap controls give smaller estimates. One worker is not
qualified for transfer throughput, and a smaller viewport is an attribution
experiment rather than a proposed user setting.

### Empty queue storage, independent synthetic test

An isolated test measured allocations made by actual production message types and
Tokio channels. All measured objects returned live allocations to their starting
level when dropped.

| Object | Requested bytes |
|---|---:|
| Empty manager inbox, capacity 1,000 | 6,432 |
| Empty manager inbox, capacity 1 | 6,432 |
| Same inbox storing boxed messages, capacity 1,000 | 800 |
| Empty incoming-peer inbox, capacity 100 | 6,432 |
| Same incoming-peer inbox storing boxed messages | 800 |
| Empty control inbox | 3,616 |
| Empty manager-event inbox | 3,104 |
| Empty metrics watch | 920 |

Tokio 1.50 allocates a 32-element block on this 64-bit target, even for a smaller
capacity. Lowering the limit alone therefore does not remove this empty cost.
Boxing the two larger inbox item types saves 11,264 bytes per pair: 1.34 MiB for
125 pairs. This is a synthetic storage result, not a production throughput result;
per-message allocations and locality may cost performance. The trace's broader
2.99 MiB channel category includes additional channel types and infrastructure.

A 100,000-piece synthetic seed with 1 MiB pieces retained 900,000 bytes in the
measured PieceManager setup; its download counterpart retained 1,424,288 bytes.
These are component geometries, not complete manager totals. They reinforce that
piece tracking follows content geometry rather than instantaneous transfer speed.

## What can be reduced next

The measurements support concrete candidates, not a claim that 40–60 MiB is
required:

1. Share or lazily load metadata representations, preserving exact info bytes and
   correctness on metadata requests, verification, and resumption.
2. Shrink stored channel item types; test delivery, backpressure, churn, and both
   upload/download throughput before changing defaults.
3. Allocate default per-torrent spare buffers only when needed. The 0.73 MiB
   default-state group includes `pending_disconnects`, `pending_failures`, and
   file-activity buffers initialized with capacity, plus smaller default state.
4. Reduce redundant snapshots/history storage and consider suspending entire idle
   managers while retaining a small admission record.

An inactive torrent can retain much less state if the application reloads it on
arrival of a peer or a scheduled announce. That design has wake-up latency, I/O,
queueing, and correctness costs. We have not implemented or qualified it here.
The empty control demonstrates a much smaller heap without torrent managers, but
removing all managers is not equivalent to keeping the catalog ready to serve.
No lower-bound proof or claim of an unavoidable 20/40/60 MiB minimum is warranted.

## Evidence, isolation, and qualification

- Original catalog/settings/metadata hashes were unchanged after all runs. A macOS
  sandbox denied networking and writes outside each disposable XDG run directory,
  apart from terminal devices and `/dev/null`. Original payloads were read-only.
- Ten new application runs were executed. Six matched diagnostic runs exited 0.
  Four older-binary exploratory runs included a stripped-symbol trace terminated
  with SIGTERM after inspection; it was not used for named allocation attribution.
- Kyoto test job `ed5a22c56e834aad8f90cfb5b5dabb15` successfully compiled the
  `memory-profile,dht,pex` library test configuration under compile-only policy.
  `torrent_manager::memory_experiments::idle_component_attribution` was then run
  directly, alone, with `--ignored --exact --nocapture --test-threads=1`: passed.
- Diagnostic build job `e578cf5d01eb45598637e22bdbe23b59` succeeded:
  `cargo build --release --no-default-features --features memory-profile,dht,pex`,
  with LTO disabled, 16 codegen units, one build worker, and stripping disabled.
  SHA-256: `b25aa0a7824ccabfc8c0fceffb8846302bbde27cae4bc191e7faee9c7ae131b3`.
  Its Cargo.lock identity and source archive are retained; do not assume dependency
  resolution or binary identity matches the earlier experiment's executable.
- `src/memory_probe.rs` is gated by `memory-profile` and starts only with an explicit
  output-file environment variable. It refuses to overwrite an existing file.
  Pressure relief additionally requires an explicit time setting. No default
  production optimization or runtime tuning was introduced in this follow-up.
- Native stack logging adds memory and failed to mark its pages as excluded from
  footprint on this host. The traced run's 66.8 MiB peak RSS is not a normal RAM
  result. It supplies allocation paths only. The `malloc`-filtered call-tree attempt
  returned no entries; attribution uses `malloc_history -allBySize` instead.
- Raw private evidence is under `tmp/memory-lifecycle/real-catalog/` and aggregate
  receipts under `tmp/memory-lifecycle/idle-attribution/`. Local runners are
  `run-catalog-attribution.py`, `run-idle-diagnostics.py`, and
  `run-empty-controls.py` in `tmp/memory-lifecycle/`. Symbol names were demangled
  using Homebrew LLVM's `llvm-cxxfilt` after removing the extra Mach-O underscore.
  `scripts/summarize-idle-attribution.py` reproduces the grouping.
- These are offline attribution experiments, not repeated online performance
  benchmarks. Live peer churn, WebRTC, wake-up behavior, and a hibernating design
  remain unqualified. No installation, commit, or deployment was performed.

## Research references

Apple describes [allocation-history and heap inspection](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/ManagingMemory/Articles/FindingPatterns.html)
and the distinction between [heap activity and memory footprint](https://developer.apple.com/videos/play/wwdc2024/10173/).
Libtorrent's [older 1.2 tuning guide](https://www.libtorrent.org/tuning.html#remove-torrents)
also describes metadata and piece state retained by paused torrents, and the
behavioral tradeoff of removing them from a session. Its old disk-cache settings
were not applied to Superseedr or assumed to describe libtorrent 2.x.
