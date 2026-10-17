# Production peer memory cleanup

Base: develop at 75ff6c80. The experiment is saved separately on `experiment/memory-126` at e03ce88b. Private run artifacts remain in that worktree under `tmp/memory-126/`.

## Scope

- uTP frees its local read buffer after EOF and spare pending-queue capacity after draining. After the complete application stream is dropped, a 30-second FIN-acknowledgement deadline applies only once all queued DATA is acknowledged. A retained read half keeps existing behavior. Normal DATA retransmissions and zero-window queues do not inherit this deadline.
- Peer-policy history maintenance runs every 30 seconds independently of the 20-minute persistence checkpoint. The one-hour inactive history retention and live policy restrictions are preserved. Torrent tables compact on 30 seconds of active ticks and on pause. Empty tables release their allocation; nonempty tables shrink only at capacity >=128 and >4x length, retaining headroom for growth. Reconnect evidence and transfer baselines are not cleared.

## Review

The prototype's blanket owner-drop timeout could discard queued payloads. Production narrows it to FIN-only waiting. Regression tests cover queued delivery after drop, a DATA acknowledgement delayed beyond 30 seconds, a live half-close, zero-window queued payloads, FIN timeout session removal, active-tick table reclamation, unchanged transfer/reconnect evidence, retention boundaries, and timer maintenance without early persistence.

Review also found that cloned watch snapshots could retain the policy table high-water capacity. Policy publication now compacts the source first, and capacity-only maintenance can refresh a retained snapshot without marking persistence dirty. A regression test checks snapshot contents and capacity.

A second review checks ownership across split streams, FIN ordering, timer cancellation, session-map RAII cleanup, dirty policy publication, checkpoint separation, and compaction headroom. No allocator instrumentation, experiment environment flags, metadata sharing, or block-flag clearing is included.

## Qualification

Validation receipts are recorded under `tmp/production-memory/`. The final source at b5bf4d5 passed the following local matrix; source hashes were rechecked after execution:

- All features: 2,420 library tests and 9 integration tests passed, with 13 existing ignored library tests.
- No default features: 2,172 library tests and 9 integration tests passed, with 4 existing ignored library tests.
- Native Clippy for both feature configurations and WASM Clippy passed with warnings denied.
- Six optimized synthetic cases passed: download, upload, connected idle, reconnect churn, and lossy uTP download/upload. Each used 2 private torrents and 16 peers for 45 measured seconds after 5 seconds of warmup. Lossy cases used 1% loss, 1% reorder, and up to 5ms delay. All cases exited successfully without reported protocol/session issues and with zero active sessions after shutdown. Download cases verified pieces; upload cases transferred payload; connected idle and churn assertions passed.

The earlier focused pass passed 258 tests with one existing ignored test, before the final policy-publication adjustment; the full suites above include that adjustment.

The initial external-volume peer-policy run hit the existing checkpoint retry test's one-second disk-write timeout. The failed aggregate is preserved in `test-results-external-volume.json` and `default-policy-external-volume.log`. The complete focused rerun passed using private internal-disk temporary directories, with no timeout or assertion changes.

The initial all-features run failed because the qualification runner injected `SUPERSEEDR_SHARED_CONFIG_DIR`, contradicting a configuration test that expects no override. That first failure poisoned a shared test lock and caused 69 subsequent failures. The failed receipts and log are preserved. Removing the runner override produced the passing full-suite results above using the identical test binary; application code and test assertions were unchanged.

Local code review is complete. GitHub automated review was blocked by the account review quota. At validation recording, macOS, Windows, browser, interop, and Rust checks passed on b5bf4d5; Linux CI was still running. This is not a completed cross-platform release qualification.

Kyoto performs compilation only under the shared policy; the runner executes the resulting test binaries separately. Test builds reuse release dependencies with this crate at opt-level 0 and 256 codegen units to constrain memory; optimized synthetic builds use opt-level 3 with LTO disabled and 16 codegen units. These checks establish functional behavior, not shipping-profile throughput parity.

The experiment's measured memory savings are not a guarantee for this narrower production implementation. Zero-window payload queues retain the existing behavior; the new FIN deadline intentionally does not solve that separate transport-liveness issue. Peak-throughput parity and platform release qualification require their own evidence.
