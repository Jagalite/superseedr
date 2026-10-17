# Selected memory changes on develop

This implementation selects three candidates from the separate memory-lifecycle
experiments. The archived reports below describe broader prototypes; their combined
RAM and throughput numbers must not be attributed to this narrower implementation.

## Implemented scope

1. **Packed block-completion flags.** One bit per 16 KiB block replaces one byte.
   Storage is private to `BlockManager`; other manager code reads it through
   `has_block`. Initialization, completion, duplicate handling, requeue and reset
   semantics remain unchanged. Completed seeds retain their full packed array.
2. **Metrics comparisons without snapshot copies.** Compare the current snapshot
   against the owned previous snapshot using derived equality, temporarily matching
   and then restoring the same three ignored fields (ETA, announce countdown and
   availability revision). Actual publication still clones and retains the snapshot.
   All newly added equality fields remain compared automatically.
3. **Lazy spare vectors.** Four per-torrent vectors start empty instead of reserving
   capacity: disconnects, failures, download file activity and upload file activity.
   They grow normally on first use and retain ordinary vector behavior afterward.

Metadata sharing, completion-time flag clearing, rarity cleanup, upload payload
limits, network buffer changes, log queue changes and allocation instrumentation
are excluded. Cargo dependencies and features are unchanged. No real catalog or
payload data was modified during this implementation.

## Expected memory effects

For the saved offline catalog snapshot (125 displayed entries, 77 metadata-ready
and complete), the 77 torrents represent 10,390,369 block flags. Byte payloads
would occupy 10,390,369 bytes (9.91 MiB); packed payloads need 1,298,835 bytes
(1.24 MiB), a calculated saving of 8.67 MiB. These figures exclude allocator
rounding and object headers and are not a measured process-RSS reduction.

The earlier independent component experiment saved 856,000 requested bytes across
125 default states using lazy vectors. Metrics comparison removed 6,600 temporary
allocations totaling 640,972,800 bytes across 100 deliberately large unchanged
snapshots; this is cumulative allocation churn, not retained RAM. These prior
component measurements are supporting evidence, not a fresh measurement of the
three-change application build.

## Validation

The packed representation has randomized comparison tests against byte flags,
per-operation comparison through updates/resets, explicit byte-boundary tests,
large arrays and independent-clone checks. A metrics property test compares every
publication decision and cached snapshot against the old clone-based reference.
Existing manager tests cover block scheduling, duplicates, verification, requeue,
completion, v1/v2/hybrid geometries and state-machine faults.

Validation uses CI's Rust 1.95 toolchain, selected explicitly in the Kyoto job
environment. Formatting and `git diff --check` passed. Kyoto completed:

- Native Clippy, all targets and all features, warnings denied:
  `20604e20c20347ea9ca707b3e4cbfe87`.
- WebAssembly library Clippy, no default features plus WebTorrent, warnings denied:
  `403de77119f640b1b81e2732ee199268`.
- Library test compilation, default features plus synthetic load:
  `e2bec9a11fff4e93882bdda2ae4b917b`. This used one compiler job and disabled test
  debug information after memory-pressure retries. Kyoto's policy compiled only;
  the resulting binary was then executed directly.

The accompanying CI repair boxes the two configuration payloads in `RuntimeOutcome`
to fix Windows Clippy's large-enum warning, with matching native and browser
callers. Its TUI test run passed 796 tests, with one ignored and no failures.
Before pushing, the same test binary passed 320 torrent-manager tests (11 ignored)
and 104 telemetry tests, with no failures. These runs used `PROPTEST_CASES=256`
and one test thread; they include the new packed-bit and metrics property tests.
Build receipts, source/binary hashes and execution logs are retained locally under
`tmp/ci-runtime-outcome/`.

The earlier orchestration scripts under `tmp/memory-selected/` are not active.
Matched baseline/candidate transfer measurements and the extended property-test
campaign remain unqualified for this selected patch. The planned release
comparison uses identical no-default-feature builds with `dht,pex,synthetic-load`,
LTO disabled and 16 codegen units; it does not qualify WebRTC transfer performance.
Hosted Windows validation remains pending. No allocator instrumentation is installed.

## Archived experiment reports

These reports were preserved from `/Volumes/seed2/Projects/superseedr-memory-lifecycle`.
Their commands, scripts and ignored raw artifacts refer to that worktree, which is
retained with the broader experimental source. Historical reports are not release
qualification for the selected changes.

- [Memory lifecycle experiments](memory-lifecycle-experiments.md)
- [Idle and slow-transfer experiments](idle-memory-experiments.md)
- [Read-only catalog baseline](real-catalog-memory-baseline.md)
- [Idle allocation attribution](idle-memory-attribution.md)
- [Metadata, metrics and runtime experiments](retained-memory-experiments.md)
