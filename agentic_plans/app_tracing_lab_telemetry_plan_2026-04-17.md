# App Tracing, Lab Tooling, and Telemetry Plan

## Summary
- superseedr should treat deep tracing, benchmarking, soak runs, and replay tooling as lab infrastructure, not as ad hoc production-app logic.
- Production code should keep only cheap, always-on health and status reporting.
- Heavy tracing and benchmark capture should move behind a dedicated feature and tooling boundary.
- The immediate DHT need is to stop growing `main.rs` benchmark logic, while preserving the current DHT benchmark and replay capabilities that made the BEP 5 parity work possible.

## Goals
- Keep production runtime behavior simple, cheap, and predictable.
- Preserve strong engineering tools for DHT parity, soak testing, and replay debugging.
- Create a path to future app-wide tracing instead of subsystem-specific one-off tracing code.
- Avoid coupling product startup, UI flow, or service behavior to benchmark-only instrumentation.

## Non-Goals
- This plan does not require a full tracing framework to be implemented immediately.
- This plan does not remove current DHT benchmark tooling right away.
- This plan does not replace lightweight production health/status reporting.

## Design Principles
- Production telemetry should be cheap.
- Lab tooling should be explicit.
- Trace capture should be sink-driven, not hand-written file output scattered through runtime code.
- Benchmarking should operate through service boundaries whenever practical.
- Long-running soaks and parity measurements should not live in default `cargo test` paths.

## Desired End State
- Production build:
- keeps runtime health counters, status snapshots, warnings, and compact operator-visible diagnostics
- does not carry benchmark orchestration, soak-run plumbing, or file trace management in the normal app path
- Lab build:
- enables heavy tracing, replay tooling, benchmark runners, soak drivers, and trace comparison tools
- can emit structured JSONL or similar event logs through pluggable sinks
- App architecture:
- exposes a small app-wide instrumentation surface
- routes events to one or more sinks
- uses a no-op sink in normal builds when heavy tracing is disabled

## Recommended Feature Split
- `dht`
- keeps the real DHT runtime and service boundary
- `pex`
- unchanged
- `lab`
- future app-wide heavy tooling feature
- enables trace sinks, benchmark runners, replay helpers, deep capture, and engineering-only diagnostics
- `dht-lab`
- optional short-term stepping stone if a narrower DHT-only gate is needed before a broader `lab` feature exists

## Architecture Direction

### Production Telemetry
- Keep always-on health snapshots and warnings in subsystem code.
- Keep runtime counters cheap and in-memory.
- Allow the app/UI/CLI to query subsystem health without needing trace files or benchmark logic.

### Instrumentation API
- Introduce a small event surface later, shared across subsystems.
- Examples:
- lookup start/finish
- peer batch emitted
- tracker announce start/finish
- peer session connect/disconnect
- persistence load/save
- reconfigure started/completed
- This should be app-wide, not DHT-specific, once implemented.

### Sinks
- Use a sink abstraction instead of direct benchmark-specific file writing in runtime code.
- Sink types:
- no-op sink
- structured logger sink
- JSONL trace sink
- benchmark metrics sink
- replay capture sink
- Production should default to no-op or minimal logging sinks.
- Heavy sinks should be feature-gated.

### Tooling Boundary
- Move engineering commands out of the normal app CLI path over time.
- Preferred short-term landing zone:
- `src/bin/dht_lab.rs`
- Preferred longer-term landing zone:
- `src/bin/lab.rs` or a small tools crate once multiple subsystems need the same pattern

## DHT Section

### Current DHT Benchmarking and Testing
- Current DHT differential and soak tooling exists and was necessary to reach parity and stability.
- Current deterministic correctness coverage includes:
- lookup-state scripted replay tests
- runtime scripted UDP replay tests
- feature-off compile checks to ensure real DHT code is excluded from private builds
- Current live tooling includes:
- `dht-benchmark`
- `analyze-dht-stability`
- `compare-dht-traces`
- Current live/parity workflow includes:
- local seeded testnet checks
- live corpus parity runs
- long soak runs against the internal backend
- trace-path driven capture for differential debugging
- Current production-adjacent telemetry used during DHT work includes:
- DHT health snapshot reporting
- route counts
- inflight query counts
- warning state
- backend identity in benchmark output

### Current DHT Structural Problem
- Much of the current benchmark and analysis flow still lives in the main application CLI path.
- Trace-path handling and benchmark-specific orchestration currently grow `main.rs` and related command plumbing.
- That is acceptable for the parity push that just finished, but it should not become the long-term pattern for other subsystems.

### DHT Near-Term Refactor Goal
- Keep the current DHT runtime behavior and correctness tests.
- Extract benchmark/analyze/trace orchestration out of the main app path.
- Preserve the existing DHT corpus benchmark and soak capability without requiring production codepaths to know about trace file management.

### DHT Target End State
- Keep in production:
- `DhtService` / `DhtHandle`
- health/status snapshots
- warnings
- compact operator-visible diagnostics
- Move to lab tooling:
- corpus benchmark runner
- soak runner
- trace comparison
- deep trace capture
- replay harness helpers that are not part of ordinary correctness tests

## Phases

### Phase 0: Inventory and Freeze
- Treat the current DHT benchmark/testing setup as the baseline.
- Do not add more benchmark-specific branching in the production app path unless needed to fix correctness.
- Record which commands, outputs, and tests must survive extraction.

### Phase 1: DHT Lab Extraction
- Create a DHT lab binary or equivalent lab entrypoint.
- Move:
- `dht-benchmark`
- `analyze-dht-stability`
- `compare-dht-traces`
- benchmark-only trace-path orchestration
- Keep deterministic unit/runtime replay tests in normal test code.
- Keep production health snapshots in normal runtime code.

### Phase 2: Sink-Based Instrumentation Core
- Introduce a minimal instrumentation interface.
- Provide a no-op sink by default.
- Port DHT trace capture to use that sink instead of direct benchmark-specific file output logic.

### Phase 3: Generalize Beyond DHT
- Expand the instrumentation interface to trackers, peer sessions, persistence, and reconfigure flow.
- Add a lab feature or app-wide telemetry feature once at least two subsystems benefit from the same model.

### Phase 4: Production Hardening
- Make sure heavy sinks are fully excluded or dormant in normal builds.
- Keep always-on metrics bounded and cheap.
- Ensure lab tooling cannot accidentally change production runtime behavior.

## Guardrails
- Do not let benchmarking logic alter runtime control flow in production mode.
- Do not make production startup depend on trace file setup.
- Do not move long soak tests into default `cargo test`.
- Do not couple subsystem correctness to a benchmark-only environment variable.
- Do not reintroduce DHT-only one-off tracing abstractions if the longer-term direction is app-wide telemetry.

## Acceptance Criteria
- Production build path still exposes lightweight status/health only.
- DHT parity/soak/replay workflows remain available after extraction.
- Engineering trace capture works through a sink or lab boundary, not scattered file-writing code.
- Private builds can still exclude real DHT code.
- The pattern is reusable for non-DHT subsystems later.

## Immediate Recommendation
- Leave the current DHT benchmarking and soak setup in place until the branch is pushed and stable.
- After that, do a focused extraction pass:
- first move the DHT benchmark/analyze/compare commands out of `main.rs`
- then introduce a small sink abstraction for DHT trace capture
- only after that, broaden the model into a full app tracing feature
