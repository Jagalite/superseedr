# Retained metadata, metrics, and runtime experiments

> Archived experiment report. The prototypes and raw artifacts live in the separate
> `superseedr-memory-lifecycle` worktree. See [selected implementation](memory-selected-implementation.md)
> for the narrower changes adopted on develop; measurements below are historical
> and do not describe a build containing only those changes.

These candidates extend the earlier idle-memory experiments. They are experimental,
uncommitted, and not a release recommendation. Measurements use the same executable
with `SUPERSEEDR_RETAINED_VARIANT=baseline|metadata|runtime|metrics|combined` in fresh
processes. Both sides retain the earlier `SUPERSEEDR_MEMORY_VARIANT=combined` changes.

## Changes

- Metadata: retain one immutable encoded info dictionary and let the parsed v1
  piece hashes share a byte slice of it. `Bytes` also makes cloning these two
  fields share ownership. Unknown encoded fields are retained; no metadata is
  deleted from disk. The candidate searches for the complete matching hash byte
  string and leaves empty or unmatched representations unchanged. This does not
  repair pre-existing canonical re-encoding in the full torrent-file parser.
- Runtime: allocate four spare vectors (disconnects, failures, download activity,
  upload activity) on first use instead of reserving storage for every torrent.
  Once used, ordinary vector growth/retention still applies; this is not manager
  suspension or an idle shrinking policy.
- Metrics: compare the current and previous metrics without cloning both peer
  snapshots. The three intentionally ignored fields are temporarily equalized
  in the privately owned previous snapshot and restored before returning. A real
  publication still retains an owned snapshot. This removes temporary allocation
  churn; it does not remove every metrics snapshot or its retained peer bitfields.
- Logging: keep the earlier reduced queue unchanged to isolate the new candidates.
  The prior trace attributed about 0.14 MiB to this queue. Further reductions
  would trade burst tolerance for a comparatively small idle saving.

The new sharing, lazy-reservation and comparison behaviors are opt-in under
`memory-profile`. The `Vec<u8>` to `Bytes` representation change itself is common
to both arms, including shared cloning; therefore these matched results do not
isolate that representation change versus the original application.

## Method and safety

Independent component measurements use a fresh single-test process for each
variant: 100,000-piece metadata; 125 default torrent states; and 100 unchanged
metrics ticks containing 32 peers with 100,000-bitfield entries each. Allocator
counts report requested Rust bytes, not RSS. The metrics test also changes a peer
bit to verify meaningful changes still publish.

The catalog experiment copies configuration/cache into private XDG directories,
blocks all networking and all writes outside its run directory, verifies the
write restriction, and hashes the original configuration before/after. The real
catalog and payload files are never modified. This qualifies offline idle only,
not live tracker/peer behavior. Each arm runs 180 seconds; the final 30 seconds
are used for settled medians. Native heap/VM inspection follows RSS sampling.

Builds use Kyoto, no default features, and `memory-profile,dht,pex`; WebRTC is not
qualified. Release LTO is disabled, codegen units are 16, and symbols are retained.
Kyoto compiles tests under its shared compile-only policy; test binaries are then
executed directly. Raw artifacts, source archive and binary hashes are retained
under `tmp/memory-lifecycle/` and excluded from Git. Raw catalog artifacts may
contain private titles; only aggregate measurements belong in this report.

## Results

Independent component tests passed in all five variants:

| Component | Baseline | Matching candidate | Reduction |
|---|---:|---:|---:|
| 100,000-piece metadata, live bytes | 4,000,116 | 2,000,140 | 1,999,976 (50%) |
| 125 default states, live bytes | 1,118,000 | 262,000 | 856,000 (77%) |
| 100 unchanged peer-snapshot checks, allocated bytes | 640,972,800 | 0 | 100% |
| Same checks, allocation calls | 6,600 | 0 | 100% |

The combined variant reproduced all three reductions. Metadata preserves exact
info bytes and piece hashes. The metrics workload retained its ordinary input
and cached snapshot outside the measured interval: zero means no allocation
inside these unchanged checks, not zero memory for telemetry or a complete
manager tick. The large peer bitfields deliberately stress copying and are not
representative of the zero-peer catalog.

Correctness: 642 focused tests passed in each of the baseline and combined
variants (parser, telemetry, managers, networking, synthetic workload helpers,
and app torrent model). Eleven manager tests remained ignored in each broad
run; the retained-state experiment was separately executed in all five variants.
Eleven logging tests also passed. Total: 1,300 passing test executions, including
repeated variants, not 1,300 distinct tests. Compile-only Kyoto success was not
counted as test execution.

Release build job: `06734954fac748f681c327e69541c5f2`.
Test compilation job: `28d80f3fe2894007a5b0c3e6e43decc0`.
Candidate SHA-256:
`3429e6645af101e50212038b4e12b73d51ca907428f2f84ca524a530594b54d3`.

### Copied catalog, combined candidates

Both runs completed with 125 entries, 77 metadata-ready, zero peers and transfer
rates, and original-file hashes unchanged. Same executable, instrumentation,
terminal dimensions and 180-second duration; settled medians use seconds 150–180.

| Metric | Baseline | Combined |
|---|---:|---:|
| Requested Rust heap median | 21.26 MiB | 17.52 MiB |
| Native allocated heap after sampling | 23.52 MiB | 19.72 MiB |
| RSS median | 26.41 MiB | 43.13 MiB |
| Physical footprint after sampling, vmmap | 36.5M | 35.8M |
| Writable regions swapped out, vmmap | 19.4M | 15.0M |

Live Rust heap fell **3.74 MiB (17.6%)**; native allocated heap fell **3.80 MiB**.
RSS increased because the processes had materially different residency/swap
conditions. This pair demonstrates reduced allocations, not reduced RSS.
Physical footprint improved only modestly. Neither RSS value should replace the
older 52 MiB observation as a stable application minimum. Native inspection takes
place after sampling, so its totals are adjacent snapshots rather than identical
instants. These are one pair of runs, not repeated estimates with confidence
intervals. The independent component tests isolate the changes more cleanly.

Raw runs: `tmp/memory-lifecycle/real-catalog/retention-{baseline,combined}180`.
Reproduce aggregate output with `scripts/summarize-retained-memory.py` and these
two directories.

### Transfer, idle-peer and low-rate checks

All 22 runs passed: six main cases (download, upload, completion, churn, v2,
hybrid), three idle cases (download, upload, churn), and two rate-limited cases,
each with baseline and combined candidates. Each used two torrents, 16 TCP peers,
10 seconds of measurement and 2 seconds of warmup. There were no protocol errors,
unexpected session failures or active sessions after shutdown. Idle cases sent
zero payload bytes; idle churn established 96 sessions and performed 80 planned
disconnects in each variant. Manager command probes remained responsive.

| Case | Baseline median live heap, MiB | Combined median live heap, MiB |
|---|---:|---:|
| Idle download peers | 2.279 | 2.265 |
| Idle upload peers | 2.271 | 2.243 |
| Idle peer churn | 2.305 | 2.288 |
| Rate-limited download | 10.811 | 10.762 |
| Rate-limited upload | 19.287 | 19.327 |

These small-torrent workloads show little retained-memory benefit from the new
candidates: their metadata is small and there are only two managers. They are
correctness controls, not a demonstration of the catalog-sized saving. Low-rate
runs requested `--target-gbps 0.001`; observed rates were 0.133/0.128 MiB/s for
download and 0.998/1.000 MiB/s for upload (baseline/combined). The requested target
is therefore not evidence that upload achieved a precise 1 Mbit/s aggregate cap.

The single unthrottled upload pair measured **463.1 → 431.0 MiB/s (-6.9%)**.
Other cases varied in the opposite direction. These short, one-repetition runs
cannot establish a throughput regression or equivalence; repeated alternating
runs remain a release gate. Several download cases completed during warmup, so
their reported steady-state download rate is zero despite successful transfers.
Generated payloads inflate startup peaks. Do not interpret those peaks as idle
application requirements. Across all 22 runs, requested heap after return was
below 0.67 MiB; this is a harness cleanup checkpoint, not proof against long-term
leaks. WebRTC, long-duration operation, real-network catalog activity and normal
release-profile behavior remain unqualified.

Raw results and aggregate summaries are under
`tmp/memory-lifecycle/retention-synthetic/`; isolated tests and logs are under
`tmp/memory-lifecycle/retention-tests/`.

## Further targets and limits

In the previous catalog inventory, encoded info dictionaries totaled 3,310,215
bytes and their piece hashes totaled 3,296,800 bytes. Almost all those encoded
bytes are hashes, so stripping descriptive fields from that representation would
save little. Sharing removes the duplicate; dropping the remaining hash table
would require a separate design that reloads it for verification and metadata
requests. It cannot be treated as unused simply because transfer speed is zero.

The prior empty-channel experiment found a smaller queue capacity did not reduce
the initial block allocation. Smaller stored message types could save about
1.34 MiB across 125 pairs of the two measured inbox types, but boxing adds
per-message allocation and needs throughput/backpressure qualification. Histories,
diagnostics, and idle-manager suspension are other distinct candidates. None of
these estimates is an established RSS saving or a minimum-memory bound.
